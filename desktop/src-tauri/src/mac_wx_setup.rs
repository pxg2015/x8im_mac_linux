// 邦信 · macOS 读微信消息前置条件 + 提权拉起（2026-11-10）
// ─────────────────────────────────────────────────────────────────────────────
// macOS 上读微信消息比 Windows 多三道关，本模块集中处理：
//
//  1) 完全磁盘访问（FDA）：系统隐私设置里授予，才能读到微信沙盒容器里的 DB。
//  2) Hardened Runtime（HR）：微信默认带 HR，会拦 task_for_pid，wxgo 无法 attach。
//     必须「重签」微信——抽原 entitlements、去掉 runtime 标志、用本机 ad-hoc 证书
//     重签（保留 allow-jit / debugger，否则微信起不来）。重签要管理员密码。
//  3) 用户态拉起：微信以当前登录用户运行，macOS 的 TCC/SIP 限制下，root 进程
//     **无法 lldb-attach 到用户态微信**（密钥提取需在微信进程内断点捕获 passphrase）。
//     故 wxgo 必须以「当前用户」经 LaunchAgent 拉起，与微信同用户、同会话，lldb 才能挂上。
//     同事在 mac 上用「用户态 LaunchAgent + lldb 挂用户微信」已验证成功，本机对齐该路径。
//     免管理员密码（LaunchAgent 在 gui/$UID 域，普通用户即可 load）。
//
// ⚠️ 令牌只经 0600 临时文件（X8TOKEN_FILE 指向它）注入，绝不进 argv / plist 明文 /
//   环境变量值；wxgo 读后即删，落盘窗口极短。邦信本进程不持 wxgo 子进程句柄
//   （用户态进程），所以 plugin_host 在 mac 上靠端口探活。
//   升级兼容：从 1.6.83（root LaunchDaemon）升级时，旧 root 守护仍占端口，故首次
//   以管理员密码 bootout 旧 system 域守护（仅当 /Library/LaunchDaemons 下 plist
//   仍存在才弹窗，之后即不再弹）。

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

/// 微信是否已重签到「可被调试」的状态（两个条件缺一不可）：
///  1) 不带 Hardened Runtime —— `codesign -dvvv` 输出里没有 `Flags=0x10000[runtime]`；
///  2) 带 `com.apple.security.get-task-allow` —— 否则 taskgated 会拒掉 lldb 的
///     task_for_pid（实测：Developer mode disabled 时，非 root 与 root 一并被拒，
///     报 "Not allowed to attach to process"）。补上这个调试授权位才能免开开发者模式。
pub fn wechat_resigned() -> bool {
    let out = Command::new("/usr/bin/codesign")
        .args(["-dvvv", &wechat_app()])
        .output();
    let no_runtime = match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            let e = String::from_utf8_lossy(&o.stderr);
            // 含 "runtime" 即仍带 Hardened Runtime → 未重签
            !format!("{s}{e}").contains("runtime")
        }
        Err(_) => return false,
    };
    if !no_runtime {
        return false;
    }
    has_get_task_allow()
}

/// 目标 app 的 entitlements 里是否已带调试授权位（get-task-allow）。
fn has_get_task_allow() -> bool {
    let out = Command::new("/usr/bin/codesign")
        .args(["-d", "--entitlements", ":-", &wechat_app()])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains("get-task-allow"),
        Err(_) => false,
    }
}

