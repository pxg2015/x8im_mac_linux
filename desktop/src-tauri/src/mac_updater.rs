//! macOS / Linux 应用内升级（2026-11-02 恢复；2026-11-04 加「确认 + 进度」）
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
//!   用 `tauri signer sign` 做（私钥 `keys/xim.key` 绝不进 CI 仓）。
//!
//! ## 2026-11-04 老8 定：升级必须「先说一声」
//! 原话：「mac 目前是能自动升级了，直接到弹密码的地方了。这个太静默了，需要提升前在 im 内
//! 有个提示，可取消，只有确认后再进行自动升级，且需要有升级进度呈现」。
//!
//! 旧行为：调度器 60s 首检 → 发现新版 → 直接 `download_and_install` → 替换 .app 时
//! 插件用 AppleScript 弹系统密码框。用户视角就是「客户端毫无征兆地跳出输密码」。
//!
//! 现在的状态机（`stage`）：
//! ```text
//! idle ──check──> available ──用户点「立即升级」──> downloading ──> installing ──> started(重启)
//!   ^                 │                                    │
//!   └──「稍后提醒」────┘                                    └──失败──> failed ──(前端可重试)──┘
//! ```
//! - **只发现、不安装**：`probe()` 只做 check，拿到版本号就停在 `available`，
//!   同时 emit `x8-upd-ask`；前端（IM 主窗内）弹确认条，点「立即升级」才调 `mac_self_update`。
//! - **进度**：`download_and_install` 的 on_chunk 回调把真实字节数写进状态，前端每 800ms
//!   轮询 `mac_upd_status`（**不赌事件通道**——远程域下 IPC 反复出过事），拿到
//!   stage/got/total 后走既有 `onAutoUpdProg` 渲染顶部横幅 + 右下角进度小窗。
//! - **可取消**：`mac_upd_snooze(hours)` 记「版本 + 截止时间」并落盘，到期前不打扰。
//!   取消只覆盖「确认前」这一档：tauri-plugin-updater 的 download_and_install 没有 abort。
//! - **过渡兼容**：本次改造之前的客户端（< 1.6.54）没有确认 UI 可弹，若一味等确认就永远
//!   升不上来。故 `needs_confirm()` 按版本分流：< 1.6.54 维持旧的静默自动装（最后一跳），
//!   >= 1.6.54 一律要求确认。只影响一次过渡。
//!
//! ## 行为速查
//! - 启动 60s 后首检，之后每 15 分钟复查；只检查、只提示。
//! - 用户确认 → `mac_self_update` 立即返回 `accepted`，真实工作在后台线程。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// 启动后首检延迟（秒）。给网络栈与界面留出启动时间。
const FIRST_CHECK_SECS: u64 = 60;
/// 复查间隔（秒）。
const RECHECK_SECS: u64 = 900;
/// 从这一版起，升级必须先经用户确认（见 `needs_confirm`）。
const CONFIRM_SINCE: &str = "1.6.54";
/// 同一时刻只允许一条升级流程。
static BUSY: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// 2026-11-04 Linux 静默升级（AppImage）
// Linux 复用同一套 `app.updater()` 通道：插件会读 `APPIMAGE` 环境变量，把 extracted path
// 指向那个 AppImage 文件，下载后按 tar 内第一个 `.AppImage` 条目原地替换（失败自动回滚）。
// 所以：
//   - **只有 AppImage 版**能真正静默（deb/rpm 装的是 /usr/bin/x8im，替换要 root）；
//   - deb/rpm 仍走 main.rs 的 `linux_self_update`（pkexec 弹一次系统授权）；
//   - 更新包由 CI 把 AppImage 打成 `.AppImage.tar.gz`（build-linux.yml 的 Collect artifacts），
//     签名同样在本机做，端点仍是主 `latest.json` 的 `platforms.linux-*` 段。
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 升级状态（前端轮询 `mac_upd_status` 拿它）
// ---------------------------------------------------------------------------
#[derive(Clone, Default)]
#[allow(dead_code)]
struct UpdSt {
    /// idle | available | downloading | installing | started | failed
    stage: String,
    ver: String,
    cur: String,
    got: u64,
    total: u64,
    err: String,
    /// 「稍后提醒」：只对这个版本、到这一刻为止不再提示
    snooze_ver: String,
    snooze_until: i64,
}

