//! macOS 静默升级（2026-11-02 恢复）
//!
//! ## 背景：mac 的静默升级曾经有，后来被拆掉了
//! 1.5.28 ~ 1.5.43 期间，mac 走的是 `tauri-plugin-updater` 原生通道：
//! `latest.json` 里带 `platforms.darwin-aarch64` / `darwin-x86_64` 两段（含 tauri minisign 签名），
//! 产物是 `.app.tar.gz` + `.app.tar.gz.sig`（服务器 `latest.json.bak_1531` /
//! `bak_20261023_090356` / `bak_20261023_230833` 里都还留着那两段签名，是实证）。
//!
//! 后来三件事把它拆了：
//! 1. 品牌改名 + 平台改造后，CI 不再产 `.app.tar.gz` / `.sig`（`tauri.macos.conf.json` 里
//!    `createUpdaterArtifacts` 为 false），`latest.json` 也不再下发 darwin 段；
//! 2. `patch_darwin_sig.py` 里的 URL 还写死 `/app/mac/X8.IM-*.app.tar.gz`，而服务器
//!    `/opt/x8im/mac/` 目录早已删除（包体全走七牛）→ 那条 URL 是死的；
//! 3. 2026-10-26 前端 P0 修复发现：远程域（https://im.x8fde.com/app/）下 **async 插件命令
//!    的 IPC 会永久 pending**，`plugin:updater|check` 取不到 rid，于是把非 Windows 分支
//!    降级成 lite 条（手动下载）。mac 就此退化成"下载 dmg 自己装"。
//!
//! ## 本模块怎么补回来（且**完全不碰 Windows**）
//! - **插件本来就三端都注册了**（`main.rs` 的 `.plugin(tauri_plugin_updater::Builder::new().build())`），
//!   缺的只是 mac 侧「谁来调它」。前端 `plugin:updater|check` 那条路在远程域下不可靠，
//!   所以**改由 Rust 进程内调用** `app.updater()`（不受 IPC pending 影响）。
//! - **端点独立**：mac 读 `https://im.x8fde.com/app/latest-mac.json`（顶层 `version` = mac 版本号）。
//!   Windows 的 `updater.rs` 读的是主 `latest.json` 的**顶层 version**（见 updater.rs 的
//!   `.get("version")`），所以 mac 发版绝不能抬主 latest.json 的顶层 version，否则 Windows
//!   会去下载 windows-x86_64 段里那个旧包 → 装完版本不变 → 死循环提示。
//!   独立端点 + 独立 version，两边彻底隔离。
//! - **平台配置独立**：端点写在 `tauri.macos.conf.json`（Tauri v2 构建 mac 时会自动合并该文件，
//!   与 `tauri.linux.conf.json` 同理），Windows 的 `tauri.conf.json` 一个字都不改。
//! - 产物形态：`.app.tar.gz`（内含 `BangXin.app`），由 CI 从 dmg 里 `tar` 出来；签名在**本机**
//!   用 `tauri signer sign` 做（私钥 `keys/x8im.key` 绝不进 CI 仓）。
//!
//! ## 行为
//! - 启动 60s 后首检，之后每 15 分钟复查 → 有新版就自动下载 → 验签 → 原地替换 `.app` → 重启。
//!   （对齐 Windows 的静默口径；替换由插件完成，权限不足时插件自己用 AppleScript 请求提权。）
//! - 用户点「⚡ 一键升级」→ 前端调 `mac_self_update` → 立刻返回 `accepted`，真实工作在后台线程。

use std::sync::atomic::AtomicBool;

/// 启动后首检延迟（秒）。给网络栈与界面留出启动时间。
const FIRST_CHECK_SECS: u64 = 60;
/// 复查间隔（秒）。
const RECHECK_SECS: u64 = 900;
/// 同一时刻只允许一条升级流程。
static BUSY: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// 日志：mac 没有 Windows 那套 ProgramData 路径，用 macOS 惯例 ~/Library/Logs
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
fn mac_log_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = std::path::PathBuf::from(home).join("Library").join("Logs").join("BangXin");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("update.log"))
}

#[cfg(target_os = "macos")]
fn mac_log(msg: &str) {
    use std::io::Write;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("[{ts}] {msg}\n");
    if let Some(p) = mac_log_path() {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            let _ = f.write_all(line.as_bytes());
        }
    }
    eprintln!("{line}");
}

#[cfg(not(target_os = "macos"))]
fn mac_log(_msg: &str) {}

// ---------------------------------------------------------------------------
// 核心：检查 → 下载 → 验签 → 原地替换
// ---------------------------------------------------------------------------

