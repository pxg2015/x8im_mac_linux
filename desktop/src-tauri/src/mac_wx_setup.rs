// 邦信 · macOS 读微信消息前置条件 + 提权拉起（2026-11-10）
// ─────────────────────────────────────────────────────────────────────────────
// macOS 上读微信消息比 Windows 多三道关，本模块集中处理：
//
//  1) 完全磁盘访问（FDA）：系统隐私设置里授予，才能读到微信沙盒容器里的 DB。
//  2) Hardened Runtime（HR）：微信默认带 HR，会拦 task_for_pid，wxgo 无法 attach。
//     必须「重签」微信——抽原 entitlements、去掉 runtime 标志、用本机 ad-hoc 证书
//     重签（保留 allow-jit / debugger，否则微信起不来）。重签要管理员密码。
//  3) 提权拉起：邦信本体以普通用户跑，但 attach 微信需要 root。用
//     `osascript -e 'do shell script ... with administrator privileges'`
//     一次性弹管理员密码，把 wxgo 注册为系统 LaunchDaemon 以 root 拉起；
//     不引入已公证的 helper（成本太高）。改用 LaunchDaemon 彻底脱离 osascript
//     会话，避免 `nohup ... &` 被会话拆除回收导致「起来即被杀」。
//
// ⚠️ 令牌只经 0600 临时文件（X8TOKEN_FILE 指向它）注入，绝不进 argv / plist 明文 /
//   环境变量值；wxgo 读后即删，落盘窗口极短。邦信本进程不持 wxgo 子进程句柄
//   （root 进程），所以 plugin_host 在 mac 上靠端口探活。

use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};

// 与 plugin_host.rs 同口径的端口等待常量（mac 提权拉起后探活用）
const BOOT_TIMEOUT_MS: u64 = 12000;
const PROBE_INTERVAL_MS: u64 = 100;

/// 微信主程序路径（标准安装位置）
fn wechat_app() -> String {
    "/Applications/WeChat.app".to_string()
}

/// 微信容器数据目录（用于 FDA 探活）：macOS 把第三方 app 数据放进沙盒容器，
/// 跨 app 读它通常需要「完全磁盘访问」权限。
fn wechat_container() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .map(|h| format!("{h}/Library/Containers/com.tencent.xinWeChat/Data"))
}

/// 微信是否已重签（移除 Hardened Runtime）。
/// `codesign -dvvv` 在 HR 开启时会输出 `Flags=0x10000[runtime]`；重签后该标志消失。
pub fn wechat_resigned() -> bool {
    let out = Command::new("/usr/bin/codesign")
        .args(["-dvvv", &wechat_app()])
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            let e = String::from_utf8_lossy(&o.stderr);
            let txt = format!("{s}{e}");
            // 含 "runtime" 即仍带 Hardened Runtime → 未重签
            !txt.contains("runtime")
        }
        Err(_) => false,
    }
}

/// 微信是否正在运行（pgrep 匹配微信主进程）。
pub fn wechat_running() -> bool {
    let out = Command::new("/usr/bin/pgrep")
        .args(["-f", "WeChat.app/Contents/MacOS/WeChat"])
        .output();
    match out {
        Ok(o) => o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        Err(_) => false,
    }
}

/// 完全磁盘访问（FDA）是否授予。此为「尽力探测」：尝试读取微信容器目录的元数据，
/// 读不到（权限拒绝）即视为未授予。真实可用性最终由 wxgo 报错兜底。
pub fn fda_granted() -> bool {
    match wechat_container() {
        Some(p) => Path::new(&p).join("Documents").metadata().is_ok(),
        None => false,
    }
}