/// 往 entitlements plist 里补 `com.apple.security.get-task-allow`（已存在则原样返回）。
fn ensure_get_task_allow(xml: &str) -> String {
    if xml.contains("get-task-allow") {
        return xml.to_string();
    }
    // 插到最后一个 </dict> 之前（根 dict 收尾处）
    match xml.rfind("</dict>") {
        Some(i) => {
            let mut s = String::with_capacity(xml.len() + 80);
            s.push_str(&xml[..i]);
            s.push_str("\t<key>com.apple.security.get-task-allow</key>\n\t<true/>\n");
            s.push_str(&xml[i..]);
            s
        }
        None => xml.to_string(),
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
    // 1.5) 关键：往 entitlements 补 get-task-allow。缺它时无论 root 与否，taskgated 都会
    //      拒掉 lldb 的 task_for_pid（除非开全局开发者模式 + 注销重登），补上即可 attach。
    let ent_xml = ensure_get_task_allow(&String::from_utf8_lossy(&dump.stdout));
    fs::write(&ent, &ent_xml).map_err(|e| format!("写 entitlements 失败：{e}"))?;

    // 2) 以 root 重签：不加 --options runtime（即移除 Hardened Runtime）；
    //    同时清掉下载隔离属性 —— 重签会让 Gatekeeper 在下次启动重新校验，
    //    残留 com.apple.quarantine 时微信会被拦成「Apple 无法检查是否包含恶意软件」。
    let inner = format!(
        "/usr/bin/codesign --force --deep --sign - --entitlements \"{ent}\" \"{app}\" \
         && /usr/bin/xattr -rd com.apple.quarantine \"{app}\"",
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
    Ok("微信已重签（已移除 Hardened Runtime 并补上调试授权 get-task-allow，隔离属性已清）。\
        请完全退出微信（⌘Q）再重新打开，否则旧签名进程仍无法被挂接。"
        .into())
}

/// 以当前用户经 LaunchAgent 拉起 wxgo（用户态，无需管理员权限）。
///
/// 为什么从 root LaunchDaemon 改回用户态 LaunchAgent：
/// 微信以当前登录用户运行；macOS 的 TCC/SIP 限制下，root 进程无法 lldb-attach 到
/// 用户态微信（密钥提取需在微信进程内断点捕获 passphrase）。同事在 mac 上用「用户态
/// 拉起 + lldb 挂用户微信」已验证成功，故本机也走用户态。LaunchAgent 在 gui/$UID 域，
/// 普通用户即可 load，令牌仍只经 0600 临时文件注入。
///
/// 升级兼容：从 1.6.83（root LaunchDaemon）升级时，旧 root 守护仍占着端口、会挡新实例
/// 绑定。故先尝试用管理员密码 bootout 旧 system 域守护（仅当 /Library/LaunchDaemons 下
/// plist 仍存在才弹窗，弹一次后 plist 即删，之后不再弹）；再 bootout 同名 user agent，
/// 保证单实例 + 新令牌生效。令牌只经 0600 临时文件（X8TOKEN_FILE）注入，绝不进 argv /
/// plist 明文 / 环境变量值；wxgo 读后即删，落盘窗口极短。本机不持子进程句柄，靠端口探活。
pub fn launch_user(bin: &Path, args: &[String], token: &str, port: u16) -> Result<(), String> {
    // 0) 升级兼容：若存在旧版 root LaunchDaemon，需管理员一次性清理（仅首次弹一次密码）。
    let old_daemon = "/Library/LaunchDaemons/com.x8fde.im.wxgo.plist";
    if Path::new(old_daemon).exists() {
        let inner = format!(
            "launchctl bootout system/com.x8fde.im.wxgo 2>/dev/null; rm -f {daemon}; true",
            daemon = shell_quote(old_daemon)
        );
        let osa = format!(
            "do shell script \"{}\" with administrator privileges",
            escape_for_osascript(&inner)
        );
        let out = Command::new("/usr/bin/osascript")
            .args(["-e", &osa])
            .output()
            .map_err(|e| format!("清理旧版后台微信插件失败：{e}"))?;
        if !out.status.success() {
            return Err(
                "清理旧版（root）后台微信插件被拒。请允许管理员密码，或手动执行：\
                 sudo launchctl bootout system/com.x8fde.im.wxgo 并删除 \
                 /Library/LaunchDaemons/com.x8fde.im.wxgo.plist 后重试。"
                    .into(),
            );
        }
    }
    // 同名 user agent 也先清掉（无需管理员），保证单实例 + 新令牌生效。
    let uid = current_uid();
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("gui/{uid}/com.x8fde.im.wxgo")])
        .output();

    // 1) 令牌写入 0600 临时文件（当前用户可读），经 X8TOKEN_FILE 注入。
    let tok_path = std::env::temp_dir().join(format!("x8_wx_token_{port}.txt"));
    fs::write(&tok_path, format!("{token}\n"))
        .map_err(|e| format!("写令牌文件失败：{e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tok_path, fs::Permissions::from_mode(0o600));
    }

    // 2) 生成 LaunchAgent plist，装入 ~/Library/LaunchAgents（用户态，免管理员）。
    let home = home_dir().ok_or_else(|| "无法定位当前用户家目录（HOME 为空）".to_string())?;
    let agent_dir = Path::new(&home).join("Library").join("LaunchAgents");
    fs::create_dir_all(&agent_dir).map_err(|e| format!("建 LaunchAgents 目录失败：{e}"))?;
    let plist_path = agent_dir.join("com.x8fde.im.wxgo.plist");
    let work_dir = bin
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/tmp".to_string());
    let log_path = format!("/tmp/x8_wx_{port}.log");
    let plist = build_wxgo_plist(bin, args, &tok_path.display().to_string(), &work_dir, &log_path);
    fs::write(&plist_path, plist).map_err(|e| format!("写 LaunchAgent plist 失败：{e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&plist_path, fs::Permissions::from_mode(0o644));
    }

    // 3) 以当前用户 load（无需管理员）。优先用现代 bootstrap gui 域；失败退用旧式 load。
    let label = format!("gui/{uid}/com.x8fde.im.wxgo");
    let mut diag = String::new();
    let out = Command::new("/bin/launchctl")
        .args([
            "bootstrap",
            &format!("gui/{uid}"),
            &plist_path.display().to_string(),
        ])
        .output();
    let bootstrap_ok = matches!(out, Ok(ref o) if o.status.success());
    if !bootstrap_ok {
        if let Ok(o) = &out {
            diag.push_str(&format!(
                "bootstrap 失败：{}\n",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
        let _ = Command::new("/bin/launchctl")
            .args(["load", &plist_path.display().to_string()])
            .output();
    }

    // 3.5) 真正拉起：plist RunAtLoad=false，必须显式 kickstart，否则只注册不运行
    //      （表现为「无日志 + 端口超时」——这是 1.6.84 失败的根因）。
    let ks = Command::new("/bin/launchctl")
        .args(["kickstart", "-k", &label])
        .output();
    if let Ok(o) = &ks {
        if !o.status.success() {
            diag.push_str(&format!(
                "kickstart 失败：{}\n",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
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
    // 超时：把真实死因（bootstrap/kickstart 报错 / 签名拦截 / panic）从日志 + 诊断拼进错误。
    let tail = read_log_tail(&log_path, 40);
    let _ = fs::remove_file(&tok_path);
    let mut reason = format!("微信插件启动超时，端口 {port} 未就绪。{tail}");
    if !diag.trim().is_empty() {
        reason.push_str(&format!("\nlaunchctl 诊断：\n{diag}"));
    }
    if tail.contains("无日志") {
        reason.push_str(
            "\n若提示端口被占用，多是 1.6.83 的旧 root 守护未清掉，请手动：\
             sudo launchctl bootout system/com.x8fde.im.wxgo",
        );
    }
    Err(reason)
}

/// 当前用户家目录（HOME）。
fn home_dir() -> Option<String> {
    let h = std::env::var("HOME").ok()?;
    if h.trim().is_empty() {
        None
    } else {
        Some(h)
    }
}

/// 当前用户 uid（用于 gui/<uid> LaunchAgent 域）。
fn current_uid() -> u32 {
    #[cfg(unix)]
    unsafe {
        libc::getuid()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// 构造 wxgo 的 LaunchAgent plist（XML）。令牌只以文件路径（X8TOKEN_FILE）传递，
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
    <!-- 只在「异常退出」（panic / 崩溃 / 被杀）时由 launchd 重新拉起；
         正常退出（/api/app/shutdown → exit 0）不复活，避免与 stop_with 打架。
         ThrottleInterval 防崩溃循环狂刷。2026-11-11：原先 KeepAlive=false，
         插件一崩就永久死掉，界面只剩「状态未知 / 插件无响应」。 -->
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>5</integer>
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

/// 按端口反查并杀掉用户态 wxgo（优雅退出由调用方先发 /api/app/shutdown）。
pub fn kill_by_port(port: u16) {
    // 优先用 launchctl 按 label 停掉 user agent（优雅，不靠猜 PID）。
    let uid = current_uid();
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("gui/{uid}/com.x8fde.im.wxgo")])
        .output();
    // 反查监听该端口的 PID，逐个 TERM（含残留 / 同名其他实例）。
    let out = Command::new("/usr/sbin/lsof")
        .arg(format!("-tiTCP:{port}"))
        .arg("-sTCP:LISTEN")
        .output();
    if let Ok(o) = out {
        for pid in String::from_utf8_lossy(&o.stdout).split_whitespace() {
            let _ = Command::new("/bin/kill").args(["-TERM", pid]).output();
        }
    }
    // 兜底：按进程名收（lsof 看不到时）
    let _ = Command::new("/usr/bin/pkill")
        .args(["-f", "wxadapter"])
        .output();
    let _ = Command::new("/usr/bin/pkill")
        .args(["-f", "wxgo"])
        .output();
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
