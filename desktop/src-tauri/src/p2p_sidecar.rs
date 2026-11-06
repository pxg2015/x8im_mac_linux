// 邦信 · P2P 传输内核 sidecar 的宿主管理（Linux 专用）
// ─────────────────────────────────────────────────────────────
// 存在理由：Linux 发行版 webkit2gtk 编译时关闭了 WebRTC（ENABLE_WEB_RTC=OFF），
// WebView 里 typeof RTCPeerConnection === 'undefined'，大文件直传在 Linux 端从架构上
// 不可能成立。这里拉起 Go + pion 写的原生传输内核，把 WebRTC 挪出 WebView。
//
// 🔴 为什么不用「本地 WebSocket」（2026-11-06 实测结论）：
// WebKitGTK 会把 https 页面里的 ws:// / http:// 当混合内容直接拦掉，**连 127.0.0.1
// 和 localhost 都不豁免**（同机实测：https 页面 → ws:// 必 error，http 页面 → open）。
// 页面又必须走 https，所以本地 WS 在 Linux 端根本走不通。
// 现在改成：sidecar 用 `-stdio` 起，双方在管道上走帧协议；前端通过 Tauri IPC 转发
// （invoke 天然不受混合内容限制）。进程与数据流全程不出本机。
//
// 帧格式（双向一致）：[4B 小端 payloadLen][1B op][payload]
//   op=1 文本（控制协议 JSON）  op=2 二进制（[1B flag][数据]，保留 DC 的 string/binary 语义）
//
// 一个 sidecar 进程 = 一个传输会话（等价于原来「一条 WS = 一个会话」），所以这里是个
// 进程池：并发传多个文件就各起一个，用完即杀。12MB 静态二进制，起停很便宜。
//
// Windows / macOS 的 WebView 自带 WebRTC，不启 sidecar —— `p2p_sidecar_info` 返回 null。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// sidecar 一帧（op=1 控制 JSON / op=2 DataChannel 数据）
pub struct Frame {
    pub op: u8,
    pub payload: Vec<u8>,
}

struct Proc {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    rx: Arc<Mutex<mpsc::Receiver<Result<Frame, String>>>>,
}

static POOL: Mutex<Option<HashMap<u32, Proc>>> = Mutex::new(None);
static SEQ: AtomicU32 = AtomicU32::new(1);

const MAX_FRAME: usize = 8 * 1024 * 1024;

