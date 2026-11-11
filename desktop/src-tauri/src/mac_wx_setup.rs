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
//     一次性弹管理员密码，把 wxgo 以 root 拉起；不引入已公证的 helper（成本太高）。
//
// ⚠️ 令牌只经临时文件重定向进 stdin（root 子进程 stdin 可靠），不进 argv / 环境变量。
//   邦信本进程不持 wxgo 子进程句柄（root 进程），所以 plugin_host 在 mac 上靠端口探活。

use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};

// 与 plugin_host.rs 同口径的端口等待常量（mac 提权拉起后探活用）
const BOOT_TIMEOUT_MS: u64 = 8000;
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

/// 以 root 经 osascript 提权拉起 wxgo（令牌经临时文件重定向进 stdin），等端口就绪后返回。
pub fn launch_root(bin: &Path, args: &[String], token: &str, port: u16) -> Result<(), String> {
    // token 写入临时文件，经 stdin 重定向喂给插件（root 子进程的 stdin 可靠）。
    let tok_path = std::env::temp_dir().join(format!("x8_wx_token_{}.txt", port));
    fs::write(&tok_path, format!("X8TOKEN {token}\n"))
        .map_err(|e| format!("写令牌文件失败：{e}"))?;

    let tok_q = shell_quote(&tok_path.display().to_string());
    let bin_q = shell_quote(&bin.display().to_string());
    let args_q: Vec<String> = args.iter().map(|a| shell_quote(a)).collect();
    let args_str = args_q.join(" ");

    // 以 root 经 osascript 提权拉起；nohup + & 脱离 osascript 的 shell，避免其退出后被 SIGHUP。
    // 注意：sh -c 用双引号包裹脚本，路径用 shell_quote 的单引号包裹 —— 不存在单引号嵌套。
    let inner = format!(
        "nohup {bin_q} {args_str} < {tok_q} >/tmp/x8_wx_{port}.log 2>&1 &",
        bin_q = bin_q,
        args_str = args_str,
        tok_q = tok_q,
        port = port
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
    // 等端口就绪（与 plugin_host 同口径：100ms 间隔，8s 上限）
    let addr = format!("127.0.0.1:{port}")
        .parse::<SocketAddr>()
        .map_err(|e| format!("端口解析失败：{e}"))?;
    let tries = BOOT_TIMEOUT_MS / PROBE_INTERVAL_MS;
    for _ in 0..tries {
        std::thread::sleep(Duration::from_millis(PROBE_INTERVAL_MS));
        if TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok() {
            return Ok(());
        }
    }
    Err(format!("微信插件（root）启动超时，端口 {port} 未就绪"))
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
