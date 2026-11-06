// 邦信 · P2P 传输内核 sidecar 的宿主管理（Linux 专用）
// ─────────────────────────────────────────────────────────────
// 存在理由：Linux 发行版 webkit2gtk 编译时关闭了 WebRTC（ENABLE_WEB_RTC=OFF），
// WebView 里 typeof RTCPeerConnection === 'undefined'，大文件直传在 Linux 端从架构上
// 不可能成立。这里拉起一个 Go + pion 写的原生传输内核，把 WebRTC 挪出 WebView。
//
// Windows / macOS 的 WebView 自带 WebRTC，不启 sidecar —— `p2p_sidecar_info` 直接返回 null，
// 前端据此继续用原生 RTCPeerConnection。两端行为一致，前端不需要区分平台。
//
// 与前端约定：`p2p_sidecar_info` 返回 { port, token }，前端连 ws://127.0.0.1:<port>/?token=<token>。
// 令牌由本模块随机生成并只通过 IPC 下发，防止同机其它进程蹭用这条本地通道。

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct Proc {
    child: Child,
    port: u16,
    token: String,
}

static PROC: Mutex<Option<Proc>> = Mutex::new(None);

/// sidecar 可执行文件位置。
///
/// Tauri 的 externalBin 打包时会去掉 target-triple 后缀，把二进制放到主程序同目录
/// （AppImage 里是 usr/bin/）。这里按「同目录 → ../bin」的顺序找，找不到就返回 None，
/// 前端会退回「无直传能力」而不是崩溃。
fn locate() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let mut cands = vec![dir.join("x8p2p-sidecar"), dir.join("x8p2p-sidecar.exe")];
    if let Some(parent) = dir.parent() {
        cands.push(parent.join("bin").join("x8p2p-sidecar"));
    }
    cands.into_iter().find(|p| p.is_file())
}

fn make_token() -> String {
    // 不引第三方依赖：时间纳秒 + pid + 地址熵，做一次 FNV-1a 足够
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!("{}-{}-{:p}", nanos, std::process::id(), &nanos);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in seed.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{:016x}", h)
}

/// 确保 sidecar 在跑；已在跑就直接返回现有端口与令牌。
///
/// 崩溃自愈：进程若已退出（try_wait 有结果），这里会重新拉起 —— 前端每次建连都调
/// 这个命令，所以传输中途 sidecar 挂掉后，下一次发送会自动换到新实例上。
pub fn ensure() -> Option<(u16, String)> {
    let mut guard = match PROC.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };

    if let Some(p) = guard.as_mut() {
        match p.child.try_wait() {
            Ok(Some(_)) => {
                log_line("sidecar 已退出，重新拉起");
                *guard = None;
            }
            Ok(None) => return Some((p.port, p.token.clone())),
            Err(_) => {
                *guard = None;
            }
        }
    }

    let bin = match locate() {
        Some(b) => b,
        None => {
            log_line("未找到 x8p2p-sidecar，P2P 直传不可用");
            return None;
        }
    };
    let token = make_token();

    let mut cmd = Command::new(&bin);
    cmd.args([
        "-listen",
        "127.0.0.1:0",
        "-token",
        &token,
        // 虚拟网卡会给出「看着像直连、其实是隧道」的候选，掩盖真实打洞结果
        "-ifskip",
        "tailscale,docker,virbr,veth,br-",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            log_line(&format!("拉起 sidecar 失败: {e}"));
            return None;
        }
    };

    // 从 stdout 读第一行 X8P2P_READY 拿端口；读超时就认定启动失败
    let stdout = child.stdout.take();
    let (tx, rx) = mpsc::channel::<Option<u16>>();
    if let Some(out) = stdout {
        std::thread::spawn(move || {
            let mut reader = BufReader::new(out);
            let mut first = true;
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let t = line.trim();
                        if first {
                            first = false;
                            let port = parse_ready(t);
                            let _ = tx.send(port);
                            if port.is_none() {
                                return;
                            }
                        }
                    }
                }
            }
        });
    } else {
        let _ = tx.send(None);
    }

    let port = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Some(p)) => p,
        _ => {
            log_line("sidecar 启动超时");
            let _ = child.kill();
            return None;
        }
    };

    log_line(&format!("sidecar 就绪 pid={} port={}", child.id(), port));
    *guard = Some(Proc {
        child,
        port,
        token: token.clone(),
    });
    Some((port, token))
}

/// 解析 `X8P2P_READY {"port":12345,"token":"..."}` 里的端口
fn parse_ready(line: &str) -> Option<u16> {
    if !line.starts_with("X8P2P_READY") {
        return None;
    }
    let i = line.find("\"port\"")?;
    let rest = &line[i + 6..];
    let colon = rest.find(':')?;
    let digits: String = rest[colon + 1..]
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse::<u16>().ok()
}

pub fn stop() {
    let mut guard = match PROC.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    if let Some(mut p) = guard.take() {
        let _ = p.child.kill();
        let _ = p.child.wait();
        log_line("sidecar 已停止");
    }
}

fn log_line(s: &str) {
    eprintln!("[x8-p2p] {s}");
}