/// sidecar 可执行文件位置。
///
/// Tauri 的 externalBin 打包时会去掉 target-triple 后缀，把二进制放到主程序同目录
/// （AppImage 里是 usr/bin/）。这里按「同目录 → ../bin」的顺序找，找不到就返回 None，
/// 前端会退回「无直传能力」而不是崩溃。
fn locate() -> Option<PathBuf> {
    // 开发/回归用：X8_P2P_BIN 直接指定二进制（cargo test、以及 Windows 上强制走 IPC 通道时）
    if let Ok(p) = std::env::var("X8_P2P_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let mut cands = vec![dir.join("x8p2p-sidecar"), dir.join("x8p2p-sidecar.exe")];
    if let Some(parent) = dir.parent() {
        cands.push(parent.join("bin").join("x8p2p-sidecar"));
    }
    cands.into_iter().find(|p| p.is_file())
}

/// 只回答「本平台能不能用直传内核」，不拉起进程（拉起由前端按需 open）
pub fn available() -> bool {
    locate().is_some()
}

/// 拉起一个 sidecar 会话，等到握手帧后返回会话 id。
pub fn open() -> Result<u32, String> {
    let bin = locate().ok_or_else(|| "sidecar_missing".to_string())?;

    let mut cmd = Command::new(&bin);
    cmd.args([
        "-stdio",
        // 虚拟网卡会给出「看着像直连、其实是隧道」的候选，掩盖真实打洞结果
        "-ifskip",
        "tailscale,docker,virbr,veth,br-",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("sidecar_spawn: {e}"))?;
    let pid = child.id();
    let stdin = child.stdin.take().ok_or("sidecar_no_stdin")?;
    let stdout = child.stdout.take().ok_or("sidecar_no_stdout")?;

    // 🔴 stderr 必须有人读：sidecar 的日志走 stderr（log.Printf），管道缓冲（64KB）写满后
    // 它会阻塞在 write 上，整个传输内核假死 —— 表现就是「传过一次之后每次都失败」。
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            let reader = BufReader::new(err);
            for line in reader.lines().map_while(Result::ok) {
                let t = line.trim();
                if !t.is_empty() {
                    log_line(&format!("[sidecar] {t}"));
                }
            }
        });
    }

    // 读线程：先交付握手帧（op=1 ready），随后持续把数据帧推进会话队列。
    let (tx, rx) = mpsc::channel::<Result<Frame, String>>();
    let tx2 = tx.clone();
    let mut out = stdout;
    std::thread::spawn(move || {
        match read_frame(&mut out) {
            Ok(f) if f.op == 1 => {
                if tx2.send(Ok(f)).is_err() {
                    return;
                }
            }
            Ok(_) => {
                let _ = tx2.send(Err("sidecar_bad_handshake".to_string()));
                return;
            }
            Err(e) => {
                let _ = tx2.send(Err(e));
                return;
            }
        }
        loop {
            match read_frame(&mut out) {
                Ok(f) => {
                    if tx2.send(Ok(f)).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx2.send(Err(e));
                    return;
                }
            }
        }
    });

    match rx.recv_timeout(Duration::from_secs(8)) {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            let _ = child.kill();
            return Err(e);
        }
        Err(_) => {
            let _ = child.kill();
            return Err("sidecar_handshake_timeout".to_string());
        }
    }

    let id = SEQ.fetch_add(1, Ordering::Relaxed);
    let proc = Proc {
        child,
        stdin: Arc::new(Mutex::new(stdin)),
        rx: Arc::new(Mutex::new(rx)),
    };
    let mut guard = pool_lock();
    guard.get_or_insert_with(HashMap::new).insert(id, proc);
    log_line(&format!("sidecar stdio 会话就绪 id={id} pid={pid}"));
    Ok(id)
}

/// 发一帧给 sidecar
pub fn send(id: u32, op: u8, payload: &[u8]) -> Result<(), String> {
    let stdin = {
        let guard = pool_lock();
        guard
            .as_ref()
            .and_then(|m| m.get(&id))
            .map(|p| p.stdin.clone())
    }
    .ok_or_else(|| "sidecar_no_session".to_string())?;

    let mut w = stdin.lock().map_err(|_| "sidecar_lock".to_string())?;
    write_frame(&mut *w, op, payload).map_err(|e| format!("sidecar_write: {e}"))
}

/// 收一帧；超时返回 Ok(None)（前端据此继续轮询，不当作错误）
pub fn recv(id: u32, timeout_ms: u64) -> Result<Option<Frame>, String> {
    let rx = {
        let guard = pool_lock();
        guard
            .as_ref()
            .and_then(|m| m.get(&id))
            .map(|p| p.rx.clone())
    }
    .ok_or_else(|| "sidecar_no_session".to_string())?;

    let ch = rx.lock().map_err(|_| "sidecar_lock".to_string())?;
    match ch.recv_timeout(Duration::from_millis(timeout_ms.max(1))) {
        Ok(Ok(f)) => Ok(Some(f)),
        Ok(Err(e)) => Err(e),
        Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err("sidecar_closed".to_string()),
    }
}

/// 结束一个会话（进程一并回收）
pub fn close(id: u32) {
    let proc = {
        let mut guard = pool_lock();
        guard.as_mut().and_then(|m| m.remove(&id))
    };
    if let Some(mut p) = proc {
        let _ = p.child.kill();
        let _ = p.child.wait();
        log_line(&format!("sidecar 会话关闭 id={id}"));
    }
}

/// 应用退出时回收全部 sidecar
pub fn stop_all() {
    let all = {
        let mut guard = pool_lock();
        guard.take()
    };
    if let Some(map) = all {
        for (id, mut p) in map {
            let _ = p.child.kill();
            let _ = p.child.wait();
            log_line(&format!("sidecar 已停止 id={id}"));
        }
    }
}

