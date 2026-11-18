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

/// 最近一次由系统「选择文件夹」对话框**亲自选中**的目录（2026-11-18）。
/// 保存目录只认这个值：页面最多只能把落盘改成用户刚在系统对话框里选的那个目录，
/// 不能凭空指定任意路径（那等于开放任意路径写盘）。
static LAST_PICKED_DIR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

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

    // GET /pickcap —— 能力探针（2026-11-18）
    // 前端靠它判断「本机客户端是否支持 Rust 原生选择器」。
    // 为什么不能按平台直接判断：mac 的原生选择器是 11-18 才加的，老客户端的 /pick
    // 直接回 pick_unsupported。前端若对老客户端也 await 一次 /pick，user gesture 就没了
    // （WKWebView 会静默丢弃 input.click()）→ 表现成「点发送文件完全没反应」（11-06 踩过）。
    // 所以口径是：**老客户端 = 同步走 input.click()（保持原样），新客户端 = 走原生面板**。
    // 老客户端没有这条路由 → 前端拿到 404 → 视为不支持 → 行为不变。
    if path_only == "/pickcap" && method == "GET" {
        let supported = cfg!(windows) || cfg!(target_os = "macos");
        let body = format!("{{\"ok\":true,\"supported\":{supported}}}");
        return save_http_reply(&mut s, 200, &cors, &body);
    }

    // GET /pick?accept=image/* —— 系统文件选择框（2026-11-06）
    // 为什么要 Rust 来弹：`<input type="file">` 在 WebView2 下**拿不到绝对路径**，
    // 发送方就永远没法「定位到源目录」，只能把文件再落一份到 downfiles 里凑合（老8 否掉）。
    // 这里只回路径，字节由前端再走 /readfile 取；任何失败都返回 ok:false，前端回退旧文件框。
    //
    // 2026-11-18：mac 也走这条（App 自己的 NSOpenPanel）—— 老8 报「mac 点图片弹的框是英文的
    // + 反应太慢」。面板若由 wry（WKWebView）弹：不过滤类型、拿不到绝对路径、起始目录不受控
    // （可能落在 iCloud 云盘目录 → macOS 枚举云盘目录把面板卡住）。详见 src/mac_pick.rs。
    if path_only == "/pick" && method == "GET" {
        // accept=image/* → 只列图片（Windows / mac 共用口径）
        #[cfg(any(windows, target_os = "macos"))]
        let want_img = {
            let mut img = false;
            for kv in query.split('&') {
                if let Some((k, v)) = kv.split_once('=') {
                    if k == "accept" && pct_decode(v).starts_with("image") {
                        img = true;
                    }
                }
            }
            img
        };
        #[cfg(windows)]
        {
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
        // 2026-11-18：mac 走 App 自己的 NSOpenPanel（不再经 WKWebView 的 input.click()）。
        // AppKit 硬性要求面板在主线程创建 → 投到主线程弹，HTTP 线程在这里等结果。
        #[cfg(target_os = "macos")]
        {
            let app2 = app.clone();
            let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<(String, u64)>, String>>();
            let dispatched = app2.run_on_main_thread(move || {
                let _ = tx.send(crate::mac_pick::pick_files(want_img));
            });
            if dispatched.is_err() {
                return save_http_reply(
                    &mut s,
                    200,
                    &cors,
                    "{\"ok\":false,\"error\":\"pick_dispatch_failed\"}",
                );
            }
            // 用户可能挑很久 → 10 分钟上限（前端那边另有 90s 兜底）
            let picked = match rx.recv_timeout(std::time::Duration::from_secs(600)) {
                Ok(r) => r,
                Err(_) => {
                    return save_http_reply(
                        &mut s,
                        200,
                        &cors,
                        "{\"ok\":false,\"error\":\"pick_timeout\"}",
                    );
                }
            };
            let files = match picked {
                Ok(v) => v,
                Err(e) => {
                    let body = serde_json::json!({ "ok": false, "error": e }).to_string();
                    return save_http_reply(&mut s, 200, &cors, &body);
                }
            };
            // 与 Windows 同一份 JSON 契约：{path, size} 列表 + rc=0（前端零改动复用）
            let arr: Vec<serde_json::Value> = files
                .iter()
                .map(|(p, sz)| serde_json::json!({ "path": p, "size": sz }))
                .collect();
            let body = serde_json::json!({ "ok": true, "files": arr, "rc": 0 }).to_string();
            return save_http_reply(&mut s, 200, &cors, &body);
        }
        // linux：仍不支持（前端回退 <input type=file>）
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            return save_http_reply(
                &mut s,
                200,
                &cors,
                "{\"ok\":false,\"error\":\"pick_unsupported\"}",
            );
        }
    }

    // ── 保存目录（2026-11-18 老8：聊天文件保存目录可指定）────────────────────
    // GET  /savedir              读当前生效目录 + 默认目录
    // GET  /pickdir              弹系统「选择文件夹」对话框（用户亲自选 → 记账）
    // POST /savedir?path=<路径>  写回（只认刚才用户亲自选中的那个；空 = 恢复默认）
    //
    // 为什么必须弹原生对话框：① WebView2 拿不到绝对路径（同 /pick 的坑）；
    // ② 「让用户亲眼选一次」本身就是安全边界 —— 只有被选中过的路径才允许写回，
    //    页面不能悄悄把落盘改到别处（那等于开放任意路径写盘）。
    if path_only == "/savedir" && method == "GET" {
        let body = save_dir_state_json().to_string();
        return save_http_reply(&mut s, 200, &cors, &body);
    }
    if path_only == "/savedir" && method == "POST" {
        let mut p = String::new();
        for kv in query.split('&') {
            if let Some((k, v)) = kv.split_once('=') {
                if k == "path" {
                    p = pct_decode(v);
                }
            }
        }
        let p = p.trim().to_string();
        if p.is_empty() {
            // 恢复默认（删配置 → 回到 安装目录\downfiles）
            save_dir_custom_save(None);
            si_log("[savedir] reset to default");
            let body = save_dir_state_json().to_string();
            return save_http_reply(&mut s, 200, &cors, &body);
        }
        // 只认用户亲自在系统对话框里选中的那个路径（LAST_PICKED_DIR 记账）
        let last = LAST_PICKED_DIR
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_default();
        if p != last {
            let body =
                serde_json::json!({ "ok": false, "error": "目录未经用户选择确认" }).to_string();
            return save_http_reply(&mut s, 403, &cors, &body);
        }
        if is_dangerous_save_dir(&p) {
            let body =
                serde_json::json!({ "ok": false, "error": "该目录不适合作为保存目录（不能是磁盘根或系统目录）" })
                    .to_string();
            return save_http_reply(&mut s, 400, &cors, &body);
        }
        save_dir_custom_save(Some(&p));
        si_log(&format!("[savedir] set {p}"));
        let body = save_dir_state_json().to_string();
        return save_http_reply(&mut s, 200, &cors, &body);
    }
    if path_only == "/pickdir" && method == "GET" {
        #[cfg(windows)]
        {
            // 初始位置 = 当前生效目录（不存在就用下载目录）—— 用户在熟悉的目录下挑
            let (cur, _) = resolve_save_dir();
            let init = if cur.is_dir() {
                cur.to_string_lossy().to_string()
            } else {
                downloads_dir().to_string_lossy().to_string()
            };
            match pick_dir_dialog(&init) {
                Some(p) => {
                    // 记账：只有这个路径允许被 /savedir 写回
                    if let Ok(mut g) = LAST_PICKED_DIR.lock() {
                        *g = Some(p.clone());
                    }
                    let body = serde_json::json!({ "ok": true, "path": p }).to_string();
                    return save_http_reply(&mut s, 200, &cors, &body);
                }
                None => {
                    // 取消 / PowerShell 被策略拦住：前端据此不弹错误提示（当成用户取消）
                    let body =
                        serde_json::json!({ "ok": false, "error": "cancelled", "cancelled": true })
                            .to_string();
                    return save_http_reply(&mut s, 200, &cors, &body);
                }
            }
        }
        #[cfg(not(windows))]
        {
            let body = serde_json::json!({ "ok": false, "error": "pickdir_unsupported" }).to_string();
            return save_http_reply(&mut s, 200, &cors, &body);
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

    // POST /saveas?name=<建议文件名>&ext=<扩展名> —— 弹系统「另存为」对话框后落盘
    // 2026-11-18（图片查看器）：为什么必须走 Rust：
    //   WebView2 不支持 showSaveFilePicker（原因同 /pick 的注释），系统另存为只能靠原生对话框；
    //   而「弹框」与「写盘」必须同进程 —— 弹框拿到的是用户亲自选的绝对路径，
    //   这个路径不允许由前端传进来（否则就是任意路径写盘）。
    // 顺序：①先把请求体收进临时文件 ②弹框拿路径 ③拷过去。
    //   ①在前是因为客户端此刻正在上传，先弹框会让用户对着一个空对话框等下载；
    //   先收完再弹，用户看到对话框时数据已就绪。
    if path_only == "/saveas" && method == "POST" {
        let mut name = String::new();
        let mut ext = String::new();
        for kv in query.split('&') {
            if let Some((k, v)) = kv.split_once('=') {
                match k {
                    "name" => name = pct_decode(v),
                    "ext" => ext = pct_decode(v).trim_start_matches('.').to_ascii_lowercase(),
                    _ => {}
                }
            }
        }
        if name.trim().is_empty() {
            return save_http_reply(&mut s, 400, &cors, "{\"ok\":false,\"error\":\"no_name\"}");
        }
        // ① 收字节 → 临时文件（放在下载目录同盘，rename 到目标路径多为同盘原子操作）
        let dir = downloads_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            let body = serde_json::json!({ "ok": false, "error": format!("无法创建目录：{}", e) }).to_string();
            return save_http_reply(&mut s, 500, &cors, &body);
        }
        let safe = sanitize_filename(&name);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = dir.join(format!(".x8saveas_{}_{}_{}", std::process::id(), stamp, safe));
        let write_res = (|| -> std::io::Result<usize> {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            let mut written = 0usize;
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
        if let Err(e) = write_res {
            let _ = std::fs::remove_file(&tmp);
            let body = serde_json::json!({ "ok": false, "error": format!("写入失败：{}", e) }).to_string();
            return save_http_reply(&mut s, 500, &cors, &body);
        }
        // ② 弹系统另存为
        #[cfg(windows)]
        {
            match pick_save_path(&safe, &ext, &dir.to_string_lossy()) {
                Some(p) => {
                    // ③ 落到用户选的位置（同盘优先 rename，跨盘/被占用退回 copy）
                    let target = std::path::PathBuf::from(&p);
                    if let Some(par) = target.parent() {
                        let _ = std::fs::create_dir_all(par);
                    }
                    let mut moved = std::fs::rename(&tmp, &target).is_ok();
                    if !moved {
                        moved = std::fs::copy(&tmp, &target).is_ok();
                    }
                    let _ = std::fs::remove_file(&tmp);
                    if !moved {
                        let body = serde_json::json!({ "ok": false, "error": "无法写入所选位置" }).to_string();
                        return save_http_reply(&mut s, 500, &cors, &body);
                    }
                    let body = serde_json::json!({
                        "ok": true, "path": p, "cancelled": false
                    })
                    .to_string();
                    return save_http_reply(&mut s, 200, &cors, &body);
                }
                None => {
                    let _ = std::fs::remove_file(&tmp);
                    let body = serde_json::json!({ "ok": false, "error": "cancelled", "cancelled": true }).to_string();
                    return save_http_reply(&mut s, 200, &cors, &body);
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _ = std::fs::remove_file(&tmp);
            // mac/linux 暂不接原生另存为（跨端无法验证）→ 前端回落「保存到下载目录」
            let body = serde_json::json!({ "ok": false, "error": "saveas_unsupported" }).to_string();
            return save_http_reply(&mut s, 200, &cors, &body);
        }
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

// ============================================================================
// 系统「选择文件夹」对话框（2026-11-18，保存目录指定用）
// ----------------------------------------------------------------------------
// 与 pick_save_path 同一套解法：PowerShell FolderBrowserDialog → base64 回传
// （PowerShell 5.1 的 stdout 是 OEM 代码页，中文路径直读会乱码）。
// 取消：对话框返回 Cancel → 脚本不输出 → 返回 None → 前端收到 cancelled。
// ============================================================================
#[cfg(windows)]
fn pick_dir_dialog(init: &str) -> Option<String> {
    // 路径里的单引号会破坏 PowerShell 的单引号串 → 先做一次转义
    let esc_init = init.replace('\'', "''");
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms|Out-Null;\
         $d=New-Object System.Windows.Forms.FolderBrowserDialog;\
         $d.Description='邦信 - 选择聊天文件保存目录';\
         $d.ShowNewFolderButton=$true;\
         $d.SelectedPath='{esc_init}';\
         $w=New-Object System.Windows.Forms.Form;\
         $w.TopMost=$true;$w.ShowInTaskbar=$false;$w.Opacity=0;$w.Width=1;$w.Height=1;\
         $w.StartPosition='CenterScreen';$w.Show()|Out-Null;\
         $r=$d.ShowDialog($w);$w.Close();\
         if($r -eq 'OK'){{[Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($d.SelectedPath))}}",
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
    let o = match out {
        Ok(o) => o,
        Err(_) => return None,
    };
    let txt = String::from_utf8_lossy(&o.stdout).to_string();
    for line in txt.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        if let Ok(bytes) = b64_decode(l) {
            if let Ok(p) = String::from_utf8(bytes) {
                if !p.trim().is_empty() {
                    return Some(p);
                }
            }
        }
    }
    None
}

// ============================================================================
// 系统「另存为」对话框（2026-11-18，图片查看器「另存」用）
// ----------------------------------------------------------------------------
// 为什么必须走 PowerShell 而不是浏览器 API：
//   ① WebView2 不支持 showSaveFilePicker（与 /pick 同一个坑）；
//   ② 「选路径」和「写盘」必须同进程 —— 弹框拿到的是用户亲自选的绝对路径，
//      这个路径绝不能让前端传进来（那等于开放任意路径写盘）。
// 做法：PowerShell System.Windows.Forms.SaveFileDialog 弹框 → 取回路径 → 写盘。
// 编码坑：PowerShell 5.1 的 stdout 是 OEM 代码页，中文路径直接读会乱码，
//   所以输出走 base64（与 /pick 同一套解法，b64_decode 复用 main.rs 的实现）。
// 取消：对话框返回 Cancel 时脚本不输出任何东西 → 返回 None → 前端收到 cancelled。
// ============================================================================
#[cfg(windows)]
fn pick_save_path(suggest_name: &str, ext: &str, init_dir: &str) -> Option<String> {
    // 文件名里的单引号会破坏 PowerShell 的单引号串 → 先做一次转义
    let esc = suggest_name.replace('\'', "''");
    // 过滤器按扩展名给：图片给对应类型，其它给全部文件
    let filter = match ext {
        "png" => "PNG 图片|*.png|所有文件|*.*",
        "jpg" | "jpeg" => "JPEG 图片|*.jpg;*.jpeg|所有文件|*.*",
        "gif" => "GIF 图片|*.gif|所有文件|*.*",
        "webp" => "WEBP 图片|*.webp|所有文件|*.*",
        "bmp" => "BMP 图片|*.bmp|所有文件|*.*",
        _ => "所有文件|*.*",
    };
    let esc_dir = init_dir.replace('\'', "''");
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms|Out-Null;\
         $d=New-Object System.Windows.Forms.SaveFileDialog;\
         $d.FileName='{esc}';$d.Filter='{filter}';\
         $d.InitialDirectory='{esc_dir}';$d.OverwritePrompt=$true;\
         $d.Title='邦信 - 另存为';\
         $w=New-Object System.Windows.Forms.Form;\
         $w.TopMost=$true;$w.ShowInTaskbar=$false;$w.Opacity=0;$w.Width=1;$w.Height=1;\
         $w.StartPosition='CenterScreen';$w.Show()|Out-Null;\
         $r=$d.ShowDialog($w);$w.Close();\
         if($r -eq 'OK'){{[Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($d.FileName))}}",
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
    let o = match out {
        Ok(o) => o,
        Err(_) => return None,
    };
    let txt = String::from_utf8_lossy(&o.stdout).to_string();
    for line in txt.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        if let Ok(bytes) = b64_decode(l) {
            if let Ok(p) = String::from_utf8(bytes) {
                if p.trim().is_empty() {
                    continue;
                }
                return Some(p);
            }
        }
    }
    None
}