#[allow(dead_code)]
fn state() -> &'static Mutex<UpdSt> {
    static S: OnceLock<Mutex<UpdSt>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(UpdSt {
            stage: "idle".to_string(),
            ..Default::default()
        })
    })
}

#[allow(dead_code)]
fn st_get() -> UpdSt {
    state().lock().map(|g| g.clone()).unwrap_or_default()
}

#[allow(dead_code)]
fn st_set(f: impl FnOnce(&mut UpdSt)) {
    if let Ok(mut g) = state().lock() {
        f(&mut g);
    }
}

#[allow(dead_code)]
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[allow(dead_code)]
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 语义化版本比较：a >= b。非数字段落按 0 处理（本项目的版本号都是纯数字点分）。
#[allow(dead_code)]
fn ver_ge(a: &str, b: &str) -> bool {
    let pa: Vec<i32> = a.split('.').map(|x| x.trim().parse().unwrap_or(0)).collect();
    let pb: Vec<i32> = b.split('.').map(|x| x.trim().parse().unwrap_or(0)).collect();
    for i in 0..pa.len().max(pb.len()) {
        let x = *pa.get(i).unwrap_or(&0);
        let y = *pb.get(i).unwrap_or(&0);
        if x != y {
            return x > y;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// 日志与状态目录：mac 没有 Windows 那套 ProgramData 路径，用各平台惯例位置
// ---------------------------------------------------------------------------
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn mac_log_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    #[cfg(target_os = "macos")]
    let dir = std::path::PathBuf::from(home)
        .join("Library")
        .join("Logs")
        .join("BangXin");
    #[cfg(target_os = "linux")]
    let dir = std::path::PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("BangXin");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("update.log"))
}

/// 「稍后提醒」落盘位置（与日志同目录，升级状态量小、不值得另建层次）
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn snooze_file() -> Option<std::path::PathBuf> {
    mac_log_path().and_then(|p| p.parent().map(|d| d.join("update-snooze.txt")))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn load_snooze() {
    let Some(p) = snooze_file() else { return };
    let Ok(txt) = std::fs::read_to_string(&p) else {
        return;
    };
    // 格式：<version>|<unix_secs>
    let mut it = txt.trim().splitn(2, '|');
    let (Some(v), Some(t)) = (it.next(), it.next()) else {
        return;
    };
    let Ok(until) = t.trim().parse::<i64>() else {
        return;
    };
    if until > now_secs() {
        let ver = v.trim().to_string();
        st_set(|s| {
            s.snooze_ver = ver;
            s.snooze_until = until;
        });
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn save_snooze(ver: &str, until: i64) {
    let Some(p) = snooze_file() else { return };
    let _ = std::fs::write(&p, format!("{ver}|{until}"));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn mac_log(msg: &str) {
    use std::io::Write;
    let line = format!("[{}] {msg}\n", now_secs());
    if let Some(p) = mac_log_path() {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            let _ = f.write_all(line.as_bytes());
        }
    }
    eprintln!("{line}");
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn mac_log(_msg: &str) {}

// ---------------------------------------------------------------------------
// 事件：提示可用（x8-upd-ask）+ 进度（x8-upd-stage，字段沿用 Windows 那套）
// ---------------------------------------------------------------------------
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn emit_ask(app: &tauri::AppHandle, ver: &str, cur: &str) {
    use tauri::Emitter;
    let _ = app.emit(
        "x8-upd-ask",
        serde_json::json!({ "ver": ver, "cur": cur }),
    );
    mac_log(&format!("[upd] 发现新版 v{ver}（当前 v{cur}），等待前端确认"));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn emit_stage(app: &tauri::AppHandle, stage: &str, got: u64, total: u64, err: &str) {
    use tauri::Emitter;
    let mut payload = serde_json::json!({ "stage": stage, "got": got, "total": total });
    if !err.is_empty() {
        payload["err"] = serde_json::Value::String(err.to_string());
    }
    let _ = app.emit("x8-upd-stage", payload);
}

// ---------------------------------------------------------------------------
// 核心①：只检查，不安装
// ---------------------------------------------------------------------------

/// 返回 `true` = 发现新版并进入 `available`（已 emit `x8-upd-ask`）。
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn probe(app: &tauri::AppHandle) -> Result<bool, String> {
    use tauri_plugin_updater::UpdaterExt;

    let updater = app
        .updater()
        .map_err(|e| format!("初始化升级组件失败: {e}"))?;
    let found = tauri::async_runtime::block_on(updater.check())
        .map_err(|e| format!("检查更新失败: {e}"))?;
    let cur = app.package_info().version.to_string();
    let Some(update) = found else {
        // 回到 idle：之前提示过的版本已经装上了（或服务器收回），前端据此收起确认条
        st_set(|s| {
            if s.stage == "available" || s.stage == "idle" {
                s.stage = "idle".to_string();
                s.ver.clear();
                s.got = 0;
                s.total = 0;
            }
        });
        mac_log("[mac-upd] 已是最新版本");
        return Ok(false);
    };
    let ver = update.version.clone();
    // 同一版本已经提示过、且用户点了「稍后」→ 静默期内不重复打扰
    let s = st_get();
    if s.snooze_until > now_secs() && s.snooze_ver == ver {
        mac_log(&format!("[upd] v{ver} 在稍后提醒期内，跳过"));
        return Ok(false);
    }
    let already = s.stage == "available" && s.ver == ver;
    st_set(|t| {
        t.stage = "available".to_string();
        t.ver = ver.clone();
        t.cur = cur.clone();
        t.got = 0;
        t.total = 0;
        t.err.clear();
    });
    if !already {
        emit_ask(app, &ver, &cur);
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// 核心②：用户已确认 → 下载 → 验签 → 原地替换
// ---------------------------------------------------------------------------

/// 返回安装成功的版本号；`"no-update"` = 复查时已无新版（服务器刚对齐）。
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn run_install(app: &tauri::AppHandle) -> Result<String, String> {
    use tauri_plugin_updater::UpdaterExt;

    let updater = app
        .updater()
        .map_err(|e| format!("初始化升级组件失败: {e}"))?;
    let found = tauri::async_runtime::block_on(updater.check())
        .map_err(|e| format!("检查更新失败: {e}"))?;
    let Some(update) = found else {
        st_set(|s| s.stage = "idle".to_string());
        mac_log("[upd] 复查已无新版，本轮结束");
        return Ok("no-update".to_string());
    };
    let ver = update.version.clone();
    mac_log(&format!("[upd] 开始下载 v{ver}（{}）", update.download_url));

    st_set(|s| {
        s.stage = "downloading".to_string();
        s.ver = ver.clone();
        s.got = 0;
        s.total = 0;
        s.err.clear();
    });
    emit_stage(app, "downloading", 0, 0, "");

    let a_chunk = app.clone();
    let a_done = app.clone();
    let mut got: u64 = 0;
    let mut total: u64 = 0;
    let mut last_emit: u64 = 0;
    let mut last_logged: u64 = 0;

    // 插件内部先 minisign 验签（pubkey 来自 tauri.conf.json 的 plugins.updater.pubkey），
    // 验签不过直接失败、绝不碰现有 app；安装时 APFS 走 renamex_np 原子交换，
    // 目标目录不可写则插件自己用 AppleScript 请求管理员授权（**这就是那个密码框**，
    // 现在只有用户点过「立即升级」才会走到）。
    let res = tauri::async_runtime::block_on(update.download_and_install(
        move |chunk, len| {
            got += chunk as u64;
            if let Some(l) = len {
                total = l;
            }
            // 进度：400ms 闸门，避免每 chunk 都抢状态锁（几百次/秒）
            let now = now_millis();
            if now.saturating_sub(last_emit) >= 400 || (total > 0 && got >= total) {
                last_emit = now;
                st_set(|s| {
                    s.got = got;
                    s.total = total;
                });
                emit_stage(&a_chunk, "downloading", got, total, "");
                if now.saturating_sub(last_logged) >= 4000 {
                    last_logged = now;
                    mac_log(&format!("[upd] 下载进度 {got}/{total}"));
                }
            }
        },
        move || {
            // 下载完成 → 进入安装（这一步可能弹系统授权窗，前端文案会提示输密码）
            st_set(|s| s.stage = "installing".to_string());
            emit_stage(&a_done, "downloaded", 0, 0, "");
            mac_log("[upd] 下载完成，开始安装");
        },
    ));

    match res {
        Ok(()) => {
            st_set(|s| s.stage = "started".to_string());
            emit_stage(app, "started", 0, 0, "");
            mac_log(&format!("[upd] 安装完成 v{ver}"));
            Ok(ver)
        }
        Err(e) => {
            let msg = format!("下载/安装失败: {e}");
            st_set(|s| {
                s.stage = "failed".to_string();
                s.err = msg.clone();
            });
            emit_stage(app, "failed", 0, 0, &msg);
            mac_log(&format!("[upd] {msg}"));
            Err(msg)
        }
    }
}

// ---------------------------------------------------------------------------
// 重启：macOS 必须用 open 拉起 .app，直接 exec 二进制会丢 LaunchServices 上下文
// （dock 图标不聚焦、Bundle identifier 关联失效）；Linux 直接在后台 `setsid` 拉起
// APPIMAGE（或当前 exe）即可，父进程退出不会带走它。
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
fn restart_app() {
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
        mac_log(&format!("[upd] 已安排重启：open '{ap}'"));
    } else {
        mac_log("[upd] 取不到当前可执行路径，无法自动重启，请手动重开客户端");
    }
    std::process::exit(0);
}

#[cfg(target_os = "linux")]
fn restart_app() {
    use std::process::Stdio;
    // AppImage 版必须重启 APPIMAGE 指向的那个文件（不是 /tmp/.mount_* 里的副本）
    let exe = std::env::var("APPIMAGE")
        .ok()
        .filter(|p| std::path::Path::new(p).is_file())
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .map(|p| p.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "x8im".to_string());
    let quoted = exe.replace('\'', "'\\''");
    let _ = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "sleep 3; (command -v setsid >/dev/null && setsid '{quoted}' || nohup '{quoted}') \
             >/dev/null 2>&1 &"
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    mac_log(&format!("[linux-upd] 已安排重启：{exe}"));
    std::process::exit(0);
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[allow(dead_code)]
fn restart_app() {}

// ---------------------------------------------------------------------------
// 前端命令（同步 fn —— 远程域下 async 命令的 IPC 会永久 pending）
// ---------------------------------------------------------------------------

/// 用户点「⚡ 立即升级」。同步、即返；真实工作在后台线程。
/// Linux 上只在 AppImage 环境下有意义（否则返回 `not-appimage`，由前端的
/// `linux_self_update` 走 pkexec 装 deb/rpm）。
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tauri::command]
pub fn mac_self_update(app: tauri::AppHandle) -> Result<String, String> {
    #[cfg(target_os = "linux")]
    if std::env::var_os("APPIMAGE").is_none() {
        return Ok("not-appimage".to_string());
    }
    if BUSY.swap(true, Ordering::SeqCst) {
        return Ok("busy".to_string());
    }
    // 用户已明确同意 → 清掉「稍后提醒」，避免装完失败后还被冷藏期挡住
    st_set(|s| {
        s.snooze_ver.clear();
        s.snooze_until = 0;
    });
    save_snooze("", 0);
    mac_log("[入口] mac_self_update（用户已确认）");
    let handle = app.clone();
    // ⚠️ 必须显式指定返回类型 ()：闭包尾是 exit 分支（类型 !），Rust 2024 起 never-type
    //    fallback 不再默认成 ()，不写就会报 dependency_on_unit_never_type_fallback。
    std::thread::spawn::<_, ()>(move || {
        match run_install(&handle) {
            Ok(ver) if ver != "no-update" => {
                // 已装上新版本 → 重启（进程即将退出，busy 不用复位）
                restart_app();
            }
            Ok(_) => {
                BUSY.store(false, Ordering::SeqCst);
            }
            Err(_e) => {
                // 失败原因已写进状态并 emit，前端 updFloatShow('failed') 会显示；
                // 这里只需放开 BUSY，允许用户重试。
                BUSY.store(false, Ordering::SeqCst);
            }
        }
    });
    Ok("accepted".to_string())
}

/// 非 mac/linux 平台：同名同签名的空实现，让 generate_handler! 的命令表三端一致。
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[tauri::command]
pub fn mac_self_update(_app: tauri::AppHandle) -> Result<String, String> {
    Ok("mac_self_update: not supported on this platform".to_string())
}

/// 升级状态（JSON 字符串）。前端每 800ms 轮询一次，**不依赖事件通道**：
/// 远程域下 event/async IPC 都出过问题，轮询同步命令是唯一稳的读法。
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tauri::command]
pub fn mac_upd_status(app: tauri::AppHandle) -> String {
    let s = st_get();
    let cur = if s.cur.is_empty() {
        app.package_info().version.to_string()
    } else {
        s.cur.clone()
    };
    let stage = if s.stage.is_empty() {
        "idle".to_string()
    } else {
        s.stage.clone()
    };
    serde_json::json!({
        "stage": stage,
        "ver": s.ver,
        "cur": cur,
        "got": s.got,
        "total": s.total,
        "err": s.err,
        "busy": BUSY.load(Ordering::SeqCst),
        // 是否需要「先说一声」：过渡版本（< CONFIRM_SINCE）没有确认 UI，只能沿用静默
        "needConfirm": ver_ge(&cur, CONFIRM_SINCE),
        "snoozeUntil": s.snooze_until,
    })
    .to_string()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[tauri::command]
pub fn mac_upd_status(_app: tauri::AppHandle) -> String {
    "{\"stage\":\"idle\",\"unsupported\":true}".to_string()
}

/// 用户点「稍后提醒」。hours 缺省 2 小时，配合前端 2 小时后再问一次。
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tauri::command]
pub fn mac_upd_snooze(app: tauri::AppHandle, hours: Option<u32>) {
    let h = hours.unwrap_or(2).clamp(1, 24) as i64;
    let until = now_secs() + h * 3600;
    let ver = st_get().ver;
    st_set(|s| {
        s.snooze_ver = ver.clone();
        s.snooze_until = until;
        if s.stage == "available" {
            s.stage = "idle".to_string();
        }
    });
    let _ = save_snooze(&ver, until);
    mac_log(&format!("[入口] mac_upd_snooze v{ver} {h}h（至 unix {until}）"));
    emit_stage(&app, "snoozed", 0, 0, "");
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[tauri::command]
pub fn mac_upd_snooze(_app: tauri::AppHandle, _hours: Option<u32>) {}

// ---------------------------------------------------------------------------
// 调度器：启动首检 + 周期复查（只发现、只提示；安装必须等用户确认）
// ---------------------------------------------------------------------------

/// 这一版客户端是否已经具备「确认 + 进度」能力。
/// false = 过渡版本（< 1.6.54）：前端还没有轮询/确认 UI，若也停下等确认就永远升不上来，
/// 故对它维持旧的静默自动装（最后一次）；true = 一律等用户点。
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn needs_confirm(app: &tauri::AppHandle) -> bool {
    ver_ge(&app.package_info().version.to_string(), CONFIRM_SINCE)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn spawn_scheduler(app: tauri::AppHandle) {
    use std::time::Duration;
    // Linux：只有 AppImage 版能自替换；deb/rpm 装的不自动跑（替换 /usr/bin/x8im 要 root，
    // 每 15 分钟失败一次只会刷日志），留给用户点「一键升级」走 pkexec。
    #[cfg(target_os = "linux")]
    if std::env::var_os("APPIMAGE").is_none() {
        mac_log("[linux-upd] 非 AppImage 运行，跳过自动静默升级调度");
        return;
    }
    load_snooze();
    std::thread::spawn::<_, ()>(move || {
        std::thread::sleep(Duration::from_secs(FIRST_CHECK_SECS));
        loop {
            if !BUSY.load(Ordering::SeqCst) {
                if needs_confirm(&app) {
                    // 新流程：只检查、只提示，安装交给前端确认后的 mac_self_update
                    if let Err(e) = probe(&app) {
                        mac_log(&format!("[upd] 检查失败：{e}"));
                    }
                } else {
                    // 过渡版本：保持旧行为（静默装），只为把 < 1.6.54 拉上来
                    mac_log("[upd] 过渡版本（< 1.6.54）：沿用静默自动升级");
                    let _ = mac_self_update(app.clone());
                }
            }
            std::thread::sleep(Duration::from_secs(RECHECK_SECS));
        }
    });
    mac_log(&format!(
        "[upd] 调度器已启动：{FIRST_CHECK_SECS}s 首检，之后每 {RECHECK_SECS}s 复查（确认制）"
    ));
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn spawn_scheduler(_app: tauri::AppHandle) {}