/* ------------------------------------------------------------------ */
/* 帧编解码                                                            */
/* ------------------------------------------------------------------ */

fn read_frame<R: Read>(r: &mut R) -> Result<Frame, String> {
    let mut h = [0u8; 5];
    r.read_exact(&mut h).map_err(|e| format!("sidecar_read: {e}"))?;
    let n = u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize;
    if n > MAX_FRAME {
        return Err(format!("sidecar_frame_too_large: {n}"));
    }
    let mut buf = vec![0u8; n];
    if n > 0 {
        r.read_exact(&mut buf).map_err(|e| format!("sidecar_read: {e}"))?;
    }
    Ok(Frame { op: h[4], payload: buf })
}

fn write_frame<W: Write>(w: &mut W, op: u8, payload: &[u8]) -> std::io::Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    let mut h = [0u8; 5];
    h[0..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    h[4] = op;
    w.write_all(&h)?;
    w.write_all(payload)?;
    w.flush()
}

/* ------------------------------------------------------------------ */
/* base64（不引第三方依赖：标准字母表 + padding 就够）                  */
/* ------------------------------------------------------------------ */

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    let mut buf = [0u8; 4];
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        buf[0] = B64[((n >> 18) & 63) as usize];
        buf[1] = B64[((n >> 12) & 63) as usize];
        buf[2] = if chunk.len() > 1 { B64[((n >> 6) & 63) as usize] } else { b'=' };
        buf[3] = if chunk.len() > 2 { B64[(n & 63) as usize] } else { b'=' };
        if let Ok(s) = std::str::from_utf8(&buf) {
            out.push_str(s);
        }
    }
    out
}

pub fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut rev = [255u8; 256];
    for (i, c) in B64.iter().enumerate() {
        rev[*c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut nbits = 0;
    for &c in s.as_bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = rev[c as usize];
        if v == 255 {
            return Err("bad_base64".to_string());
        }
        acc = (acc << 6) | v as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push(((acc >> nbits) & 0xff) as u8);
        }
    }
    Ok(out)
}

fn pool_lock() -> std::sync::MutexGuard<'static, Option<HashMap<u32, Proc>>> {
    match POOL.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

fn log_line(s: &str) {
    eprintln!("[x8-p2p] {s}");
}

/* ------------------------------------------------------------------ */
/* 回归：cargo test 里跑一遍真实管道握手（不依赖 Tauri / WebView）       */
/* ------------------------------------------------------------------ */
//
// 跑法（Windows 本机即可，用 Go 现编一个 sidecar）：
//   X8_P2P_BIN=E:\AI\X8_Im\_tmp_sidecar.exe cargo test bridge_ -- --nocapture
#[cfg(test)]
mod tests {
    use super::*;

    fn skip() -> bool {
        std::env::var("X8_P2P_BIN").map(|p| PathBuf::from(p).is_file()).unwrap_or(false) == false
    }

    #[test]
    fn bridge_handshake_and_pipe_speed() {
        if skip() {
            eprintln!("跳过：未设置 X8_P2P_BIN");
            return;
        }
        let id = open().expect("open session");

        // init → sidecar 必回 ready，一次往返就同时验证了收发两条管道
        send(id, 1, br#"{"t":"init"}"#).expect("send init");
        let f = recv(id, 5000).expect("recv").expect("no frame");
        let s = String::from_utf8_lossy(&f.payload).to_string();
        assert!(s.contains("ready"), "期望 ready，实际: {s}");

        // 吞吐：64KB 帧 × 320 = 20MB（此刻 dc 还没建，sidecar 收下即丢，测的是管道本身）
        let payload = vec![7u8; 64 * 1024 + 1];
        let n = 320;
        let t0 = std::time::Instant::now();
        for _ in 0..n {
            send(id, 2, &payload).expect("send data");
        }
        let dt = t0.elapsed().as_secs_f64();
        let mb = (payload.len() * n) as f64 / 1048576.0;
        eprintln!("管道吞吐: {mb:.1}MB / {dt:.2}s = {:.1}MB/s", mb / dt);
        assert!(mb / dt > 5.0, "管道吞吐过低: {:.1}MB/s", mb / dt);

        close(id);
    }
}