/// 重签微信：抽出原 entitlements → 去掉 Hardened Runtime（重签时不传 --options runtime）→
/// 用本机 ad-hoc 证书重签（保留 allow-jit / debugger 等，否则微信起不来）。需管理员密码。
///
/// 注意：重签只改磁盘上的二进制，运行中的微信需重启才生效（签名变更在内存里不生效）。
pub fn resign_wechat() -> Result<String, String> {
    let app = wechat_app();
    // 1) 抽出原 entitlements（务必保留，含 allow-jit / debugger）
    let ent = std::env::temp_dir().join("wechat_entitlements.plist");
    let dump = Command::new("/usr/bin/codesign")
        .args(["-d", "--entitlements", ":-", &app])
        .output()
        .map_err(|e| format!("读取微信签名失败：{e}"))?;
    if !dump.status.success() {
        return Err(format!(
            "读取微信签名失败：{}",
            String::from_utf8_lossy(&dump.stderr)
        ));
    }
    fs::write(&ent, &dump.stdout).map_err(|e| format!("写 entitlements 失败：{e}"))?;

    // 2) 以 root 重签：不加 --options runtime（即移除 Hardened Runtime）
    let inner = format!(
        "/usr/bin/codesign --force --deep --sign - --entitlements \"{ent}\" \"{app}\"",
        ent = ent.display(),
        app = app
    );
    let osa = format!(
        "do shell script \"{}\" with administrator privileges",
        escape_for_osascript(&inner)
    );
    let out = Command::new("/usr/bin/osascript")
        .args(["-e", &osa])
        .output()
        .map_err(|e| format!("提权重签微信失败：{e}"))?;
    if !out.status.success() {
        return Err(format!(
            "重签微信失败：{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok("微信已重签（已移除 Hardened Runtime）。请退出并重新打开微信使其生效。".into())
}

/// 以 root 经 LaunchDaemon 提权拉起 wxgo（彻底脱离 osascript 会话，避免被回收）。
///
/// 为什么不用 `do shell script "... &"` / `nohup ... &`：
/// osascript 的授权会话结束后，launchd/authorizationhost 会把残留子进程清掉，
/// 即便 nohup 也只能挡 SIGHUP、挡不住会话拆除 —— wxgo 起来瞬间即被杀，端口从未稳定监听。
/// 改用系统 LaunchDaemon：launchd 成为其父进程，与邦信进程 / osascript 会话完全解耦，
/// 只要邦信不主动 stop，它就能一直存活。
///
/// 令牌只经 0600 临时文件（X8TOKEN_FILE 指向它）喂给插件，绝不进 argv / plist 明文 /
/// 环境变量值；wxgo 读后即删该文件，落盘窗口极短。本机不持子进程句柄，靠端口探活。
pub fn launch_root(bin: &Path, args: &[String], token: &str, port: u16) -> Result<(), String> {
    // 1) 令牌写入 0600 临时文件（仅 root 守护进程可读），经 X8TOKEN_FILE 注入。
    let tok_path = std::env::temp_dir().join(format!("x8_wx_token_{port}.txt"));
    fs::write(&tok_path, format!("{token}\n"))
        .map_err(|e| format!("写令牌文件失败：{e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tok_path, fs::Permissions::from_mode(0o600));
    }

    // 2) 生成 LaunchDaemon plist（含 bin + 参数 + X8TOKEN_FILE 路径 + 日志重定向）。
    let work_dir = bin
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/tmp".to_string());
    let log_path = format!("/tmp/x8_wx_{port}.log");
    let plist = build_wxgo_plist(bin, args, &tok_path.display().to_string(), &work_dir, &log_path);
    let plist_tmp = std::env::temp_dir().join("com.x8fde.im.wxgo.plist");
    fs::write(&plist_tmp, plist).map_err(|e| format!("写 LaunchDaemon plist 失败：{e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&plist_tmp, fs::Permissions::from_mode(0o600));
    }

    // 3) 以 root 经 osascript 提权：拷 plist 到 /Library/LaunchDaemons、赋权、bootstrap + kickstart。
    //    路径用 shell_quote 单引号包裹（含空格安全）；bootstrap 已加载时静默失败，靠 kickstart 重启。
    let plist_q = shell_quote(&plist_tmp.display().to_string());
    let inner = format!(
        "cp {plist_q} /Library/LaunchDaemons/com.x8fde.im.wxgo.plist; \
         chown root:wheel /Library/LaunchDaemons/com.x8fde.im.wxgo.plist; \
         chmod 644 /Library/LaunchDaemons/com.x8fde.im.wxgo.plist; \
         launchctl bootstrap system /Library/LaunchDaemons/com.x8fde.im.wxgo.plist 2>/dev/null; \
         launchctl kickstart -k system/com.x8fde.im.wxgo 2>/dev/null || \
         launchctl load /Library/LaunchDaemons/com.x8fde.im.wxgo.plist 2>/dev/null",
        plist_q = plist_q
    );
    let osa = format!(
        "do shell script \"{}\" with administrator privileges",
        escape_for_osascript(&inner)
    );
    let out = Command::new("/usr/bin/osascript")
        .args(["-e", &osa])
        .output()
        .map_err(|e| format!("提权拉起微信插件失败：{e}"))?;
    if !out.status.success() {
        return Err(format!(
            "提权拉起微信插件失败：{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }

    // 4) 等端口就绪（探测前先 settle 500ms，让 launchd 真正拉起进程；总上限 12s）。
    std::thread::sleep(Duration::from_millis(500));
    let addr = format!("127.0.0.1:{port}")
        .parse::<SocketAddr>()
        .map_err(|e| format!("端口解析失败：{e}"))?;
    let tries = BOOT_TIMEOUT_MS / PROBE_INTERVAL_MS;
    for _ in 0..tries {
        std::thread::sleep(Duration::from_millis(PROBE_INTERVAL_MS));
        if TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok() {
            // 端口已起：wxgo 已读过令牌文件，本机删掉它缩短落盘窗口。
            let _ = fs::remove_file(&tok_path);
            return Ok(());
        }
    }
    // 超时：把真实死因（panic / 被拦 / 签名拦截）从日志尾巴拼进错误，便于定位。
    let tail = read_log_tail(&log_path, 40);
    let _ = fs::remove_file(&tok_path);
    Err(format!("微信插件（root）启动超时，端口 {port} 未就绪。{tail}"))
}

/// 构造 wxgo 的 LaunchDaemon plist（XML）。令牌只以文件路径（X8TOKEN_FILE）传递，
/// 绝不把明文写进 plist；--parent-watch 被刻意剔除（mac 无父管道，靠 HTTP 优雅退出回收）。
fn build_wxgo_plist(
    bin: &Path,
    args: &[String],
    token_file: &str,
    work_dir: &str,
    log: &str,
) -> String {
    let mut prog = String::new();
    prog.push_str(&format!(
        "        <string>{}</string>\n",
        xml_escape(&bin.display().to_string())
    ));
    for a in args {
        if a == "--parent-watch" {
            continue; // mac 无父管道，靠 HTTP 优雅退出 + kill_by_port 回收
        }
        prog.push_str(&format!("        <string>{}</string>\n", xml_escape(a)));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.x8fde.im.wxgo</string>
    <key>ProgramArguments</key>
    <array>
{prog}    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>X8TOKEN_FILE</key>
        <string>{token_file}</string>
    </dict>
    <key>WorkingDirectory</key>
    <string>{work_dir}</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>RunAtLoad</key>
    <false/>
    <key>KeepAlive</key>
    <false/>
</dict>
</plist>
"#,
        prog = prog,
        token_file = xml_escape(token_file),
        work_dir = xml_escape(work_dir),
        log = xml_escape(log),
    )
}

/// 读日志末尾 N 行（超时兜底诊断用）。
fn read_log_tail(path: &str, n: usize) -> String {
    let Ok(s) = fs::read_to_string(path) else {
        return "(无日志)".to_string();
    };
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    let tail = lines[start..].join("\n");
    if tail.trim().is_empty() {
        "(日志为空，插件可能未被执行——疑被 Gatekeeper 拦下或未重签)".to_string()
    } else {
        format!("日志末尾：\n{tail}")
    }
}

/// XML 文本转义（plist 里路径可能含 & < > "）。
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 按端口反查并杀掉 root 拉起的 wxgo（优雅退出由调用方先发 /api/app/shutdown）。
pub fn kill_by_port(port: u16) {
    // 反查监听该端口的 PID（本机监听对当前用户可见），逐个 TERM。
    let out = Command::new("/usr/sbin/lsof")
        .arg(format!("-tiTCP:{port}"))
        .arg("-sTCP:LISTEN")
        .output();
    let mut killed = false;
    if let Ok(o) = out {
        for pid in String::from_utf8_lossy(&o.stdout).split_whitespace() {
            let _ = Command::new("/bin/kill").args(["-TERM", pid]).output();
            killed = true;
        }
    }
    if !killed {
        // 兜底：按进程名收（lsof 看不到 root 进程时）
        let _ = Command::new("/usr/bin/pkill")
            .args(["-f", "wxadapter"])
            .output();
    }
}

/// 单引号包裹（POSIX sh 标准转义，内部单引号用 '\'' 处理）；路径含空格也安全。
fn shell_quote(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// 嵌入 `do shell script "..."` 的双引号字符串时，转义反斜杠与双引号。
fn escape_for_osascript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ─────────────────────────── Tauri 命令 ───────────────────────────

/// 前端向导轮询：当前 mac 读微信的三项前置条件。
#[tauri::command]
pub fn wx_setup_state() -> Value {
    let resigned = wechat_resigned();
    let running = wechat_running();
    json!({
        "supported": true,
        "fda": fda_granted(),
        "resigned": resigned,
        "wechat_running": running,
        "ready": resigned && running,
    })
}

/// 前端点「一键重签」：以管理员密码提权重签微信。重签后若微信仍在跑，提示重启。
#[tauri::command]
pub fn wx_setup_run() -> Value {
    match resign_wechat() {
        Ok(msg) => {
            let running = wechat_running();
            json!({
                "ok": true,
                "resigned": true,
                "wechat_running": running,
                "msg": if running {
                    format!("{msg}；微信仍在运行，请退出并重新打开微信后，再点「开始读取」。")
                } else {
                    msg
                }
            })
        }
        Err(e) => json!({ "ok": false, "err": e }),
    }
}
