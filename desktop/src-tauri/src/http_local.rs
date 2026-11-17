// ============================================================================
// 本地落盘 HTTP 服务（2026-11-06 首版；2026-11-17 从 main.rs 拆出，逻辑零改动）
// ----------------------------------------------------------------------------
// 为什么单独成文件：main.rs 一份文件里同时装「窗口/托盘/下载/更新/桥/落盘」六件事，
// 改一处要在一万行里找锚点。这里只装「127.0.0.1 上那个极简 HTTP 服务」：
// 本地落盘（大文件绕开 IPC）、/x8ipc/ 命令兜底通道、/ping 探活。
// 父模块关系：命令实现函数仍住在 main.rs，本文件用 `use super::*` 引用，行为不变。
// ============================================================================
use super::*;
use super::bridge::x8bridge_dispatch;

// ==================== 本地落盘 HTTP 服务（2026-11-06） ====================
// 起因：P2P 收到的 152.9MB 文件在桌面端点「打开/保存」时永远卡在「分块写入中…」。
// 实测证据：`%USERPROFILE%\Downloads\downfiles` 从未被创建、磁盘上无任何 `.part` 残留，
// 说明前端 `invoke` 连**几百字节**的 `save_chunk_begin` 都没送达 Rust —— WebView2 的
// IPC（postMessage 桥）在本机表现就是「发了没回」，Promise 永挂，不是 payload 大小问题。
//
// 处置：不再经过 IPC 传二进制。Rust 在 127.0.0.1 起一个极简 HTTP 服务，前端 `XHR` 直接
// POST Blob（Chromium 把 127.0.0.1 视为可信来源，https 页面请求不触发 mixed-content 拦截），
// 后端按 Content-Length 流式写盘，内存占用与文件大小无关。
//
// 约束：
//   · 只监听回环，不对外
//   · 落盘目录仍走 save_target_dir（防穿越），文件名仍走 sanitize_filename
//   · 只接受 im.x8fde.com / tauri 外壳来源（Origin 校验），空 Origin（本机直连）放行
const SAVE_HTTP_PORTS: std::ops::Range<u16> = 47615..47625;

/// 并发连接上限（2026-11-17）：原实现每连接直接 spawn 一条线程、完全没上限，
/// 配合 1800s 读超时，任何一个卡住的连接都能长期占一条线程；本地被扫描/前端连发会把线程堆上去。
/// 24 条足够覆盖「主业务 IPC + 落盘 + 下载」的正常并发，超限直接 503 关连接，不排队。
const SAVE_HTTP_MAX_CONN: usize = 24;
static SAVE_HTTP_CONNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 在飞连接计数守卫：拿到即占一个名额，drop 自动归还（线程 panic 也会归还）。
struct SaveConnGuard;

impl SaveConnGuard {
    fn acquire() -> Option<SaveConnGuard> {
        use std::sync::atomic::Ordering::SeqCst;
        let n = SAVE_HTTP_CONNS.fetch_add(1, SeqCst) + 1;
        if n > SAVE_HTTP_MAX_CONN {
            SAVE_HTTP_CONNS.fetch_sub(1, SeqCst);
            return None;
        }
        Some(SaveConnGuard)
    }
}

impl Drop for SaveConnGuard {
    fn drop(&mut self) {
        SAVE_HTTP_CONNS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 启动本地落盘服务（幂等：绑定失败说明端口被占，换下一个；全占则直接放弃，
/// 前端探测不到会自动退回 IPC / 浏览器下载）。
pub(crate) fn save_http_start(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let mut bound: Option<std::net::TcpListener> = None;
        for port in SAVE_HTTP_PORTS {
            if let Ok(l) = std::net::TcpListener::bind(("127.0.0.1", port)) {
                bound = Some(l);
                break;
            }
        }
        let listener = match bound {
            Some(l) => l,
            None => return,
        };
        for conn in listener.incoming() {
            if let Ok(stream) = conn {
                match SaveConnGuard::acquire() {
                    Some(g) => {
                        let a = app.clone();
                        std::thread::spawn(move || {
                            let _g = g;
                            let _ = save_http_handle(stream, &a);
                        });
                    }
                    None => {
                        let mut s = stream;
                        let cors = save_http_cors("");
                        let _ = save_http_reply(&mut s, 503, &cors, "{\"ok\":false,\"error\":\"busy\"}");
                        let _ = s.shutdown(std::net::Shutdown::Both);
                    }
                }
            }
        }
    });
}