/// 返回 `Some(版本号)` = 已装上新版本；`None` = 已是最新。
#[cfg(target_os = "macos")]
fn check_and_install(app: &tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_updater::UpdaterExt;

    let updater = app.updater().map_err(|e| format!("初始化升级组件失败: {e}"))?;
    // check() 是 async；本模块全在后台线程里跑，用 block_on 等它，不占用主线程。
    let found = tauri::async_runtime::block_on(updater.check())
        .map_err(|e| format!("检查更新失败: {e}"))?;
    let Some(update) = found else {
        mac_log("[mac-upd] 已是最新版本");
        return Ok(None);
    };
    let ver = update.version.clone();
    mac_log(&format!(
        "[mac-upd] 发现新版 v{}（{}），开始下载",
        ver,
        update.download_url
    ));
    // 插件内部会先 minisign 验签（pubkey 来自 tauri.conf.json 的 plugins.updater.pubkey），
    // 验签不过直接失败、绝不碰现有 app；安装时 APFS 走 renamex_np 原子交换，
    // 目标目录不可写则插件自己用 AppleScript 请求管理员授权。
    let mut got: u64 = 0;
    let mut total: u64 = 0;
    tauri::async_runtime::block_on(update.download_and_install(
        move |chunk, len| {
            got += chunk as u64;
            if let Some(l) = len {
                total = l;
            }
            if total > 0 && got % (2 * 1024 * 1024) < chunk as u64 {
                mac_log(&format!("[mac-upd] 下载进度 {got}/{total}"));
            }
        },
        || {
            mac_log("[mac-upd] 下载完成，开始安装");
        },
    ))
    .map_err(|e| format!("下载/安装失败: {e}"))?;
    mac_log(&format!("[mac-upd] 安装完成 v{ver}"));
    Ok(Some(ver))
}

// ---------------------------------------------------------------------------
// 重启：macOS 必须用 open 拉起 .app，直接 exec 二进制会丢 LaunchServices 上下文
// （dock 图标不聚焦、Bundle identifier 关联失效）。
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
fn restart_mac() {
    use std::process::Stdio;
    // /Applications/BangXin.app/Contents/MacOS/BangXin → 上溯三层拿到 .app 根
    let app_path = std::env::current_exe().ok().and_then(|p| {
        p.parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .map(|p| p.to_string_lossy().to_string())
    });
    if let Some(ap) = app_path {
        let quoted = ap.replace('\'', "'\\''");
        // 独立子进程：父进程 exit(0) 不会把它带走（与 Linux 版 setsid/nohup 同理）
        let _ = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("sleep 2; /usr/bin/open '{quoted}'"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        mac_log(&format!("[mac-upd] 已安排重启：open '{ap}'"));
    } else {
        mac_log("[mac-upd] 取不到当前可执行路径，无法自动重启，请手动重开客户端");
    }
    std::process::exit(0);
}

// ---------------------------------------------------------------------------
// 前端命令（同步 fn —— 远程域下 async 命令的 IPC 会永久 pending）
// ---------------------------------------------------------------------------

/// 用户点「⚡ 一键升级」。同步、即返；真实工作在后台线程。
#[cfg(target_os = "macos")]
#[tauri::command]
pub fn mac_self_update(app: tauri::AppHandle) -> Result<String, String> {
    use std::sync::atomic::Ordering;
    if BUSY.swap(true, Ordering::SeqCst) {
        return Ok("busy".to_string());
    }
    let handle = app.clone();
    std::thread::spawn(move || {
        match check_and_install(&handle) {
            Ok(Some(_)) => {
                // 已装上新版本 → 重启（进程即将退出，busy 不用复位）
                restart_mac();
            }
            Ok(None) => {
                BUSY.store(false, Ordering::SeqCst);
            }
            Err(e) => {
                mac_log(&format!("[mac-upd] 升级失败：{e}"));
                BUSY.store(false, Ordering::SeqCst);
            }
        }
    });
    Ok("accepted".to_string())
}

/// 非 mac 平台：同名同签名的空实现，让 generate_handler! 的命令表三端一致。
#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub fn mac_self_update(_app: tauri::AppHandle) -> Result<String, String> {
    Ok("mac_self_update: not supported on this platform".to_string())
}

// ---------------------------------------------------------------------------
// 调度器：启动首检 + 周期复查（静默自动升级，完全不经过前端 IPC）
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
pub fn spawn_scheduler(app: tauri::AppHandle) {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(FIRST_CHECK_SECS));
        loop {
            if !BUSY.load(Ordering::SeqCst) {
                let _ = mac_self_update(app.clone());
            }
            std::thread::sleep(Duration::from_secs(RECHECK_SECS));
        }
    });
    mac_log(&format!(
        "[mac-upd] 调度器已启动：{FIRST_CHECK_SECS}s 首检，之后每 {RECHECK_SECS}s 复查"
    ));
}

#[cfg(not(target_os = "macos"))]
pub fn spawn_scheduler(_app: tauri::AppHandle) {}