/// 查询参数百分号解码（文件名/subdir 用；+ 视为空格）
fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn save_http_cors(origin: &str) -> String {
    let allow = if origin.contains("im.x8fde.com")
        || origin.contains("tauri.localhost")
        || origin.contains("localhost")
        || origin.is_empty()
    {
        if origin.is_empty() { "*".to_string() } else { origin.to_string() }
    } else {
        "null".to_string()
    };
    format!(
        "Access-Control-Allow-Origin: {}\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nAccess-Control-Allow-Private-Network: true\r\nAccess-Control-Max-Age: 86400\r\n",
        allow
    )
}

fn save_http_reply(
    s: &mut std::net::TcpStream,
    code: u16,
    cors: &str,
    body: &str,
) -> std::io::Result<()> {
    use std::io::Write;
    let reason = match code {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let msg = format!(
        "HTTP/1.1 {} {}\r\n{}Content-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        code,
        reason,
        cors,
        body.as_bytes().len(),
        body
    );
    s.write_all(msg.as_bytes())?;
    s.flush()
}

fn save_http_handle(mut s: std::net::TcpStream, app: &tauri::AppHandle) -> std::io::Result<()> {
    use std::io::{Read, Write};
    // 2026-11-17：1800s → 120s。这是**单次 read 的空闲超时**（不是总时长），
    // 持续传输不受影响；但连接被卡住/对端不吭声时，线程最多占 2 分钟而不是半小时。
    s.set_read_timeout(Some(std::time::Duration::from_secs(120)))?;

    // 1) 读请求头（到 \r\n\r\n 为止），已读到的多余字节属于 body 开头
    let mut head: Vec<u8> = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    let hdr_end = loop {
        if let Some(p) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        let n = s.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&tmp[..n]);
        if head.len() > 256 * 1024 {
            let cors = save_http_cors("");
            return save_http_reply(&mut s, 400, &cors, "{\"ok\":false,\"error\":\"head_too_large\"}");
        }
    };
    let head_str = String::from_utf8_lossy(&head[..hdr_end]).to_string();
    let pre_body = head[hdr_end + 4..].to_vec();

    let mut lines = head_str.split("\r\n");
    let req_line = lines.next().unwrap_or("").to_string();
    let rp: Vec<&str> = req_line.split(' ').collect();
    let method = rp.first().copied().unwrap_or("").to_string();
    let target = rp.get(1).copied().unwrap_or("/").to_string();

    let mut clen: usize = 0;
    let mut origin = String::new();
    for l in lines {
        let low = l.to_ascii_lowercase();
        if let Some(v) = low.strip_prefix("content-length:") {
            clen = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = low.strip_prefix("origin:") {
            origin = v.trim().to_string();
        }
    }

    let cors = save_http_cors(&origin);
    if origin.contains("im.x8fde.com")
        || origin.contains("tauri.localhost")
        || origin.contains("localhost")
        || origin.is_empty()
    {
        // 放行
    } else {
        return save_http_reply(&mut s, 403, &cors, "{\"ok\":false,\"error\":\"bad_origin\"}");
    }

    if method == "OPTIONS" {
        return save_http_reply(&mut s, 204, &cors, "");
    }

    let (path_only, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };

    if path_only == "/ping" {
        let body = format!("{{\"ok\":true,\"svc\":\"x8save\",\"pid\":{}}}", std::process::id());
        return save_http_reply(&mut s, 200, &cors, &body);
    }

    // POST /x8ipc/<命令名>　body = 参数 JSON（2026-11-16）
    // 为什么有这条：WebView2 的 invoke 桥在部分机器上会整体失效（首条命令 0ms 返回、
    // 回包却回不到 JS，此后命令全发不出去，实证见 ipc_probe.log）。而 HTTP 这条通道
    //（主业务 + 本地落盘）一直稳。前端 inv() 优先走这里，不可用才回退 IPC。
    // 回包约定：200 + {ok:true,data} / 200 + {ok:false,error}（业务失败）/ 404（不接管，前端回退）。
    if let Some(cmd) = path_only.strip_prefix("/x8ipc/") {
        // 路径可能带 %3A / %7C 之类的编码（前端 encodeURIComponent 过的命令名），
        // 解码一次再分发，否则 plugin:updater|check 这类会认不出来。
        let cmd = pct_decode(cmd.trim_matches('/'));
        if cmd.is_empty() {
            return save_http_reply(&mut s, 400, &cors, "{\"ok\":false,\"error\":\"no_cmd\"}");
        }
        // 20MB：save_bytes / mail_attachment 的 base64 附件可达数 MB，1MB 会直接 413。
        // 本机回环 + 已校验 Origin，不对外暴露。
        if clen > 20 * 1024 * 1024 {
            return save_http_reply(&mut s, 413, &cors, "{\"ok\":false,\"error\":\"body_too_large\"}");
        }
        // 请求体就是参数 JSON，通常几百字节；head 里可能已经带了一部分
        let mut buf = pre_body.clone();
        while buf.len() < clen {
            let mut chunk = vec![0u8; (clen - buf.len()).min(8192)];
            match s.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
        }
        let args: serde_json::Value = if buf.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_slice(&buf).unwrap_or_else(|_| serde_json::json!({}))
        };
        let out = match x8bridge_dispatch(app, &cmd, &args) {
            Ok(v) => serde_json::json!({ "ok": true, "data": v }),
            // 本层不接管的命令：回 404 让前端老老实实回退 IPC（别把「没实现」当业务错误）
            Err(e) if e == "UNKNOWN_CMD" => {
                return save_http_reply(&mut s, 404, &cors, "{\"ok\":false,\"error\":\"unknown_cmd\"}")
            }
            Err(e) => serde_json::json!({ "ok": false, "error": e }),
        };
        return save_http_reply(&mut s, 200, &cors, &out.to_string());
    }

    // GET /pick?accept=image/* —— 系统文件选择框（2026-11-06）
    // 为什么要 Rust 来弹：`<input type="file">` 在 WebView2 下**拿不到绝对路径**，
    // 发送方就永远没法「定位到源目录」，只能把文件再落一份到 downfiles 里凑合（老8 否掉）。
    // 这里只回路径，字节由前端再走 /readfile 取；任何失败都返回 ok:false，前端回退旧文件框。
    if path_only == "/pick" && method == "GET" {
        #[cfg(windows)]
        {
            let mut want_img = false;
            for kv in query.split('&') {
                if let Some((k, v)) = kv.split_once('=') {
                    if k == "accept" && pct_decode(v).starts_with("image") {
                        want_img = true;
                    }
                }
            }
            let filter = if want_img {
                "$d.Filter='图片|*.png;*.jpg;*.jpeg;*.gif;*.webp;*.bmp'"
            } else {
                "$d.Filter='所有文件|*.*'"
            };
            // 输出一律 base64 单行：PowerShell 5.1 的 stdout 是 OEM 代码页，中文路径直接读会乱码。
            // 给对话框挂一个不可见的 TopMost 宿主窗口：没有 owner 时对话框可能被邦信主窗口挡住，
            // 用户就会以为「点了没反应」—— 这也是今天反复踩的同一类坑。
            let script = format!(
                "Add-Type -AssemblyName System.Windows.Forms|Out-Null;\
                 $d=New-Object System.Windows.Forms.OpenFileDialog;\
                 $d.Multiselect=$true;$d.CheckFileExists=$true;{};\
                 $d.Title='邦信 - 选择要发送的文件';\
                 $w=New-Object System.Windows.Forms.Form;\
                 $w.TopMost=$true;$w.ShowInTaskbar=$false;$w.Opacity=0;$w.Width=1;$w.Height=1;\
                 $w.StartPosition='CenterScreen';$w.Show()|Out-Null;\
                 $r=$d.ShowDialog($w);$w.Close();\
                 if($r -eq 'OK'){{foreach($f in $d.FileNames){{$i=Get-Item $f;\"$([Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($f)))|$($i.Length)\"}}}}",
                filter
            );
            let out = Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-STA",
                    "-NonInteractive",
                    "-WindowStyle",
                    "Hidden",
                    "-Command",
                    &script,
                ])
                .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                .output();
            match out {
                Ok(o) => {
                    let txt = String::from_utf8_lossy(&o.stdout).to_string();
                    // 每行：<base64(路径)>|<字节数> —— 带上大小，前端才能对大文件回退旧文件框
                    //（那条路是磁盘句柄、零拷贝；这条路要把字节读进内存，超大文件会直接吃光内存）
                    let mut files: Vec<serde_json::Value> = Vec::new();
                    for line in txt.lines() {
                        let l = line.trim();
                        if l.is_empty() {
                            continue;
                        }
                        let (b64, sz) = match l.split_once('|') {
                            Some((a, b)) => (a, b.trim().parse::<u64>().unwrap_or(0)),
                            None => (l, 0),
                        };
                        if let Ok(bytes) = b64_decode(b64) {
                            if let Ok(p) = String::from_utf8(bytes) {
                                files.push(serde_json::json!({ "path": p, "size": sz }));
                            }
                        }
                    }
                    // rc 一并带回去：PowerShell 被安全策略拦住（Add-Type 被禁）时 rc≠0 且没有输出，
                    // 前端据此回退旧文件框 —— 否则会被当成「用户点了取消」，表现成「点+没反应」。
                    let rc = o.status.code().unwrap_or(-1);
                    let err_txt = String::from_utf8_lossy(&o.stderr).to_string();
                    let body = serde_json::json!({
                        "ok": true, "files": files, "rc": rc,
                        "err": err_txt.chars().take(300).collect::<String>()
                    })
                    .to_string();
                    return save_http_reply(&mut s, 200, &cors, &body);
                }
                Err(e) => {
                    let body = serde_json::json!({ "ok": false, "error": format!("{e}") }).to_string();
                    return save_http_reply(&mut s, 200, &cors, &body);
                }
            }
        }
        #[cfg(not(windows))]
        {
            return save_http_reply(
                &mut s,
                200,
                &cors,
                "{\"ok\":false,\"error\":\"pick_unsupported\"}",
            );
        }
    }

    // GET /readfile?path=<绝对路径> —— 原始字节（把原生对话框选中的文件读成 File）
    // 必须流式转发：2GB 上限的直传件一次性读进内存会直接把客户端打爆。
    if path_only == "/readfile" && method == "GET" {
        let mut p = String::new();
        for kv in query.split('&') {
            if let Some((k, v)) = kv.split_once('=') {
                if k == "path" {
                    p = pct_decode(v);
                }
            }
        }
        let pb = std::path::PathBuf::from(&p);
        if !pb.is_file() {
            return save_http_reply(&mut s, 404, &cors, "{\"ok\":false,\"error\":\"not_found\"}");
        }
        let len = std::fs::metadata(&pb).map(|m| m.len()).unwrap_or(0);
        let mut f = match std::fs::File::open(&pb) {
            Ok(f) => f,
            Err(e) => {
                let body = serde_json::json!({ "ok": false, "error": format!("{e}") }).to_string();
                return save_http_reply(&mut s, 500, &cors, &body);
            }
        };
        use std::io::Write;
        let header = format!(
            "HTTP/1.1 200 OK\r\n{}Content-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            cors, len
        );
        if s.write_all(header.as_bytes()).is_err() {
            return Ok(());
        }
        let _ = std::io::copy(&mut f, &mut s);
        return Ok(());
    }

    // GET /reveal?name=<文件名>&subdir=<子目录>&open=0|1
    // 2026-11-06：为什么定位/打开也要走这里 —— 这台机器上 invoke 到 Rust 的 Promise
    // 会「发了不回」（save_chunk_begin、reveal_file 实测都挂死，前端没有 reject 也没有 resolve），
    // 而本地 HTTP 通稳定可用。存在则 explorer /select（open=1 则直接用默认程序打开），
    // 不存在则打开所在目录，绝不让按钮「没反应」。
    if path_only == "/reveal" && method == "GET" {
        let mut name = String::new();
        let mut subdir: Option<String> = None;
        let mut want_open = false;
        let mut probe_only = false;
        let mut abs_path = String::new();
        for kv in query.split('&') {
            if let Some((k, v)) = kv.split_once('=') {
                match k {
                    "name" => name = pct_decode(v),
                    // 绝对路径直达（2026-11-06）：发送方定位/打开自己发出的**原始文件**，
                    // 不经过 downfiles —— 那是接收方的落地目录，发送方没必要存副本。
                    "path" => abs_path = pct_decode(v),
                    "subdir" => {
                        let d = pct_decode(v);
                        if !d.is_empty() {
                            subdir = Some(d);
                        }
                    }
                    "open" => want_open = pct_decode(v) == "1",
                    // probe=1：只查存在性，不做任何 UI 动作（前端据此决定「要不要再存一份」）
                    "probe" => probe_only = pct_decode(v) == "1",
                    _ => {}
                }
            }
        }
        // 绝对路径优先于「downfiles 里按名字找」：发送方传的是他自己的源文件路径
        if !abs_path.trim().is_empty() {
            let p = std::path::PathBuf::from(&abs_path);
            if p.is_file() {
                let ps = p.to_string_lossy().to_string();
                if probe_only {
                    let body = serde_json::json!({
                        "ok": true, "path": ps, "selected": true, "opened": false
                    })
                    .to_string();
                    return save_http_reply(&mut s, 200, &cors, &body);
                }
                let mut opened = false;
                if want_open {
                    opened = open_external(&ps).is_ok();
                }
                reveal_in_folder(&p);
                let body = serde_json::json!({
                    "ok": true, "path": ps, "selected": true, "opened": opened
                })
                .to_string();
                return save_http_reply(&mut s, 200, &cors, &body);
            }
            // 源文件已被移动/删除：不假装成功，让前端给人一句话
            let body = serde_json::json!({
                "ok": true, "path": "", "selected": false, "opened": false, "gone": true
            })
            .to_string();
            return save_http_reply(&mut s, 200, &cors, &body);
        }

        let dir = save_target_dir(subdir.as_deref());
        // 命中同基名（容忍 ` (n)` 递增后缀）即为已有副本，不要求名字逐字节相等
        let target = if name.trim().is_empty() {
            None
        } else {
            resolve_saved_file(&dir, &sanitize_filename(&name))
        };
        let mut selected = false;
        let mut opened = false;
        let path_str = match target {
            Some(p) if p.is_file() => {
                let ps = p.to_string_lossy().to_string();
                selected = true;
                if probe_only {
                    return save_http_reply(
                        &mut s,
                        200,
                        &cors,
                        &serde_json::json!({
                            "ok": true, "path": ps, "selected": true,
                            "opened": false, "dir": dir.to_string_lossy().to_string()
                        })
                        .to_string(),
                    );
                }
                if want_open {
                    opened = open_external(&ps).is_ok();
                }
                #[cfg(windows)]
                {
                    // explorer /select, 必须分开传参：合成一个时含空格路径会被整体加引号，
                    // explorer 解析不到开关（2026-10-22 修过同一处）
                    let _ = Command::new("explorer")
                        .arg("/select,")
                        .arg(ps.replace('/', "\\"))
                        .spawn();
                }
                #[cfg(target_os = "macos")]
                {
                    if let Ok(c) = Command::new("open").args(["-R", &ps]).spawn() {
                        reap_in_background(c);
                    }
                }
                #[cfg(not(any(windows, target_os = "macos")))]
                {
                    if let Ok(c) = Command::new("xdg-open")
                        .arg(p.parent().unwrap_or(std::path::Path::new("/")))
                        .spawn()
                    {
                        reap_in_background(c);
                    }
                }
                ps
            }
            _ => {
                // 只探测：明确告诉前端「不在」，不做任何 UI 动作
                if probe_only {
                    return save_http_reply(
                        &mut s,
                        200,
                        &cors,
                        &serde_json::json!({
                            "ok": true, "path": "", "selected": false,
                            "opened": false, "dir": dir.to_string_lossy().to_string()
                        })
                        .to_string(),
                    );
                }
                // 文件不在（被移走/删除）：直接把人送到目录，仍然「有反应」
                if dir.exists() {
                    let _ = open_external(&dir.to_string_lossy());
                }
                String::new()
            }
        };
        let body = serde_json::json!({
            "ok": true,
            "path": path_str,
            "selected": selected,
            "opened": opened,
            "dir": dir.to_string_lossy().to_string()
        })
        .to_string();
        return save_http_reply(&mut s, 200, &cors, &body);
    }

    if path_only != "/save" || method != "POST" {
        return save_http_reply(&mut s, 404, &cors, "{\"ok\":false,\"error\":\"not_found\"}");
    }

    // 2) 解析 name / subdir
    let mut name = String::new();
    let mut subdir: Option<String> = None;
    for kv in query.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            match k {
                "name" => name = pct_decode(v),
                "subdir" => {
                    let d = pct_decode(v);
                    if !d.is_empty() {
                        subdir = Some(d);
                    }
                }
                _ => {}
            }
        }
    }
    if name.trim().is_empty() {
        return save_http_reply(&mut s, 400, &cors, "{\"ok\":false,\"error\":\"no_name\"}");
    }

    // 3) 流式写 .part → rename（同名自动加序号，绝不覆盖已有文件）
    let dir = save_target_dir(subdir.as_deref());
    if let Err(e) = std::fs::create_dir_all(&dir) {
        let body = serde_json::json!({ "ok": false, "error": format!("无法创建目录：{}", e) }).to_string();
        return save_http_reply(&mut s, 500, &cors, &body);
    }
    let safe = sanitize_filename(&name);
    let tmp = dir.join(format!("{}.{}.x8part", safe, std::process::id()));
    let mut final_path = dir.join(&safe);
    if final_path.exists() {
        let p = std::path::Path::new(&safe);
        let stem = p.file_stem().map(|x| x.to_string_lossy().to_string()).unwrap_or_else(|| safe.clone());
        let ext = p.extension().map(|x| format!(".{}", x.to_string_lossy())).unwrap_or_default();
        let mut n = 1;
        while n < 999 {
            let cand = dir.join(format!("{} ({}){}", stem, n, ext));
            if !cand.exists() {
                final_path = cand;
                break;
            }
            n += 1;
        }
    }

    let write_res = (|| -> std::io::Result<usize> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        let mut written = 0usize;
        // 请求头之后已顺带读到的 body 片段
        if !pre_body.is_empty() {
            let take = if clen > 0 { pre_body.len().min(clen) } else { pre_body.len() };
            f.write_all(&pre_body[..take])?;
            written += take;
        }
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            if clen > 0 && written >= clen {
                break;
            }
            let want = if clen > 0 { (clen - written).min(buf.len()) } else { buf.len() };
            let n = s.read(&mut buf[..want])?;
            if n == 0 {
                break;
            }
            f.write_all(&buf[..n])?;
            written += n;
        }
        f.flush()?;
        Ok(written)
    })();

    match write_res {
        Ok(written) => {
            let _ = std::fs::rename(&tmp, &final_path);
            let body = serde_json::json!({
                "ok": true,
                "path": final_path.to_string_lossy().to_string(),
                "bytes": written
            })
            .to_string();
            save_http_reply(&mut s, 200, &cors, &body)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            let body = serde_json::json!({ "ok": false, "error": format!("写入失败：{}", e) }).to_string();
            save_http_reply(&mut s, 500, &cors, &body)
        }
    }
}