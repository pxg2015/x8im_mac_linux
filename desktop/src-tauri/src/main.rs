// 邦信 Tauri 壳：原生托盘 + 关闭藏托盘
// 2026-10-12：加 windows_subsystem（去 DOS 窗）、tray-icon（右下角图标/菜单/双击）
// 2026-10-14：加 target_os 限定 + macOS 跨平台分支，使同一份源码可在两平台构建
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use std::process::Command;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::{TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, UserAttentionType,
};

// 2026-10-26 重写：自动升级收敛为独立模块。
// 此前 main.rs 里并存五套「检查→下载→验签→触发」实现（win_auto_update / _nc /
// win_patch_update / _nc / win_upd_start），改一处漏四处；且入口全绑前端 IPC，
// 而主窗是远程页面（https://im.x8fde.com/app/）——远程域下 async 命令的 IPC 会
// 永久 pending，用户点「立即升级」实际什么都没发生。详见 updater.rs 头部说明。
#[cfg(windows)]
mod updater;

// 升级用的原生 Win32 UI（确认框 + 下载进度窗）：IPC 通道在远程域下不通，
// 「必须让用户看见/点击」的交互只能走系统原生窗口。
#[cfg(windows)]
mod upd_ui;

// 2026-11-06 Linux P2P 传输内核（Go + pion sidecar）。
// 为什么只有 Linux：发行版 webkit2gtk 编译时关了 WebRTC，WebView 里没有
// RTCPeerConnection，大文件直传在 Linux 端架构性不可行。Windows/mac 的
// WebView 自带 WebRTC，不启 sidecar（命令返回 null，前端继续走原生）。
// 模块本身三端都编译（纯 std），是否启用由 p2p_enabled() 在运行期判定 ——
// 这样 X8_P2P_FORCE=1 时能在 Windows 上回归同一条 IPC 通道。
// 2026-11-07 方案 B PoC：主窗 origin 不变，/app/assets/* 由随包 dist 响应。
// 未配置 X8_LOCAL_ASSETS_DIR 且 resource_dir()/app-assets 不存在时整个模块不激活。
#[cfg(windows)]
mod local_assets;

/// 🔴 2026-11-16 Apple 吊销整改：mac 本地微信读取向导的**桩实现**。
/// 原 `mac_wx_setup.rs`（重签 WeChat.app / 注入 get-task-allow / lldb attach 取库密钥 /
/// 恢复原签名）已整体停用，文件留在 `_internal_wxlocal/` 以备申诉后复审。
/// 这 4 个命令名保持注册，是为了让前端 `useWxMsg.js` 的探测有确定返回值（恒 unsupported），
/// 而不是 `command not found`；前端据此永不弹向导。
mod mac_wx_setup {
    use serde_json::Value;
    #[tauri::command]
    pub fn wx_setup_state() -> Value {
        serde_json::json!({ "supported": false, "reason": "当前版本未包含本地微信读取能力" })
    }
    #[tauri::command]
    pub fn wx_setup_run() -> Value {
        serde_json::json!({ "ok": false, "err": "当前版本未包含本地微信读取能力" })
    }
    #[tauri::command]
    pub fn wx_restore_wechat() -> Value {
        serde_json::json!({ "ok": false, "err": "当前版本未包含本地微信读取能力" })
    }
    #[tauri::command]
    pub fn wx_wechat_relogin() -> Value {
        serde_json::json!({ "ok": false, "err": "当前版本未包含本地微信读取能力" })
    }
}

mod p2p_sidecar;
mod mail;   // 2026-11-07 内置邮件（Rust 原生 IMAP/SMTP，方案 A）
mod mail_cache; // 2026-11-08 客户端 SQLite 邮件缓存（清单/正文秒开）
mod plugin_host; // 2026-11-10 通用插件宿主：找到 → 拉起 → 验契约 → 看门狗 → 退出回收
mod wx_sidecar; // 2026-11-10 WX 消息插件（Go）的薄封装，转调 plugin_host
mod wx_cache;   // 2026-11-10 本地微信消息库（mail.db 的 wx_* 表，客户端读写）
mod signing;    // 2026-11-10 组件包三端共用验签（minisign，与 updater 同一把公钥）
mod wx_updater; // 2026-11-10 wxgo 组件自动下载 / 自动更新 / 编译时间上报
mod ipc_probe;  // 2026-11-16 IPC 卡死定性探针（双 ping + ENTER/EXIT 埋点，只观测不改业务）

// 非 Windows：mac/linux 的升级走「下载新包手动装 / 系统包管理器」，
// 不参与这套 Windows 自主升级状态机（它依赖 schtasks / NSIS / Win32 原生窗）。
// 这里提供**同名同签名**的空实现，让 generate_handler! 的命令表三端完全一致
// （前端不必按平台分支改调用），调用即返回 disabled。
#[cfg(not(windows))]
mod updater {
    use tauri::AppHandle;

    pub fn legacy_forward(_app: &AppHandle, _tag: &str) -> Result<String, String> {
        Ok("updater: not supported on this platform".into())
    }

    pub fn spawn_scheduler(_app: AppHandle) {}

    #[tauri::command]
    pub fn win_upd_start(_app: AppHandle) -> Result<String, String> {
        Ok("updater: not supported on this platform".into())
    }

    #[tauri::command]
    pub fn win_upd_snooze(_hours: Option<u32>) {}
}

/// macOS 静默升级（2026-11-02 恢复）：见 mac_updater.rs 头部说明。
/// 与 Windows 的 updater 模块彻底隔离 —— 端点（latest-mac.json）、version 口径
/// （顶层 version 归 Windows）、平台配置（tauri.macos.conf.json）三处都独立，
/// Windows 侧一个字都不动。
mod mac_updater;

/// 远程协助组件（RustDesk/X8Assist）可执行文件路径。
///
/// 2026-11-11 方案 A：组件**不再打进安装包**，改为按需下载（安装包 28~34MB → ~6MB）。
/// - Windows: `C:\Program Files\X8Assist\X8Assist.exe`（品牌客户端，安装期或运行期下载）
/// - macOS:   `~/Library/Application Support/BangXin/assist/bin/mac/RustDesk.app`（运行期下载缓存）
///            → 历史落位 `/Applications/RustDesk.app` → 安装包内资源（老版本才有）
/// - Linux:   系统 PATH / 常见位置（官方 deb/rpm，一键安装）
fn rustdesk_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let _ = app; // Linux 走系统安装（PATH + 常见位置），不读 resource_dir
        return linux_rustdesk_find();
    }
    #[cfg(not(target_os = "linux"))]
    {
        // 已装 X8Assist（定制客户端）优先：其 IPC 管道与服务名独立于官方版，
        // 用便携 rustdesk.exe 调 CLI 会连不到 X8Assist 的服务（v1.4.1）
        #[cfg(windows)]
        if let Some(p) = installed_assist_path() {
            return Some(p);
        }
        // mac 2026-11-11 方案 A：组件改为按需下载到用户目录缓存，优先于安装包内资源
        // （自 1.6.86 起安装包不再内置 RustDesk.app）。
        #[cfg(target_os = "macos")]
        if let Some(p) = macos_cached_exe() {
            return Some(p);
        }
        let dir = app.path().resource_dir().ok()?;
        // mac：不拼死文件名 —— 官方包内可执行叫 `RustDesk`（大写），
        // 一律从 Info.plist 的 CFBundleExecutable 取（见 macos_bundle_exe）
        #[cfg(target_os = "macos")]
        let exe = macos_bundle_exe(&dir.join("bin").join("mac").join("RustDesk.app"));
        #[cfg(not(target_os = "macos"))]
        let exe = Some(dir.join("bin").join("rustdesk.exe"));
        exe.filter(|p| p.is_file())
    }
}

/// 2026-10-24 P2-RS-5：Unix 子进程退出后若不 reap 会留僵尸进程（Windows 无此问题，句柄关闭即回收）。
/// spawn 后起一个后台线程 `wait()` 收尸（fire-and-forget reaper），不阻塞调用方、不改时序。
#[cfg(not(windows))]
fn reap_in_background(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// 2026-11-07 专家评审 P1-2：原来只判 spawn() 成功就返回 Ok，`xdg-open` 不存在或
/// 退出码非 0 时前端会收到「已用浏览器打开」、但客户端已经退会 → 两头落空。
/// 现在等待子进程退出码（最多 1.5s，仍在运行视为已成功拉起，后台收尸），失败返回 false
/// 让调用方换下一个候选程序 / 走复制回落。
#[cfg(not(windows))]
fn open_checked(mut cmd: Command) -> Result<bool, String> {
    let mut child = cmd.spawn().map_err(|e| format!("{e}"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return Ok(st.success()),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    // 浏览器已拉起但进程还没退（xdg-open 偶见），不能无限等
                    reap_in_background(child);
                    return Ok(true);
                }
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            Err(e) => return Err(format!("{e}")),
        }
    }
}

/// 2026-11-07 A：AppImage / 打包运行时会把 LD_LIBRARY_PATH 等指向自带库目录，
/// 子进程（xdg-open -> gio -> Chrome）继承后 Chrome 加载到不匹配的 libnss3/libglib
/// 会直接崩，而桌面入口层仍返回退出码 0 —— 表现为「报成功、浏览器没打开」。
/// 拉起外部程序前清掉这些污染变量；DBUS / DISPLAY / XDG_* 必须保留。
#[cfg(not(any(windows, target_os = "macos")))]
fn x8_clean_child_env(c: &mut Command) {
    for k in [
        "LD_LIBRARY_PATH", "LD_PRELOAD", "LD_AUDIT", "LD_DEBUG",
        "QT_PLUGIN_PATH", "QT_QPA_PLATFORM_PLUGIN_PATH", "GTK_PATH", "GTK_MODULES",
        "GIO_EXTRA_MODULES", "GDK_PIXBUF_MODULE_FILE", "GSETTINGS_SCHEMA_DIR",
        "PYTHONPATH", "PERL5LIB",
    ] {
        c.env_remove(k);
    }
}

/// 用系统默认程序打开一个外部 URL / 协议链接（各平台分别走 cmd start / open / xdg-open）
/// 2026-10-14：抽出为跨平台 helper，供 rustdesk:// 协议与工作台浏览器回落共用。
fn open_external(url: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        Command::new("cmd")
            .args(["/C", "start", "", url])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("拉起外链失败: {e}"))
    }
    #[cfg(target_os = "macos")]
    {
        let mut c = Command::new("open");
        c.arg(url);
        match open_checked(c) {
            Ok(true) => Ok(()),
            Ok(false) => Err("open 未能打开默认浏览器".into()),
            Err(e) => Err(format!("拉起外链失败: {e}")),
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        // 2026-11-07 A：http(s) 链接优先「直启已知浏览器」。
        // xdg-open / gio open 在 AppImage 环境常返回 0 却没拉起浏览器（DBus 激活发到
        // 错误会话、或浏览器继承污染的动态库后立刻崩），单看退出码挡不住。
        // 直启可执行文件绕开桌面入口层，并继续用 open_checked 复核退出码。
        let is_web = url.starts_with("https://") || url.starts_with("http://");
        let mut last = String::new();
        if is_web {
            for prog in [
                "google-chrome", "google-chrome-stable", "chromium",
                "chromium-browser", "firefox", "microsoft-edge",
            ] {
                let mut c = Command::new(prog);
                x8_clean_child_env(&mut c);
                c.arg(url);
                match open_checked(c) {
                    Ok(true) => return Ok(()),
                    Ok(false) => last = format!("{prog} 退出码非 0"),
                    Err(e) => last = format!("{prog}: {e}"),
                }
            }
        }
        // 回落到桌面默认入口（本地目录/文件也只走这里）
        for (prog, need_open) in [("xdg-open", false), ("gio", true), ("www-browser", false)] {
            let mut c = Command::new(prog);
            x8_clean_child_env(&mut c);
            if need_open { c.arg("open"); }
            c.arg(url);
            match open_checked(c) {
                Ok(true) => return Ok(()),
                Ok(false) => last = format!("{prog} 退出码非 0"),
                Err(e) => last = format!("{prog}: {e}"),
            }
        }
        Err(format!("系统浏览器拉起失败（{last}），请手动打开链接"))
    }
}

/// 2026-11-07 P2-8：Linux 原生 WebRTC 开关探针。
/// WebKitSettings:enable-webrtc 自 2.38 起默认 FALSE（官方 reference 明确写了 Default value FALSE），
/// 而 wry 0.57 / tauri-runtime-wry 2.12 全文没有调用过任何 WebSettings 设置 —— Tauri 建的
/// WebView 就一直用这个 false。若发行版 webkit2gtk 编译时带了 WebRTC，这里打开后
/// `window.RTCPeerConnection` 就会出现，Linux 客户端不再只能看直播。
/// 注意：这仍是探针，需 Linux CI 构建 + 真机用「🔍 诊断」回执确认；编译期没带 WebRTC 的
/// 发行版（部分精简包）打开也无效，此时 A 方案「去浏览器」仍是兜底。
#[cfg(target_os = "linux")]
fn enable_linux_webrtc(app: &tauri::AppHandle) {
    // webkit2gtk 2.0.2 没有 prelude 模块，必须显式导入各 auto trait（写法对齐 wry 0.57）
    use webkit2gtk::{
        glib::prelude::*,                 // Cast: downcast_ref
        PermissionRequestExt,            // allow / deny
        SettingsExt,                     // set_enable_webrtc
        UserMediaPermissionRequest,
        UserMediaPermissionRequestExt,   // is_for_audio_device / is_for_video_device
        WebViewExt,                      // connect_permission_request / settings
    };
    let Some(win) = app.get_webview_window("main") else {
        eprintln!("[x8][webrtc] main webview 未找到，开关未生效");
        return;
    };
    let ret = win.with_webview(|w| {
        let wv = w.inner();
        // 2026-11-07 二次评审 P1-4：wry 只在 attributes.permission_handler 有值时才
        // connect_permission_request；Tauri 用 config 建窗时 tauri-runtime-wry 的
        // pending.permission_request_handler 为 None → WebKitGTK 默认拒绝 getUserMedia。
        // 这里自行接管：只放行纯麦克风请求，摄像头/屏幕采集仍拒（客户端本就不支持）。
        wv.connect_permission_request(|_wv, req| {
            if let Some(m) = req.downcast_ref::<UserMediaPermissionRequest>() {
                if m.is_for_audio_device() && !m.is_for_video_device() {
                    req.allow();
                } else {
                    req.deny();
                }
                return true;
            }
            false
        });
        match WebViewExt::settings(&wv) {
            Some(s) => {
                s.set_enable_webrtc(true);        // WebKit 会同时隐含 enable-media-stream
                eprintln!("[x8][webrtc] enable-webrtc=true，麦克风权限已放行");
            }
            None => eprintln!("[x8][webrtc] WebSettings 取不到，开关未生效"),
        }
    });
    if let Err(e) = ret {
        eprintln!("[x8][webrtc] with_webview 失败: {e}");
    }
}

/// 探测已安装的 X8Assist（rdgen 定制客户端，2026-10-15 老8 点头 v1.4.1）。
/// rdgen 安装器（NSIS）落位由其内部 appname 决定：C:\Program Files\X8Assist\X8Assist.exe。
/// 2026-10-15 老8 定：对外只认 X8Assist 自己的，官方便便版自安装（RustDesk\RustDesk.exe）
/// 不再算数 —— 否则托盘照样露 RustDesk 品牌。
#[cfg(windows)]
fn installed_assist_path() -> Option<std::path::PathBuf> {
    let roots = [
        std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into()),
        std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".into()),
    ];
    for root in roots {
        let p = std::path::Path::new(&root).join(r"X8Assist\X8Assist.exe");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// 读取本机 RustDesk ID（无配置时首次运行会自动生成）
/// 2026-10-16 根治卡死：async —— Tauri 的**同步**命令跑在主线程，本函数内部
/// `Command::output()` 会阻塞到子进程结束（起 X8Assist 取 ID 常需数秒），
/// 主线程被占死 → 整窗「未响应」（实测踩过）。async 后落到运行时线程池。
#[tauri::command]
async fn get_rustdesk_id(app: tauri::AppHandle) -> Result<String, String> {
    // 2026-10-24 P1-RS-1：rustdesk_get_id 保持同步 fn，但它是阻塞子进程等待（Command::output），
    // 在 async 命令里直调会占死 tokio worker —— 调用处用 spawn_blocking 包住（与 download_file 同款）。
    tauri::async_runtime::spawn_blocking(move || rustdesk_get_id(&app))
        .await
        .map_err(|e| format!("取 RustDesk ID 线程异常：{e}"))?
}

/// 2026-10-15 老8 点头：远程协助统一走自建 RustDesk 服务器（地址见构建期注入的 RD_SERVER，
/// 阿里云北京），摆脱官方公共节点（美国 rs-ny）国内注册不稳导致的「设备离线/连不上」。
/// 把 ID/中继服务器 + Key 幂等写进用户级 RustDesk2.toml（路径随平台标准位置）。
/// 返回是否发生了写入；写入前先结束在跑的 RustDesk，防止其退出时回写旧配置。
/// ⚠️ 2026-10-16 实测修正（老8 点头「查证并修」）：**写入目标必须按实际消费方定**。
/// Windows 上实际跑的是 rdgen 品牌客户端 X8Assist（app-name = X8Assist），它读写的配置是
/// `%APPDATA%\X8Assist\config\X8Assist2.toml`；而 `%APPDATA%\RustDesk\config` 属于**官方 RustDesk**
/// （本机实测：该目录 `RustDesk_local.toml` 的 remote_id=493113368，品牌客户端是 255610226，
/// 两个目录对应两个互不相干的实例；品牌目录里才有 `X8Assist_local.toml`/`peers/`/日志）。
/// 原实现把三键写进 `%APPDATA%\RustDesk\config\X8Assist2.toml` → 无人读取，等于从未生效；
/// 目前能连自建服务器靠的是 `librustdesk.dll` 里**编译期注入**的地址/Key（实测 dll 内
/// 内置地址与 key 各若干处）—— 一旦换地址/换 Key 或换到未注入的包，就会失联。
/// mac/linux 仍是官方 RustDesk（无品牌包），目录与文件名保持 `RustDesk` / `RustDesk2.toml`。
// 🔴 2026-10-26：改回**构建期环境变量注入**，不再硬编码 —— mac/linux 开源构建仓
//    （pxg2015/x8im_mac_linux，public）会把本文件原样公开，写死等于把基础设施地址外泄。
//    RD_SERVER / RD_SERVER_KEY 由 CI 的 GitHub Secrets 或本地 _build.sh 注入；
//    漏配 → 远程协助静默不可用（其它功能不受影响）。
const RD_SERVER: &str = match option_env!("RD_SERVER") { Some(v) => v, None => "" };
const RD_SERVER_KEY: &str = match option_env!("RD_SERVER_KEY") { Some(v) => v, None => "" };

// 官方 RustDesk 用户级配置目录（mac/linux 用；Windows 走品牌 X8Assist 自己的目录，
// 不经过此函数 —— 故加 not(windows) 门，避免 Windows 构建下被判死代码）。
#[cfg(not(windows))]
fn rustdesk_user_config_dir() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        std::env::var("APPDATA")
            .ok()
            .map(|d| std::path::Path::new(&d).join("RustDesk").join("config"))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var("HOME")
            .ok()
            .map(|d| std::path::Path::new(&d).join("Library/Preferences/com.carriez.RustDesk"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var("HOME")
            .ok()
            .map(|d| std::path::Path::new(&d).join(".config/rustdesk"))
    }
}

/// 2026-10-15 老8 要求：拉起 RustDesk 前先结束已有实例（远程机曾 3 个并存，
/// 身份/注册互相踩、hbbs 只认最后注册的那个）。仅杀用户级 UI 进程；
/// SYSTEM 服务进程不受 taskkill /IM 影响（服务化常驻场景不受破坏）。
fn kill_rustdesk_procs() {
    #[cfg(windows)]
    for img in ["rustdesk.exe", "X8Assist.exe"] {
        // 🔴 2026-11-07 根治「托盘点退出完全没反应」：旧实现是 `.output()`（管道 +
        // read_to_end 等 EOF）。taskkill 早已退出、但 stdout 写端若被常驻进程继承，
        // 就永久等不到 EOF —— 老8 23:10 实测：退出线程 0 CPU / EventPairLow 常驻、
        // 进程列表里没有 taskkill 残留，进程就是不退。10-24 已因同一问题把别处换成
        // win_run_capture_to（输出落临时文件 + try_wait + 超时 kill），这里漏改。
        // 改后最坏 3s 必返回，且所有「拉起 RustDesk 前先杀旧实例」的调用点都不再干挂。
        let _ = win_run_capture_to("taskkill", &["/F", "/IM", img], 3);
    }
    // mac/linux：官方 RustDesk 进程名恒为 rustdesk（与 Windows taskkill rustdesk.exe 同语义；
    // 用户自装的官方 RustDesk 一并杀掉，与 Windows 行为一致）。-x 精确匹配进程名，不会误伤。
    #[cfg(not(windows))]
    {
        let _ = Command::new("pkill").args(["-x", "rustdesk"]).output();
    }
}

fn ensure_rustdesk_server_config() -> Result<(), String> {
    // 🔴 2026-11-06 根治「Key 不匹配」（老8 报 Windows 连 mac）：
    // 构建期漏注入时 RD_SERVER_KEY 是空字符串，旧实现照样写盘 → `key = ''`。
    // 而 rustdesk 的 `get_key()`（src/common.rs:2158）在配置取不到 key 时会**静默回落**
    // 编译期常量 RS_PUB_KEY（官方公网 key），于是本机带着公网 key 去连自建 hbbs，
    // 服务端 `rendezvous_server.rs:682` 比对失败 → 客户端只显示「Key 不匹配」，
    // 既看不出是哪一端、也看不出是空值。故：注入缺失一律拒写并显式报错。
    if RD_SERVER.is_empty() || RD_SERVER_KEY.is_empty() {
        return Err("远程协助未配置服务器：本安装包缺少 RD_SERVER / RD_SERVER_KEY 注入，请升级到完整包".into());
    }
    // Windows：品牌客户端 X8Assist 的配置（实测读写处）
    #[cfg(windows)]
    let (dir, fname) = match x8assist_config_dir() {
        Some(d) => (d, "X8Assist2.toml"),
        None => return Err("未找到 X8Assist 配置目录（远程协助组件可能未安装）".into()),
    };
    // mac/linux：邦信内嵌/依赖的是官方 RustDesk（app-name = RustDesk）
    #[cfg(not(windows))]
    let (dir, fname) = match rustdesk_user_config_dir() {
        Some(d) => (d, "RustDesk2.toml"),
        None => return Err("未找到 RustDesk 配置目录（HOME 不可读）".into()),
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(format!("创建远程协助配置目录失败：{e}"));
    }
    let path = dir.join(fname);
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let want_hdr = format!("rendezvous_server = '{}:21116'", RD_SERVER);
    let want = [
        format!("custom-rendezvous-server = '{}'", RD_SERVER),
        format!("relay-server = '{}'", RD_SERVER),
        format!("key = '{}'", RD_SERVER_KEY),
    ];
    // 幂等：三键 + 头部已就位则不动文件（也就不必杀进程）
    if old.lines().any(|l| l.trim() == want_hdr)
        && want.iter().all(|w| old.lines().any(|l| l.trim() == *w))
    {
        return Ok(());
    }
    // RustDesk 在跑会在退出时回写旧配置 → 先结束再写（三平台同理，kill_rustdesk_procs 已跨平台）
    kill_rustdesk_procs();
    // 逐行替换头部与旧键，[options] 在末尾则追加键，否则补一节
    let mut lines: Vec<String> = Vec::new();
    let mut hdr_done = false;
    for line in old.lines() {
        let t = line.trim();
        let kn = t.split('=').next().unwrap_or("").trim();
        if kn == "rendezvous_server" {
            lines.push(want_hdr.clone());
            hdr_done = true;
            continue;
        }
        if kn == "custom-rendezvous-server" || kn == "relay-server" || kn == "key" {
            continue;
        }
        lines.push(line.to_string());
    }
    if !hdr_done {
        lines.insert(0, want_hdr.clone());
    }
    let mut content = lines.join("\n");
    if !content.is_empty() {
        content.push('\n');
    }
    if old.contains("[options]") {
        for w in &want {
            content.push_str(w);
            content.push('\n');
        }
    } else {
        content.push_str("\n[options]\n");
        for w in &want {
            content.push_str(w);
            content.push('\n');
        }
    }
    if let Err(e) = std::fs::write(&path, &content) {
        return Err(format!("写入远程协助配置失败（{}）：{e}", path.display()));
    }
    // 写盘后**回读校验**：RustDesk 只在进程启动时读一次配置，写错/写丢等于静默失效
    // （历史病灶：配置路径写错无人读取，全靠 dll 编译期注入兜底）。
    let now = std::fs::read_to_string(&path).unwrap_or_default();
    let ok = now.lines().any(|l| l.trim() == want_hdr)
        && want.iter().all(|w| now.lines().any(|l| l.trim() == *w));
    if !ok {
        return Err(format!("远程协助配置写入后校验失败：{}", path.display()));
    }
    Ok(())
}

/// 2026-11-06 根治「Key 不匹配」补强（老8 授权「根治」）：三端登录后都无条件对齐一次远控服务器配置。
/// 此前只有 Windows 在登录同步里写配置，mac/linux **只在用户点「接受协助」时才写** →
/// 没点过的机器配置里根本没有 key，rustdesk 便回落官方公网 key，表现为「连不上、报 Key 不匹配」。
/// 本命令只做幂等写盘（一致时不动文件、不杀进程、不弹任何系统授权），可安全地在登录链路调用。
#[tauri::command]
async fn ensure_assist_server() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(ensure_rustdesk_server_config)
        .await
        .map_err(|e| format!("远程协助配置线程异常：{e}"))?
}

fn rustdesk_get_id(app: &tauri::AppHandle) -> Result<String, String> {
    let exe = rustdesk_path(app).ok_or("远程协助组件未安装")?;
    let out = Command::new(&exe)
        .arg("--get-id")
        .output()
        .map_err(|e| format!("启动 RustDesk 失败: {e}"))?;
    // rustdesk 会往 stdout 混入 "skip .\xxx.dll" 等杂行，取最后一个纯数字行作为 ID
    let id = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty() && l.trim().chars().all(|c| c.is_ascii_digit()))
        .last()
        .unwrap_or_default()
        .trim()
        .to_string();
    if !id.is_empty() {
        Ok(id)
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { "RustDesk 未返回有效 ID".into() } else { err })
    }
}

/// 首次使用远程协助的就绪保障（2026-10-15 老8 定 per-device 密码方案）：
/// 本机 RustDesk ID 与服务端期望不一致（新机器/重装/从未注册）时，
/// 用系统授权（Win:UAC / mac:管理员确认 / Linux:pkexec，一次性）执行：
///   便携未装 → 静默安装 → 按 RustDesk H1 哈希格式（01#+hex(sha256(pw+salt))）直接改写
///   %APPDATA%/RustDesk/config/RustDesk.toml 的 password 字段（rdgen 客户端无 --password CLI，
///   必须写盘；salt 复用既有值，写盘前先杀用户态实例避免退出回写覆盖）。
/// 密码由前端随机生成传入（Rust 侧不引入随机依赖），设置成功后前端连同 ID 一起上报服务端。
/// 返回 (id, password)：changed=false 表示本来就绪，password=None 无需上报。
/// 2026-10-16 根治卡死（P0）：本函数必须 async —— 同步命令在主线程执行，而这里
/// 有 4 处阻塞调用（kill taskkill、rustdesk_get_id 起进程、提权 powershell
/// `Start-Process -Verb RunAs -Wait` 等 UAC + 安装器 + 设密码，动辄数十秒甚至
/// 无限等用户点授权）。主线程被占死 → 窗口「未响应」、用户看到卡死也不知要授权；
/// 卡死又导致 setid 上报没跑完 → 服务端 pwd_set 永远 false → 每次接受/发起
/// 再弹一次提权链路，形成死循环（实测踩过）。
/// 2026-10-16 A+ 方案（老8 定「融合进邦信、一次性装好、好友间最少点击」）：
/// 安装期 installer.nsh（已管理员权限）经 `X8Assist.exe --password` 把每机随机密码写入
/// X8Assist 自己的配置（RustDesk 1.5 为加密存储，外部拼串不可靠），并把该明文落盘到此文件。
/// 运行时邦信只需读此文件即可拿到本机密码用于上报（零 UAC）。
/// 2026-10-16 实测修正：落点改到 `C:\Program Files\X8Assist\x8pwd.txt` —— 该目录由 POSTINSTALL
/// 的 robocopy 保证存在（普通用户对 Program Files 下文件默认可读）；改用 ProgramData 时
/// POSTINSTALL 的 CreateDirectory 在安装上下文未生效，导致密码文件缺失。
fn x8assist_pwd_file() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        std::path::PathBuf::from(r"C:\Program Files\X8Assist\x8pwd.txt")
    }
    #[cfg(not(windows))]
    {
        std::path::PathBuf::from("/etc/x8assist/x8pwd.txt")
    }
}

/// 2026-10-17 B 方案：记录「本机 X8Assist 密码已确认落盘」的标记文件。
/// 存于用户可写目录（%APPDATA%\X8Assist\x8pwd_applied.txt），内容为已应用的明文密码；
/// 与 x8pwd.txt 比对：一致=已就绪（跳过主窗链路），不一致=重装换密（重跑链路）。
/// 不放 Program Files（普通用户不可写），也不放 config 内（避免被 X8Assist 配置回写覆盖）。
#[cfg(windows)]
fn x8assist_applied_marker() -> Option<std::path::PathBuf> {
    x8assist_config_dir().and_then(|d| d.parent().map(|p| p.join("x8pwd_applied.txt")))
}
#[cfg(windows)]
fn read_applied_pwd() -> Option<String> {
    x8assist_applied_marker()
        .and_then(|p| std::fs::read_to_string(&p).ok())
        .map(|s| s.trim().to_string())
}
#[cfg(windows)]
fn write_applied_pwd(pwd: &str) -> bool {
    match x8assist_applied_marker() {
        Some(p) => {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&p, pwd).is_ok()
        }
        None => false,
    }
}

/// 控制端对端偏好（view_style/collapse_toolbar）写入目录：
/// Windows = 品牌客户端 X8Assist 的配置目录；mac/linux = 官方 RustDesk 的配置目录。
/// 2026-10-16 跨平台化：官方 RustDesk 同样读 `peers/<id>.toml`，控制端也能预置顺手值。
fn assist_peers_dir() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        x8assist_config_dir().map(|d| d.join("peers"))
    }
    #[cfg(not(windows))]
    {
        rustdesk_user_config_dir().map(|d| d.join("peers"))
    }
}

/// 2026-10-16 老8 诉求（远程时"只点一下"、鼠标要准）：控制端连接前，把本机保存的
/// 该对端偏好调成顺手默认 ——
///   view_style = 'adaptive'   画面整幅缩放进窗口（出厂 'original' 会超出窗口，得滚动才点得准）
///   collapse_toolbar = 'Y'    收起远程工具栏（实测只剩监视器小按钮），不挡视野
/// 实测（X8Assist 1.5.0+68）：完整 peer toml 改这两键有效、连接过程不会覆盖；
/// 字段残缺的 toml 会被 RustDesk 解析失败回退出厂值并整份重写 —— 故此处
/// ①已有完整文件只就地改键；②文件不存在时写完整默认模板（字段齐全）。
/// 任何异常静默返回 false，绝不阻断连接。
fn ensure_peer_prefs(remote_id: &str) -> bool {
    let id = remote_id.trim();
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let Some(dir) = assist_peers_dir() else {
        return false;
    };
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!("{id}.toml"));
    let mut text = std::fs::read_to_string(&file).unwrap_or_default();
    let fresh = text.trim().is_empty();
    if fresh {
        text = PEER_TOML_TEMPLATE.to_string();
    }
    let before = text.clone();
    text = set_toml_top_key(&text, "view_style", "adaptive");
    text = set_toml_option_key(&text, "collapse_toolbar", "Y");
    if fresh || text != before {
        return std::fs::write(&file, text).is_ok();
    }
    true
}

/// 品牌客户端（X8Assist）用户级配置目录。
/// 2026-10-16 实测（1.5.0+68）：rdgen 品牌化后 app-name = X8Assist，运行时读写的配置
/// 就是 `%APPDATA%\X8Assist\config`（与 `rustdesk_user_config_dir()` 指的
/// `%APPDATA%\RustDesk\config` **不是同一处**，后者只对官方 RustDesk 有意义）。
#[cfg(windows)]
fn x8assist_config_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("APPDATA")
        .map(|v| std::path::PathBuf::from(v).join("X8Assist").join("config"))
}

/// 2026-10-16 老8 诉求：会话窗口那块「权限」面板不想要，只要「点控制 / 点接受」。
/// 该面板属于 RustDesk 的**连接管理器窗口**（`--cm`，Sciter 版 `src/ui/cm.tis`：
/// 左头像/ID + 中间权限图标区 [Permissions] + 右 Accept/Disconnect），不是工具栏，
/// 收起工具栏管不到它。它的显示开关就是官方 `hide_cm`：
///   `src/ui.rs:116` `--cm` → `ui.rs:121` `HIDE_CM = ipc::get_config("hide_cm")`
///   → `src/ipc.rs:972` 仅当 `is_pro() || is_custom_client()` 才求值（我们是品牌客户端，true）
///   → `hbb_common/src/password_security.rs:88` = `approve-mode=='password'`
///     && `verification-method=='use-permanent-password'` && `allow-hide-cm`
///   → `ui.rs:186` 命中则以 `frame.collapse(true)` 创建隐藏窗口（整窗不出现）。
/// 本机实测（1.5.0+68 自连）：写这三项 → 该窗口 visible=0；不写 → visible=1。确认有效。
/// 三个都是普通 `[options]` 项，写在品牌配置目录，无需重编 RustDesk。
/// 代价：RustDesk 侧不再弹「点击接受」——IM 侧「接受」不受影响（A+ 走固定密码直连），
/// 但**未就绪机器会失去点击回落**，故仅在密码已就绪（x8pwd.txt 有值）时才写。
/// mac/linux：邦信用的是官方 RustDesk（`is_custom_client()==false`）→ hide_cm 拿不到值，
/// 写了也没用，故非 Windows 直接返回 false，不动官方配置。
#[cfg(windows)]
fn ensure_hide_cm() -> bool {
    let Some(dir) = x8assist_config_dir() else {
        return false;
    };
    // 密码没就绪就不动这条链路（保留 RustDesk 侧「点击接受」兜底）
    if std::fs::read_to_string(x8assist_pwd_file())
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        return false;
    }
    let file = dir.join("X8Assist2.toml");
    let old = match std::fs::read_to_string(&file) {
        Ok(t) if !t.trim().is_empty() => t,
        // 客户端还没跑过（无配置）→ 不造文件，等它自己生成后再写
        _ => return false,
    };
    let mut text = old.clone();
    text = set_toml_option_key(&text, "approve-mode", "password");
    text = set_toml_option_key(&text, "verification-method", "use-permanent-password");
    text = set_toml_option_key(&text, "allow-hide-cm", "Y");
    if text == old {
        return true;
    }
    // RustDesk 退出时会回写内存里的旧配置 → 写盘前先结束在跑的实例（与服务器配置同套路）
    kill_rustdesk_procs();
    std::fs::write(&file, text).is_ok()
}

/// mac/linux（官方 RustDesk）对应 ensure_hide_cm 的核心：把审批模式切成「永久密码」。
/// 2026-10-16 老8「与 Windows 操作类似、不需要多点」：
/// 官方客户端 `is_custom_client()==false` → ipc.rs:972 对 hide_cm 恒不求值 → 藏不住
/// 连接管理器窗口；但 approve-mode='password' 时 cm.tis:46
/// `show_accept_btn = approve-mode != 'password'` —— 窗口**没有 Accept 按钮**，
/// 密码正确即 authorized（connection.rs:3044-3052）且 3 秒后自动最小化（cm.tis:432-436），
/// 全程无需多点一下。不写 allow-hide-cm（官方 IPC 恒返回 None，写了也不求值）。
/// ⚠️ 时机：必须在密码设置成功之后调用 —— 否则密码没设成时被控端只剩一个没有按钮的面板，死局。
#[cfg(not(windows))]
fn ensure_approve_mode() -> bool {
    let Some(dir) = rustdesk_user_config_dir() else {
        return false;
    };
    let file = dir.join("RustDesk2.toml");
    let old = match std::fs::read_to_string(&file) {
        Ok(t) if !t.trim().is_empty() => t,
        // 客户端还没跑过（无配置）→ 不造文件，等它自己生成后再写
        _ => return false,
    };
    let mut text = old.clone();
    text = set_toml_option_key(&text, "approve-mode", "password");
    text = set_toml_option_key(&text, "verification-method", "use-permanent-password");
    if text == old {
        return true;
    }
    // RustDesk 退出时会回写内存里的旧配置 → 写盘前先结束在跑的实例（与服务器配置同套路）
    kill_rustdesk_procs();
    std::fs::write(&file, text).is_ok()
}

/// 连接前幂等确保审批模式就绪（Windows=hide_cm 三键；mac/linux=approve-mode 两键）。
fn ensure_approve_config() {
    #[cfg(windows)]
    let _ = ensure_hide_cm();
    #[cfg(not(windows))]
    let _ = ensure_approve_mode();
}

/// mac/linux：官方 RustDesk 主进程是否在跑（pgrep -x 精确匹配进程名，不会误伤）。
#[cfg(not(windows))]
fn unix_rustdesk_running() -> bool {
    Command::new("pgrep")
        .args(["-x", "rustdesk"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// mac/linux：官方 RustDesk 主 IPC 管道就绪探活。
/// ⚠️ 2026-10-17 真机纠正（mac 报「RustDesk 启动超时」的根因）：1.5.0 起管道路径带 uid 后缀 ——
/// `hbb_common/src/config.rs:139-146` 的 `ipc_parent_dir_for_uid()` 对用户实例返回
/// `/tmp/{APP_NAME}-{uid}`，即 `/tmp/RustDesk-<uid>/ipc`（mac uid 通常 501、linux 1000）；
/// 只有 1.4.x 及更早才是 `/tmp/RustDesk/ipc`。
///
/// ⚠️⚠️ 2026-10-18 甘真机第二跳（`Connection refused (os error 61)` 的根因）：
/// 原先只判 `ipc` **文件存在** 就算就绪。可进程被 `pkill -x rustdesk` 杀掉后，
/// Unix domain socket 文件会残留（stale），文件还在、没人在 listen ——
/// 于是探活立刻为 true → root CLI 去连那个死 socket → ECONNREFUSED →
/// 前端只看到「密码写入失败：Connection refused」。整个链路一次都没真连过。
/// 现在改为**真连一次**：connect 成功才算就绪；stale socket 返回 ECONNREFUSED 被正确判否。
#[cfg(not(windows))]
fn unix_ipc_ready() -> bool {
    use std::os::unix::net::UnixStream;
    if UnixStream::connect("/tmp/RustDesk/ipc").is_ok() {
        return true;
    }
    if let Ok(rd) = std::fs::read_dir("/tmp") {
        for e in rd.flatten() {
            let name = e.file_name();
            let n = name.to_string_lossy();
            // `RustDesk-<uid>` 是用户实例；`RustDesk-service` 是服务实例，同样视为就绪
            if n.starts_with("RustDesk-") {
                let ipc = e.path().join("ipc");
                if ipc.exists() && UnixStream::connect(&ipc).is_ok() {
                    return true;
                }
            }
        }
    }
    false
}

/// mac/linux：清掉**连不上的** stale IPC socket（进程被 SIGTERM 杀掉后常留）。
/// 只删 connect 失败的 —— 活着的实例绝不动（防误杀在跑的服务）。
/// 不清的话，RustDesk 重启时可能因「已有实例」判定（check_pid）而放弃监听。
#[cfg(not(windows))]
fn unix_scrub_stale_ipc() {
    use std::os::unix::net::UnixStream;
    let Ok(rd) = std::fs::read_dir("/tmp") else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let n = name.to_string_lossy();
        if !n.starts_with("RustDesk-") {
            continue;
        }
        let dir = e.path();
        let ipc = dir.join("ipc");
        if !ipc.exists() || UnixStream::connect(&ipc).is_ok() {
            continue;
        }
        // 文件在、却连不上 = stale，删掉（无权限则忽略）
        let _ = std::fs::remove_file(&ipc);
    }
}

/// mac：读 .app bundle 的 Info.plist 某个键（plutil，系统自带）。
/// 用 plutil 而非文本 grep —— 官方包有的是二进制 plist，文本匹配会读不到。
#[cfg(target_os = "macos")]
fn macos_plist_value(app: &std::path::Path, key: &str) -> Option<String> {
    let plist = app.join("Contents/Info.plist");
    if !plist.is_file() {
        return None;
    }
    let out = Command::new("/usr/bin/plutil")
        .args(["-extract", key, "raw", "-o", "-"])
        .arg(&plist)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// mac：bundle 版本号。读不到 → None，调用方降级为「存在即用」，
/// 绝不因版本不可知就把用户环境搞坏。
#[cfg(target_os = "macos")]
fn macos_bundle_version(app: &std::path::Path) -> Option<String> {
    macos_plist_value(app, "CFBundleShortVersionString")
}

/// mac：bundle 内的可执行文件 —— **从 CFBundleExecutable 取，这是唯一事实源**。
///
/// ⚠️ 2026-10-18 甘真机 `codesign -dv` 实证：官方包内是
/// `Executable=/Applications/RustDesk.app/Contents/MacOS/RustDesk`（大写 R/D），
/// 而代码里一律写小写 `rustdesk`。默认 APFS 大小写不敏感能跑通，
/// 但用户若把盘格成 case-sensitive，`is_file()` 直接 false → 误判「组件缺失 / 落位失败」。
/// 读不到 plist 时回落两个候选名（小写优先，兼容旧目录结构）。
#[cfg(target_os = "macos")]
fn macos_bundle_exe(app: &std::path::Path) -> Option<std::path::PathBuf> {
    let macos_dir = app.join("Contents/MacOS");
    if let Some(name) = macos_plist_value(app, "CFBundleExecutable") {
        let p = macos_dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    ["rustdesk", "RustDesk"]
        .iter()
        .map(|n| macos_dir.join(n))
        .find(|p| p.is_file())
}

/// mac：已落位到 /Applications 的那份（与 `--password` 同源）。
/// 只做纯路径检查，不提权、不弹框 —— 供 open_rustdesk 这类高频入口用。
#[cfg(target_os = "macos")]
fn macos_installed_exe() -> Option<std::path::PathBuf> {
    macos_bundle_exe(std::path::Path::new("/Applications/RustDesk.app"))
}

/// mac 按需下载组件的缓存目录（2026-11-11 方案 A）。
///
/// 刻意不用 app_data_dir / local_data_dir：那两个目录（identifier com.x8fde.im）
/// 在启动时会被清理（见 main() setup 注释），缓存会被反复删掉重下。
/// 用独立 `~/Library/Application Support/BangXin/assist`，与 Windows 的
/// `%ProgramData%\BangXin\assist` 对称，卸载/升级都不受影响。
#[cfg(target_os = "macos")]
fn macos_assist_cache_dir() -> Option<std::path::PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| std::path::Path::new(&h).join("Library/Application Support/BangXin/assist"))
}

/// mac 缓存里的可执行文件（`.../assist/bin/mac/RustDesk.app/Contents/MacOS/<CFBundleExecutable>`）。
#[cfg(target_os = "macos")]
fn macos_cached_exe() -> Option<std::path::PathBuf> {
    let dir = macos_assist_cache_dir()?.join("bin").join("mac");
    macos_bundle_exe(&dir.join("RustDesk.app"))
}

// 🔴 2026-11-16 Apple 吊销整改 B：已删除 macos_clear_bundle_quarantine() 与 RD_REPAIR_SH。
// 理由：两者都会执行 `xattr -dr com.apple.quarantine`，其中 RD_REPAIR_SH 还是经
// 「with administrator privileges」提权后执行 —— 在 Apple 公证扫描器眼里等同于
// Gatekeeper bypass + 提权改系统目录，是本次账号被吊销的直接触发项之一。
// 我们自己的包经 Developer ID 签名 + 公证后不需要剥隔离标记；直接 exec 也不经过
// LaunchServices 的隔离检查，删除后不影响远程协助主链路。

/// mac：检查某 RustDesk.app 的框架符号链接是否完好（纯只读、不提权）。
///
/// 🩸 2026-10-18 甘真机：Tauri 把 RustDesk.app 作为 resource 打包时会解引用符号链接，
/// 导致 19/19 框架缺失 `Versions/Current -> A`、根级二进制变实体，Flutter 启动即崩。
/// 本函数用于在真正修复前判断「是否需要动用户的包」，避免健康安装每次启动都弹授权框。
#[cfg(target_os = "macos")]
fn macos_bundle_symlinks_ok(bundle: &std::path::Path) -> bool {
    let fw = bundle.join("Contents/Frameworks");
    let Ok(entries) = std::fs::read_dir(&fw) else {
        return false;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("framework") {
            continue;
        }
        let a = p.join("Versions/A");
        let cur = p.join("Versions/Current");
        if !a.is_dir() {
            continue;
        }
        // 框架内必须有 Versions/Current -> A 的符号链接
        if !cur.is_symlink() {
            return false;
        }
        // 根级条目（除 Versions 外）应指向 Versions/Current 下同名项的符号链接
        if let Ok(inner) = std::fs::read_dir(&a) {
            for ie in inner.flatten() {
                let name = ie.file_name();
                if name == "Versions" {
                    continue;
                }
                if !p.join(&name).is_symlink() {
                    return false;
                }
            }
        }
    }
    true
}

// 🔴 2026-11-16 Apple 吊销整改 C：已删除 macos_repair_bundle_symlinks()。
// 它是 RD_REPAIR_SH 的唯一调用方（写 /tmp 脚本 → osascript 提权 bash 执行 →
// 重签 /Applications 下的第三方 app），属同一处提权改造，一并移除。

/// mac：定位一个「既可用于启动、也可用于 `--password`」的 RustDesk bundle。
///
/// 🔴 2026-11-16 Apple 吊销整改 C（重大变更）：**不再自行安装/搬移任何第三方 app**。
/// 旧实现在 /Applications 缺失或结构损坏时，会用
/// `do shell script "… ditto … mv … xattr -dr com.apple.quarantine …" with administrator privileges`
/// 索要管理员密码、替换 /Applications 下的应用、剥掉 Gatekeeper 隔离属性 ——
/// 在 Apple 公证扫描器眼里等同 Gatekeeper bypass + 提权改系统目录，是本次账号被吊销的
/// 直接触发项之一，已整体删除（同批删除的还有 RD_REPAIR_SH 与 macos_repair_bundle_symlinks）。
///
/// 现策略（只读、无提权）：
///   1) 用户在 /Applications 里自己装过官方 RustDesk → 直接复用（TCC 授权跨版本保留）；
///      结构损坏时**不修**，只报可读原因让用户重装 —— 修它就得 ad-hoc 重签 + 改系统目录。
///   2) 否则用下载到用户目录缓存（`~/Library/Application Support/BangXin/assist`）那份：
///      只启动，不落位、不提权。注意该位置不满足 RustDesk 的 `is_installed()` 判据，
///      `--password` 会被 CLI 静默拒绝（exit 0），调用方需按「免密不可用」降级。
///   3) 都没有 → 报错，由前端引导用户从官网安装。
///
/// 返回 (canonical exe, 是否发生了替换)。第二个值恒为 false，保留仅为调用方签名兼容。
#[cfg(target_os = "macos")]
fn macos_canonical_exe(raw: &std::path::Path) -> Result<(std::path::PathBuf, bool), String> {
    let inst_dir = std::path::Path::new("/Applications/RustDesk.app");
    if let Some(exe) = macos_installed_exe() {
        if macos_bundle_symlinks_ok(inst_dir) {
            return Ok((exe, false));
        }
        return Err(
            "已安装的 RustDesk 结构不完整，请在「应用程序」里删除它后从官网重新安装".into(),
        );
    }
    if let Some(exe) = macos_cached_exe() {
        return Ok((exe, false));
    }
    if raw.is_file() {
        return Ok((raw.to_path_buf(), false));
    }
    Err("远程协助组件未就绪，请重新下载或升级邦信客户端".into())
}

/// mac/linux：提权把永久密码写进官方 RustDesk。Err 里带**真实原因**（stdout/stderr）。
///
/// 为什么必须这么绕：`--password` 全平台走 IPC，且 CLI 侧门槛是
/// `is_installed() && is_root()`（core_main.rs:446-453）——
///   mac：current_exe 须在 `/Applications/RustDesk.app` 内（platform/macos.rs:869-877）
///   linux：current_exe 路径须以 `/usr` 或 `/nix/store` 开头（platform/linux.rs:1545-1552）
/// 不满足时 CLI 只打印 "Installation and administrative privileges required!" 后**退出码 0**
/// 静默退出（调用方无感）—— 原实现直接跑 Tauri 资源里的便携版，密码从未落盘。
/// 成功时 CLI 打印 "Done!"（core_main.rs:450）。
///
/// ⚠️ 2026-10-17（老8 定 A 方案）：返回类型从 `bool` 改成 `Result<(), String>`，
/// 把所有失败原因**分档上抛**。旧实现三种完全不同的故障（用户取消 / 位置校验不过 /
/// IPC 无 ACK）被压成同一句「未获得系统授权」，甘真机因此卡了两轮、我们都看不出问题在哪。
/// 🔴 2026-11-16 Apple 吊销整改：本函数现在**只用于 Linux**。mac 分支（osascript 提权写密码）
/// 已整段删除 —— 它是本次账号被吊销的触发项之一，且 mac 组件不再落位 /Applications 后
/// 该路径本就不满足 RustDesk 的 `is_installed()` 判据。mac 上密码改为用户手动输入。
#[cfg(all(unix, not(target_os = "macos")))]
fn unix_set_permanent_password(exe: &std::path::Path, pw: &str) -> Result<(), String> {
    // 兜底校验：pw 会被拼进 osascript/pkexec 的 shell 字符串（无引号包裹转义），
    // 只放行前端生成的 [A-Za-z0-9] 随机密码（与服务端 setid 校验同口径），
    // 任何可疑字符直接判失败，杜绝经命令注入把任意命令以 root 跑起来。
    if pw.len() < 8 || !pw.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("密码格式不合法".into());
    }
    // 🔴 2026-11-16 Apple 吊销整改：原 mac 分支（osascript 提权执行 `rustdesk --password`）
    // 已整段删除，本函数现在只有 Linux 分支。mac 侧不再索要管理员权限、不再写永久密码。
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let p = exe.to_string_lossy().to_string();
        let installed_ok = p.starts_with("/usr/") || p.starts_with("/nix/store/");
        let script = if installed_ok {
            format!("'{}' --password {}", p, pw)
        } else {
            // PATH 里的 rustdesk 不在 is_installed 认可位置（AppImage / ~/bin 等）→ 先落到 /usr/bin
            format!(
                "install -m 755 '{}' /usr/bin/rustdesk && /usr/bin/rustdesk --password {}",
                p, pw
            )
        };
        let out = Command::new("pkexec")
            .args(["sh", "-c", &script])
            .output()
            .map_err(|e| format!("提权执行失败: {e}"))?;
        let so = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let se = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !out.status.success() {
            if se.contains("Not authorized") || se.contains("dismissed") || se.contains("cancelled")
            {
                return Err("你取消了系统授权，远程协助密码未写入".into());
            }
            return Err(format!(
                "提权写密码失败：{}",
                if se.is_empty() { so.as_str() } else { se.as_str() }
            ));
        }
        if so.contains("Done!") {
            return Ok(());
        }
        if so.contains("Installation and administrative privileges") {
            return Err("RustDesk 未通过安装位置校验（须位于 /usr 或 /nix/store）".into());
        }
        Err(format!(
            "远程协助密码写入失败：{}",
            if so.is_empty() { "无响应（RustDesk 未在运行或版本不匹配）" } else { so.as_str() }
        ))
    }
}

/// mac/linux：rustdesk 是否存在活跃远程会话（ESTABLISHED 且远端端口不是 hbbs/hbbr 的
/// 21116/21117）。mac 用 lsof、linux 用 ss；两者都取不到时返回 true（宁可不回收，
/// 避免误杀进行中的会话 —— 与 Windows assist_has_live_session 的失败方向一致）。
#[cfg(not(windows))]
fn unix_has_live_session() -> bool {
    #[cfg(target_os = "macos")]
    {
        // lsof 行：rustdesk 1234 user 18u IPv4 ... TCP 192.168.1.5:54980->1.2.3.4:21116 (ESTABLISHED)
        if let Ok(out) = Command::new("lsof")
            .args(["-nP", "-iTCP", "-sTCP:ESTABLISHED"])
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                if !line.contains("rustdesk") {
                    continue;
                }
                if let Some(remote) = line.rsplit("->").next() {
                    let host_port = remote.trim().split_whitespace().next().unwrap_or("");
                    if let Some(port) = host_port.rsplit(':').next() {
                        if let Ok(p) = port.trim().parse::<u16>() {
                            if p != 21116 && p != 21117 {
                                return true;
                            }
                        }
                    }
                }
            }
            return false;
        }
        true
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // ss -tnp 行：ESTAB 0 0 192.168.1.5:54980 1.2.3.4:21116 users:(("rustdesk",pid=123,fd=7))
        // 列：State Recv-Q Send-Q Local-Address:Port Peer-Address:Port Process
        if let Ok(out) = Command::new("ss").args(["-tnp"]).output() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                if !line.contains("rustdesk") || !line.contains("ESTAB") {
                    continue;
                }
                let cols: Vec<&str> = line.split_whitespace().collect();
                if let Some(peer) = cols.get(4) {
                    if let Some(port) = peer.rsplit(':').next() {
                        if let Ok(p) = port.trim().parse::<u16>() {
                            if p != 21116 && p != 21117 {
                                return true;
                            }
                        }
                    }
                }
            }
            return false;
        }
        true
    }
}

/// 顶层键：`key = ...` 行就地改写；缺失则插到文件最前（TOML 顶层键必须在首个 table 之前）。
fn set_toml_top_key(text: &str, key: &str, val: &str) -> String {
    let line = format!("{key} = '{val}'");
    let prefix = format!("{key} ");
    let mut out: Vec<String> = Vec::new();
    let mut hit = false;
    for l in text.lines() {
        if !hit && l.trim_start().starts_with(&prefix) {
            out.push(line.clone());
            hit = true;
        } else {
            out.push(l.to_string());
        }
    }
    if !hit {
        out.insert(0, line);
    }
    out.join("\n") + "\n"
}

/// `[options]` 段内键：就地改写；缺失则插到 `[options]` 行后；整段缺失则补一段。
fn set_toml_option_key(text: &str, key: &str, val: &str) -> String {
    let line = format!("{key} = '{val}'");
    let prefix = format!("{key} ");
    let mut out: Vec<String> = Vec::new();
    let mut hit = false;
    let mut in_opts = false;
    let mut opts_at: Option<usize> = None;
    for l in text.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            in_opts = t == "[options]";
            if in_opts {
                opts_at = Some(out.len());
            }
        }
        if !hit && in_opts && t.starts_with(&prefix) {
            out.push(line.clone());
            hit = true;
        } else {
            out.push(l.to_string());
        }
    }
    if !hit {
        match opts_at {
            Some(i) => out.insert(i + 1, line),
            None => {
                out.push("[options]".into());
                out.push(line);
            }
        }
    }
    out.join("\n") + "\n"
}

/// 新对端用的完整默认 peer toml（照抄实测 RustDesk 1.5.0+68 连接后落盘的结构，
/// 只把 view_style / collapse_toolbar 预置成顺手值；[info]/[ui_flutter]/[transfer]
/// 由客户端连接后自行补写）。
const PEER_TOML_TEMPLATE: &str = r#"password = []
size = [
    0,
    0,
    0,
    0,
]
size_ft = [
    0,
    0,
    0,
    0,
]
size_pf = [
    0,
    0,
    0,
    0,
]
view_style = 'adaptive'
scroll_style = 'scrollauto'
edge_scroll_edge_thickness = 100
image_quality = 'balanced'
custom_image_quality = [50]
show_remote_cursor = false
lock_after_session_end = false
terminal-persistent = false
privacy_mode = false
allow_swap_key = false
port_forwards = []
direct_failures = 0
disable_audio = false
disable_clipboard = false
enable-file-copy-paste = true
show_quality_monitor = false
follow_remote_cursor = false
follow_remote_window = false
keyboard_mode = 'map'
view_only = false
show_my_cursor = false
sync-init-clipboard = false
trackpad-speed = 100

[options]
collapse_toolbar = 'Y'
swap-left-right-mouse = ''
remote-menubar-frac = '0.5'
remote-menubar-edge = 'top'
zoom-cursor = ''
i444 = ''
codec-preference = 'auto'
custom-fps = '30'

[ui_flutter]
wm_RemoteDesktop = '{"width":1300.0,"height":740.0,"offsetWidth":0.0,"offsetHeight":0.0,"isMaximized":false,"isFullscreen":false}'

[info]
username = ''
hostname = ''
platform = 'Windows'

[transfer]
write_jobs = []
read_jobs = []
"#;

/// 2026-10-24 P1-RS-1：函数体内全是阻塞等待（rustdesk --get-id/--password 的 Command::output、
/// kill_rustdesk_procs、以及轮询 sleep：Windows 兜底链 500ms×24、mac/linux 就绪链最长 20s），
/// 直接跑在 async 命令里会占死 tokio worker（拖累 IPC 等其它任务）。照 download_file 范式：
/// 整段逻辑移入 spawn_blocking（阻塞线程池），async 壳只做转发；各平台分支代码原样不动。
#[tauri::command]
async fn ensure_rustdesk(app: tauri::AppHandle, expected_rid: String, password: String) -> Result<EnsureOut, String> {
    tauri::async_runtime::spawn_blocking(move || ensure_rustdesk_blocking(app, expected_rid, password))
        .await
        .map_err(|e| format!("远程协助就绪线程异常：{e}"))?
}

fn ensure_rustdesk_blocking(app: tauri::AppHandle, expected_rid: String, password: String) -> Result<EnsureOut, String> {
    // 2026-10-15 老8 定：Windows 只认品牌化 X8Assist，便携 rustdesk.exe 不再参与安装链路
    #[cfg(not(windows))]
    let exe = rustdesk_path(&app).ok_or("远程协助组件未安装")?;
    // 先幂等写自建服务器配置（可能结束在跑的 RustDesk），再取 ID（新起进程即读新配置）
    // 🔴 2026-11-06：失败必须上抛 —— 配置写不进去就别取 ID，否则拿到的是官方节点的身份，
    // 后续连接一律「Key 不匹配」，且用户完全看不出根因。
    ensure_rustdesk_server_config().map_err(|e| format!("远程协助服务器配置未就绪：{e}"))?;
    // 2026-10-16 A+：前端统一传 expectedRid=''，不再在此早退。平台分支各自读取/设置本机密码
    // （Windows 读 %PROGRAMDATA%\X8Assist\x8pwd.txt；非 Windows 用前端生成的随机密码经 --password 提权设置），
    // 以便被控端也能把 per-device 密码上报服务端，控制端 getconn 才能免确认直连。
    // expected_rid / password 均可能在本平台分支用不到（Windows 两者都不直接用），
    // 显式消引用避免 unused 警告，同时保留命令签名兼容前端传参。
    let _ = &expected_rid;
    #[cfg(not(windows))]
    let pw = password.as_str();
    #[cfg(windows)]
    let _ = &password;
        #[cfg(windows)]
        {
            // 2026-10-16 A+ 方案（老8 定「融合进邦信、一次性装好、好友间最少点击」）：
            // 密码已由安装期 installer.nsh（管理员权限）经 `X8Assist.exe --password` 写入，
            // 明文落盘到 C:\Program Files\X8Assist\x8pwd.txt（每机随机、运行时零 UAC）。
            // 这里只读取该文件并连同 ID 返回前端上报，不再手写 toml —— RustDesk 1.5 改为
            // 加密存储（external crate `permanent_password`），外部拼串不可靠；且 rdgen 配置
            // 目录是 %APPDATA%\X8Assist\config（非 RustDesk）。始终返回 x8pwd.txt 中的密码
            // （若文件存在），由前端幂等上报，避免「ID 一致就跳过」导致服务端永远缺密码。
            if installed_assist_path().is_none() {
                return Err("远程协助组件未安装，请重新安装或升级邦信客户端".into());
            }
            let cur = match rustdesk_get_id(&app) {
                Ok(id) => id,
                Err(e) => {
                    // 2026-10-24 P2-RS-9：取 ID 失败不再静默吞掉，记日志后仍按空 id 上报（前端回落）。
                    eprintln!("[BangXin] 取 X8Assist ID 失败（按空 id 上报）: {e}");
                    String::new()
                }
            };
            let pwd_file = x8assist_pwd_file();
            let pwd = std::fs::read_to_string(&pwd_file)
                .unwrap_or_default()
                .trim()
                .to_string();
            if pwd.is_empty() {
                // 兜底：无密码文件（老机器未升级安装包），前端回落「点击接受」路径
                return Ok(EnsureOut { id: cur, password: None, changed: false, multi: win_has_rdp_session() });
            }
            // 2026-10-17 B 方案（老8 定）：平时（刷新/登录静默上报）不再拉起 X8Assist 主窗。
            // 仅当密码尚未确认落盘到 X8Assist 配置时才走「杀实例→起主窗→IPC 写密码→清实例」
            // 的兜底链路（根治 2026-10-16 根因 A/B）；落盘成功后写本地标记，后续调用直接
            // 读文件返回，零主窗闪动。标记存于用户可写目录（%APPDATA%\X8Assist），内容为
            // 已应用的明文密码；与 x8pwd.txt 不一致（重装换密）即视为未就绪，重跑链路。
            if let Some(applied) = read_applied_pwd() {
                if applied == pwd {
                    return Ok(EnsureOut { id: cur, password: Some(pwd), changed: false, multi: win_has_rdp_session() });
                }
            }
            // ── 以下为「密码尚未确认落盘」兜底链路（与旧逻辑同构，零 UAC、幂等）──
            // 2026-10-16 老8 诉求「不要再手工输密码」兜底（两轮本机实证定案）：
            // ① 根因 A：installer.nsh 提权上下文 %APPDATA% 错位（安装日志 LOCALAPPDATA=C:\ProgramData），
            //    `--password` 写进了提权账号配置，登录用户 X8Assist 主界面回落「一次性密码」→ 永不匹配。
            // ② 根因 B（21:16 落盘 diff 实证）：`--password` **必须经 IPC**（--server 在跑）才写配置；
            //    无实例时直接跑 exit 0 但 X8Assist.toml 纹丝不动（静默失败）。
            // 故这里以登录用户身份完整复刻写入链路：杀实例 → 起主窗 → 轮询 IPC 就绪
            // （--get-id 成功即管道就绪，不死睡）→ --password 写入 → 清理临时实例。
            let _ = ensure_hide_cm();
            kill_rustdesk_procs();
            let mut applied_ok = false;
            if let Some(exe) = installed_assist_path() {
                // 裸启动（主进程模式）而非 --server：实测 --server 会自我提升成管理员实例
                //（普通权限杀不掉、残留），而裸启动 IPC 同样能处理 --password（本机实测 Done!）。
                let child = Command::new(&exe)
                    .creation_flags(0x0800_0000)
                    .spawn()
                    .ok();
                if let Some(ch) = child {
                    // 2026-10-26 根治闪窗（老8 实测仍会黑屏闪一下）：原来只做 SW_HIDE，
                    // 输在时序 —— flutter runner 的 Show() 是硬编码 SW_SHOWNORMAL，
                    // 我们的隐藏线程每 50ms 才扫一次，中间这帧照漏。
                    // 现在从 spawn 起就按 child.id 每 10ms 把窗口挪到屏幕外 + 隐藏，
                    // 位置移走是持久的（Show 不会改回来），抢不到那一帧也看不见。
                    let pid = ch.id();
                    std::thread::spawn(move || {
                        use std::collections::HashSet;
                        let mut set: HashSet<u32> = HashSet::new();
                        set.insert(pid);
                        for i in 0..260 {
                            // 首轮补一次 PID 全集（刚 kill 过，此时只会有我们刚起的实例）
                            if i == 0 {
                                for s in assist_pids() {
                                    if let Ok(p) = s.parse::<u32>() {
                                        set.insert(p);
                                    }
                                }
                            }
                            park_pid_windows(&set);
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                    });
                    // 轮询 IPC 就绪（最多 12s）：--get-id 走同一管道，能返回即就绪。
                    // 命中即记 ipc_ready —— 实例活着且我们已下发 --password，可判定密码已落盘。
                    let mut ipc_ready = false;
                    for _ in 0..24 {
                        if rustdesk_get_id(&app).is_ok() {
                            ipc_ready = true;
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                    // 经 IPC 写入密码；stdout 含 "Done!" 才表示真正落盘（无实例时静默 exit 0 为假成功）
                    let out = Command::new(&exe)
                        .arg("--password")
                        .arg(&pwd)
                        .creation_flags(0x0800_0000)
                        .output();
                    let done = out
                        .map(|o| String::from_utf8_lossy(&o.stdout).contains("Done!"))
                        .unwrap_or(false);
                    // 2026-10-17 修复（老8 实测踩坑）：旧逻辑只认 stdout "Done!"，本机构建该串取不到
                    // → applied_ok 恒 false → 标记永远写不下 → 每次刷新都重跑兜底链闪窗。
                    // 改三重判据任一命中即视为已落盘：Done! / IPC 已就绪。只要实例活着且
                    // --password 已下发，密码即已写入（安装期本就设好，幂等重设无害）。
                    applied_ok = done || ipc_ready;
                    // 给 IPC 写盘留一点收尾时间，再清掉临时实例（open_rustdesk 之后按需再拉）
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    kill_rustdesk_procs();
                }
            }
            // 仅当密码确认落盘才记标记：避免「假成功」导致后续永久跳过兜底链路。
            // 标记一旦写入，后续刷新直接读文件返回，零主窗闪动（最多只首次跑一次）。
            if applied_ok {
                let _ = write_applied_pwd(&pwd);
            }
            return Ok(EnsureOut { id: cur, password: Some(pwd), changed: true, multi: win_has_rdp_session() });
        }
    // ---------- mac/linux（官方 RustDesk）----------
    // 2026-10-16 老8「与 Windows 操作类似、不需要多点」重写。原实现两个致命伤（均已实证）：
    // ① `--password` 门槛 `is_installed() && is_root()`（core_main.rs:446-453）：mac 要求 exe
    //    在 /Applications/RustDesk.app 内、linux 要求 /usr|/nix/store 内 —— Tauri 资源里的
    //    便携版/系统 PATH 里的其他路径全部不满足，CLI 只打印 "Installation and administrative
    //    privileges required!" 后静默退出（exit 0），密码从未落盘；
    // ② 没写 approve-mode，官方默认 'click' → 被控端必弹带 Accept 按钮的连接管理器（多点）。
    // 新链路与 Windows A+ 同构（零常驻、按需启动、一次性提权）：
    //   服务器配置已写（函数入口）→ 登录用户裸启动（IPC 管道探活）→ 提权落位 + --password
    //   （校验 stdout "Done!"）→ 杀临时实例 → 写 approve-mode=password（免 Accept）→ 上报。
    #[cfg(not(windows))]
    {
        // A 方案（2026-10-17 老8 定）：先把「跑的那份」和「设密码那份」统一成同一个 bundle。
        // 甘真机事故：机器上同时有邦信与一份独立 RustDesk（启动台两个图标），
        // 旧实现用资源包那份启动、却用 /Applications 那份执行 --password，两份版本/路径都不同，
        // IPC 拿不到 ACK 只会报一句「未获得系统授权」。统一后两者必然同源。
        #[cfg(target_os = "macos")]
        let (exe, replaced) = macos_canonical_exe(&exe)?;
        #[cfg(all(unix, not(target_os = "macos")))]
        let (exe, replaced) = (exe.clone(), false);
        #[cfg(all(unix, not(target_os = "macos")))]
        let _ = replaced;
        // 刚替换过 bundle → 在跑的可能是旧二进制，必须清掉重起（否则仍是旧版本实例）
        #[cfg(target_os = "macos")]
        if replaced {
            kill_rustdesk_procs();
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        // 只清连不上的 stale socket（活着的实例不动）：否则探活会被死 socket 骗过，
        // 或让新实例以为「已有实例」而放弃监听
        unix_scrub_stale_ipc();
        // 判据是「有没有可用的 IPC」，不是「有没有进程」：
        // 进程活着但 socket 不可用（正在退出 / 崩了 / stale）时也要重起，
        // 否则会一路空转到 20s 超时（2026-10-18 甘真机踩过）。
        if !unix_ipc_ready() {
            if unix_rustdesk_running() {
                kill_rustdesk_procs();
                // SIGTERM 到 socket 真正消失有间隙，等它退干净（最多 2s）
                for _ in 0..20 {
                    if !unix_rustdesk_running() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            unix_scrub_stale_ipc();
            Command::new(&exe)
                .spawn()
                .map_err(|e| format!("启动 RustDesk 失败: {e}"))?;
        }
        // 轮询 IPC 就绪（最多 20s）：管道文件出现即就绪，不死睡。
        // 2026-10-17 mac 真机：未签名 bundle 首启要过 syspolicyd 完整性扫描，12s 常踩在边界上，
        // 放宽到 20s；同时若进程已消失（Gatekeeper 拦截/架构不符/当场崩溃）就不必等满，
        // 立刻给可读原因 —— 旧实现一律报「启动超时」，用户和我们都看不出是被系统拦了。
        let mut ready = false;
        let mut died = false;
        for i in 0..40 {
            if unix_ipc_ready() {
                ready = true;
                break;
            }
            // 前 3s 容忍启动瞬态（进程尚未可见），之后仍无进程即判死
            if i >= 6 && !unix_rustdesk_running() {
                died = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        if !ready {
            kill_rustdesk_procs();
            return Err(if died {
                "RustDesk 未能运行（疑似被系统安全策略拦截）。macOS：到「系统设置 → 隐私与安全性」\
                 底部点「仍要打开」后再点一次；仍不行请重装邦信"
                    .into()
            } else {
                "RustDesk 启动超时，远程协助暂未就绪".into()
            });
        }
        // Err 里带真实原因（用户取消 / 位置校验不过 / IPC 无 ACK），不再压成一句万能文案
        // 🔴 2026-11-16 Apple 吊销整改：mac 不再自动写永久密码（原走 osascript 提权，
        // 是本次账号被吊销的触发项之一）；密码改为用户在 RustDesk 界面手动查看/输入。
        #[cfg(all(unix, not(target_os = "macos")))]
        unix_set_permanent_password(&exe, pw)?;
        #[cfg(target_os = "macos")]
        let _ = (&exe, pw);
        // 清理临时实例（open_rustdesk 之后按需再拉；与 Windows 就绪链路同套路）
        kill_rustdesk_procs();
        // 密码就绪后才切审批模式（免 Accept 按钮；官方无 hide_cm，面板 3s 自最小化）
        let _ = ensure_approve_mode();
        let id = rustdesk_get_id(&app)?;
        return Ok(EnsureOut { id, password: Some(pw.to_string()), changed: true, multi: false });
    }
}

// ============================================================================
// Linux 远程协助组件一键安装（2026-10-17 老8「都对齐下吧，让用户操作要更方便」）
// ============================================================================
/// Linux 版不内置 RustDesk（官方 deb/rpm 强依赖发行版专属 .so，抽单文件跑不起来），
/// 系统里没装时此前直接抛「内置 RustDesk 缺失」—— 功能完全不可用，用户也不知道该装什么。
/// 这里做成一键：判发行版/架构 → 从自建 CDN 拉官方包 → pkexec 提权安装 → 关掉官方自启 → 校验 PATH。
///
/// 返回 "already"（本来就能用）/ "installed"（本次装好）。
/// Windows/mac 恒为 "already"：Windows 内置品牌化 X8Assist、mac 内置 RustDesk.app，
/// 都随安装包装好，不需要系统级安装（保持与现有链路一致，不给用户多一个按钮）。
#[tauri::command]
async fn install_rustdesk() -> Result<String, String> {
    // 2026-10-24 P1-RS-5：linux_install_rustdesk 内含 curl/wget .output() + pkexec .output()
    // （用户授权可无限阻塞），直跑 async 体占死 tokio worker。移入 spawn_blocking。
    tauri::async_runtime::spawn_blocking(|| {
        #[cfg(target_os = "linux")]
        {
            linux_install_rustdesk()
        }
        #[cfg(windows)]
        {
            win_install_assist()
        }
        #[cfg(target_os = "macos")]
        {
            mac_install_rustdesk()
        }
    })
    .await
    .map_err(|e| format!("远程协助组件安装线程异常：{e}"))?
}

/// Windows：按需下载安装品牌化 X8Assist（2026-11-11 方案 A：组件不再打进安装包）。
///
/// 安装期 installer.nsh 已经尝试下载安装；本函数是**运行期兜底**（安装时断网/内网/
/// 网络抖动会导致组件缺失）。用户点「远程协助」时触发一次，会弹一次 UAC。
///
/// 链路：curl 下载 zip → elevate.exe 提权 PowerShell（解压到 Program Files → 生成
/// 每机随机密码落 x8pwd.txt → 起 `--server` 让 IPC 就绪 → `--password` 写入 → 收尾）。
///
/// PowerShell 用 `-EncodedCommand`（Base64/UTF-16LE）传：彻底规避中文用户名路径、
/// NTFS 空格与三层引号转义（历史上 `--password` 内联引号踩过静默空密码）。
///
/// 返回 "already"（本来就有）/ "installed"（本次装好）。
#[cfg(windows)]
fn win_install_assist() -> Result<String, String> {
    if installed_assist_path().is_some() {
        return Ok("already".into());
    }
    // 1) 取组件清单（版本化 URL + sha256），再下载组件包。
    //    清单放 ProgramData：提权后的 PowerShell 用 $env:ProgramData 也能定位到同一份；
    //    放用户 %TEMP% 会因提权后 $env:TEMP 变成管理员的 Temp 而找不到文件。
    let base = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
    let dir = std::path::Path::new(&base).join("BangXin").join("assist");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建组件下载目录失败：{e}"))?;
    let zip = dir.join("x8assist.zip");
    let man = dir.join("latest-assist.json");
    let _ = std::fs::remove_file(&zip);
    let _ = Command::new("curl.exe")
        .args(["-L", "--fail", "-s", "--max-time", "60", "-o"])
        .arg(&man)
        .arg("https://im.x8fde.com/app/latest-assist.json")
        .status();

    let mut url = String::new();
    let mut want_sha = String::new();
    if let Ok(txt) = std::fs::read_to_string(&man) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
            if let Some(p) = v.get("platforms").and_then(|p| p.get("windows-x86_64")) {
                url = p.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
                want_sha = p.get("sha256").and_then(|s| s.as_str()).unwrap_or("").to_string();
            }
        }
    }
    if url.is_empty() {
        return Err("远程协助组件清单不可用（请检查网络后重试）".into());
    }
    let dl = Command::new("curl.exe")
        .args(["-L", "--fail", "-s", "--max-time", "600", "-o"])
        .arg(&zip)
        .arg(&url)
        .status();
    if !matches!(dl, Ok(s) if s.success()) || !zip.is_file() {
        return Err("远程协助组件下载失败（请检查网络后重试，或到下载页手动安装组件）".into());
    }
    // sha256 校验：下载不完整的 zip 解压会得到残缺组件（更难排查），宁可这里失败重试。
    if !want_sha.is_empty() {
        use sha2::{Digest, Sha256};
        match std::fs::read(&zip) {
            Ok(bytes) => {
                let got = format!("{:x}", Sha256::digest(&bytes));
                if got != want_sha {
                    let _ = std::fs::remove_file(&zip);
                    return Err("组件包校验失败（下载不完整），请重试".into());
                }
            }
            Err(e) => return Err(format!("读取组件包失败：{e}")),
        }
    }

    // 2) 提权安装（全 ASCII 脚本）。$env:ProgramData 在提权上下文仍是 C:\ProgramData。
    let ps = r#"$ErrorActionPreference='Stop';
$zip = Join-Path $env:ProgramData 'BangXin\assist\x8assist.zip';
$dst = 'C:\Program Files\X8Assist';
if (Test-Path $dst) { Remove-Item $dst -Recurse -Force -ErrorAction SilentlyContinue };
Add-Type -AssemblyName System.IO.Compression.FileSystem;
[IO.Compression.ZipFile]::ExtractToDirectory($zip, 'C:\Program Files');
$chars = (48..57)+(65..90)+(97..122) | ForEach-Object { [char]$_ };
$p = -join (1..12 | ForEach-Object { $chars | Get-Random });
[IO.File]::WriteAllText((Join-Path $dst 'x8pwd.txt'), $p);
$exe = Join-Path $dst 'X8Assist.exe';
Start-Process -FilePath $exe -ArgumentList '--server' -WindowStyle Hidden;
Start-Sleep -Seconds 6;
& $exe --password $p | Out-Null;
Start-Sleep -Seconds 2;
Get-Process X8Assist -ErrorAction SilentlyContinue | Stop-Process -Force;
if (Test-Path $exe) { exit 0 } else { exit 9 }"#;
    let utf16: Vec<u8> = ps.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
    let enc = {
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine as _;
        B64.encode(&utf16)
    };
    let elevate = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.join("elevate.exe")))
        .unwrap_or_else(|| std::path::PathBuf::from("elevate.exe"));
    let st = Command::new(&elevate)
        .args([
            "powershell.exe",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
            &enc,
        ])
        .status()
        .map_err(|e| format!("提权安装组件失败：{e}"))?;
    let _ = std::fs::remove_file(&zip);
    if !st.success() {
        return Err("组件安装未完成（可能取消了系统授权）。可重试，或到下载页手动安装组件".into());
    }
    if installed_assist_path().is_none() {
        return Err("组件安装后仍未找到 X8Assist.exe，请到下载页手动安装组件".into());
    }
    Ok("installed".into())
}

/// mac：按需下载官方 RustDesk.app 到用户目录缓存（2026-11-11 方案 A）。
///
/// 与 Windows 共用一份清单（latest-assist.json，按架构取 darwin-aarch64 /
/// darwin-x86_64，带 sha256 校验）。解压后放 `~/Library/Application Support/
/// BangXin/assist/bin/mac/`，清 quarantine 后直接 exec。
///
/// 与 Windows 的关键差异：**不写 /Applications，因此不需要提权、不弹密码**——
/// 写 /Applications 属于系统目录，那才是之前弹管理员框的原因。
///
/// 为什么用 ditto 而不是 cp -R：RustDesk.app 的 framework 里有 `Versions/Current`
/// 符号链接，cp -R 在部分场景会把它解引用成实体目录，破坏签名（CI 里踩过同一坑）。
#[cfg(target_os = "macos")]
fn mac_install_rustdesk() -> Result<String, String> {
    if macos_cached_exe().is_some() || macos_installed_exe().is_some() {
        return Ok("already".into());
    }
    let dir = macos_assist_cache_dir().ok_or("HOME 不可读，无法定位组件缓存目录")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建组件缓存目录失败：{e}"))?;

    let man = dir.join("latest-assist.json");
    let _ = std::fs::remove_file(&man);
    let _ = Command::new("/usr/bin/curl")
        .args(["-L", "--fail", "-s", "--max-time", "60", "-o"])
        .arg(&man)
        .arg("https://im.x8fde.com/app/latest-assist.json")
        .status();

    let plat = if cfg!(target_arch = "aarch64") {
        "darwin-aarch64"
    } else {
        "darwin-x86_64"
    };
    let mut url = String::new();
    let mut want_sha = String::new();
    if let Ok(txt) = std::fs::read_to_string(&man) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
            if let Some(p) = v.get("platforms").and_then(|p| p.get(plat)) {
                url = p.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
                want_sha = p.get("sha256").and_then(|s| s.as_str()).unwrap_or("").to_string();
            }
        }
    }
    if url.is_empty() {
        return Err("远程协助组件清单不可用（请检查网络后重试）".into());
    }

    let tgz = dir.join("RustDesk.app.tar.gz");
    let _ = std::fs::remove_file(&tgz);
    let dl = Command::new("/usr/bin/curl")
        .args(["-L", "--fail", "-s", "--max-time", "600", "-o"])
        .arg(&tgz)
        .arg(&url)
        .status();
    if !matches!(dl, Ok(s) if s.success()) || !tgz.is_file() {
        return Err("远程协助组件下载失败（请检查网络后重试）".into());
    }
    if !want_sha.is_empty() {
        use sha2::{Digest, Sha256};
        match std::fs::read(&tgz) {
            Ok(bytes) => {
                let got = format!("{:x}", Sha256::digest(&bytes));
                if got != want_sha {
                    let _ = std::fs::remove_file(&tgz);
                    return Err("组件包校验失败（下载不完整），请重试".into());
                }
            }
            Err(e) => return Err(format!("读取组件包失败：{e}")),
        }
    }

    let stage = dir.join("stage");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).map_err(|e| format!("创建解压目录失败：{e}"))?;
    let ex = Command::new("/usr/bin/tar")
        .arg("-xzf")
        .arg(&tgz)
        .arg("-C")
        .arg(&stage)
        .status()
        .map_err(|e| format!("解压组件失败：{e}"))?;
    if !ex.success() {
        let _ = std::fs::remove_dir_all(&stage);
        return Err("组件解压失败，请重试".into());
    }
    let src = stage.join("RustDesk.app");
    if !src.is_dir() {
        let _ = std::fs::remove_dir_all(&stage);
        return Err("组件包内容异常（缺 RustDesk.app），请重试".into());
    }
    let dst_dir = dir.join("bin").join("mac");
    std::fs::create_dir_all(&dst_dir).map_err(|e| format!("创建组件目录失败：{e}"))?;
    let dst = dst_dir.join("RustDesk.app");
    let _ = std::fs::remove_dir_all(&dst);
    let dt = Command::new("/usr/bin/ditto")
        .arg(&src)
        .arg(&dst)
        .status()
        .map_err(|e| format!("落位组件失败：{e}"))?;
    let _ = std::fs::remove_dir_all(&stage);
    let _ = std::fs::remove_file(&tgz);
    if !dt.success() {
        return Err("组件落位失败，请重试".into());
    }
    // 🔴 2026-11-16 Apple 吊销整改 B：已删除原先这里的
    //   `xattr -dr com.apple.quarantine <dst>`
    // 说明：我们对缓存里的组件一律用 CLI 直接 exec，不经过 LaunchServices，因此不会触发
    // Gatekeeper 的隔离检查；而「主动剥隔离属性」在公证扫描器眼里是绕过 Gatekeeper 的动作，
    // 是本次被吊销的触发项之一。若将来改为 GUI 拉起，应走正常签名/公证，不得再 xattr 去掉。
    if macos_cached_exe().is_none() {
        return Err("组件安装后未找到可执行文件，请到下载页手动安装".into());
    }
    Ok("installed".into())
}

/// Linux：PATH 里是否已有可执行的 rustdesk（与 rustdesk_path() 同判据）。
/// 注意这是实时扫目录、不是进程启动时的缓存，所以刚装完立刻调用也能发现。
#[cfg(target_os = "linux")]
/// 🩸 2026-10-26：原来只扫 PATH。实测（老8 那台 rpm 系机器）包其实已经装上、
/// dnf 输出「已完成 / 无需任何处理」，但 GUI 会话的 PATH 里没有 /usr/bin，
/// 判据直接判死 → 报「安装后仍未找到 rustdesk」。改为 PATH 优先 + 常见安装位置兜底。
#[cfg(target_os = "linux")]
fn linux_rustdesk_find() -> Option<std::path::PathBuf> {
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            if !dir.is_empty() {
                dirs.push(std::path::PathBuf::from(dir));
            }
        }
    }
    // 兜底目录：桌面会话启动的进程 PATH 常缺 /usr/sbin、/opt 之类
    for d in [
        "/usr/bin",
        "/usr/local/bin",
        "/bin",
        "/opt/rustdesk",
        "/usr/lib/rustdesk",
    ] {
        dirs.push(std::path::PathBuf::from(d));
    }
    for dir in dirs {
        let p = dir.join("rustdesk");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn linux_rustdesk_in_path() -> bool {
    linux_rustdesk_find().is_some()
}

/// Linux：判定包管理器家族（deb / rpm）。
/// 🩸 2026-10-26 重写（老8 点头）：**命令探测优先**，os-release 只作兜底。
///   原实现只按 /etc/os-release 的 ID / ID_LIKE 名单匹配（debian/ubuntu/kylin/uos…），
///   名单天生漏 —— 不在名单里的发行版（openKylin、各类国产衍生版、精简系统）直接判死，
///   用户看到「未能识别当前 Linux 发行版」且远程协助组件永远装不上，功能完全不可用。
///   install.sh 早已是「以包管理器是否存在为准」的口径（比 ID_LIKE 可靠：麒麟/UOS 的
///   ID_LIKE 并不统一），客户端与之对齐。os-release 名单保留为最后兜底（极简系统）。
#[cfg(target_os = "linux")]
fn linux_pkg_family() -> Option<&'static str> {
    // ---- 1) 包管理器存在性（能装包才算数，与发行版叫什么无关） ----
    // 用扫 PATH 而非 `sh -c command -v`：无 shell 依赖、无进程开销，
    // 与 linux_rustdesk_in_path() 同款写法。另补两个标准系统目录 ——
    // 部分桌面会话的 PATH 不含 /usr/sbin。
    let has_bin = |bin: &str| {
        if let Ok(path) = std::env::var("PATH") {
            for dir in path.split(':') {
                if !dir.is_empty() && std::path::Path::new(dir).join(bin).is_file() {
                    return true;
                }
            }
        }
        ["/usr/bin", "/usr/sbin", "/bin", "/sbin"]
            .iter()
            .any(|d| std::path::Path::new(d).join(bin).is_file())
    };
    if has_bin("apt-get") || has_bin("dpkg") {
        return Some("deb");
    }
    if has_bin("dnf") || has_bin("yum") || has_bin("zypper") || has_bin("rpm") {
        return Some("rpm");
    }

    // ---- 2) 兜底：os-release 名单 ----
    // 个别精简系统 / 容器里 /etc/os-release 缺失，只放 /usr/lib/os-release，两处都试。
    let raw = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .unwrap_or_default()
        .to_lowercase();
    let mut id = String::new();
    let mut like = String::new();
    for line in raw.lines() {
        if let Some(v) = line.strip_prefix("id_like=") {
            like = v.trim_matches('"').to_string();
        } else if let Some(v) = line.strip_prefix("id=") {
            id = v.trim_matches('"').to_string();
        }
    }
    let has = |s: &str, w: &str| s.split_whitespace().any(|x| x == w);
    let deb_words = [
        "debian", "ubuntu", "deepin", "uos", "kali", "mint", "kylin", "uniontech", "openkylin",
        "neokylin", "bclinux",
    ];
    let rpm_words = [
        "rhel", "fedora", "centos", "openeuler", "anolis", "rocky", "almalinux", "suse",
        "opensuse", "tencentos", "alios", "euleros", "bclinux-for-euler",
    ];
    if deb_words.iter().any(|w| has(&like, w) || id == *w) {
        return Some("deb");
    }
    if rpm_words.iter().any(|w| has(&like, w) || id == *w) {
        return Some("rpm");
    }
    None
}

/// Linux：下载官方包（优先 curl，回落 wget —— 最小化桌面可能只有其一）。返回下载后的本地路径。
#[cfg(target_os = "linux")]
fn linux_download(url: &str, dest: &str) -> Result<(), String> {
    let ok_curl = Command::new("curl")
        .args(["-fL", "--connect-timeout", "20", "--max-time", "900", "-o", dest, url])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok_curl {
        return Ok(());
    }
    let ok_wget = Command::new("wget")
        .args(["-q", "-O", dest, url])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok_wget {
        return Ok(());
    }
    Err("组件下载失败，请检查网络后重试".into())
}

/// Linux：一键安装官方 RustDesk（版本固定与 mac 内置一致，避免配置/协议差异）。
#[cfg(target_os = "linux")]
fn linux_install_rustdesk() -> Result<String, String> {
    // 与 build-macos.yml 内置的官方包同版本（1.5.0）：--password / approve-mode 行为已实证
    const RD_VER: &str = "1.5.0";
    if linux_rustdesk_in_path() {
        return Ok("already".into());
    }
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => return Err(format!("暂不支持的 CPU 架构：{other}（请到下载页取通用 AppImage）")),
    };
    let fam = linux_pkg_family().ok_or_else(|| {
        "未能识别当前 Linux 发行版，请到 https://im.x8fde.com/app/download.html 手动下载组件".to_string()
    })?;
    // 包名与七牛对象名一致（x8im_dl.php 白名单已放行），走 CDN 中转避免 GitHub 慢/被墙
    let file = format!("rustdesk-{RD_VER}-{arch}.{fam}");
    let url = format!("https://im.x8fde.com/x8/x8im_dl.php?f={file}");
    let dest = format!("/tmp/{file}");
    linux_download(&url, &dest)?;
    if !std::path::Path::new(&dest).is_file() {
        return Err("组件下载失败（文件未落盘），请重试".into());
    }
    // 安装 + 关掉官方包自带的开机自启服务（邦信沿用「按需启动、用完即 kill、不开机自启」口径）
    let inst = if fam == "deb" {
        "apt-get install -y"
    } else {
        "dnf install -y"
    };
    let script = format!(
        "DEBIAN_FRONTEND=noninteractive {} '{}' >/tmp/x8im-rd-install.log 2>&1 || \
         (rpm -Uvh --replacepkgs '{}' >>/tmp/x8im-rd-install.log 2>&1); \
         systemctl disable --now rustdesk >/dev/null 2>&1; true",
        inst, dest, dest
    );
    let out = Command::new("pkexec")
        .args(["sh", "-c", script.as_str()])
        .output()
        .map_err(|e| format!("提权执行失败: {e}"))?;
    let _ = std::fs::remove_file(&dest);
    if !out.status.success() {
        // pkexec 用户取消 = 126/127；其余多因无 polkit 环境（服务器版/精简桌面）
        return Err("安装未完成（可能取消了系统授权）。可重试，或到下载页手动安装组件".into());
    }
    if !linux_rustdesk_in_path() {
        // apt 报错但退出码为 0 的极少见；附日志尾部便于排查
        let log = std::fs::read_to_string("/tmp/x8im-rd-install.log").unwrap_or_default();
        let tail: String = log.lines().rev().take(3).collect::<Vec<_>>().join(" / ");
        // 附上诊断信息：PATH 与固定位置探测结果，下次不用再让用户手工跑 which
        let path_env = std::env::var("PATH").unwrap_or_else(|_| "-".into());
        let hits: Vec<&str> = ["/usr/bin/rustdesk", "/usr/local/bin/rustdesk", "/usr/lib/rustdesk/rustdesk"]
            .into_iter()
            .filter(|p| std::path::Path::new(p).exists())
            .collect();
        let hit_txt = if hits.is_empty() { "无".to_string() } else { hits.join(",") };
        return Err(format!(
            "安装后仍未找到 rustdesk（PATH={path_env}；固定位置={hit_txt}）（{tail}）"
        ));
    }
    Ok("installed".into())
}

// 2026-10-26 Linux 应用内升级（1.6.39）：复用 install_rustdesk 已实证的 pkexec 模式，
// 把「下载安装包 + 手动双击」收敛成一次点击。Windows/mac 不走这条（恒返回错误由前端忽略）。
//
// 约束与踩坑预防：
//   · 只对包管理器安装生效：AppImage 跑在临时挂载点，装包覆盖不到它 → 直接报错让用户下 AppImage
//   · 包名／架构口径与七牛对象名一致（x8im_dl.php 已放行），走中转不用直链
//   · 全流程是阻塞调用（curl + pkexec），必须 spawn_blocking，否则整窗未响应（AppHang 老账）
// P2P 传输内核 sidecar 的本地通道（2026-11-06）
//
// 前端 p2p.js 在「原生 RTCPeerConnection 不存在」时会问这里要直传能力，
// 拿到 {transport:"ipc"} 之后就用下面三兄弟把 sidecar 当虚拟 DataChannel 用。
// 非 Linux 返回 null → 前端照旧走原生 WebRTC，不需要按平台分支。
//
// 🔴 为什么不是本地 WebSocket：WebKitGTK 把 https 页面里的 ws://（含 127.0.0.1）
// 全部按混合内容拦掉，实测 error；http 页面则正常。页面必须 https，所以改走 IPC：
// 二进制在 JS 侧转 base64，由这三条命令转进 sidecar 的 stdin/stdout 帧流。
//
// X8_P2P_FORCE=1 时 Windows/macOS 也会启用这条通道（仅用于本机回归 IPC 桥，
// 正常发版不带这个环境变量；Windows/mac 照旧走原生 WebRTC）。
fn p2p_enabled() -> bool {
    if !p2p_sidecar::available() {
        return false;
    }
    if cfg!(target_os = "linux") {
        return true;
    }
    std::env::var("X8_P2P_FORCE").ok().as_deref() == Some("1")
}

#[tauri::command]
fn p2p_sidecar_info() -> Option<serde_json::Value> {
    if p2p_enabled() {
        return Some(serde_json::json!({ "transport": "ipc" }));
    }
    None
}

// 开一个传输会话（一个进程 = 一次传输），返回会话 id
#[tauri::command]
async fn p2p_sc_open() -> Result<u32, String> {
    if !p2p_enabled() {
        return Err("p2p sidecar 未启用".to_string());
    }
    tauri::async_runtime::spawn_blocking(p2p_sidecar::open)
        .await
        .unwrap_or_else(|e| Err(format!("join: {e}")))
}

// op=1 时 payload 是控制 JSON 原文；op=2 时是二进制帧的 base64
#[tauri::command]
async fn p2p_sc_send(id: u32, op: u8, payload: String) -> Result<(), String> {
    if !p2p_enabled() {
        return Err("p2p sidecar 未启用".to_string());
    }
    let bytes = if op == 1 {
        payload.into_bytes()
    } else {
        p2p_sidecar::b64_decode(&payload)?
    };
    tauri::async_runtime::spawn_blocking(move || p2p_sidecar::send(id, op, &bytes))
        .await
        .unwrap_or_else(|e| Err(format!("join: {e}")))
}

// 拉一帧；超时返回 null（前端继续轮询），携带 {op, payload}
#[tauri::command]
async fn p2p_sc_recv(id: u32, timeout_ms: u64) -> Result<Option<serde_json::Value>, String> {
    if !p2p_enabled() {
        return Err("p2p sidecar 未启用".to_string());
    }
    let r = tauri::async_runtime::spawn_blocking(move || p2p_sidecar::recv(id, timeout_ms))
        .await
        .unwrap_or_else(|e| Err(format!("join: {e}")))?;
    Ok(r.map(|f| {
        let payload = if f.op == 1 {
            String::from_utf8_lossy(&f.payload).to_string()
        } else {
            p2p_sidecar::b64_encode(&f.payload)
        };
        serde_json::json!({ "op": f.op, "payload": payload })
    }))
}

#[tauri::command]
async fn p2p_sc_close(id: u32) -> Result<(), String> {
    let _ = tauri::async_runtime::spawn_blocking(move || p2p_sidecar::close(id)).await;
    Ok(())
}

#[tauri::command]
async fn linux_self_update(app: tauri::AppHandle) -> Result<String, String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = app;
        Err("应用内升级仅在 Linux 版支持".to_string())
    }
    #[cfg(target_os = "linux")]
    {
        // 2026-11-04：AppImage 版改走 tauri-plugin-updater 静默通道（插件按 APPIMAGE
        // 环境变量定位待替换文件，下载 .AppImage.tar.gz → 验签 → 原地替换 → 重启）。
        // 不弹 pkexec、不需要用户输密码；deb/rpm 仍走下面那条包管理器路径。
        if std::env::var_os("APPIMAGE").is_some() {
            return crate::mac_updater::mac_self_update(app.clone());
        }
        let _ = app;
        // tauri 2 的 spawn_blocking 返回 JoinHandle<Result<T, tauri::Error>>（1.6.39 首轮 CI 实测：
        // 直接 .await 得到 Result<Result<String,String>, tauri::Error>，mismatched types）。
        // 故先 map_err 成 String 再 ? 拆一层，与命令签名 Result<String, String> 对齐。
        tauri::async_runtime::spawn_blocking(linux_self_update_blocking)
            .await
            .map_err(|e| format!("升级线程异常: {e}"))?
    }
}

#[cfg(target_os = "linux")]
fn linux_self_update_blocking() -> Result<String, String> {
    // AppImage：程序本体在 /tmp/.mount_xxx 里，装 deb/rpm 覆盖不到，也不该在运行中替换
    if std::env::var("APPIMAGE").is_ok() {
        return Err("当前是 AppImage 版，请下载新的 AppImage 替换即可".to_string());
    }
    let arch = match std::env::consts::ARCH {
        "x86_64" => "",
        "aarch64" => "-arm64",
        other => return Err(format!("暂不支持的 CPU 架构：{other}")),
    };
    let fam = linux_pkg_family()
        .ok_or_else(|| "未能识别当前 Linux 发行版，请到下载页手动下载安装".to_string())?;
    let file = format!("BangXin-linux{arch}.{fam}");
    let url = format!("https://im.x8fde.com/x8/x8im_dl.php?f={file}");
    let dest = format!("/tmp/{file}");
    let _ = std::fs::remove_file(&dest);
    linux_download(&url, &dest)?;
    if !std::path::Path::new(&dest).is_file() {
        return Err("安装包下载失败（文件未落盘），请重试".to_string());
    }
    let log = "/tmp/x8im-self-update.log";
    let inst = if fam == "deb" {
        format!("DEBIAN_FRONTEND=noninteractive apt-get install -y '{dest}'")
    } else {
        // rpm 系：dnf 失败（老系统）再兜 rpm -Uvh --replacepkgs
        format!("(dnf install -y '{dest}' || yum install -y '{dest}' || rpm -Uvh --replacepkgs '{dest}')")
    };
    let script = format!("{inst} >{log} 2>&1");
    let out = Command::new("pkexec")
        .args(["sh", "-c", script.as_str()])
        .output()
        .map_err(|e| format!("提权执行失败（缺 pkexec？）: {e}"))?;
    let _ = std::fs::remove_file(&dest);
    if !out.status.success() {
        let tail: String = std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .join(" / ");
        return Err(format!("安装未完成（可能取消了系统授权）：{tail}"));
    }
    // 装完自动重启：新二进制已落盘，先脱离当前进程再拉起（setsid 断开父子关系，避免一起被回收）
    //
    // 2026-10-26 二改（老8 反馈「装完要能自动跑起新版」）：上一版用 600ms 后 spawn 一个
    // `sh -c "sleep 1; setsid ..."` 再 exit(0)，实测有时新进程不出现，排查下来是三个软点：
    //   ① apt/dpkg 装完还要刷 desktop 数据库，1 秒不够 → 延到 3 秒；
    //   ② 只用 setsid，若极简系统缺 util-linux 就整条命令失败且无声 → 加 nohup 直接起做兜底；
    //   ③ 无日志，出了问题无从判断 → 统一追加到 /tmp/x8im-restart.log（含时间戳与退出码）。
    // 另外把子进程三件套都指向 null，父进程退出时不会因为管道断开而连坐。
    let exe = if std::path::Path::new("/usr/bin/x8im").exists() {
        "/usr/bin/x8im".to_string()
    } else {
        std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "x8im".to_string())
    };
    let rlog = "/tmp/x8im-restart.log";
    let script = format!(
        "echo \"[restart] $(date '+%F %T') exe={exe} disp=$DISPLAY wayl=$WAYLAND_DISPLAY\" >>{rlog}; \
         ( setsid '{exe}' >>{rlog} 2>&1 </dev/null & ) \
         || ( nohup '{exe}' >>{rlog} 2>&1 </dev/null & ) ; \
         echo \"[restart] launched rc=$?\" >>{rlog}"
    );
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = Command::new("sh")
            .arg("-c")
            .arg(format!("sleep 3; {script}"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    });
    std::thread::sleep(std::time::Duration::from_millis(500));
    // 前端此时不会收到返回值（进程已退出重启）；成功路径靠「界面重开且版本号变了」验收。
    // 2026-11-10 P1-3：process::exit 绕过 RunEvent::Exit，必须显式收掉插件。
    wx_sidecar::stop_all_fast();
    std::process::exit(0);
}

#[derive(serde::Serialize)]
struct EnsureOut {
    id: String,
    password: Option<String>,
    changed: bool,
    /// 2026-10-16 老8 诉求（远程时"只点一下"）配套：本机是否挂着 RDP 会话。
    /// RustDesk 的 Windows 会话列表无条件含 Console，只要还有 RDP 会话，对端建连就会被
    /// 追问「发现多个 Windows 会话」，而默认选中空的 Console（连过去黑屏）。
    /// 前端把该标记上报服务端，控制端建连前据此提示「选带 (running) 的那一项」。
    multi: bool,
}

/// 2026-10-16 实测：`qwinsta` 输出里活动 RDP 会话名为 `rdp-tcp#N`，而纯 `rdp-tcp`
/// 是无会话的侦听器行 —— 只认带 `#` 的，避免把「仅侦听」误判成多会话。
#[cfg(windows)]
fn win_has_rdp_session() -> bool {
    // qwinsta.exe 是控制台程序：CREATE_NO_WINDOW 否则远程协助就绪检查闪黑窗
    // （2026-10-22 与 download_file curl 同批修复）。
    let out = match Command::new("qwinsta").creation_flags(0x0800_0000).output() {
        Ok(o) => o,
        Err(_) => return false,
    };
    let text = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    text.lines().any(|l| l.contains("rdp-tcp#"))
}

#[cfg(not(windows))]
fn win_has_rdp_session() -> bool {
    false
}

/// 2026-10-15 老8 定：对外拉起必须用品牌化 X8Assist.exe（托盘/窗口不露 RustDesk）。
/// Windows：未装不回落便携版，直接报错提示重新安装/升级；非 Windows 维持内置客户端。
#[cfg(windows)]
fn assist_exe(_app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    installed_assist_path()
        .ok_or_else(|| "远程协助组件未安装，请重新安装或升级 X8Assist".to_string())
}

/// mac/linux：真正「启动给人用」的那份可执行文件。
///
/// ⚠️ mac 2026-10-18 甘真机实证，必须与 `--password` 用**同一份**（`/Applications` 那份）：
///  ① 资源包里的 `RustDesk.app` 带着 dmg 下载带入的 `com.apple.quarantine`，
///     一启动就是 Gatekeeper 的「RustDesk 已损坏，无法打开」—— 用户原话
///     「我原来也没这个 RustDesk，装了 IM 之后提示损坏」，正是这份；他手动
///     `xattr -dr /Applications/RustDesk.app` 后才跳过，说明我们启动的其实是**另一份**
///     （用户清的是 /Applications，弹的却还是损坏 → 两份都带隔离标记）。
///  ② 与设密码那份跨 bundle 时，root CLI 找 IPC 目标也会对不上。
/// 故：优先 `/Applications` 那份（已由 ensure_rustdesk 清隔离 + 对齐版本）；
/// 只有它还不存在（首次、尚未走过 ensure_rustdesk）时才回落资源包。
#[cfg(not(windows))]
fn assist_exe(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    // mac 2026-11-11 方案 A：优先按需下载的缓存（与 ensure_rustdesk 的 rustdesk_path
    // 同源，满足「--password 与启动必须是同一份」），其次才是历史上落位到
    // /Applications 的那份（老机器升级上来的），最后回落到安装包内资源（1.6.86 起已无）。
    #[cfg(target_os = "macos")]
    if let Some(p) = macos_cached_exe() {
        return Ok(p);
    }
    #[cfg(target_os = "macos")]
    if let Some(p) = macos_installed_exe() {
        return Ok(p);
    }
    rustdesk_path(app).ok_or_else(|| {
        "远程协助组件未安装，请在「远程协助」里点安装，或到下载页手动安装组件".to_string()
    })
}

/// 拉起 rustdesk:// 协议或直连（控制方用），绕过 WebView 外链限制。
/// 2026-10-15：target 为空 = 以 --tray 启动本机 RustDesk 进入待控状态（被控端接受协助后用）——
///             托盘常驻、不弹主窗口（RustDesk 1.4.3 core_main.rs：--tray 分支三平台均支持，
///             已在跑则直接返回，不会重复启动）。
/// 2026-10-15：password 非空时随 --connect 一并下发（拼进 uni_link ?password=，官方支持），
///             控制端免手输密码；被控端验证方式含密码时即免确认弹窗（与 QQ 远程同体验）。
/// 2026-10-16：async —— 内部 kill_rustdesk_procs（taskkill .output()）等阻塞调用
///             不得占主线程，否则点「一键连接」也会整窗未响应。
#[tauri::command]
async fn open_rustdesk(app: tauri::AppHandle, target: String, password: Option<String>) -> Result<(), String> {
    // 2026-10-24 P1-RS-2：整段（kill taskkill/pkill .output() + 多次 fs 写 + 再杀进程）都是阻塞调用，
    // 直跑 async 体占死 tokio worker（AppHangB1 冻结病灶残留）。移入 spawn_blocking，async 壳只转发。
    tauri::async_runtime::spawn_blocking(move || {
    if target.trim().is_empty() {
        // 空 = 按需启动本机 X8Assist 进入待控状态，等对方连入。
        // 2026-10-16 老8 定（性能优先）：**不再开机常驻**（installer.nsh 已删 HKLM Run 自启），
        // 只在「接受远程协助」时拉起，协助结束或空闲由 assist_idle_reap / close_rustdesk 回收。
        // 启动方式 = **主进程模式（不带 `--server`）**，两点实测依据：
        //   · `--server` 会置 IS_SERVER=true，于是 `src/server/connection.rs:2836` 在连接建立时
        //     `run_as_user("--tray")` 拉起托盘进程 → 托盘出现 X8Assist 图标（老8 不要）。
        //   · 主进程模式 IS_SERVER=false → 不拉 `--tray`；本机实测（自连 255610226）进程数不增、
        //     无托盘，且 IPC 管道 `X8Assist\query` 正常就绪（供邦信驱动），冷启动 `--get-id` 0.2s。
        // 窗口保持隐藏：hide_cm 三项（连接管理器窗口不出现）。
        // 2026-10-15 老8 要求：先结束已有实例再拉起，避免多实例并存（hbbs 注册互踩）
        kill_rustdesk_procs();
        // 2026-10-15：连接前幂等确保自建服务器配置已写入（防「配置没落位」打回公共节点）
        // 🔴 2026-11-06：写不进去直接报错，别把用户送到官方节点去撞「Key 不匹配」
        ensure_rustdesk_server_config().map_err(|e| format!("远程协助服务器配置未就绪：{e}"))?;
        // 2026-10-16 老8：本机进入待控状态前，先把「连接管理器窗口」（含权限面板）关成不显示
        ensure_approve_config();
        // 2026-10-15 老8 定：必须品牌化 X8Assist.exe，未装报错提示重装/升级
        let exe = assist_exe(&app)?;
        Command::new(&exe)
            .spawn()
            .map_err(|e| format!("启动 RustDesk 失败: {e}"))?;
        // 2026-10-16 老8：主窗（你的桌面）别盖在邦信上 —— 后台线程重试隐藏
        hide_assist_windows_retried();
        // mac/linux：官方客户端无 hide_cm，对端连入时连接管理器仍会弹（无 Accept 按钮、
        // 3s 自最小化）—— 待控期间持续隐藏所有 rustdesk 窗口，视觉与 Windows 一致。
        #[cfg(not(windows))]
        hide_assist_windows_while_running();
        Ok(())
    } else if target.chars().all(|c| c.is_ascii_digit()) {
        // 纯数字 = 对方 ID，直接发起控制；带密码则一并下发（免输密码/免确认）
        // 2026-10-15：连接前幂等确保自建服务器配置已写入（防「配置没落位就直连」打回公共节点）
        // 2026-10-15 老8 要求：先结束已有实例再拉起，避免多实例并存
        // 🔴 2026-11-06：同上，配置未就绪一律中断，不静默连公共节点
        ensure_rustdesk_server_config().map_err(|e| format!("远程协助服务器配置未就绪：{e}"))?;
        kill_rustdesk_procs();
        // 2026-10-16 老8：旧实例已退出、新进程尚未启动 —— 正是把本机保存的该对端偏好
        // 写成顺手值的时机（画面适应窗口 + 收起工具栏），用户点一下就走，不必再调菜单。
        let _ = ensure_peer_prefs(&target);
        // 2026-10-16 老8：控制端同理 —— 本机会话窗口（权限面板）不要弹
        ensure_approve_config();
        // 2026-10-15 老8 定：控制端同样必须品牌化 X8Assist.exe
        let exe = assist_exe(&app)?;
        let mut cmd = Command::new(&exe);
        cmd.arg("--connect").arg(&target);
        if let Some(p) = password.as_deref().filter(|p| !p.is_empty()) {
            cmd.arg("--password").arg(p);
        }
        cmd.spawn().map_err(|e| format!("启动 RustDesk 失败: {e}"))?;
        // 2026-10-17 修复（老8 报「点了/对方接受/远程窗口没出来」）：控制端**绝不可**无差别隐藏
        // X8Assist 窗口。原 hide_assist_windows_retried() 在拉起后 6s 内把进程名下所有可见窗口
        // SW_HIDE —— 局域网/同公司网络打洞快，远程桌面「会话窗口」常在 6s 内弹出即被永久藏死，
        // 用户眼前只剩邦信、看不到对方屏幕（被控端那支保留隐藏，彼时还没有会话窗口）。
        // 连接管理器窗口（--cm）在密码已就绪机器上已由 ensure_hide_cm()（approve-mode=password）
        // 从配置层压掉，无需靠循环藏；会话窗口必须可见 —— 故此处不调 hide。
        Ok(())
    } else if target.starts_with("rustdesk://") {
        open_external(&target).map_err(|e| format!("拉起协议失败: {e}"))?;
        Ok(())
    } else {
        Err("无效的 RustDesk 目标".into())
    }
    })
    .await
    .map_err(|e| format!("远程协助拉起线程异常：{e}"))?
}

/// 2026-10-15 老8 要求：结束远程协助时把托盘里的 RustDesk 一并关掉 —— 被控端 --tray
/// 常驻实例、控制端连接窗口都是用户级 UI 进程，taskkill /IM 可杀；SYSTEM 服务进程
/// 杀不到，服务化常驻场景不受影响。
/// 2026-10-16：async —— 内部 taskkill 阻塞调用移出主线程（同上）。
#[tauri::command]
async fn close_rustdesk() {
    // 2026-10-24 P1-RS-3：taskkill/pkill .output() 阻塞，移出 tokio worker。
    tauri::async_runtime::spawn_blocking(kill_rustdesk_procs)
        .await
        .ok();
}

/// 2026-10-16 老8 定「不用时就都 kill」：空闲回收。
/// 本机 X8Assist 已按需启动、但**当前没有任何远程会话连接**时，全部结束（省资源、不留残进程）。
/// 会话判定（Windows）：X8Assist 各进程的 TCP ESTABLISHED 连接里，存在**远端端口非 21116**
/// 的连接即视为「会话中」（21116 = hbbs 注册/打洞口，空闲时也常驻；直连为对端随机端口、
/// 中继为 21117，都算）。返回 true 表示本次确实回收了进程（前端据此提示并停掉心跳）。
/// 用 tasklist/netstat 而非 Win32 API，避免额外依赖；只按 ASCII 字段解析，
/// 中文系统 GBK 行内汉字不影响（状态列 "ESTABLISHED" 恒为英文）。
#[tauri::command]
async fn assist_idle_reap() -> bool {
    // 2026-10-24 P1-RS-4：tasklist/netstat/taskkill（及非 Win 的 pgrep/lsof/ss/pkill）全 .output() 阻塞，
    // 直跑 async 体占死 tokio worker。整段移入 spawn_blocking。
    tauri::async_runtime::spawn_blocking(move || {
    #[cfg(not(windows))]
    {
        // mac/linux（2026-10-16 补平台缺口）：与 Windows 同判据 —— 进程在、但没有
        // 「真正的会话」连接（远端端口全是 hbbs/hbbr 的 21116/21117）就回收。
        // 取不到网络信息时 unix_has_live_session 返回 true → 宁可不回收（防误杀）。
        if !unix_rustdesk_running() {
            return false; // 本来就没进程 → 无需回收
        }
        if unix_has_live_session() {
            return false; // 会话进行中 → 绝不动
        }
        kill_rustdesk_procs();
        true
    }
    #[cfg(windows)]
    {
        let pids = assist_pids();
        if pids.is_empty() {
            return false; // 本来就没进程 → 无需回收
        }
        if assist_has_live_session(&pids) {
            return false; // 会话进行中 → 绝不动
        }
        kill_rustdesk_procs();
        true
    }
    })
    .await
    .unwrap_or(false)
}

/// 本机 X8Assist 进程 PID 列表（tasklist CSV 行：`"X8Assist.exe","11664",...`）
#[cfg(windows)]
fn assist_pids() -> Vec<String> {
    let out = match Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq X8Assist.exe", "/FO", "CSV", "/NH"])
        .creation_flags(0x0800_0000)
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("X8Assist.exe"))
        .filter_map(|l| l.split("\",\"").nth(1))
        .map(|s| s.trim_matches('"').trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 2026-10-16 老8 诉求：X8Assist 主窗口（「你的桌面/控制远程桌面」）不要盖在邦信上。
/// 把 X8Assist 进程名下**所有可见顶层窗口** ShowWindow(SW_HIDE)。
/// 只在「本机先 kill 全部实例 → 刚 spawn 新实例」的就绪/拉起窗口期调用（此时不存在
/// 进行中的远程会话窗口，不会误藏会话画面；邦信流程保证 spawn 前必 kill）。
/// flutter 窗口异步创建，调用方需在 spawn 后多次重试（见 hide_assist_windows_retried）。
#[cfg(windows)]
fn hide_assist_windows() {
    use std::collections::HashSet;
    use windows_sys::Win32::Foundation::BOOL;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindowVisible, ShowWindow, SW_HIDE,
    };
    let pids: HashSet<u32> = assist_pids()
        .iter()
        .filter_map(|s| s.parse::<u32>().ok())
        .collect();
    if pids.is_empty() {
        return;
    }
    unsafe extern "system" fn enum_proc(hwnd: *mut std::ffi::c_void, lparam: isize) -> BOOL {
        unsafe {
            let pids = &*(lparam as *const HashSet<u32>);
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pids.contains(&pid) && IsWindowVisible(hwnd) != 0 {
                ShowWindow(hwnd, SW_HIDE);
            }
        }
        1 // 继续枚举
    }
    unsafe {
        EnumWindows(Some(enum_proc), &pids as *const _ as isize);
    }
}

/// 2026-10-26 老8 报：安装后第一次启动邦信，X8Assist 窗口仍会黑屏闪一下。
/// 根因：兜底写密码链路里 flutter 主窗先 Show 后我们才抢到 SW_HIDE，中间必然漏一帧
/// （Show 是 Flutter runner 里硬编码的 SW_SHOWNORMAL，STARTUPINFO 的 SW_HIDE 管不住它）。
/// 对策：把**窗口位置本身**挪到屏幕外（-32000,-32000），Flutter 的 Show 只改可见性、
/// 不会把位置改回来 —— 位置一旦移走就永久生效，抢不到可见性也不会被看到。
/// 只对指定 PID 集合生效（spawn 后拿 child.id 直接命中，不必反复 tasklist）。
#[cfg(windows)]
fn park_pid_windows(pids: &std::collections::HashSet<u32>) {
    use windows_sys::Win32::Foundation::BOOL;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindowVisible, SetWindowPos, ShowWindow,
        SWP_NOACTIVATE, SWP_NOZORDER, SWP_NOSIZE, SW_HIDE,
    };
    if pids.is_empty() {
        return;
    }
    unsafe extern "system" fn enum_proc(hwnd: *mut std::ffi::c_void, lparam: isize) -> BOOL {
        unsafe {
            let pids = &*(lparam as *const std::collections::HashSet<u32>);
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pids.contains(&pid) {
                // 先挪出可见区域（幂等、可重复调用），再叠一次 SW_HIDE
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    -32000,
                    -32000,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
                if IsWindowVisible(hwnd) != 0 {
                    ShowWindow(hwnd, SW_HIDE);
                }
            }
        }
        1 // 继续枚举
    }
    unsafe {
        EnumWindows(Some(enum_proc), pids as *const _ as isize);
    }
}

/// mac/linux 对应实现：隐藏官方 RustDesk 的全部窗口（等价 Windows EnumWindows+SW_HIDE）。
///   mac：**2026-11-16 已停用**。原实现是
///        `osascript -e 'tell application "System Events" to set visible of process "rustdesk" to false'`，
///        即用 UI 脚本把另一个进程的窗口设为不可见 —— 这在 Apple 公证扫描器眼里是
///        隐蔽远控的典型特征（也是本次账号被吊销的触发项之一），已整段删除。
///        代价：被控端待控时，官方 RustDesk 的连接面板会显示出来（功能不受影响，只是视觉回退）。
///        后续如需隐藏，只能用 NSRunningApplication.hide()（进程内 API），不得再用 AppleScript。
///   linux：wmctrl 可用时把 rustdesk 类窗口置 hidden（发行版未装 wmctrl 则跳过）。
#[cfg(not(windows))]
fn hide_assist_windows() {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // wmctrl -lx 行：`0x04200000  0 rustdesk.Rustdesk  host  title`，第 3 列是类名
        if let Ok(out) = Command::new("wmctrl").args(["-lx"]).output() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                let cls = line.split_whitespace().nth(2).unwrap_or("");
                if cls.to_lowercase().contains("rustdesk") {
                    if let Some(id) = line.split_whitespace().next() {
                        let _ = Command::new("wmctrl")
                            .args(["-i", "-r", id, "-b", "add,hidden"])
                            .output();
                    }
                }
            }
        }
    }
}

/// spawn 后台线程多次重试隐藏（flutter 主窗口创建是异步的，通常 1~3 秒内出现）。
/// 每 500ms 一次、共 12 次（6s），命中过一次后仍继续跑完（多窗口/晚创建都覆盖）。
/// ⚠️ 仅用于**被控端待控**分支（空 target，拉起后还没会话窗口）。
/// 控制端（--connect）**不得**调用：局域网打洞快，远程画面会话窗口常在 6s 内弹出即被永久藏死。
fn hide_assist_windows_retried() {
    std::thread::spawn(|| {
        for _ in 0..12 {
            hide_assist_windows();
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    });
}

/// 被控端待控期间**持续**隐藏 RustDesk 窗口（仅 mac/linux）：官方客户端没有 hide_cm，
/// 对端连入时连接管理器窗口会晚于启动期弹出（authorized 时无 Accept 按钮、3s 自最小化，
/// 但为与 Windows「眼前只有邦信」的体感一致，实例存活期间每 500ms 藏一次）。
/// 控制端不能用任何 hide（会藏掉远程画面会话窗口）；控制端连接管理器由 ensure_hide_cm() 配置层压掉。
/// 实例被回收（close_rustdesk / assist_idle_reap / 会话结束）后循环自行退出。
#[cfg(not(windows))]
fn hide_assist_windows_while_running() {
    std::thread::spawn(|| loop {
        if !unix_rustdesk_running() {
            break;
        }
        hide_assist_windows();
        std::thread::sleep(std::time::Duration::from_millis(500));
    });
}

/// 给定 PID 集合里是否存在「真正的远程会话」TCP 连接（远端端口 != 21116）
#[cfg(windows)]
fn assist_has_live_session(pids: &[String]) -> bool {
    let out = match Command::new("netstat")
        .args(["-ano", "-p", "tcp"])
        .creation_flags(0x0800_0000)
        .output()
    {
        Ok(o) => o,
        // 取不到网络信息时宁可不回收（避免误杀进行中的会话）
        Err(_) => return true,
    };
    String::from_utf8_lossy(&out.stdout).lines().any(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 5
            || !f[0].eq_ignore_ascii_case("TCP")
            || !f[3].eq_ignore_ascii_case("ESTABLISHED")
        {
            return false;
        }
        if !pids.iter().any(|p| p == f[4]) {
            return false;
        }
        f[2].rsplit(':').next().unwrap_or("") != "21116"
    })
}

/// 用系统默认浏览器打开一个 https 页面（2026-10-15 老8：更新条「下载 Windows 版」
/// 在 WebView 里点 <a download> 没反应 —— Tauri WebView 不处理下载事件，
/// 改为打开系统浏览器触发真实下载）。
#[tauri::command]
async fn open_in_browser(url: String) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("无效地址".into());
    }
    // 2026-11-07 二次评审 P1-3：open_checked 会等子进程退出码（Linux 三个候选串行最多
    // 1.5s×3）。同步 command 跑在主线程会把 WebView 一起卡住，必须丢到 blocking 线程池。
    tokio::task::spawn_blocking(move || open_external(&url))
        .await
        .map_err(|e| format!("拉起外链失败: {e}"))?
}

/// 仅这些「查看型」扩展名在下载后自动用默认程序打开；其余（含可执行/脚本类）
/// 只落盘不打开 —— 避免接收恶意 .exe/.ps1/.msi 等被自动执行（S1 修复）。
const SAFE_OPEN_EXT: &[&str] = &[
    // 仅「查看型」扩展名自动打开；svg/html/htm 可携带脚本，接收恶意文件时自动打开 = 代码执行面，故剔除（S5）
    "png", "jpg", "jpeg", "gif", "bmp", "webp", "heic",
    "pdf", "txt", "md", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "csv", "rtf", "odt", "ods", "odp",
    "mp3", "mp4", "wav", "mov", "avi", "mkv", "webm", "flac",
    "zip", "rar", "7z", "tar", "gz", "tgz", "bz2",
    "json", "xml",
];

// ==================== 本地落盘 HTTP 服务（2026-11-06） ====================
// 2026-11-17 拆分：本地 HTTP 服务实现 → http_local.rs；/x8ipc 命令兜底桥 → bridge.rs。
// 两者都是 main.rs 的子模块，内部 `use super::*` 拿命令函数，行为与拆分前完全一致。
mod http_local;
mod bridge;
use http_local::*;

// ── 事件补投队列（2026-11-16）──────────────────────────────────────────────
// Tauri 的 emit 走 WebView 的 IPC 回程 —— 桥一失效，Rust 这边照发不误，前端却永远
// 收不到（x8mail 新邮件角标不更新、托盘「检查更新」点了没反应都是这个）。
// HTTP 没有「服务端主动推送」的等价物，所以这里给每条事件编号入环形队列，
// 前端用 2 秒 HTTP 轮询补取。listen 保留：两条路都在时以轮询为准（seq 去重）。
mod evq {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use tauri::Emitter;

    static SEQ: AtomicU64 = AtomicU64::new(1);
    static Q: Mutex<VecDeque<(u64, String, serde_json::Value)>> = Mutex::new(VecDeque::new());
    const KEEP: usize = 128;

    /// 发事件：正常 emit + 入队（补投用）
    pub fn push(app: &tauri::AppHandle, name: &str, payload: serde_json::Value) {
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut q) = Q.lock() {
            q.push_back((seq, name.to_string(), payload.clone()));
            while q.len() > KEEP {
                q.pop_front();
            }
        }
        let _ = app.emit(name, payload);
    }

    /// 取 after 之后的事件；调用方拿返回里的 seq 作为下次游标
    pub fn since(after: u64) -> serde_json::Value {
        let cur = SEQ.load(Ordering::SeqCst).saturating_sub(1);
        let mut evs: Vec<serde_json::Value> = Vec::new();
        if let Ok(q) = Q.lock() {
            for (s, n, p) in q.iter() {
                if *s > after {
                    evs.push(serde_json::json!({ "seq": s, "name": n, "payload": p }));
                }
            }
        }
        serde_json::json!({ "seq": cur, "events": evs })
    }
}

/// 下载目录（Windows `%USERPROFILE%\Downloads`；其余 `~/Downloads`）。
/// 2026-10-23 S2：抽成函数 —— download_file 落盘与 reveal_file 的目录校验必须用同一份定义，
/// 分两处写字面量迟早会漂移（改一处忘一处，校验判错直接变「任意路径可定位」）。
fn downloads_dir() -> std::path::PathBuf {
    if cfg!(windows) {
        std::path::PathBuf::from(format!(
            "{}\\Downloads",
            std::env::var("USERPROFILE").unwrap_or_default()
        ))
    } else {
        std::path::PathBuf::from(format!(
            "{}/Downloads",
            std::env::var("HOME").unwrap_or_default()
        ))
    }
}

// ==================== 自动落盘文件保留期（清理 下载/downfiles）· 2026-11-13 ====================
// 为什么需要：聊天文件「收即落盘」到 `下载/downfiles`，只增不减；图片/视频攒一个月就是几个 GB。
// 口径（老8 定）：默认保留 10 天、可改、最少 1 天，客户端自动清理（启动 3 分钟后首跑，之后每 6 小时）。
// 范围硬约束：只清 `下载/downfiles` 的**直接子文件** —— 不递归、不删目录，绝不碰下载目录根
//（那是用户自己的下载，删了就是事故）。
const RETENTION_DEFAULT_DAYS: u32 = 10;
const RETENTION_MIN_DAYS: u32 = 1;
const RETENTION_MAX_DAYS: u32 = 365;

/// 用户级配置目录：升级/换版都不受影响（Windows %LOCALAPPDATA%\BangXin）。
fn bangxin_user_dir() -> std::path::PathBuf {
    if cfg!(windows) {
        std::path::PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join("BangXin")
    } else if cfg!(target_os = "macos") {
        std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join("Library/Application Support/BangXin")
    } else {
        let base = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("{}/.config", std::env::var("HOME").unwrap_or_default()));
        std::path::PathBuf::from(base).join("BangXin")
    }
}

fn retention_cfg_path() -> std::path::PathBuf {
    bangxin_user_dir().join("file_retention.json")
}

fn clamp_days(d: u64) -> u32 {
    d.clamp(RETENTION_MIN_DAYS as u64, RETENTION_MAX_DAYS as u64) as u32
}

fn retention_days_load() -> u32 {
    let raw = std::fs::read_to_string(retention_cfg_path()).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    match v.get("days").and_then(|x| x.as_u64()) {
        Some(d) => clamp_days(d),
        None => RETENTION_DEFAULT_DAYS,
    }
}

fn retention_days_save(days: u32) -> u32 {
    let d = clamp_days(days as u64);
    let dir = bangxin_user_dir();
    let _ = std::fs::create_dir_all(&dir);
    let body = serde_json::json!({ "days": d }).to_string();
    let _ = std::fs::write(retention_cfg_path(), body);
    d
}

/// 自动落盘目录：`<下载目录>/downfiles`（前端收即落盘传的 subdir，见 save_target_dir）。
fn auto_saved_dir() -> std::path::PathBuf {
    downloads_dir().join("downfiles")
}

/// 扫描 downfiles：`purge_days` 有值时顺带删除超期文件。
/// 返回 (剩余文件数, 剩余字节, 本次删除数)。任何一步失败都静默跳过，绝不因清理失败影响启动。
fn scan_auto_saved(purge_days: Option<u32>) -> (usize, u64, usize) {
    let dir = auto_saved_dir();
    let cutoff = purge_days.and_then(|d| {
        std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(
            clamp_days(d as u64) as u64 * 86_400,
        ))
    });
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return (0, 0, 0);
    };
    let (mut count, mut bytes, mut removed) = (0usize, 0u64, 0usize);
    for e in rd.flatten() {
        let p = e.path();
        let Ok(md) = e.metadata() else { continue };
        if !md.is_file() {
            continue; // 子目录一律不碰（用户可能自己往里放东西）
        }
        count += 1;
        bytes += md.len();
        let Some(cut) = cutoff else { continue };
        if !in_downloads(&p) {
            continue;
        }
        let mt = md.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if mt < cut && std::fs::remove_file(&p).is_ok() {
            removed += 1;
            count -= 1;
            bytes = bytes.saturating_sub(md.len());
        }
    }
    (count, bytes, removed)
}

fn retention_state_json(extra: serde_json::Value) -> serde_json::Value {
    let (count, bytes, _) = scan_auto_saved(None);
    let mut v = serde_json::json!({
        "days": retention_days_load(),
        "min": RETENTION_MIN_DAYS,
        "max": RETENTION_MAX_DAYS,
        "count": count,
        "bytes": bytes,
        "dir": auto_saved_dir().to_string_lossy(),
    });
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        for (k, val) in e {
            o.insert(k.clone(), val.clone());
        }
    }
    v
}

#[tauri::command]
fn file_retention_get() -> Result<serde_json::Value, String> {
    Ok(retention_state_json(serde_json::json!({})))
}

#[tauri::command]
fn file_retention_set(days: u32) -> Result<serde_json::Value, String> {
    let d = retention_days_save(days);
    let (_, _, removed) = scan_auto_saved(Some(d));
    si_log(&format!("[retention] set {d}d, removed {removed}"));
    Ok(retention_state_json(serde_json::json!({ "removed": removed })))
}

#[tauri::command]
fn file_retention_clean(days: Option<u32>) -> Result<serde_json::Value, String> {
    let d = match days {
        Some(x) => retention_days_save(x),
        None => retention_days_load(),
    };
    let (_, _, removed) = scan_auto_saved(Some(d));
    si_log(&format!("[retention] manual clean keep {d}d, removed {removed}"));
    Ok(retention_state_json(serde_json::json!({ "removed": removed })))
}

// ── 开机自启动（2026-11-14 老8）────────────────────────────────────────────
// 默认开启：首次运行（含从老版本升上来、配置还没有的那一次）自动写系统自启动项；
// 个人资料 →「启动 → 开机自启动」可关。三端统一，均不需要管理员权限：
//   Windows  HKCU\Software\Microsoft\Windows\CurrentVersion\Run（值名 BangXin）
//   macOS    ~/Library/LaunchAgents/com.x8fde.im.autostart.plist（RunAtLoad）
//   Linux    ~/.config/autostart/bangxin.desktop
// 配置只记「用户意图」，是否真的开了以系统里的实际项为准（用户可能在任务管理器里禁用）
// ——故每次启动做一次对齐：意图开但项缺失/指向旧 exe → 补写；意图关但项还在 → 删掉。
const AUTOSTART_REG_VALUE: &str = "BangXin";
const AUTOSTART_MAC_LABEL: &str = "com.x8fde.im.autostart";

fn autostart_cfg_path() -> std::path::PathBuf {
    bangxin_user_dir().join("autostart.json")
}

/// None = 用户从未表过态（首次安装 / 从没有本功能的老版本升上来）→ 按默认「开」处理。
fn autostart_intent_load() -> Option<bool> {
    let raw = std::fs::read_to_string(autostart_cfg_path()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("enabled").and_then(|x| x.as_bool())
}

fn autostart_intent_save(on: bool) {
    let dir = bangxin_user_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(
        autostart_cfg_path(),
        serde_json::json!({ "enabled": on }).to_string(),
    );
}

/// 自启动要拉起的可执行文件。
/// Linux 以 AppImage 运行时 current_exe() 指向临时挂载点（/tmp/.mount_xxx/…，退出即消失），
/// 必须取 $APPIMAGE 指向的原始文件。
fn autostart_target_exe() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(p) = std::env::var("APPIMAGE") {
            if !p.trim().is_empty() {
                return Some(std::path::PathBuf::from(p));
            }
        }
    }
    std::env::current_exe().ok()
}

#[cfg(windows)]
mod autostart_sys {
    // 刻意不引入 winreg / windows-sys 的 Registry feature：实测 winreg 0.56 会拉进
    // windows-sys 0.61 全量 feature 重编（opt-level=z + codegen-units=1 下一次要 40 分钟以上），
    // 为一个 Run 键读写付这个编译代价不值。这里直接声明 advapi32 的四个导出，零新增依赖。
    use super::{autostart_target_exe, AUTOSTART_REG_VALUE};
    use std::os::windows::ffi::OsStrExt;

    type Hkey = isize;
    const HKEY_CURRENT_USER: Hkey = 0x8000_0001u32 as i32 as isize;
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const KEY_QUERY_VALUE: u32 = 0x0001;
    const KEY_SET_VALUE: u32 = 0x0002;
    const REG_SZ: u32 = 1;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegCreateKeyExW(
            hkey: Hkey,
            lpsubkey: *const u16,
            reserved: u32,
            lpclass: *const u16,
            dwoptions: u32,
            samdesired: u32,
            lpsecurityattributes: *const core::ffi::c_void,
            phkresult: *mut Hkey,
            lpdwdisposition: *mut u32,
        ) -> i32;
        fn RegOpenKeyExW(
            hkey: Hkey,
            lpsubkey: *const u16,
            uloptions: u32,
            samdesired: u32,
            phkresult: *mut Hkey,
        ) -> i32;
        fn RegSetValueExW(
            hkey: Hkey,
            lpvaluename: *const u16,
            reserved: u32,
            dwtype: u32,
            lpdata: *const u8,
            cbdata: u32,
        ) -> i32;
        fn RegQueryValueExW(
            hkey: Hkey,
            lpvaluename: *const u16,
            lpreserved: *const u32,
            lptype: *mut u32,
            lpdata: *mut u8,
            lpcbdata: *mut u32,
        ) -> i32;
        fn RegDeleteValueW(hkey: Hkey, lpvaluename: *const u16) -> i32;
        fn RegCloseKey(hkey: Hkey) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    fn read() -> Option<String> {
        unsafe {
            let mut hk: Hkey = 0;
            if RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, KEY_QUERY_VALUE, &mut hk) != 0 {
                return None;
            }
            let name = wide(AUTOSTART_REG_VALUE);
            let mut ty: u32 = 0;
            let mut cb: u32 = 0;
            let rc = RegQueryValueExW(hk, name.as_ptr(), std::ptr::null(), &mut ty, std::ptr::null_mut(), &mut cb);
            if rc != 0 || cb == 0 {
                RegCloseKey(hk);
                return None;
            }
            let mut buf = vec![0u8; cb as usize];
            let rc = RegQueryValueExW(hk, name.as_ptr(), std::ptr::null(), &mut ty, buf.as_mut_ptr(), &mut cb);
            RegCloseKey(hk);
            if rc != 0 {
                return None;
            }
            let units = (cb as usize) / 2;
            let w = std::slice::from_raw_parts(buf.as_ptr() as *const u16, units);
            let s = String::from_utf16_lossy(w);
            Some(s.trim_end_matches('\0').to_string())
        }
    }

    pub fn write(exe: &std::path::Path) -> Result<(), String> {
        unsafe {
            let mut hk: Hkey = 0;
            let rc = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                wide(RUN_KEY).as_ptr(),
                0,
                std::ptr::null(),
                0,
                KEY_SET_VALUE,
                std::ptr::null(),
                &mut hk,
                std::ptr::null_mut(),
            );
            if rc != 0 {
                return Err(format!("打开注册表失败（{rc}）"));
            }
            // 带空格的路径必须整串加引号，否则登录时会被系统拆成「程序 + 参数」
            let cmd = wide(&format!("\"{}\"", exe.to_string_lossy()));
            let rc = RegSetValueExW(
                hk,
                wide(AUTOSTART_REG_VALUE).as_ptr(),
                0,
                REG_SZ,
                cmd.as_ptr() as *const u8,
                (cmd.len() * 2) as u32,
            );
            RegCloseKey(hk);
            if rc != 0 {
                return Err(format!("写入自启动项失败（{rc}）"));
            }
            Ok(())
        }
    }

    pub fn remove() -> Result<(), String> {
        unsafe {
            let mut hk: Hkey = 0;
            if RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, KEY_SET_VALUE, &mut hk) != 0 {
                return Ok(()); // 根键都没有 = 本来就没项
            }
            let rc = RegDeleteValueW(hk, wide(AUTOSTART_REG_VALUE).as_ptr());
            RegCloseKey(hk);
            // 2 = ERROR_FILE_NOT_FOUND，本来就没有，视作成功
            if rc != 0 && rc != 2 {
                return Err(format!("删除自启动项失败（{rc}）"));
            }
            Ok(())
        }
    }

    /// 真正启用且指向当前 exe（升级换过安装目录时旧路径要重写）
    pub fn is_on() -> bool {
        let want = autostart_target_exe().map(|p| format!("\"{}\"", p.to_string_lossy()));
        match (read(), want) {
            (Some(cur), Some(w)) => cur.trim().eq_ignore_ascii_case(w.trim()),
            _ => false,
        }
    }
}

#[cfg(target_os = "macos")]
mod autostart_sys {
    use super::AUTOSTART_MAC_LABEL;

    fn plist_path() -> std::path::PathBuf {
        std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join("Library/LaunchAgents")
            .join(format!("{AUTOSTART_MAC_LABEL}.plist"))
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }

    pub fn write(exe: &std::path::Path) -> Result<(), String> {
        let p = plist_path();
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string></array>
  <key>RunAtLoad</key><true/>
</dict>
</plist>
"#,
            label = AUTOSTART_MAC_LABEL,
            exe = xml_escape(&exe.to_string_lossy())
        );
        // 登录时系统自己会读 LaunchAgents，这里不需要 launchctl 立即加载
        std::fs::write(&p, body).map_err(|e| e.to_string())
    }

    pub fn remove() -> Result<(), String> {
        match std::fs::remove_file(plist_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn is_on() -> bool {
        plist_path().exists()
    }
}

#[cfg(target_os = "linux")]
mod autostart_sys {
    fn desktop_path() -> std::path::PathBuf {
        let base = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("{}/.config", std::env::var("HOME").unwrap_or_default()));
        std::path::PathBuf::from(base).join("autostart").join("bangxin.desktop")
    }

    pub fn write(exe: &std::path::Path) -> Result<(), String> {
        let p = desktop_path();
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let body = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=邦信\n\
             Comment=邦信 X8.IM 开机自启动\n\
             Exec=\"{}\"\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n\
             Hidden=false\n",
            exe.to_string_lossy()
        );
        std::fs::write(&p, body).map_err(|e| e.to_string())
    }

    pub fn remove() -> Result<(), String> {
        match std::fs::remove_file(desktop_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn is_on() -> bool {
        // 以文件内容为准更准，但存在性已足以表达「登录时会被拉起」
        desktop_path().exists()
    }
}

fn autostart_apply(on: bool) -> Result<(), String> {
    if on {
        let exe = autostart_target_exe().ok_or_else(|| "取不到程序路径".to_string())?;
        autostart_sys::write(&exe)
    } else {
        autostart_sys::remove()
    }
}

/// 启动时对齐一次：默认开 + 自愈（升级换目录后重写指向当前 exe）。
/// 失败只记日志，绝不阻断启动。
fn autostart_boot_sync() {
    let want = match autostart_intent_load() {
        Some(v) => v,
        None => {
            autostart_intent_save(true); // 首次：默认开启
            true
        }
    };
    if let Err(e) = autostart_apply(want) {
        si_log(&format!("[autostart] sync failed want={want}: {e}"));
    }
}

#[tauri::command]
fn autostart_get() -> Result<serde_json::Value, String> {
    Ok(serde_json::json!({
        "enabled": autostart_sys::is_on(),
        "intent": autostart_intent_load(),
        "exe": autostart_target_exe().map(|p| p.to_string_lossy().to_string()),
        "platform": std::env::consts::OS,
    }))
}

#[tauri::command]
fn autostart_set(on: bool) -> Result<serde_json::Value, String> {
    autostart_apply(on)?;
    autostart_intent_save(on);
    si_log(&format!("[autostart] set {on}"));
    autostart_get()
}

/// 后台清理线程：启动 3 分钟后首跑（避开启动高峰），此后每 6 小时一次。
/// 三端共用（downfiles 位置在 mac/linux 同样成立）。
fn spawn_retention_cleaner() {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(180));
        loop {
            let d = retention_days_load();
            let (_, _, removed) = scan_auto_saved(Some(d));
            if removed > 0 {
                si_log(&format!("[retention] auto clean keep {d}d, removed {removed}"));
            }
            std::thread::sleep(std::time::Duration::from_secs(6 * 3600));
        }
    });
}

/// WX 组件的自主更新（2026-11-16）
/// 为什么要放在后端：IPC 桥不可靠时，前端「点页签 → 探测 → 准备组件」这条链会整条断掉，
/// 组件就永远装不上、也永不拉起。更新决策本来也不需要用户参与——版本号比对是纯机械判断。
/// 所以后端自己来：启动 60s 后查一次（避开启动高峰），之后每 30 分钟一轮；
/// 有新版就走 wx_updater::prepare 的既有链路（拉清单 → 下载 → 验签 → 落盘 → 拉起），
/// 断网/验签失败保留旧版，绝不阻断页面。
fn spawn_wx_autoupdate(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(60));
        loop {
            // 只对本机「装过组件」的机器做后台更新（available = 磁盘上有组件本体）。
            // 从没用过 W消息 的人没有组件 → 这里什么都不做，不会被凭空拉起一个
            // 读微信库的进程；用过的人则保持组件常新、点页签即就绪。
            if crate::wx_sidecar::available() {
                match crate::wx_updater::prepare(&app, false) {
                    Ok(v) => {
                        let ver = v
                            .get("version")
                            .and_then(|x| x.as_str())
                            .unwrap_or("?")
                            .to_string();
                        crate::ipc_probe::mark(format!("autoupdate ok version={ver}"));
                    }
                    Err(e) => crate::ipc_probe::mark(format!("autoupdate err {e}")),
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(1800));
        }
    });
}

/// 文件名消毒（2026-10-23 S3）。只收敛「本机不合法 / 会出事」的部分，其余尽量保留原貌
/// （用户要靠文件名认文件，不做拼音化、不改扩展名大小写）。
///   · 路径分隔与 Windows 非法字符 → 去掉（否则可跑出下载目录，或建出子路径）///   · cmd 元字符（& % ^）→ 去掉：Windows 侧「打开」走 `cmd /C start "" <路径>`，
///     文件名含 `&` 会被拆成两条命令（文件名来自服务端，等同一条注入路径），`%VAR%` 会被展开
///   · 控制字符 → 去掉（UI/日志/explorer 都会错乱）
///   · 结尾的点与空格 → 去掉（Windows 静默吞掉，落盘名与预期不一致，reveal 找不到）
///   · 超长 → 截断主干、保留扩展名（单段文件名受 255 字节限制）
///   · CON/PRN/AUX/NUL/COM1-9/LPT1-9 → 前置下划线（Windows 保留设备名，创建即失败）
fn sanitize_filename(name: &str) -> String {
    let mut s: String = name
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .filter(|c| !matches!(c, '&' | '%' | '^'))
        .filter(|c| !c.is_control())
        .collect();
    s = s.trim().trim_end_matches('.').trim_end().to_string();
    // 长度上限按字符数算（中文文件名按 1 处理，留足余量）
    const MAX_CHARS: usize = 150;
    if s.chars().count() > MAX_CHARS {
        let ext = std::path::Path::new(&s)
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default();
        let keep = if ext.is_empty() {
            MAX_CHARS
        } else {
            MAX_CHARS.saturating_sub(ext.chars().count() + 1)
        };
        let stem: String = s.chars().take(keep).collect();
        let stem = stem.trim_end_matches('.').trim_end().to_string();
        s = if ext.is_empty() { stem } else { format!("{}.{}", stem, ext) };
    }
    if s.is_empty() || s == "." || s == ".." {
        return "file".to_string();
    }
    let stem_upper = std::path::Path::new(&s)
        .file_stem()
        .map(|x| x.to_string_lossy().to_uppercase())
        .unwrap_or_default();
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&stem_upper.as_str()) {
        s = format!("_{}", s);
    }
    s
}

/// 在目录里找「这个名字」的文件，容忍重名递增后缀。
/// 2026-11-06：downfiles 里同名文件会被存成 `a (1).zip`、`a (3).zip`，而消息里只有 `a.zip`。
/// 旧实现按名字精确匹配 → 明明已经落过盘也判成「没有」→ 每点一次「打开」又存一份新副本
/// （老8 反馈「每次点击打开就多保存一份」的机制）。这里按「基名 + 扩展名」匹配，
/// 多个候选取修改时间最新的那份。
fn resolve_saved_file(dir: &std::path::Path, want: &str) -> Option<std::path::PathBuf> {
    let exact = dir.join(want);
    if exact.is_file() {
        return Some(exact);
    }
    let split = |s: &str| -> (String, String) {
        let p = std::path::Path::new(s);
        let stem = p
            .file_stem()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_default();
        let ext = p
            .extension()
            .map(|x| x.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        (stem, ext)
    };
    // 去掉结尾的 " (n)"（Windows 重名递增格式）
    let strip_counter = |s: &str| -> String {
        let t = s.trim_end();
        if !t.ends_with(')') {
            return t.to_string();
        }
        if let Some(i) = t.rfind(" (") {
            let mid = &t[i + 2..t.len() - 1];
            if !mid.is_empty() && mid.chars().all(|c| c.is_ascii_digit()) {
                return t[..i].to_string();
            }
        }
        t.to_string()
    };
    let (want_stem, want_ext) = split(want);
    let want_base = strip_counter(&want_stem);
    let rd = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        let fname = match p.file_name() {
            Some(n) => n.to_string_lossy().to_string(),
            None => continue,
        };
        let (stem, ext) = split(&fname);
        if ext != want_ext {
            continue;
        }
        if strip_counter(&stem) != want_base {
            continue;
        }
        let mt = e
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if best.as_ref().map(|(t, _)| mt > *t).unwrap_or(true) {
            best = Some((mt, p));
        }
    }
    best.map(|(_, p)| p)
}

/// 在系统文件管理器里「选中」某个文件（注意：是选中，不是打开它）。
/// 2026-11-06：/reveal 的两条分支（downfiles 里的副本 / 发送方原始路径）共用同一份实现，
/// 免得 Windows 的 `explorer /select,` 参数拼法在多处漂移。
fn reveal_in_folder(p: &std::path::Path) {
    #[cfg(windows)]
    {
        // 路径单独作为参数传：合并成一个字符串时，含空格的路径会被整体加引号，
        // explorer 解析不到 /select 开关（2026-10-22 踩过）
        let _ = Command::new("explorer")
            .arg("/select,")
            .arg(p.to_string_lossy().replace('/', "\\"))
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let ps = p.to_string_lossy().to_string();
        if let Ok(c) = Command::new("open").args(["-R", &ps]).spawn() {
            reap_in_background(c);
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        if let Some(parent) = p.parent() {
            if let Ok(c) = Command::new("xdg-open").arg(parent).spawn() {
                reap_in_background(c);
            }
        }
    }
}

/// 落盘目标目录解析（save_bytes 与分块落盘三个命令共用）。
/// 2026-11-05 收即落盘：subdir 非空时落到「下载目录/子目录」（如 downfiles），
/// 否则仍落下载目录根（向后兼容中转文件「打开/下载」旧调用，不传 subdir）。
/// 子目录名做防穿越：剔除路径分隔符、拒绝 "." / ".." 整段，join 必在 downloads 之下。
fn save_target_dir(subdir: Option<&str>) -> std::path::PathBuf {
    let downloads = downloads_dir();
    match subdir {
        Some(sd) => {
            let cleaned: String = sd.chars().filter(|c| !matches!(c, '/' | '\\')).collect();
            if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
                downloads
            } else {
                downloads.join(cleaned)
            }
        }
        None => downloads,
    }
}

/// 分块落盘只认「下载目录之下」的路径，防止前端拿任意 token 写到别处。
fn in_downloads(p: &std::path::Path) -> bool {
    let base = downloads_dir();
    match (p.canonicalize(), base.canonicalize()) {
        (Ok(a), Ok(b)) => a.starts_with(&b),
        _ => p.starts_with(&base),
    }
}

/// P2P 直传收到的文件落盘（2026-10-23）。
///
/// 为什么需要它：WebView 里 `<a download>` 不生效，网页端能靠浏览器保存，桌面端只能走命令。
/// 为什么用 base64 而不是 Vec<u8>：Tauri 把 Vec<u8> 当普通数组逐元素序列化，几十 MB 会卡死主线程；
/// 字符串一次过桥虽然多占 1/3 体积，但可控且不会卡死。
/// 落盘走「.part → rename」原子写，与 download_file 同款，目录里不会出现半截文件。
/// 🔴 2026-11-06 实测上限：单次 base64 约 48MB（文件）以后，invoke 在 WebView2 侧永不返回
/// （Rust 端连下载目录都不会创建）——大文件必须改走 save_chunk_* 分块链路。
#[tauri::command]
async fn save_bytes(name: String, b64: String, subdir: Option<String>) -> Result<String, String> {
    let dir = save_target_dir(subdir.as_deref());
    // 2026-10-24 P2-RS-1：base64 解码 + 写盘都是阻塞 CPU/IO，跑在 async 体里会占住 tokio worker
    //（几十 MB 解码可达数百 ms）——整段移入 spawn_blocking，与 download_file 一致。
    let result = tauri::async_runtime::spawn_blocking(move || {
    let bytes = b64_decode(&b64)?;
    if bytes.is_empty() {
        return Err("文件内容为空".into());
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建下载目录：{}", e))?;
    let safe = sanitize_filename(&name);
    let dest = dir.join(&safe);
    let tmp = dir.join(format!("{}.{}.part", safe, std::process::id()));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("写盘失败：{}", e))?;
    #[cfg(windows)]
    {
        if dest.exists() {
            let _ = std::fs::remove_file(&dest);
        }
    }
    if let Err(e) = std::fs::rename(&tmp, &dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("落盘失败：{}", e));
    }
    Ok(dest.to_string_lossy().to_string())
    })
    .await
    .map_err(|e| format!("落盘线程异常：{}", e))?;
    result
}

/// 分块落盘 · 开始（2026-11-06）。
/// 为什么要有：单次 `save_bytes` 把整个文件编成 base64 过 IPC，152.9MB 文件 = 204MB 字符串，
/// WebView2 侧 `postMessage` 直接把消息丢掉（Rust 端连目录都没建），前端表现为永久「正在写盘…」。
/// 改为前端按 4MB 分块多次调用 append，每次过桥仅 5.4MB，彻底绕开这个上限。
/// 返回临时文件路径作 token（后续 append/end 都要带回来，且必须仍在下载目录内）。
#[tauri::command]
async fn save_chunk_begin(name: String, subdir: Option<String>) -> Result<String, String> {
    let dir = save_target_dir(subdir.as_deref());
    let safe = sanitize_filename(&name);
    let tmp = dir.join(format!("{}.{}.part", safe, std::process::id()));
    tauri::async_runtime::spawn_blocking(move || {
        std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建下载目录：{}", e))?;
        // 清掉上次中断留下的同名残片，避免新内容接在旧字节后面
        let _ = std::fs::remove_file(&tmp);
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|e| format!("无法创建临时文件：{}", e))?;
        Ok(tmp.to_string_lossy().to_string())
    })
    .await
    .map_err(|e| format!("建盘线程异常：{}", e))?
}

/// 分块落盘 · 追加一片。返回已写入的总字节数，前端据此显示进度。
#[tauri::command]
async fn save_chunk_append(token: String, b64: String) -> Result<u64, String> {
    let tmp = std::path::PathBuf::from(&token);
    if !in_downloads(&tmp) {
        return Err("非法写入路径".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        use std::io::Write;
        let bytes = b64_decode(&b64)?;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&tmp)
            .map_err(|e| format!("打开临时文件失败：{}", e))?;
        f.write_all(&bytes).map_err(|e| format!("写入失败：{}", e))?;
        f.flush().map_err(|e| format!("刷新失败：{}", e))?;
        Ok(std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0))
    })
    .await
    .map_err(|e| format!("写盘线程异常：{}", e))?
}

/// 分块落盘 · 收尾：临时文件 rename 成正式文件名，返回落盘路径（供「在文件夹中显示」）。
#[tauri::command]
async fn save_chunk_end(token: String, name: String) -> Result<String, String> {
    let tmp = std::path::PathBuf::from(&token);
    if !in_downloads(&tmp) {
        return Err("非法写入路径".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let dir = tmp.parent().map(|p| p.to_path_buf()).unwrap_or_else(downloads_dir);
        let dest = dir.join(sanitize_filename(&name));
        let size = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
        if size == 0 {
            let _ = std::fs::remove_file(&tmp);
            return Err("文件内容为空".into());
        }
        #[cfg(windows)]
        {
            if dest.exists() {
                let _ = std::fs::remove_file(&dest);
            }
        }
        if let Err(e) = std::fs::rename(&tmp, &dest) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("落盘失败：{}", e));
        }
        Ok(dest.to_string_lossy().to_string())
    })
    .await
    .map_err(|e| format!("落盘线程异常：{}", e))?
}

/// 极简 base64 解码（标准字母表）。不引外部 crate，避免为一个小功能动依赖树。
fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    // 2026-10-24 P2-RS-6：解码后字节数上限 512 MiB。base64 4:3，len/4*3 为解码上界，
    // 先拒绝超限入参，防止下方 Vec::with_capacity(len/4*3) 被任意大输入撑爆内存。
    const MAX_DECODED: usize = 512 * 1024 * 1024;
    if s.len() / 4 * 3 > MAX_DECODED {
        return Err("文件过大，单文件上限 512MB".into());
    }
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut map = [255u8; 256];
    for (i, &c) in TBL.iter().enumerate() {
        map[c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for b in s.bytes() {
        if b == b'=' || b == b'\n' || b == b'\r' || b == b' ' {
            continue;
        }
        let v = map[b as usize];
        if v == 255 {
            return Err("文件内容解码失败".into());
        }
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

// ── 截屏（2026-10-23 老8 定）────────────────────────────────────────────────
// 不引任何截图库（安装包零增量）：直接唤 Windows 自带截图工具（ms-screenclip:，就是
// Win+Shift+S 那个，选区/标注/马赛克都是系统现成的），截完图落在剪贴板，这里读出来
// 转 PNG base64 回传给前端，进「待发送」区。用户感受：点截屏 → 拖框 → 图已挂在输入框下。
// 剪贴板优先取 "PNG" 注册格式（截图工具直接给 PNG 字节，免解码）；退化到 CF_DIBV5 / CF_DIB 手转。
#[tauri::command]
async fn screen_clip_grab(timeout_ms: Option<u64>) -> Result<String, String> {
    #[cfg(windows)]
    {
        let ms = timeout_ms.unwrap_or(20000).clamp(3000, 60000);
        return tauri::async_runtime::spawn_blocking(move || win_screen_clip(ms))
            .await
            .map_err(|e| format!("截屏线程异常：{}", e))?;
    }
    #[cfg(not(windows))]
    {
        let _ = timeout_ms;
        Err("当前平台暂不支持客户端截屏，请用系统截图后直接粘贴".into())
    }
}

#[cfg(windows)]
fn win_screen_clip(timeout_ms: u64) -> Result<String, String> {
    use std::time::{Duration, Instant};
    use windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let seq0 = unsafe { GetClipboardSequenceNumber() };
    let (op, file) = (wide("open"), wide("ms-screenclip:"));
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL as i32,
        )
    };
    if rc as isize <= 32 {
        return Err(format!("唤不起系统截图工具（ShellExecute={}）", rc as isize));
    }

    // 轮询剪贴板：截图工具是异步的，用户拖完框才会写剪贴板；序列号变化才去读，避免抢读半截
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut last_err = String::new();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(220));
        if unsafe { GetClipboardSequenceNumber() } == seq0 {
            continue;
        }
        match read_clip_image() {
            Ok(b64) => return Ok(b64),
            Err(e) => last_err = e,   // 用户复制的可能不是图（或截图还没写完），继续等
        }
    }
    Err(if last_err.is_empty() {
        "没有截到图（可能被取消了）。也可以 Win+Shift+S 截图后直接 Ctrl+V 粘贴".into()
    } else {
        format!("没有截到图（{}）。也可以 Win+Shift+S 截图后直接粘贴", last_err)
    })
}

/// 读剪贴板里的图片 → PNG base64
#[cfg(windows)]
fn read_clip_image() -> Result<String, String> {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
        RegisterClipboardFormatW,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    // CF_* 常量在 windows-sys 里挂在 Ole 模块下（CLIPBOARD_FORMAT = u16）
    use windows_sys::Win32::System::Ole::{CF_DIB, CF_DIBV5};

    fn clip_bytes(fmt: u32) -> Option<Vec<u8>> {
        unsafe {
            let h = GetClipboardData(fmt);
            if h.is_null() {
                return None;
            }
            let p = GlobalLock(h);
            if p.is_null() {
                return None;
            }
            let size = GlobalSize(h);
            let mut v = vec![0u8; size];
            std::ptr::copy_nonoverlapping(p as *const u8, v.as_mut_ptr(), size);
            GlobalUnlock(h);
            Some(v)
        }
    }

    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("剪贴板被其他程序占用".into());
        }
        let res = (|| -> Result<String, String> {
            let name: Vec<u16> = "PNG".encode_utf16().chain(std::iter::once(0)).collect();
            let fmt_png = RegisterClipboardFormatW(name.as_ptr());
            if fmt_png != 0 && IsClipboardFormatAvailable(fmt_png) != 0 {
                if let Some(mut bytes) = clip_bytes(fmt_png) {
                    // GlobalSize 给的是内存块大小，可能带尾部垃圾；按 IEND 截断成合法 PNG
                    if let Some(pos) = bytes.windows(4).rposition(|w| w == b"IEND") {
                        if pos + 8 <= bytes.len() {
                            bytes.truncate(pos + 8);
                        }
                    }
                    if !bytes.is_empty() {
                        return Ok(b64_encode(&bytes));
                    }
                }
            }
            for fmt in [CF_DIBV5, CF_DIB] {
                let fmt = fmt as u32;
                if IsClipboardFormatAvailable(fmt) != 0 {
                    if let Some(dib) = clip_bytes(fmt) {
                        if let Ok(png) = dib_to_png(&dib) {
                            return Ok(b64_encode(&png));
                        }
                    }
                }
            }
            Err("剪贴板里没有图片".into())
        })();
        CloseClipboard();
        res
    }
}

/// CF_DIB(D) 裸数据（无 BMP 文件头）→ PNG 字节。截图工具给的都是 BI_RGB 的 24/32 位。
#[cfg(windows)]
fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>, String> {
    if dib.len() < 40 {
        return Err("DIB 数据不完整".into());
    }
    let u32at = |o: usize| u32::from_le_bytes([dib[o], dib[o + 1], dib[o + 2], dib[o + 3]]);
    let hdr = u32at(0) as usize;
    if hdr < 40 || dib.len() < hdr {
        return Err("DIB 头异常".into());
    }
    let w = i32::from_le_bytes([dib[4], dib[5], dib[6], dib[7]]);
    let h_raw = i32::from_le_bytes([dib[8], dib[9], dib[10], dib[11]]);
    let bpp = u16::from_le_bytes([dib[14], dib[15]]) as usize;
    let comp = u32at(16);
    if comp != 0 {
        return Err(format!("不支持的 DIB 压缩格式 {}（只支持 BI_RGB）", comp));
    }
    if w <= 0 || h_raw == 0 {
        return Err("DIB 尺寸异常".into());
    }
    if bpp != 32 && bpp != 24 {
        return Err(format!("不支持的位深 {}（只支持 24/32）", bpp));
    }
    let top_down = h_raw < 0;
    let (wu, hu) = (w as u32, h_raw.unsigned_abs());
    let stride = ((w as usize * bpp + 31) / 32) * 4;
    if dib.len() < hdr + stride * hu as usize {
        return Err("DIB 像素数据被截断".into());
    }
    let mut rgba = vec![0u8; wu as usize * hu as usize * 4];
    for y in 0..hu as usize {
        let src_y = if top_down { y } else { hu as usize - 1 - y };
        let row = hdr + src_y * stride;
        for x in 0..wu as usize {
            let p = row + x * (bpp / 8);
            let d = (y * wu as usize + x) * 4;
            rgba[d] = dib[p + 2]; // R
            rgba[d + 1] = dib[p + 1]; // G
            rgba[d + 2] = dib[p]; // B
            rgba[d + 3] = if bpp == 32 { dib[p + 3] } else { 255 };
        }
    }
    // 有些工具写 32 位 DIB 时 alpha 全 0 → 视为不透明，否则整张图透明看不见
    if bpp == 32 && rgba.chunks_exact(4).all(|px| px[3] == 0) {
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 255;
        }
    }
    let img = image::RgbaImage::from_raw(wu, hu, rgba).ok_or("构造图像失败")?;
    let mut out: Vec<u8> = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| format!("PNG 编码失败：{}", e))?;
    Ok(out)
}

/// 极简 base64 编码（标准字母表），与 b64_decode 配套，不引 base64 crate
#[allow(dead_code)]
fn b64_encode(data: &[u8]) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity((data.len() + 2) / 3 * 4);
    for c in data.chunks(3) {
        let n = ((c[0] as u32) << 16)
            | ((*c.get(1).unwrap_or(&0) as u32) << 8)
            | (*c.get(2).unwrap_or(&0) as u32);
        s.push(TBL[((n >> 18) & 63) as usize] as char);
        s.push(TBL[((n >> 12) & 63) as usize] as char);
        s.push(if c.len() > 1 { TBL[((n >> 6) & 63) as usize] as char } else { '=' });
        s.push(if c.len() > 2 { TBL[(n & 63) as usize] as char } else { '=' });
    }
    s
}

/// 桌面端保存聊天里的文件：WebView 里 `<a download>` 不生效（main.rs:1755 已踩坑），
/// 且私有文件需带 Bearer 鉴权（外部浏览器无会话会 401），故由本命令在客户端拉取并落盘到下载目录。
/// 2026-10-23 起用进程内 reqwest（不再 spawn 系统 curl）；S1 仍生效：只有 SAFE_OPEN_EXT
/// 内的查看型文件才自动打开，可执行/脚本类只落盘。
#[tauri::command]
async fn download_file(app: tauri::AppHandle, url: String, token: String, name: String, open: Option<bool>, key: Option<String>) -> Result<String, String> {
    // 2026-10-24 稳定性修复（AppHangB1）：原为同步命令，reqwest blocking 下载 + 写盘全跑在主线程，
    // 大文件/慢网时整窗冻结（表现为「没反应、关不掉、移不动」）。改 async + spawn_blocking：
    // 阻塞 IO 移出主线程，await 期间 UI 正常响应；.part → rename 原子落盘逻辑不变。
    // 2026-11-06：加 app/key -> 下载过程 emit('x8dl') 报进度（前端卡片显示接收 %）。
    let app2 = app.clone();
    let key2 = key.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
    // 只允许 https（及本机调试用的 http://localhost）——Bearer 令牌不能跟着任意明文地址发出去
    let local_http = url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost");
    if !url.starts_with("https://") && !local_http {
        return Err("无效地址".into());
    }
    let downloads = downloads_dir();
    if downloads.as_os_str().is_empty() {
        return Err("找不到下载目录".into());
    }
    std::fs::create_dir_all(&downloads)
        .map_err(|e| format!("文件下载失败（无法创建下载目录：{}）", e))?;
    // 文件名消毒后再拼路径：dest 一定落在 downloads 下，且用平台原生分隔符
    // （旧写法 `format!("{}/{}")` 在 Windows 上产生混合分隔符，explorer /select, 认不出来）
    let safe = sanitize_filename(&name);
    let dest = downloads.join(&safe);

    // 2026-10-23 重构：**弃用系统 curl，改为进程内 blocking HTTP 下载**。
    // 原因（老8 真机报错）：`curl: (6) getaddrinfo() thread failed to start`
    // —— Windows 版 curl 的异步 DNS 解析要另起线程，线程创建失败就直接解析不了域名，
    // 与鉴权/证书/网络无关，且与机器当时状态相关（时好时坏），外挂进程无法根治。
    // 附带收益：
    //   ① 不再 spawn 控制台程序 → 「下载弹黑窗」从根上消失（不再依赖 CREATE_NO_WINDOW）
    //   ② 令牌不再落到临时配置文件（原为绕开命令行泄令牌）→ 少一次临时文件读写
    //   ③ 失败原因可精确回传（HTTP 状态码 / 网络错误 / 写盘错误分开）
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(20))
        .timeout(std::time::Duration::from_secs(900))
        .build()
        .map_err(|e| format!("文件下载失败（初始化 HTTP：{}）", e))?;
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .map_err(|e| format!("文件下载失败（网络：{}）", e))?;
    if !resp.status().is_success() {
        return Err(format!("文件下载失败（HTTP {}）", resp.status()));
    }
    // 2026-10-23 O4：先写同目录的 <name>.<pid>.part，完整写完再 rename 到位。
    // 旧实现直接 File::create(dest)：下到一半失败（断网/超时/进程退出）会在下载目录留半截文件，
    // 用户双击就是坏文件；而且下次同名下载会从中间截断旧文件。rename 同分区是原子操作，
    // 目录里永远只会出现「完整」或「不存在」两种状态。
    let tmp = downloads.join(format!("{}.{}.part", safe, std::process::id()));
    let mut f = std::fs::File::create(&tmp)
        .map_err(|e| format!("文件下载失败（无法写入下载目录：{}）", e))?;
    // 2026-11-06：手写分块拷贝替代 std::io::copy —— 边下边 emit 进度，
    // 前端卡片才能显示「接收中 42%」。按 ~200ms 节流，避免 100MB 文件刷出上万条事件。
    let total: u64 = resp.content_length().unwrap_or(0);
    let mut got: u64 = 0;
    let copied: std::io::Result<()> = {
        use std::io::{Read, Write};
        let mut body = resp;
        let mut buf = vec![0u8; 256 * 1024];
        let mut last = std::time::Instant::now();
        (|| -> std::io::Result<()> {
            loop {
                let n = body.read(&mut buf)?;
                if n == 0 { break; }
                f.write_all(&buf[..n])?;
                got += n as u64;
                if last.elapsed().as_millis() >= 200 {
                    last = std::time::Instant::now();
                    // evq::push = emit + 入队（HTTP 轮询补投用），只发一次
                    evq::push(&app2, "x8dl", serde_json::json!({ "key": key2, "got": got, "total": total, "done": false }));
                }
            }
            Ok(())
        })()
    };
    let _ = std::io::Write::flush(&mut f);
    drop(f);
    if let Err(e) = copied {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("文件下载失败（写盘：{}）", e));
    }
    // Windows 的 rename 在目标已存在时会失败（覆盖不原子），先删旧文件；mac/Linux 直接覆盖
    #[cfg(windows)]
    {
        if dest.exists() {
            let _ = std::fs::remove_file(&dest);
        }
    }
    if let Err(e) = std::fs::rename(&tmp, &dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("文件下载失败（落盘：{}）", e));
    }
    if !dest.is_file() {
        return Err("文件下载失败（未生成文件）".into());
    }
    // 落盘完成：补一条终态事件，前端据此把卡片切成「已保存」并记住路径
    evq::push(&app2, "x8dl", serde_json::json!({
        "key": key2, "got": got, "total": total, "done": true,
        "path": dest.to_string_lossy()
    }));

    // S1 修复：仅查看型扩展名自动打开；其它（含 exe/bat/ps1/msi/…）只落盘不打开。
    // 对齐微信：默认行为 = 用户点击「打开」才打开；「在文件夹中显示」等传 open=false 只落盘不打开，
    // 避免被动自动打开（macOS 无关联程序时会弹英文「选择应用程序」框，造成中英不一致）。
    let should_open = open.unwrap_or(true);
    let ext = std::path::Path::new(&safe)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if should_open && SAFE_OPEN_EXT.contains(&ext.as_str()) {
        let _ = open_external(&dest.to_string_lossy());
    }
    Ok(dest.to_string_lossy().to_string())
    })
    .await
    .map_err(|e| format!("下载线程异常：{}", e))?;
    result
}

/// 在文件管理器中定位并选中文件（对齐微信「在文件夹中显示」）。
/// mac: `open -R`；Windows: `explorer /select,`；Linux: `xdg-open <dir>`。
/// 2026-11-06：改 async —— 同步命令跑在主线程，explorer 一旦卡住会把整个线程占住，
/// 后面所有 IPC 都「发了不回」。放后台线程里，最差也只是这一个动作失败。
#[tauri::command]
async fn reveal_file(path: String) -> Result<(), String> {
    let p = std::path::PathBuf::from(&path);
    if !p.is_file() {
        return Err("文件尚未下载，无法定位".into());
    }
    // 2026-10-23 S2：只允许定位「下载目录内」的文件。
    // 旧实现直接 explorer /select, <任意路径>：前端（或被注入的脚本）传什么就打开什么，
    // 等于把「定位文件」变成任意本地路径存在性探测器 + 文件管理器导航能力。
    // 两侧都 canonicalize：a) 吃掉 `..`/符号链接；b) Windows 统一成 `\\?\C:\...` 前缀，比较才可靠。
    let dir = downloads_dir();
    let canon_p = std::fs::canonicalize(&p).map_err(|e| format!("无法解析路径：{}", e))?;
    let canon_dir = std::fs::canonicalize(&dir).map_err(|e| format!("无法解析下载目录：{}", e))?;
    if !canon_p.starts_with(&canon_dir) {
        return Err("只允许在下载目录中定位文件".into());
    }
    let path = canon_p.to_string_lossy().to_string();

    #[cfg(target_os = "macos")]
    {
        if let Ok(child) = Command::new("open").args(["-R", &path]).spawn() {
            // 2026-10-24 P2-RS-5：后台收尸，防 Unix 僵尸进程（Windows 分支不动）
            reap_in_background(child);
        }
    }
    #[cfg(target_os = "windows")]
    {
        // 2026-10-22 修复两处（旧实现「在文件夹中显示」在 Windows 上打不开/不选中）：
        //   1) explorer /select, 只认反斜杠路径，混合分隔符（`Downloads/文件.md`）会定位失败；
        //   2) `/select,` 与路径必须分开传参：合成一个参数时路径含空格会被 Rust 整体加引号
        //      （`"/select,C:\a b\文件.md"`），explorer 解析不到开关，打开的是错误位置。
        let p = path.replace('/', "\\");
        // 2026-10-24：.status() 会阻塞等 explorer 退出（有时窗口开着就一直不退），
        // 改 .spawn() 只发起不等待——定位动作本身不需要等返回码。
        let _ = Command::new("explorer").arg("/select,").arg(&p).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let dir = std::path::Path::new(&path).parent().unwrap_or(std::path::Path::new("/"));
        if let Ok(child) = Command::new("xdg-open").arg(dir).spawn() {
            // 2026-10-24 P2-RS-5：同上，后台收尸
            reap_in_background(child);
        }
    }
    Ok(())
}

/// 在客户端内嵌新窗口打开邦 FDE 工作台（IM 左栏「工作台」）；
/// 窗口已存在则聚焦并导航到最新带票 URL；建窗失败回落系统浏览器（老8 2026-10-14 定）。
#[tauri::command]
async fn open_workbench(app: tauri::AppHandle, url: String) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("无效地址".into());
    }
    if let Some(w) = app.get_webview_window("workbench") {
        // 2026-10-14 老8：二次点击只把已开的工作台唤到前台，不重新导航（保住页内位置与登录态）
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        return Ok(());
    }
    let parsed = url.parse().map_err(|e| format!("地址解析失败: {e}"))?;
    match tauri::WebviewWindowBuilder::new(&app, "workbench", tauri::WebviewUrl::External(parsed))
        .title("FDE 工作台 — 关闭此窗口返回邦信")
        .inner_size(1280.0, 860.0)
        .min_inner_size(900.0, 600.0)
        .center()
        .build()
    {
        Ok(_) => Ok(()),
        Err(e) => open_external(&url)
            .map_err(|e2| format!("新建窗口失败({e})，浏览器回落失败({e2})")),
    }
}

/// 扩展面板：在独立内嵌窗口打开小程序/工具（2026-10-24 老8 定）。
/// 与 open_workbench 同款架构，差异：①窗口名单例 ext-<appId>（每个小程序各自独立、可同时开多个）；
/// ②二次点击聚焦并按调用方给定尺寸改大小（浮窗「↗ 转独立窗」带出当前尺寸）；③建窗失败回落系统浏览器。
/// 安全：appId 仅限字母数字（防窗口名注入/路径穿越），url 必须 https。
#[tauri::command]
async fn ext_open_window(app: tauri::AppHandle, app_id: String, url: String, w: f64, h: f64) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("无效地址".into());
    }
    if app_id.is_empty() || app_id.len() > 32 || !app_id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("非法 appId".into());
    }
    let label = format!("ext-{}", app_id);
    let (w, h) = (w.clamp(320.0, 3840.0), h.clamp(240.0, 2160.0));
    if let Some(win) = app.get_webview_window(&label) {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_size(tauri::Size::Logical(tauri::LogicalSize { width: w, height: h }));
        let _ = win.set_focus();
        return Ok(());
    }
    let parsed = url.parse().map_err(|e| format!("地址解析失败: {e}"))?;
    match tauri::WebviewWindowBuilder::new(&app, &label, tauri::WebviewUrl::External(parsed))
        .title("小程序 — 关闭此窗口返回邦信")
        .inner_size(w, h)
        .min_inner_size(320.0, 240.0)
        .center()
        .build()
    {
        Ok(_) => Ok(()),
        Err(e) => open_external(&url)
            .map_err(|e2| format!("新建窗口失败({e})，浏览器回落失败({e2})")),
    }
}

/// 关闭某个小程序的独立窗口（不存在则静默成功，供前端删除小程序时联动关窗）
#[tauri::command]
async fn ext_close_window(app: tauri::AppHandle, app_id: String) -> Result<(), String> {
    if app_id.is_empty() || app_id.len() > 32 || !app_id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("非法 appId".into());
    }
    if let Some(win) = app.get_webview_window(&format!("ext-{}", app_id)) {
        win.close().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 把某个小程序的独立窗口唤到前台（多窗场景下点面板按钮时用）
#[tauri::command]
async fn ext_focus_window(app: tauri::AppHandle, app_id: String) -> Result<(), String> {
    if app_id.is_empty() || app_id.len() > 32 || !app_id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("非法 appId".into());
    }
    if let Some(win) = app.get_webview_window(&format!("ext-{}", app_id)) {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
    Ok(())
}

/// 托盘图标（2026-10-23 老8：托盘要亮、要显眼，仿微信亮色底）。
/// 编译期嵌入 icons/tray.png —— 2026-10-23 起「托盘 = 应用图标」同一套视觉
/// （品牌橙圆角块 + 白色 X8），窗口/任务栏/快捷方式/mac/Store 全尺寸同源，
/// 见 icons/_gen_icons.py。一暗一亮以它为基准，压暗后明暗呼吸自然明显。
fn tray_icon_image() -> tauri::image::Image<'static> {
    // 2026-10-24 P2-RS-4：notify_unread / show_main / start_tray_flash 每次都调本函数，
    // 旧实现每次重解码 PNG 太浪费。用 OnceLock 只解码一次，缓存 RGBA 原始数据 + 尺寸
    // （tauri Image 非 Clone，不能缓存实例），调用时克隆像素重建轻量 Image——远比解码便宜。
    static CACHE: std::sync::OnceLock<(u32, u32, Vec<u8>)> = std::sync::OnceLock::new();
    let (w, h, rgba) = CACHE.get_or_init(|| {
        let img = image::load_from_memory(include_bytes!("../icons/tray.png"))
            .expect("内置 tray.png 解析失败")
            .to_rgba8();
        (img.width(), img.height(), img.into_raw())
    });
    tauri::image::Image::new_owned(rgba.clone(), *w, *h)
}

/// 压暗版图标（2026-10-22 老8：托盘要像微信那样「一暗一亮」闪）。
///
/// 只乘 RGB、**不动 alpha** —— 托盘图标带透明边角，缩放 alpha 会让边缘出现灰边，
/// 而 Windows 托盘背景可能是深色也可能是浅色，只有保持轮廓纯粹、只压亮度，
/// 明暗切换才不会出现「闪的时候形状在变」的错觉。
fn dim_icon(base: &tauri::image::Image<'_>, factor: f32) -> Option<tauri::image::Image<'static>> {
    let (w, h) = (base.width(), base.height());
    if w == 0 || h == 0 {
        return None;
    }
    let mut px = base.rgba().to_vec();
    let f = factor.clamp(0.0, 1.0);
    for i in (0..px.len()).step_by(4) {
        px[i] = (px[i] as f32 * f).round() as u8;
        px[i + 1] = (px[i + 1] as f32 * f).round() as u8;
        px[i + 2] = (px[i + 2] as f32 * f).round() as u8;
    }
    Some(tauri::image::Image::new_owned(px, w, h))
}

/// 找 Tauri 自己的托盘图标 (hwnd, uID)。
///
/// tray-icon crate 的托盘窗口类名固定是 `tray_icon_app`，uID 是进程内自增（第一个托盘 = 1）。
/// 按「窗口类名 + 当前进程 PID」枚举即可安全定位，不会误抓别的程序的托盘。
#[cfg(target_os = "windows")]
fn tray_icon_handle() -> Option<(*mut std::ffi::c_void, u32)> {
    use windows_sys::Win32::Foundation::BOOL;
    use windows_sys::Win32::UI::Shell::{Shell_NotifyIconW, NIF_STATE, NIM_MODIFY, NOTIFYICONDATAW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindowThreadProcessId,
    };

    struct Ctx {
        pid: u32,
        hwnd: *mut std::ffi::c_void,
    }
    unsafe extern "system" fn enum_proc(hwnd: *mut std::ffi::c_void, lparam: isize) -> BOOL {
        unsafe {
            let ctx = &mut *(lparam as *mut Ctx);
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == ctx.pid {
                let mut buf = [0u16; 128];
                let n = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
                if n > 0 && String::from_utf16_lossy(&buf[..n as usize]) == "tray_icon_app" {
                    ctx.hwnd = hwnd;
                    return 0; // 找到即停
                }
            }
        }
        1
    }

    let mut ctx = Ctx {
        pid: std::process::id(),
        hwnd: std::ptr::null_mut(),
    };
    unsafe { EnumWindows(Some(enum_proc), &mut ctx as *mut _ as isize) };
    if ctx.hwnd.is_null() {
        return None;
    }

    // uID 探测：NIF_STATE + dwStateMask=0 是空操作，能返 TRUE 说明该 uID 的图标存在。
    // 全探测失败时按 tray-icon 的约定兜底为 1（进程内第一个托盘）。
    for uid in 1..=8u32 {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = ctx.hwnd;
        nid.uID = uid;
        nid.uFlags = NIF_STATE;
        nid.dwState = 0;
        nid.dwStateMask = 0;
        if unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) } != 0 {
            return Some((ctx.hwnd, uid));
        }
    }
    Some((ctx.hwnd, 1))
}

// 🔴🔴 2026-11-17 二改（v1.7.13）：**绝对不要**把托盘图标钉到 NOTIFYICON_VERSION_4。
//
// 上午「B1 根治」用 `NIM_SETVERSION` 把图标钉成 v4（想修同事机器「看得见点不动」），方向反了：
// 钉上 v4 之后，Shell 把 **icon id 塞进回调消息 lParam 的高字**（v3 下 lParam 就是纯鼠标消息），
// 而 tray-icon 0.25.1 的消息分发是**整字比较**：
//     `WM_USER_TRAYICON if matches!(lparam as u32, WM_LBUTTONDOWN | ... | WM_LBUTTONUP ...)`
//     （tray-icon-0.25.1/src/platform_impl/windows/mod.rs:389-402）
// id=1 时 lParam = 0x0001_0202，永不等于 WM_LBUTTONUP(0x0202) → 落到 `_ => {}` → DefWindowProc。
// 结果：左键不出事件、右键不弹菜单、Enter/Leave/Move 一并消失，而且**一行日志都不会有**
// （分发在进 `TrayIconEvent::send` 之前就返回了）。这正是 1.7.11/1.7.12 真机上
// 「托盘图标点不动、nav.log 全空」的直接原因 —— v4 是这版自己引入的回归。
//
// 上游 tray-icon 从不上 SETVERSION，v3 下点击 / 菜单 / GetRect 都是正常路径（全球 tauri 应用默认如此）。
// 因此：v4 钉死逻辑整体删除；`GetRect` 真失效时由下面的 wndproc 钩子兜底（不依赖矩形）。

/// 托盘窗口收到的回调消息（tray-icon `WM_USER_TRAYICON`，值固定 6002）。
#[cfg(target_os = "windows")]
const WM_USER_TRAYICON: u32 = 6002;

/// 被钩子换下来的原始 wndproc（0 = 未挂载）。
#[cfg(target_os = "windows")]
static TRAY_HOOK_OLD: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
/// 钩子里回调主窗要用的 AppHandle。
#[cfg(target_os = "windows")]
static TRAY_HOOK_APP: std::sync::Mutex<Option<tauri::AppHandle>> = std::sync::Mutex::new(None);
/// 唤起去重：钩子与 tray-icon 原生事件可能同时触发，600ms 内只认一次。
#[cfg(target_os = "windows")]
static TRAY_LAST_OPEN: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// 托盘唤起的唯一入口（去重后调 show_main）。
fn tray_open_main(app: &tauri::AppHandle) {
    #[cfg(target_os = "windows")]
    {
        if let Ok(mut g) = TRAY_LAST_OPEN.lock() {
            if let Some(t) = *g {
                if t.elapsed() < std::time::Duration::from_millis(600) {
                    return;
                }
            }
            *g = Some(std::time::Instant::now());
        }
    }
    show_main(app);
}

/// 托盘窗口 wndproc 钩子：**不依赖 `Shell_NotifyIconGetRect`** 的点击通道。
///
/// 为什么需要：tray-icon 分发的第一步是 `get_tray_rect()`，拿不到矩形就 `return 0`
/// （windows/mod.rs:412-415）—— 图标在溢出面板 / 句柄刚重注册 / explorer 刚重启时都可能拿不到，
/// 于是「点了完全静默」。这里在 tray-icon 之前先看一眼原始 lParam，能认就直接唤起主窗，
/// 再把消息原样交给它（原生路径可用时照样弹右键菜单，靠 600ms 去重不会重复唤起）。
///
/// lParam 解析：v4 下低字=鼠标消息、高字=icon id；v3 下 lParam 就是鼠标消息（高字 0）。
/// 统一按低字判定，两种版本都能用。
#[cfg(target_os = "windows")]
unsafe extern "system" fn tray_hook_proc(
    hwnd: *mut std::ffi::c_void,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, DefWindowProcW, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_MOUSEMOVE,
        WM_RBUTTONUP,
    };

    // NIN_SELECT / NIN_KEYSELECT（v4 的键盘/辅助激活），值 = WM_USER + 0 / + 1
    const NIN_SELECT: u32 = 0x0400;
    const NIN_KEYSELECT: u32 = 0x0401;

    if msg == WM_USER_TRAYICON {
        let raw = lparam as u32;
        let lo = raw & 0xFFFF;
        let hi = raw >> 16;
        // Move 在图标上滑一下就发，一次几十条会把 nav.log 冲爆 → 不记
        if lo != WM_MOUSEMOVE {
            nav_write(&format!("tray raw: lo={lo:#06x} hi={hi} wp={wparam:#x}"));
        }
        let open = lo == WM_LBUTTONUP || lo == WM_LBUTTONDBLCLK || lo == NIN_SELECT || lo == NIN_KEYSELECT;
        if open {
            let app = TRAY_HOOK_APP.lock().ok().and_then(|g| g.clone());
            if let Some(app) = app {
                tray_open_main(&app);
                nav_write("tray raw: 左键 → 唤起主窗（钩子通道生效）");
            }
        } else if lo == WM_RBUTTONUP {
            // 菜单仍由 tray-icon 原生路径弹（它依赖 GetRect）；这里只留证据，
            // 下次再报「右键没菜单」能一眼分清是没收到还是弹不出来。
            nav_write("tray raw: 右键到达（菜单走 tray-icon 原生路径）");
        }
    }

    let old = TRAY_HOOK_OLD.load(Ordering::SeqCst);
    if old != 0 {
        let f: unsafe extern "system" fn(*mut std::ffi::c_void, u32, usize, isize) -> isize =
            std::mem::transmute(old);
        CallWindowProcW(Some(f), hwnd, msg, wparam, lparam)
    } else {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

/// 把上面的钩子挂到 `tray_icon_app` 窗口（幂等；窗口还没建好时后台重试约 3s）。
#[cfg(target_os = "windows")]
fn tray_hook_install(app: tauri::AppHandle) {
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowLongPtrW, GWLP_WNDPROC};

    if let Ok(mut g) = TRAY_HOOK_APP.lock() {
        *g = Some(app);
    }
    std::thread::spawn(move || {
        for _ in 0..30 {
            if TRAY_HOOK_OLD.load(Ordering::SeqCst) != 0 {
                return;
            }
            if let Some((hwnd, _uid)) = tray_icon_handle() {
                let hook: unsafe extern "system" fn(*mut std::ffi::c_void, u32, usize, isize) -> isize =
                    tray_hook_proc;
                let old =
                    unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, hook as usize as isize) };
                if old != 0 {
                    TRAY_HOOK_OLD.store(old, Ordering::SeqCst);
                    nav_write("tray hook: 已挂到 tray_icon_app（点击不再依赖 GetRect）");
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        nav_write("tray hook: 挂载失败（始终没拿到 tray_icon_app 窗口）");
    });
}

/// Windows 托盘气泡提示（2026-10-19 老8 诉求：托盘图标要有气泡，才有提醒作用）。
///
/// 不走 tauri-plugin-notification 的原因：那条路是 WinRT toast，依赖开始菜单快捷方式注册的
/// AUMID，未安装 / 绿色版跑 exe 时可能静默不弹；Shell_NotifyIcon 的 NIF_INFO 没有这个前提。
///
/// 🩸 实测坑（Win10 22H2 19045）：**自建隐藏托盘项**（NIS_HIDDEN）发气泡，NIM_MODIFY 返回 1
/// 但屏幕什么都不出现、通知中心也无记录；换成**可见**托盘图标后同一段代码立刻弹成 toast
/// （通知中心可见「邦信新消息 / 李四：…」）。所以必须挂在 Tauri 自己的托盘图标上。
#[cfg(target_os = "windows")]
fn win_tray_balloon(title: &str, body: &str) {
    use windows_sys::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_INFO, NIIF_INFO, NIM_MODIFY, NOTIFYICONDATAW,
    };

    let Some((hwnd, uid)) = tray_icon_handle() else {
        return;
    };

    fn put_wide(dst: &mut [u16], s: &str) {
        let max = dst.len().saturating_sub(1);
        let v: Vec<u16> = s.encode_utf16().take(max).collect();
        dst[..v.len()].copy_from_slice(&v);
    }

    unsafe {
        // ⚠️ 不要在这里 SETVERSION v4（见 tray_hook 上方长注释：v4 会让 tray-icon 分发整字比较失败）
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = uid;
        nid.uFlags = NIF_INFO;
        put_wide(&mut nid.szInfoTitle, title);
        put_wide(&mut nid.szInfo, body);
        nid.Anonymous.uTimeout = 10_000;
        nid.dwInfoFlags = NIIF_INFO;
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// 托盘图标闪烁（2026-10-22 老8：新消息时托盘图标要「闪得明显」）。
///
/// 背景：此前有未读仅把图标静态换成红底数字角标——不动就不抓眼；而任务栏闪烁
/// （request_user_attention）在窗口藏进托盘后根本没有按钮可闪。托盘区 24 小时
/// 可见的只剩图标本身，所以让图标自己闪：每 500ms 在「红底未读角标 ↔ 默认 logo」
/// 之间整图切换（QQ/微信式红白闪，托盘区最高对比度组合）。
///
/// 退出条件（任一即停，并把图标落到此刻应然的静态态）：
///   ① 未读清零 —— notify_unread(count=0) / show_main 调 stop_tray_flash
///   ② 循环自查主窗已回到前台且可见（人已经在看，静态角标足够，不再闪）
///   ③ 世代被接替 —— 新消息进来开新一代，旧代下一 tick 自行退出（不碰图标）
/// 防重入套路同 bring_to_front 的 FRONT_SEQ：世代号 AtomicU64 保证任意时刻只有一个循环。
/// 未读数走共享 AtomicI64，循环每 tick 重读——新消息不用重启线程，角标数字自动刷新。
static TRAY_FLASH_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TRAY_FLASH_UNREAD: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// 闪烁半周期（毫秒）。
/// 2026-11-17 由 500 改 900：每次「换图标」= 新建一个 HICON + Shell_NotifyIcon(NIM_MODIFY)
/// + 立刻 DestroyIcon 旧的，500ms 一次对通知区太密（见 start_tray_heal 头的失效说明）。
/// 900ms 仍是每秒一次以上的明暗变化，肉眼照样抓得住，写入次数少四成。
const TRAY_FLASH_INTERVAL_MS: u64 = 900;

/// 闪烁总时长上限（tick 数，≈ 900ms × 24 ≈ 21 秒）。
/// 闪够就停在亮态：老8 要的是「进来消息时抓一下眼」，不是永久躁动；
/// 长时间高频写托盘还是把图标句柄写坏的主嫌疑，限次从源头收窄。
const TRAY_FLASH_MAX_TICKS: u32 = 24;

/// 暗态亮度系数（微信式「一暗一亮」的暗侧）。0.35 实测足够看出呼吸感，
/// 又不会暗到像「图标被禁用/掉线」——托盘区白天在浅色任务栏上也看得清。
const TRAY_FLASH_DIM: f32 = 0.4;

/// 启动/接续托盘图标闪烁。由 notify_unread / away_notice 在「有新消息」时调用
/// （幂等：老循环自行退场）。
/// 按值收 AppHandle：线程里要用它取 tray / 主窗，引用过不了 'static（E0521）。
///
/// 2026-10-23 老8：不再用红底未读数角标 —— 有消息就「原托盘图标一暗一亮」，
/// 与微信一致；数字交给右下角浮窗和 tooltip，不在托盘这张小图上挤。
fn start_tray_flash(app: tauri::AppHandle, count: i64) {
    use std::sync::atomic::Ordering;
    // 2026-11-17 诊断：留一行「开闪那一刻图标句柄是否还活着」。配合 start_tray_heal 的
    // "tray heal: 图标句柄失效" 行，下次真机再哑掉能直接判定是闪烁写坏还是别的原因。
    if count > 0 {
        nav_write(&format!(
            "tray flash start n={count} alive={}",
            tray_rect_alive(&app)
        ));
    }
    TRAY_FLASH_UNREAD.store(count.max(0), Ordering::SeqCst);
    let my_gen = TRAY_FLASH_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        // def_icon 必须在闭包内取：default_window_icon() 借的是 app 本身，在闭包外取会
        // 连带把 app 借住，移动进线程直接过不了 'static（E0597/E0505，2026-10-22 踩过）。
        // 且必须转成 'static 副本：闪烁缓存变量是 Option<Image<'static>>，直接 cloned()
        // 出来的仍是借用 app 的 Image<'_>，会被要求 'static 而报 E0597。
        let def_icon = Some(tray_icon_image());
        // 一暗一亮：亮 ↔ 压暗 35%，隔 500ms 交替。不再做「角标 ↔ logo」整图替换式闪烁——
        // 那个是红方块和白 logo 互换，形状跳、观感躁，老8 要的是微信那种「图标自己在呼吸」。
        let mut show_bright = true;
        let mut cached_n: i64 = -1;
        let mut bright: Option<tauri::image::Image<'static>> = None;
        let mut dark: Option<tauri::image::Image<'static>> = None;
        // 2026-11-06：主窗句柄在循环外取一次（Window 是 Arc 克隆，复用零成本），
        // 供下面「人已回到窗口前台」自查用；不在每 tick 里重新 get_webview_window。
        let main_win = app.get_webview_window("main");
        // 前台自查降频：每 2 个 tick（1s）查一次，够快又不占用每帧。
        const FRONT_CHECK_EVERY: u32 = 2;
        let mut tick: u32 = 0;
        loop {
            use std::sync::atomic::Ordering;
            if TRAY_FLASH_GEN.load(Ordering::SeqCst) != my_gen {
                // ③ 被新消息接替：不碰图标，新一代全权负责
                return;
            }
            // ② 人已回到主窗前台（获得焦点 + 可见 + 未最小化）：立刻停闪并落回默认 logo。
            // 判据与 notify_unread 里的 away 完全同源（都用 OS 级窗口状态），口径一致。
            // 此前这条退出条件只写在注释里、代码没实现 —— 窗口一直开着、消息也看完，
            // 但 unread 计数没归零时循环会无限闪（2026-11-06 老8 反馈）。
            // 只 return、不清 TRAY_FLASH_UNREAD、不动 GEN：新消息进来照常开新一代。
            if tick % FRONT_CHECK_EVERY == 0 {
                if let Some(w) = main_win.as_ref() {
                    if w.is_focused().unwrap_or(false)
                        && w.is_visible().unwrap_or(false)
                        && !w.is_minimized().unwrap_or(false)
                    {
                        if let Some(tray) = app.tray_by_id("x8im-tray") {
                            let _ = tray.set_icon(def_icon.clone());
                        }
                        return;
                    }
                }
            }
            tick = tick.wrapping_add(1);
            let n = TRAY_FLASH_UNREAD.load(Ordering::SeqCst);
            if n <= 0 {
                // ① 已清完：自己还原默认 logo（与调用方的还原幂等，踩中竞态也只是同写一张图）
                if let Some(tray) = app.tray_by_id("x8im-tray") {
                    let _ = tray.set_icon(def_icon.clone());
                }
                return;
            }
            // ④ 闪够时长（≈21s）：落回默认 logo 收工，不再高频写托盘。
            // 未读数不会丢——tooltip / 右下角浮窗照旧，用户点开窗口即清。
            if tick > TRAY_FLASH_MAX_TICKS {
                if let Some(tray) = app.tray_by_id("x8im-tray") {
                    let _ = tray.set_icon(def_icon.clone());
                }
                return;
            }
            // 未读数变了才重建图标：32x32 逐像素压暗很便宜，但没必要每 500ms 重算
            if n != cached_n {
                // 亮态固定用默认 logo（老8：不用红点角标，原图标一暗一亮即好）
                let base = def_icon.clone().expect("tray 默认图标缺失");
                dark = dim_icon(&base, TRAY_FLASH_DIM);
                bright = Some(base);
                cached_n = n;
            }
            if let Some(tray) = app.tray_by_id("x8im-tray") {
                let icon = if show_bright {
                    bright.clone()
                } else {
                    dark.clone().or_else(|| bright.clone())
                };
                let _ = tray.set_icon(icon);
            }
            show_bright = !show_bright;
            std::thread::sleep(std::time::Duration::from_millis(TRAY_FLASH_INTERVAL_MS));
        }
    });
}

/// 停止托盘闪烁并作废在跑的循环（①② 的公共出口）。
/// 图标还原由调用方负责——notify_unread(0) 与 show_main 本来就会把图标设回默认 logo；
/// 若此时没有循环在跑（静态角标态），调用方的还原就是唯一还原点。
fn stop_tray_flash() {
    use std::sync::atomic::Ordering;
    TRAY_FLASH_UNREAD.store(0, Ordering::SeqCst);
    TRAY_FLASH_GEN.fetch_add(1, Ordering::SeqCst); // 在跑的循环下一 tick 发现世代过期即退出
}

// ─────────────────────── 托盘图标自愈（2026-11-17） ───────────────────────
//
// 现象（老8 真机报）：图标明明在闪，左键点不开、右键菜单也不弹。
// 根因链：tray-icon 0.25.1 的消息分发第一步是 `Shell_NotifyIconGetRect(hwnd, uID)`，
//   拿不到矩形就 `return 0`（mod.rs:412）——**连菜单都不会弹**。所以「看得见、点不动」
//   说明通知区里那个图标句柄已经失效，屏幕上的只是残影。重启程序即恢复 → 是运行期被写坏的。
// 头号嫌疑：闪烁每次换图都是「新建 HICON → NIM_MODIFY → 立刻 DestroyIcon 旧的」，
//   高频下和 explorer 的通知区更新抢跑（已把闪烁降到 900ms/最长 21s 收窄概率）。
// 兜底：常驻自愈线程——连续 3 次（每 20s 一次）拿不到图标矩形，就 NIM_DELETE + NIM_ADD
//   重新注册一遍再还原图标/提示。修不了概率，但保证「哑掉最多 1 分钟、自己活回来」。
fn tray_rect_alive(app: &tauri::AppHandle) -> bool {
    match app.tray_by_id("x8im-tray") {
        Some(t) => matches!(t.rect(), Ok(Some(_))),
        None => false,
    }
}

/// 把托盘图标钉到 `NOTIFYICON_VERSION_3`（**消息格式与默认一致**：wParam=uID、lParam=纯鼠标消息）。
///
/// 为什么必须显式钉这一下：1.7.11/1.7.12 曾把记录钉成 v4（图标记录里的 version 是**持久属性**，
/// NIM_MODIFY 改不动，只有 NIM_SETVERSION 能改）。老机器上那条 v4 记录会跨版本残留在 explorer 里，
/// 于是即便新版本不再钉 v4，Shell 仍按 v4 发包（lParam 高字=icon id）→ tray-icon 整字比较继续失败。
/// 显式改回 3，等于把「记录属性」修回来，比删建图标副作用小（不会让图标在通知区里跳位置）。
#[cfg(target_os = "windows")]
fn tray_set_version_3() {
    use windows_sys::Win32::UI::Shell::{Shell_NotifyIconW, NIM_SETVERSION, NOTIFYICONDATAW};

    let Some((hwnd, uid)) = tray_icon_handle() else {
        return;
    };
    unsafe {
        let mut v: NOTIFYICONDATAW = std::mem::zeroed();
        v.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        v.hWnd = hwnd;
        v.uID = uid;
        v.Anonymous.uVersion = 3; // NOTIFYICON_VERSION_3
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &v);
    }
}

/// 直接给图标写提示文字（`NIF_TIP`）。
///
/// 🔴 2026-11-17 现场实测（1.7.12）：图标注册时 `szTip` 是空的 —— 鼠标移上去 explorer 根本
/// 不生成 tooltip 窗口（对照组别的程序一悬停就出）。tray-icon 的 `attrs.tooltip` 这条链在
/// 真机上不可靠，所以这里绕过它，用 Shell_NotifyIcon 硬写一遍，幂等。
#[cfg(target_os = "windows")]
fn tray_write_tip(text: &str) {
    use windows_sys::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_TIP, NIM_MODIFY, NOTIFYICONDATAW,
    };

    let Some((hwnd, uid)) = tray_icon_handle() else {
        return;
    };
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = uid;
    nid.uFlags = NIF_TIP;
    let v: Vec<u16> = text.encode_utf16().take(127).collect();
    nid.szTip[..v.len()].copy_from_slice(&v);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// 启动收口：把图标记录的版本修回 v3、补写提示文字，并自检「到底注册上没有」。
///
/// 自检判据：`NIM_MODIFY + NIF_STATE + dwStateMask=0` 是空操作，返回 TRUE 只可能因为该
/// (hWnd, uID) 的图标真实存在。返回 FALSE 的典型原因是 `NIM_ADD` 当年就失败了
/// （explorer 未就绪 / 同 id 被旧实例占着）——屏幕上那个图标其实是**残留幽灵**。
#[cfg(target_os = "windows")]
fn tray_fixup(app: &tauri::AppHandle) {
    use windows_sys::Win32::UI::Shell::{Shell_NotifyIconW, NIF_STATE, NIM_MODIFY, NOTIFYICONDATAW};

    let Some((hwnd, uid)) = tray_icon_handle() else {
        nav_write("tray check: 找不到 tray_icon_app 窗口（托盘根本没建出来？）");
        return;
    };
    // ① 版本修回 v3（v4 记录会跨版本残留，不修则点击永远静默）
    tray_set_version_3();
    // ② 提示文字硬写（真机上 tray-icon 的 tooltip 没写进 szTip）
    tray_write_tip("邦信即时通讯");

    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = uid;
    nid.uFlags = NIF_STATE;
    nid.dwState = 0;
    nid.dwStateMask = 0;
    let registered = unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) } != 0;
    let rect_ok = tray_rect_alive(app);
    nav_write(&format!(
        "tray check: uid={uid} registered={registered} rect_ok={rect_ok} ver=3+tip"
    ));
    if !registered {
        nav_write("tray check: 通知区里没有我们的图标（屏幕上的多半是残留幽灵）→ 重注册一次");
        let _ = tray_revive(app);
        nav_write(&format!(
            "tray check: 重注册后 rect_ok={}",
            tray_rect_alive(app)
        ));
    }
}

/// 就地重注册托盘图标（NIM_DELETE + NIM_ADD + 还原图标/提示），返回重注册后是否可点。
/// 自愈线程与「点 X 时托盘不可用」两条路径共用，动作完全一致。
///
/// ⚠️ `Shell_NotifyIconGetRect` 拿不到矩形有两种原因，**API 不区分**（tray-icon 只看 S_OK，
///    其余一律当 None，见 get_tray_rect）：①句柄真失效（残影）②图标被折进「隐藏的图标」
///    溢出面板。所以调用方必须能容忍「重注册后仍拿不到」——那多半是②，不是坏了。
fn tray_revive(app: &tauri::AppHandle) -> bool {
    if let Some(tray) = app.tray_by_id("x8im-tray") {
        let _ = tray.set_visible(false);
        std::thread::sleep(std::time::Duration::from_millis(120));
        let _ = tray.set_visible(true);
        let _ = tray.set_icon(Some(tray_icon_image()));
        let _ = tray.set_tooltip(Some("邦信即时通讯"));
    }
    // 重注册后记录回到默认版本，但**v4 残留**要显式改回 3（见 tray_set_version_3）；
    // 同时硬写一遍提示文字（tray-icon 的 tooltip 在真机上没落进 szTip）。
    #[cfg(target_os = "windows")]
    {
        tray_set_version_3();
        tray_write_tip("邦信即时通讯");
    }
    // 通知区落位要一点时间：隔 50ms 问一次，最多 6 次（≈300ms）
    for _ in 0..6 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if tray_rect_alive(app) {
            return true;
        }
    }
    false
}

/// 托盘自愈线程（2026-11-17 B：提速到约 10s 一轮）。
///
/// 旧版 20s × 3 次才动手（最长 1 分钟不响）；现在 5s × 2 次，真失效 ≈10s 内复活。
/// 因为上面那条「API 不区分溢出区」的限制，重注册后仍拿不到的，按 10s→20s→40s→60s
/// 退避（封顶 60s 一次），避免把「图标在溢出面板」误判成坏掉而每 10s 折腾一次通知区。
fn start_tray_heal(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let mut bad: u32 = 0;
        let mut wait: u64 = 5;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(wait));
            if tray_rect_alive(&app) {
                bad = 0;
                wait = 5;
                continue;
            }
            bad += 1;
            if bad < 2 {
                continue;
            }
            bad = 0;
            if app.tray_by_id("x8im-tray").is_none() {
                continue;
            }
            if tray_revive(&app) {
                wait = 5;
                nav_write("tray heal: 图标句柄失效，已重新注册");
            } else {
                wait = (wait * 2).min(60);
                nav_write(&format!("tray heal: 重注册后仍拿不到矩形，退避到 {wait}s"));
            }
        }
    });
}

// ─────────────────────── 右下角卡通 FDE 浮动提醒（2026-10-22） ───────────────────────
//
// 老8 诉求：像微信收到消息时那样，屏幕右下角浮出一个卡通头像小窗提示，不用非得盯着托盘。
//
// 为什么必须是独立窗口：主窗可能已最小化 / 藏进托盘 / 被别的窗口盖住，浮窗要的是
// 「不管主窗在哪都能看见」。塞进主窗 DOM 就完全被主窗的显隐和层级绑架了。
//
// 窗口属性：无边框 + 透明 + 置顶 + 不进任务栏 + 不抢焦点（否则会打断正在打字的人）。
// 内容走线上页 https://im.x8fde.com/app/fde-float.html —— 跟主窗同一套架构：
// 改浮窗样式只发 web，不用重新发客户端。

/// 浮窗尺寸（逻辑像素）：2026-11-16 升级为「AI邦」精灵 —— 96px 精灵 + 气泡两行。
const FDE_FLOAT_W: f64 = 148.0;
const FDE_FLOAT_H: f64 = 196.0;
/// 🔴 必须与 tauri.conf.json 主窗 `additionalBrowserArgs` **逐字一致**。
/// WebView2 同一进程只接受一套 Environment 参数：第二次 `CreateCoreWebView2EnvironmentWithOptions`
/// 若 args 不同，直接返回 ERROR_INVALID_STATE(0x8007139F)「组或资源的状态不是执行请求操作的正确状态」。
/// 2026-11-17 真机定位：AI邦 浮窗在**任何**机器上都建不出来（历史版本同样，只是当时失败被静默吞掉），
/// 主窗能建、第二窗必挂，就是因为代码创建时漏了这个参数。改主窗参数时这里要一起改。
const X8_WEBVIEW_ARGS: &str = "--disable-background-networking --disable-component-update --no-first-run --disable-safebrowsing --disable-features=msEdgeComponentUpdates,msEdgeShoppingAssistant,EdgeReadingList";
/// 自动收起延时：**只对「无未读的临时提示」生效**（升级提示等）。
/// 有未读（count>0）时改为常驻屏上，直到未读清零 / 用户手动收起 ——
/// 2026-11-16 专家评审 P0：9.2s 自动消失是「提示不明显」的根因之一，人离开两分钟就完全错过。
const FDE_FLOAT_AUTO_HIDE_MS: u64 = 9_200;
/// 浮窗世代号：每次显示 +1，上一轮的自动隐藏计时发现自己过期就放弃（同 TRAY_FLASH_GEN 套路）。
static FDE_FLOAT_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// 精灵「钉住」：托盘「召唤 AI邦」置 true —— 出来后**永不自动收**，
/// 只有用户手工关（点精灵唤起主窗 / 页面 ✕ / 右键收起）才失效。
/// 老8 2026-11-17 明确：召唤出来的 AI邦 不要自己消失，一直挂着，想关我再关。
static PET_PIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// ─────────────────────── AI邦 精灵配置（2026-11-16）───────────────────────
//
// 落盘 %LOCALAPPDATA%\BangXin\pet.json（与 file_retention.json 同目录，升级不受影响）：
//   { "x": 1720, "y": 700,      // 用户拖过的落点（逻辑像素），换屏/拔屏后自动作废
//     "muted": false,           // 免打扰：只停提示音，视觉提醒照旧
//     "enabled": true }         // 总开关：false = 完全回到 2026-10-22 的旧浮窗行为
//
// 老8 2026-11-16 的诉求是「提示更明显」，所以这里的原则是：
// 精灵只叠加在既有 6 层提醒之上，**不改 notify_unread 语义**；出问题关 enabled 即回滚。

fn pet_cfg_path() -> std::path::PathBuf {
    bangxin_user_dir().join("pet.json")
}

fn pet_cfg_load() -> serde_json::Value {
    let raw = std::fs::read_to_string(pet_cfg_path()).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    if v.is_object() {
        v
    } else {
        serde_json::json!({})
    }
}

fn pet_cfg_save(v: &serde_json::Value) {
    let dir = bangxin_user_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(pet_cfg_path(), v.to_string());
}

fn pet_flag(key: &str, def: bool) -> bool {
    pet_cfg_load()
        .get(key)
        .and_then(|x| x.as_bool())
        .unwrap_or(def)
}

fn pet_enabled() -> bool {
    pet_flag("enabled", true)
}

fn pet_muted() -> bool {
    pet_flag("muted", false)
}

fn pet_saved_pos() -> Option<(f64, f64)> {
    let v = pet_cfg_load();
    let x = v.get("x").and_then(|x| x.as_f64())?;
    let y = v.get("y").and_then(|x| x.as_f64())?;
    Some((x, y))
}

fn pet_save_pos(x: f64, y: f64) {
    let mut v = pet_cfg_load();
    v["x"] = serde_json::json!(x.round() as i64);
    v["y"] = serde_json::json!(y.round() as i64);
    pet_cfg_save(&v);
}

/// 新消息提示音（Windows：PlaySoundW 播系统通知音，零资源文件、零新增依赖）。
/// 免打扰（pet.json: muted）时静默；mac/linux 暂不播（后续接 afplay / paplay）。
#[cfg(target_os = "windows")]
fn pet_sound() {
    use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_NODEFAULT};
    let alias: Vec<u16> = "SystemNotification\0".encode_utf16().collect();
    unsafe {
        let _ = PlaySoundW(
            alias.as_ptr(),
            std::mem::zeroed(),
            SND_ALIAS | SND_ASYNC | SND_NODEFAULT,
        );
    }
}
#[cfg(not(target_os = "windows"))]
fn pet_sound() {}

/// 指定屏幕坐标所在的**工作区**（已扣掉任务栏）。多显示器全靠它：
/// MONITOR_DEFAULTTONEAREST 保证坐标落到最近那块屏，永不返回空。
#[cfg(target_os = "windows")]
fn work_area_at(px: i32, py: i32) -> Option<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    unsafe {
        let pt = POINT { x: px, y: py };
        let h = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(h, &mut mi) == 0 {
            return None;
        }
        Some((
            mi.rcWork.left,
            mi.rcWork.top,
            mi.rcWork.right,
            mi.rcWork.bottom,
        ))
    }
}

/// 主屏工作区（物理像素 left/top/right/bottom）。
/// 不用 monitor.size()：那块含任务栏，浮窗会被任务栏压掉半个。
#[cfg(target_os = "windows")]
fn primary_work_area() -> Option<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETWORKAREA};
    let mut r: RECT = unsafe { std::mem::zeroed() };
    let ok = unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut r as *mut _ as *mut _, 0) };
    if ok != 0 {
        Some((r.left, r.top, r.right, r.bottom))
    } else {
        None
    }
}

/// 把逻辑坐标夹进指定显示器的可视区（留 8px 边距）。
///
/// 🔴 2026-11-17 mac 1.7.13「AI邦 根本没出来」的兜底：macOS 的显示器 API 受主线程约束，
/// 拿不到屏时下面会回落成 1920×1080 / sf=1 的假设值 → 算出的 x=1756 在 Retina 逻辑宽
/// 1512 的屏上是**屏外**，窗口建出来却永远看不见（置顶也一样）。
/// 所以非 Windows 平台一律把落点夹回「当前屏可视区」，宁可位置不完美也要在屏上。
fn clamp_to_monitor(x: f64, y: f64, sf: f64, mpx: i32, mpy: i32, mw: u32, mh: u32) -> (f64, f64) {
    let sf = if sf > 0.1 { sf } else { 1.0 };
    let left = mpx as f64 / sf;
    let top = mpy as f64 / sf;
    let right = (mpx as f64 + mw as f64) / sf;
    let bottom = (mpy as f64 + mh as f64) / sf;
    let max_x = (right - FDE_FLOAT_W - 8.0).max(left);
    let max_y = (bottom - FDE_FLOAT_H - 8.0).max(top);
    (x.clamp(left, max_x), y.clamp(top, max_y))
}

/// 精灵落点（2026-11-16 重写）：
///   ① 用户拖过的位置优先（pet.json），但落点必须仍在某块屏的工作区内，否则作废回落
///   ② 回落位置 = **主窗所在那块屏**的工作区右下角（距右 16 / 距底 8 逻辑像素）
/// 旧实现死钉 primary_monitor，双屏用户主窗在副屏时精灵会跑到另一块屏上。
fn fde_float_pos(app: &tauri::AppHandle) -> (f64, f64) {
    // 主窗所在屏优先；拿不到再退回主屏
    let mon = app
        .get_webview_window("main")
        .and_then(|w| w.current_monitor().ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten())
        // 🔴 2026-11-17 mac：上面两条在非主线程 / AppKit 尚未就绪时会**双双返回 None**
        // （Windows 不受此限，所以只有 mac 出问题）。退一步把枚举到的屏里最大的那块当主屏，
        // 比死守 1920×1080 假设靠谱——后者在 Retina 上直接把窗口算到屏外。
        .or_else(|| {
            app.available_monitors().ok().and_then(|v| {
                v.into_iter().max_by_key(|m| {
                    let s = m.size();
                    s.width as u64 * s.height as u64
                })
            })
        });
    let mut sf = 1.0_f64;
    let mut mpx = 0_i32;
    let mut mpy = 0_i32;
    let mut mw = 1920_u32;
    let mut mh = 1080_u32;
    if let Some(m) = mon {
        sf = m.scale_factor();
        let p = m.position();
        let s = m.size();
        mpx = p.x;
        mpy = p.y;
        mw = s.width;
        mh = s.height;
    } else {
        nav_write("[pet] 落点: 拿不到任何显示器信息，回落 1920x1080 假设");
    }

    if let Some((sx, sy)) = pet_saved_pos() {
        #[cfg(target_os = "windows")]
        {
            // 换算成物理像素问一句「这个点还在某块屏的工作区里吗」
            let cx = ((sx + FDE_FLOAT_W / 2.0) * sf) as i32;
            let cy = ((sy + FDE_FLOAT_H / 2.0) * sf) as i32;
            if work_area_at(cx, cy).is_some() && sx > -FDE_FLOAT_W && sy > -FDE_FLOAT_H {
                return (sx, sy);
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            // mac/linux：拖过的点也要夹回当前屏（换屏 / 缩放变了之后老坐标会跑出屏外）
            let (cx, cy) = clamp_to_monitor(sx, sy, sf, mpx, mpy, mw, mh);
            if (cx - sx).abs() > 1.0 || (cy - sy).abs() > 1.0 {
                nav_write(&format!(
                    "[pet] 落点: 记忆点 ({sx:.0},{sy:.0}) 不在屏内 → 夹到 ({cx:.0},{cy:.0})"
                ));
            }
            return (cx, cy);
        }
    }

    #[cfg(target_os = "windows")]
    {
        let cx = mpx + mw as i32 / 2;
        let cy = mpy + mh as i32 / 2;
        if let Some((l, t, r, b)) = work_area_at(cx, cy) {
            let w = (r - l) as f64 / sf;
            let h = (b - t) as f64 / sf;
            return (w - FDE_FLOAT_W - 16.0, h - FDE_FLOAT_H - 8.0);
        }
    }
    // 非 Windows / 取不到工作区：退回显示器整块，额外让出 56px 当任务栏
    let x = (mpx as f64 + mw as f64) / sf - FDE_FLOAT_W - 16.0;
    let y = (mpy as f64 + mh as f64) / sf - FDE_FLOAT_H - 8.0 - 56.0;
    if cfg!(windows) {
        (x, y)
    } else {
        clamp_to_monitor(x, y, sf, mpx, mpy, mw, mh)
    }
}

/// 百分号编码（只用于把中文提示塞进 URL query）
fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// 转成能安全塞进 eval 的 JS 字符串字面量
fn js_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push(' '),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// 关闭浮窗（若在显示）并作废自动隐藏计时。
///
/// 🔴 2026-11-17 老8：召唤出来的 AI邦 不要自动消失，一直挂着、要关再手工关。
/// 所以「钉住」期间（托盘召唤后）这里直接不干活 —— 未读清零、主窗回前台、
/// 自动收起计时全都不生效；只有点精灵 / 页面 ✕ 这两条手工路径会先清钉再收。
fn fde_float_close(app: &tauri::AppHandle) {
    use std::sync::atomic::Ordering;
    if PET_PIN.load(Ordering::SeqCst) {
        return;
    }
    FDE_FLOAT_GEN.fetch_add(1, Ordering::SeqCst);
    if let Some(w) = app.get_webview_window("fde-float") {
        let _ = w.hide();
    }
}

/// 显示/刷新精灵窗。count=0 时只显示精灵不显示角标（消息落在已打开会话里，未读数没涨）。
///
/// 2026-11-16 AI邦：
///   · pet.json 里 enabled=false → 整层不出现（回滚旧行为的唯一开关）
///   · count>0 → **常驻**，不再挂自动收起计时；由 notify_unread(0) / pet_close 收
///   · count=0 → 临时提示（升级通知等），保留 9.2s 自动收起
///   · 有新未读且未静音时播一声系统通知音（silent=true 时不响：登录自动问候用）
fn fde_float_show_impl(app: &tauri::AppHandle, count: i64, tip: &str, silent: bool) {
    fde_float_show_ex(app, count, tip, silent, false);
}

/// force = true 时忽略 pet.json 的 enabled 总开关。
/// 只给托盘「召唤 AI邦」用：用户关掉精灵是「别拿新消息打扰我」，不代表不许手动叫出来。
/// 建 AI邦 浮窗：三级降级 + 失败留痕（2026-11-17）。
///
/// 起因：老8 新电脑点「AI邦形象 → 卡通精灵」毫无反应。实测 EnumWindows 枚举该进程
/// 顶层窗口只有 main + tray_icon_app，**fde-float 根本没被建出来**；而原代码收尾是
/// `Err(_) => {}` —— 建窗失败连一行日志都没有，等于黑盒（旧机器上也可能是同样静默失败）。
/// 三级尝试：① 原样（透明 + 无边框 + 置顶）→ ② 不透明（透明窗在部分显卡/远程桌面/
/// WebView2 版本上建不出来）→ ③ 最低配（不置顶、带阴影）。每级失败都写 nav.log，
/// 成功走降级也记一行，便于事后确认到底哪一级不行。
fn fde_build_window(
    app: &tauri::AppHandle,
    path: &str,
    x: f64,
    y: f64,
    show_after: bool,
) -> Option<tauri::WebviewWindow> {
    // 2026-11-17 二改：改用**本地打包页**（dist/fde-float.html）而不是远程 External URL。
    // 真机实测：External(https://…) 建第二个 webview 一律 0x8007139F（L1~L4 全败），
    // 而主窗（App 协议 + 本地资源）正常 —— 差距就在 URL 形态上，故先按本地页验证。
    let mk_url = || tauri::WebviewUrl::App(path.into());
    // (transparent, shadow, always_on_top)
    // 🔴 2026-11-17 mac：本仓没开 tauri 的 macos-private-api（Cargo.toml）也没开
    // macOSPrivateApi（tauri.conf.json）—— 这种组合下 macOS 的 transparent 档很容易
    // 建出「全透明空窗」：窗口存在、visible=true，但一个像素都不画，用户看到的就是
    // 「AI邦 根本没出来」（Windows 的等价坑见下面 WebView2 那段注释）。
    // 所以 mac 从**不透明**档起试，先保证看得见；要透明美化得先把 private api 打开。
    #[cfg(target_os = "macos")]
    let attempts: [(bool, bool, bool); 3] = [
        (false, false, true),
        (false, true, true),
        (true, false, true),
    ];
    #[cfg(not(target_os = "macos"))]
    let attempts: [(bool, bool, bool); 3] = [
        (true, false, true),
        (false, false, true),
        (false, true, false),
    ];
    for (i, (transparent, shadow, on_top)) in attempts.into_iter().enumerate() {
        let r = tauri::WebviewWindowBuilder::new(app, "fde-float", mk_url())
        .title("AI邦")
        .additional_browser_args(X8_WEBVIEW_ARGS)
        // 🔴 2026-11-17 真机根因（第三轮）：WebView2 规定「同一个 user data folder 里，
        // 只要 Environment options 有任何差异，第二次建 controller 就报 0x8007139F」。
        // 主窗的 args 来自 tauri.conf.json，浮窗走代码 builder —— 即使字符串写得一样，
        // 也不是同一个 Environment 实例，实测照样 0x8007139F（L1~L4 全败、args 已对齐仍败）。
        // 唯一可靠解法：给浮窗一个**独立 data_directory**（独立 Environment），互不干涉。
        // 代价：多一个 msedgewebview2 进程组，只在浮窗存活期间；浮窗是静态页，不需要共享登录态。
        .data_directory(bangxin_user_dir().join("pet-webview"))
        .inner_size(FDE_FLOAT_W, FDE_FLOAT_H)
        .position(x, y)
        .decorations(false)
        .transparent(transparent)
        .always_on_top(on_top)
        .skip_taskbar(true)
        .focused(false)
        .resizable(false)
        .shadow(shadow)
        // 🔴 2026-11-17 诊断：之前一律 `visible(false)` 建窗、随后 show() —— 真机上
        // 窗口 vis=1、rect 正确、非 cloaked，但 WebView2 **一点内容都不画**（连不透明档
        // 该有的白底都没有）。改为「要显示就以 visible(true) 建窗」，让 WebView2 在
        // 可见窗口上初始化合成层；预建（show_after=false）仍然隐藏建。
        .visible(show_after)
        .build();
        match r {
            Ok(w) => {
                nav_write(&format!(
                    "[pet] fde-float 建成 L{}（transparent={transparent} on_top={on_top} visible={show_after}）",
                    i + 1
                ));
                if show_after {
                    // 保险：1px 抖一下，强制 WebView2 重算视口（隐藏期建窗偶发 0x0 视口）
                    let _ = w.set_size(tauri::LogicalSize::new(FDE_FLOAT_W + 1.0, FDE_FLOAT_H));
                    let _ = w.set_size(tauri::LogicalSize::new(FDE_FLOAT_W, FDE_FLOAT_H));
                    let _ = w.set_position(tauri::LogicalPosition::new(x, y));
                }
                return Some(w);
            }
            Err(e) => nav_write(&format!("[pet] fde-float 建窗失败 L{}: {e}", i + 1)),
        }
    }
    // L4 纯诊断：与主窗同构（有边框、可见、占任务栏、不置顶不透明）。
    // 2026-11-17：L1~L3 在同进程里全部 0x8007139F，且 tauri 的 create_webview 本身
    // 已投递主线程（tauri-runtime-wry 2.12 `send_user_message` + rx.recv），
    // 所以要么是某个窗口属性触发，要么同进程第二个 webview 根本建不出来。
    // L4 建成 → 是属性问题，逐个加回即可；L4 也失败 → 必须换实现（不要第二个 webview）。
    let r = tauri::WebviewWindowBuilder::new(app, "fde-float", mk_url())
    .title("AI邦")
    .additional_browser_args(X8_WEBVIEW_ARGS)
    .data_directory(bangxin_user_dir().join("pet-webview"))
    .inner_size(320.0, 420.0)
    .position(x, y)
    .build();
    match r {
        Ok(w) => {
            nav_write("[pet] fde-float L4 最小属性建成 ✓（说明是窗口属性触发，不是 webview 本身）");
            if show_after {
                let _ = w.show();
            }
            Some(w)
        }
        Err(e) => {
            nav_write(&format!("[pet] fde-float L4 也失败，第二个 webview 建不出来: {e}"));
            None
        }
    }
}

fn fde_float_show_ex(app: &tauri::AppHandle, count: i64, tip: &str, silent: bool, force: bool) {
    use std::sync::atomic::Ordering;
    if !force && !pet_enabled() {
        return;
    }
    let n = count.max(0);
    // keep 恒 1：页面不要挂 8 秒淡出，精灵出来就一直在（老8 2026-11-17）
    let keep = 1;

    // 🔴 2026-11-17 mac：落点计算必须**在主线程内**做。
    // fde_float_pos 要调 current_monitor / primary_monitor，macOS 上这些是 AppKit 接口，
    // 在后台线程调用会静默返回 None（Windows 无此限制）→ 上次的真机表现就是
    // pet_autoshow（thread::spawn 里跑的）把窗口算到屏外，然后「AI邦 一直没出来」。
    // 所以整段显示逻辑（取坐标 → eval → show / 建窗）统一投到主线程闭包里，不在这里先算。
    if app.get_webview_window("fde-float").is_some() {
        // 已存在：原地更新数字/文案 + 重播入场动画。不整窗 reload——reload 会白闪一下
        let js = format!(
            "window.__fdeUpdate && window.__fdeUpdate({n}, {}, {keep})",
            js_str(tip)
        );
        let h_main = app.clone();
        if let Err(e) = app.run_on_main_thread(move || {
            let (x, y) = fde_float_pos(&h_main);
            if let Some(w) = h_main.get_webview_window("fde-float") {
                let _ = w.eval(&js);
                let _ = w.set_position(tauri::LogicalPosition::new(x, y));
                let _ = w.show();
            }
        }) {
            nav_write(&format!("[pet] 投主线程失败（更新已有窗口）: {e}"));
        }
    } else {
        // 本地打包页（dist/fde-float.html），带首次渲染所需的参数；后续更新走 __fdeUpdate
        let path = format!("fde-float.html?n={}&t={}&k={keep}", n, url_encode(tip));
        let tip_owned = tip.to_string();
        let h_main = app.clone();
        if let Err(e) = app.run_on_main_thread(move || {
            // 排队期间可能已被另一次调用建好：改成原地更新，别再建一遍
            if let Some(w) = h_main.get_webview_window("fde-float") {
                let _ = w.eval(&format!(
                    "window.__fdeUpdate && window.__fdeUpdate({n}, {}, {keep})",
                    js_str(&tip_owned)
                ));
                let (x, y) = fde_float_pos(&h_main);
                let _ = w.set_position(tauri::LogicalPosition::new(x, y));
                let _ = w.show();
                return;
            }
            let (x, y) = fde_float_pos(&h_main);
            // 建窗才记一行：区分「命令根本没到」与「到了但建窗失败」（见 fde_build_window 头注）
            nav_write(&format!("[pet] 建 fde-float n={n} pos=({x:.0},{y:.0})"));
            fde_build_window(&h_main, &path, x, y, true);
            pet_show_probe(h_main.clone());
        }) {
            nav_write(&format!("[pet] 投主线程失败（建窗）: {e}"));
        }
    }

    // 提示音：只在新未读时响，且尊重免打扰
    if n > 0 && !silent && !pet_muted() {
        pet_sound();
    }

    // 🔴 2026-11-17 老8：精灵出来就不要自己消失，一直在，等手工关（点它 / ✕ / 右键收起）。
    // 所以这里**不再挂任何自动收起计时** —— 未读常驻、召唤常驻、登录问候也常驻。
    FDE_FLOAT_GEN.fetch_add(1, Ordering::SeqCst);
}

/// 建窗后 1.2s 回读窗口的真实状态（位置 / 尺寸 / 所在屏 / 可见性）。
///
/// 为什么必须回读：mac 上「窗口建出来了但落在屏外」和「压根没建出来」在用户眼里完全一样
/// （都是没东西）。这两条日志一句话就能分开，下次再有这类反馈不用靠读二进制猜。
fn pet_show_probe(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1200));
        match app.get_webview_window("fde-float") {
            Some(w) => {
                let vis = w.is_visible().unwrap_or(false);
                let pos = w
                    .outer_position()
                    .map(|p| format!("({},{})", p.x, p.y))
                    .unwrap_or_else(|e| format!("err:{e}"));
                let size = w
                    .outer_size()
                    .map(|s| format!("{}x{}", s.width, s.height))
                    .unwrap_or_else(|e| format!("err:{e}"));
                let mon = w
                    .current_monitor()
                    .ok()
                    .flatten()
                    .map(|m| {
                        let p = m.position();
                        let s = m.size();
                        format!("mon({},{} {}x{} sf={})", p.x, p.y, s.width, s.height, m.scale_factor())
                    })
                    .unwrap_or_else(|| "mon=None".into());
                nav_write(&format!(
                    "[pet] 1.2s 回读: visible={vis} pos={pos} size={size} {mon}"
                ));
            }
            None => nav_write("[pet] 1.2s 回读: 窗口不存在（建窗失败）"),
        }
    });
}

/// 前端接口：显示右下角 FDE 浮动提醒（由 notify_unread / away_notice 内部调用，
/// 也允许页面直接触发，便于以后扩展「任务提醒」等场景）。
#[tauri::command]
fn fde_float_show(
    app: tauri::AppHandle,
    count: Option<i64>,
    tip: Option<String>,
    silent: Option<bool>,
) -> Result<(), String> {
    let t = tip.unwrap_or_default();
    fde_float_show_impl(&app, count.unwrap_or(0), &t, silent.unwrap_or(false));
    Ok(())
}

/// 前端接口：收起浮窗（未读清零 / 主窗回前台时由 front-end 显式调用）。
#[tauri::command]
fn fde_float_hide(app: tauri::AppHandle) {
    fde_float_close(&app);
}

/// 浮窗被点击：唤起主窗（浮窗自身由 show_main → fde_float_close 收掉）。
#[tauri::command]
fn fde_float_click(app: tauri::AppHandle) {
    use std::sync::atomic::Ordering;
    // 点精灵 = 用户要干活了：解除钉住，允许随之收起
    PET_PIN.store(false, Ordering::SeqCst);
    fde_float_close(&app);
    show_main(&app);
}

/// 托盘「召唤 AI邦」：无条件把精灵叫到屏上并**常驻**。
///
/// 为什么需要这个入口：精灵原来只有一条出镜路径 —— 前端 notify_unread 判定「人在别处/
/// 不在当前会话」才弹。老8 2026-11-17 反馈「还是没看到 AI邦」时，能做的只有等一条
/// 不属于当前会话的新消息；主窗开着、正看着那个会话时它永远不出现，等于不可验证。
/// 召唤路径不看 enabled、不等消息、也不挂 9.2s 自动收起 —— 要看到就必须看得到。
fn pet_summon(app: &tauri::AppHandle) {
    use std::sync::atomic::Ordering;
    // 🔴 2026-11-17 真机第二次踩坑（老8 点托盘「召唤 AI邦」没反应）：
    //   早先这里「先 destroy 再重建」，想清掉可能的透明空窗。但 destroy 是**投递主线程异步执行**的，
    //   轮询 1.5s 后 tauri 侧句柄往往还在 → fde_float_show_ex 走「已存在」分支 →
    //   对**已排队待销毁的死窗** eval/show 全静默失败 → 窗口没了、屏上再也没有。
    //   真机日志对得上：第二次点只有 `summon from tray`（在本函数尾），没有任何建窗记录。
    //   结论：召唤**绝不 destroy**。已有窗口就原地 eval + show（幂等，重复点只是刷新文案），
    //   没有才建。页面已本地打包（dist/fde-float.html），不存在远程白屏那类空窗。
    // 钉住：召唤出来的精灵不自动消失（未读清零 / 主窗回前台都不收），等用户手工关。
    // 与 pet_autoshow 同理，pin 必须在显示**之前**置位。
    PET_PIN.store(true, Ordering::SeqCst);
    fde_float_show_ex(app, 0, "我在，点我打开邦信", true, true);
    // 作废上面为「纯提示」排的自动收起计时：召唤出来的精灵要留着，
    // 直到点它 / 主窗回前台 / 未读清零（与未读常驻同一套收起条件）。
    FDE_FLOAT_GEN.fetch_add(1, Ordering::SeqCst);
    // 诊断：show 是异步投递的，这里只做「事后回看」——真看不到时日志能说清是没建窗、
    // 还是建了但落在屏外（2026-11-17 mac：后者正是「召唤没反应」的真相）。
    pet_show_probe(app.clone());
    si_log("[pet] summon from tray");
}

/// 启动即把 AI邦 挂到屏上并常驻（2026-11-17 老8：**默认打开着**，加个小 ✕ 手工关，
/// 平时不再自动收）。
///
/// 与 pet_summon 的区别只有一处：召唤是用户主动点托盘（不看 enabled、也不改任何配置），
/// 这里是开机自启行为 —— 要尊重 pet.json 的 enabled 总开关（关掉精灵的人不该每次启动
/// 又被弹一脸）。钉住逻辑相同：未读清零 / 主窗回前台都不收，只有 ✕、右键收起、点精灵才收。
fn pet_autoshow(app: &tauri::AppHandle) {
    use std::sync::atomic::Ordering;
    if !pet_enabled() {
        si_log("[pet] autoshow skipped: enabled=false");
        return;
    }
    // 🔴 先置 pin 再显示：show_ex 的建窗是**投递主线程异步执行**的，若先显示后 pin，
    // 中间那几十毫秒里任何一次 close（前端收起升级提示、未读清零）都可能把刚建好的窗收掉。
    // 真机 13:35 那轮就是这么丢的：日志里建成 visible=true，几秒后 WS_VISIBLE 没了。
    PET_PIN.store(true, Ordering::SeqCst);
    fde_float_show_ex(app, 0, "AI邦 · 点我打开邦信", true, false);
    FDE_FLOAT_GEN.fetch_add(1, Ordering::SeqCst);
    si_log("[pet] autoshow on boot");
    // 诊断：2s 后回看窗口是否真在屏上（曾出现「建窗 visible=true、几秒后变 hidden」）
    let h = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(2));
        match h.get_webview_window("fde-float") {
            Some(w) => nav_write(&format!(
                "[pet] autoshow 后 2s: visible={}",
                w.is_visible().unwrap_or(false)
            )),
            None => nav_write("[pet] autoshow 后 2s: 窗口不存在"),
        }
    });
}

// ── AI邦 精灵的前端接口（2026-11-16，四处同步：本体 / generate_handler! /
//    build.rs 名单 / capabilities/remote.json）──────────────────────────────

/// 手动收起精灵（不改变未读）：用户右键「收起」或点 ✕ 时调用。
#[tauri::command]
fn pet_close(app: tauri::AppHandle) {
    use std::sync::atomic::Ordering;
    // 手工关 = 解除钉住，再真正收起
    PET_PIN.store(false, Ordering::SeqCst);
    fde_float_close(&app);
}

/// 拖动精灵：由前端 mousedown 触发（不走 window.startDragging 的 JS 命名空间，
/// 与 drag_begin 同因——远端页面下 .window 命名空间不可靠）。
/// 松手后落点自动记忆（start_dragging 在 Windows 上立即返回，故轮询到位置稳定再存盘）。
#[tauri::command]
fn pet_drag_begin(app: tauri::AppHandle) {
    let Some(w) = app.get_webview_window("fde-float") else {
        return;
    };
    let _ = w.start_dragging();
    std::thread::spawn(move || {
        let mut last: Option<(i32, i32)> = None;
        let mut stable = 0;
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let Ok(p) = w.outer_position() else { break };
            let cur = (p.x, p.y);
            if last == Some(cur) {
                stable += 1;
                if stable >= 3 {
                    break;
                }
            } else {
                stable = 0;
                last = Some(cur);
            }
        }
        if let (Some((x, y)), Ok(sf)) = (last, w.scale_factor()) {
            let sf = if sf > 0.1 { sf } else { 1.0 };
            pet_save_pos(x as f64 / sf, y as f64 / sf);
        }
    });
}

/// 显式保存当前位置（拖动结束前端兜底调一次）。
#[tauri::command]
fn pet_pos_save(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("fde-float") {
        if let Ok(p) = w.outer_position() {
            let sf = w.scale_factor().unwrap_or(1.0);
            let sf = if sf > 0.1 { sf } else { 1.0 };
            pet_save_pos(p.x as f64 / sf, p.y as f64 / sf);
        }
    }
}

/// 免打扰开关（只静音提示音，视觉提醒照旧）；返回落盘后的值。
#[tauri::command]
fn pet_set_muted(on: bool) -> bool {
    let mut v = pet_cfg_load();
    v["muted"] = serde_json::json!(on);
    pet_cfg_save(&v);
    on
}

/// 精灵状态（前端首次渲染取静音态用）。
#[tauri::command]
fn pet_status() -> serde_json::Value {
    serde_json::json!({
        "enabled": pet_enabled(),
        "muted": pet_muted(),
        "avatar": pet_avatar(),
        "pos": pet_saved_pos().map(|(x, y)| serde_json::json!({ "x": x, "y": y })).unwrap_or(serde_json::Value::Null)
    })
}

/// AI邦 用哪个形象（pet.json: avatar）：空串 = 内置卡通精灵；data:/http(s) = 用户自己的头像。
///
/// 为什么存 data URL 而不是头像链接：精灵窗是**独立 webview**，没有主窗的登录 token，
/// MM 头像接口要 Bearer 才给图（主窗拿到的是 blob:，跨窗不可用）。所以由主窗把头像
/// 压成 160px 的 jpeg data URL 传进来，落盘后精灵窗直接用，不依赖登录态也不怕链接过期。
fn pet_avatar() -> String {
    pet_cfg_load()
        .get("avatar")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

/// 设置 AI邦 形象；传空串 = 换回卡通精灵。返回落盘后的值。
#[tauri::command]
fn pet_set_avatar(app: tauri::AppHandle, url: String) -> String {
    let v = url.trim().to_string();
    let mut cfg = pet_cfg_load();
    cfg["avatar"] = serde_json::json!(v);
    pet_cfg_save(&cfg);
    // 精灵窗开着就地换脸，不用等下次弹出
    if let Some(w) = app.get_webview_window("fde-float") {
        let js = format!("window.__fdeAvatar && window.__fdeAvatar({})", js_str(&v));
        let _ = w.eval(&js);
    }
    v
}

// ───────────────── 右下角「升级中」进度小窗（2026-10-23 老8 定） ─────────────────
//
// 老8 原话：「升级中我需要有升级中的小框，正在下载、还有多少时间、已过多少时间，
// 要让用户看到进度」。此前静默升级只在卡通浮窗里塞一行文字（118px 宽，会被省略号截断），
// 且 9.2 秒自动收起——几十秒的下载过程用户基本看不到进度，只觉得「卡在那儿了」。
//
// 故独立开一个窗：320x108 横版卡片，进度条 + 百分比 + 已用/剩余时间；
// **不做自动收起**（升级期间常驻，直到安装器 taskkill 本进程，或前端显式 upd_float_hide）。
// 与消息浮窗 fde-float 完全独立：升级开始时前端会先收起消息浮窗，避免两窗叠在一起。
//
// 内容同样走线上页 https://im.x8fde.com/app/fde-upd.html —— 调样式只发 web，不用重发客户端。

/// 升级小窗尺寸（逻辑像素）：装下标题行 + 6px 进度条 + 两行时间文案。
const FDE_UPD_W: f64 = 320.0;
const FDE_UPD_H: f64 = 108.0;

/// 升级小窗落点：主屏工作区右下角，距右 16、距底 8（与消息浮窗同锚点，升级时消息浮窗已收）。
fn fde_upd_pos(app: &tauri::AppHandle) -> (f64, f64) {
    let sf = app
        .primary_monitor()
        .ok()
        .flatten()
        .map(|m| m.scale_factor())
        .unwrap_or(1.0);
    #[cfg(target_os = "windows")]
    {
        if let Some((l, t, r, b)) = primary_work_area() {
            let w = (r - l) as f64 / sf;
            let h = (b - t) as f64 / sf;
            return (w - FDE_UPD_W - 16.0, h - FDE_UPD_H - 8.0);
        }
    }
    if let Ok(Some(m)) = app.primary_monitor() {
        let sf2 = m.scale_factor();
        let s = m.size();
        let p = m.position();
        return (
            (p.x as f64 + s.width as f64) / sf2 - FDE_UPD_W - 16.0,
            (p.y as f64 + s.height as f64) / sf2 - FDE_UPD_H - 8.0 - 56.0,
        );
    }
    (0.0, 0.0)
}

/// 显示/刷新升级进度小窗。eta < 0 表示还估不出来（前端渲染成「通常还需 20-40 秒」）；
/// got/total 为已下载/总字节，仅在下载阶段有意义（total=0 表示服务端没给 Content-Length）。
fn fde_upd_show_impl(
    app: &tauri::AppHandle,
    stage: &str,
    pct: u32,
    el: u64,
    eta: i64,
    ver: &str,
    got: u64,
    total: u64,
) {
    // 记住目标版本：Rust 侧（无 Channel 的 _nc 模式）后续推阶段时带回去，标题不丢 vX.Y.Z
    if !ver.is_empty() {
        if let Ok(mut g) = UPD_VER.lock() {
            *g = Some(ver.to_string());
        }
    }
    let (x, y) = fde_upd_pos(app);

    if let Some(w) = app.get_webview_window("fde-upd") {
        // 已存在：原地更新（进度每 0.5-1s 刷一次，整窗 reload 会白闪）
        let js = format!(
            "window.__updUpdate && window.__updUpdate({}, {}, {}, {}, {}, {}, {})",
            js_str(stage),
            pct,
            el,
            eta,
            js_str(ver),
            got,
            total
        );
        let _ = w.eval(&js);
        // 🔴 2026-10-26 去抖（配合下载进度节流，双保险）：
        // 旧实现每次调用都 set_position + show 各一次，属**窗口消息**；
        // 下载期每秒上百次 → 窗口消息循环被打满，主窗口拖动直接卡死。
        // 现在：坐标没变就不挪（拖动主窗口时浮窗才需要跟着挪），已可见就不再 show。
        let key = (x.round() as i32, y.round() as i32);
        let moved = UPD_FLAST_POS
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .map(|mut g| {
                let changed = *g != Some(key);
                *g = Some(key);
                changed
            })
            .unwrap_or(true);
        if moved {
            let _ = w.set_position(tauri::LogicalPosition::new(x, y));
        }
        if !w.is_visible().unwrap_or(false) {
            let _ = w.show();
        }
        return;
    }

    let url = format!(
        "https://im.x8fde.com/app/fde-upd.html?stage={}&pct={}&el={}&eta={}&ver={}&got={}&total={}",
        url_encode(stage),
        pct,
        el,
        eta,
        url_encode(ver),
        got,
        total
    );
    let parsed = match url.parse() {
        Ok(u) => u,
        Err(_e) => return,
    };
    if let Ok(w) = tauri::WebviewWindowBuilder::new(
        app,
        "fde-upd",
        tauri::WebviewUrl::External(parsed),
    )
    .title("邦信升级中")
    .inner_size(FDE_UPD_W, FDE_UPD_H)
    .position(x, y)
    .decorations(false)
    .transparent(true)
    .always_on_top(true)
    .skip_taskbar(true)
    .focused(false)
    .resizable(false)
    .shadow(false)
    .visible(false)
    .build()
    {
        let _ = w.show();
    }
}

/// 前端接口：显示/刷新「升级中」进度小窗。
/// 参数全部可缺省（老前端不传也能用）：stage/pct/el/eta/ver/got/total。
#[tauri::command]
fn upd_float_show(
    app: tauri::AppHandle,
    stage: Option<String>,
    pct: Option<u32>,
    el: Option<u64>,
    eta: Option<i64>,
    ver: Option<String>,
    got: Option<u64>,
    total: Option<u64>,
) -> Result<(), String> {
    fde_upd_show_impl(
        &app,
        &stage.unwrap_or_else(|| "checking".to_string()),
        pct.unwrap_or(0),
        el.unwrap_or(0),
        eta.unwrap_or(-1),
        &ver.unwrap_or_default(),
        got.unwrap_or(0),
        total.unwrap_or(0),
    );
    Ok(())
}

/// 前端接口：收起升级进度小窗（升级失败 / 已是最新 / 用户取消时调用）。
#[tauri::command]
fn upd_float_hide(app: tauri::AppHandle) {
    // 浮窗下次再现时必须重新落位（去抖缓存作废），否则会停在旧坐标。
    if let Ok(mut g) = UPD_FLAST_POS
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
    {
        *g = None;
    }
    if let Some(w) = app.get_webview_window("fde-upd") {
        let _ = w.hide();
    }
}

/// 标题栏原生拖动（2026-10-24 老8 报「远程虚拟机上窗口又拖不动了」）。
///
/// **旧链路的病根**：前端 `startDragging()` → IPC → wry 执行
/// `ReleaseCapture() + SendMessageW(WM_NCLBUTTONDOWN, HTCAPTION)`，把拖动交给系统的
/// 非客户区移动循环。该循环依赖「鼠标仍处于按下状态」，而 IPC 是**异步**的：本机往返
/// 1~3ms 无感，RDP 远程桌面 / 虚拟机下常达几十~上百毫秒，等命令到达时用户早已 mouseup
/// → 系统进入循环后立刻退出 → 表现为「完全拖不动」。同时前端为防双触发做了
/// `stopImmediatePropagation`，把 tauri 内置 drag.js 那条路也堵死，等于无退路。
///
/// **这里的做法**：不依赖系统移动循环，自己跟随鼠标。
///   GetCursorPos 取光标 → 与上一帧比较得增量 → SetWindowPos 平移窗口 →
///   GetAsyncKeyState(VK_LBUTTON) 判左键是否仍按下，抬起即退出。
/// 即使 IPC 迟到 200ms，也只是一开始晚跟一下，之后始终跟手，不会再「拖不动」。
///
/// 最大化状态下的还原改为「拖动阈值门」（2026-10-25 老8 报「最大化窗口单击标题栏
/// 就被还原，应按传统习惯：点还原按钮或双击才还原」）：此前 mousedown 一进来就
/// `unmaximize()`，纯单击（无拖动意图）也会触发还原。对齐 Windows 原生习惯——
/// 拖最大化窗口时光标位移超出系统拖拽阈值（SM_CXDRAG/SM_CYDRAG，默认 4px）才算
/// 「真的在拖」，此时才还原并继续跟手；纯单击（按下即抬起 / 光标未动）窗口保持
/// 最大化不变。还原仍只认：还原按钮 / 双击标题栏 / 拖出阈值。
#[tauri::command]
async fn drag_begin(window: tauri::WebviewWindow) -> Result<(), String> {
    #[cfg(windows)]
    {
        let hwnd = match window.hwnd() {
            Ok(h) => h.0 as isize,
            Err(e) => return Err(e.to_string()),
        };
        std::thread::spawn(move || {
            use windows_sys::Win32::Foundation::{POINT, RECT};
            use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                GetCursorPos, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, SetWindowPos,
                ShowWindow, GWL_STYLE, SM_CXDRAG, SM_CYDRAG, SWP_NOACTIVATE, SWP_NOSIZE,
                SWP_NOZORDER, SW_RESTORE, WS_MAXIMIZE,
            };
            unsafe {
                let h = hwnd as *mut core::ffi::c_void;
                // 拖拽阈值（系统级，通常 4px）：未超阈值不算拖动
                let thr_x = GetSystemMetrics(SM_CXDRAG).max(4);
                let thr_y = GetSystemMetrics(SM_CYDRAG).max(4);
                let mut p = POINT { x: 0, y: 0 };
                if GetCursorPos(&mut p) == 0 {
                    return;
                }
                let (mut lx, mut ly) = (p.x, p.y);
                // 初始窗口位置：纯 Win32 GetWindowRect，避免子线程调 Tauri 窗口方法
                // （is_maximized/unmaximize/outer_position 在 spawned 线程上不可靠，会挂起/失败）
                let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
                if GetWindowRect(h, &mut rect) == 0 {
                    return;
                }
                let (mut wx, mut wy) = (rect.left, rect.top);
                // 是否最大化：读窗口样式（纯 Win32，子线程安全）
                let style = GetWindowLongPtrW(h, GWL_STYLE) as u32;
                let maximized = style & WS_MAXIMIZE != 0;
                if maximized {
                    // 阈值门：等光标移出阈值（→ 还原并继续拖）或左键抬起（→ 原样退出，保持最大化）
                    loop {
                        if (GetAsyncKeyState(VK_LBUTTON as i32) as u16) & 0x8000 == 0 {
                            return;
                        }
                        let mut c = POINT { x: 0, y: 0 };
                        if GetCursorPos(&mut c) == 0 {
                            return;
                        }
                        if (c.x - lx).abs() >= thr_x || (c.y - ly).abs() >= thr_y {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(8));
                    }
                    // 纯 Win32 还原（SW_RESTORE），不依赖 Tauri unmaximize（子线程会挂起/失败）
                    ShowWindow(h, SW_RESTORE);
                    // 还原是异步的（窗口尺寸/位置在下一帧才生效），先让它落地再校准
                    std::thread::sleep(std::time::Duration::from_millis(90));
                    let mut r2 = RECT { left: 0, top: 0, right: 0, bottom: 0 };
                    if GetWindowRect(h, &mut r2) != 0 {
                        wx = r2.left;
                        wy = r2.top;
                        lx = p.x;
                        ly = p.y;
                    }
                }
                loop {
                    // 左键已抬起 → 结束（GetAsyncKeyState 高位为按下）
                    if (GetAsyncKeyState(VK_LBUTTON as i32) as u16) & 0x8000 == 0 {
                        break;
                    }
                    let mut c = POINT { x: 0, y: 0 };
                    if GetCursorPos(&mut c) == 0 {
                        break;
                    }
                    let (dx, dy) = (c.x - lx, c.y - ly);
                    if dx != 0 || dy != 0 {
                        wx += dx;
                        wy += dy;
                        SetWindowPos(
                            h,
                            std::ptr::null_mut(),
                            wx,
                            wy,
                            0,
                            0,
                            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                        lx = c.x;
                        ly = c.y;
                    }
                    // 8ms ≈ 125Hz，比 60Hz 屏还密一档；增量式移动不会累积误差
                    std::thread::sleep(std::time::Duration::from_millis(8));
                }
            }
        });
    }
    #[cfg(not(windows))]
    {
        // 非 Windows 仍由前端走 tauri 的 startDragging（mac 无此问题）
        let _ = window;
        return Err("unsupported".to_string());
    }
    #[cfg(windows)]
    Ok(())
}

/// 2026-10-25 标题栏窗控加固：原 WinTitleBar 直接调 `window.__TAURI__.window.getCurrentWindow()`
/// 的 `.window` 全局命名空间做 minimize/maximize/close。实测该命名空间在部分构建的运行时不可用
/// （`core` 可用、`window` 不可用），导致「app 其它功能正常、但标题栏放大/缩小/关闭全失效」。
/// 改为经 Rust 命令 + 已验证可靠的 `core.invoke` 通道执行窗控，彻底绕开 `.window` 命名空间。
#[tauri::command]
fn win_minimize(app: tauri::AppHandle) -> Result<(), String> {
    app.get_webview_window("main")
        .map(|w| w.minimize().map_err(|e| e.to_string()))
        .unwrap_or(Err("主窗口未找到".into()))
}
#[tauri::command]
fn win_toggle_maximize(app: tauri::AppHandle) -> Result<(), String> {
    let w = app.get_webview_window("main").ok_or("主窗口未找到".to_string())?;
    // WebviewWindow 无 toggle_maximize 方法，手动判断后切换
    let is_max = w.is_maximized().map_err(|e| e.to_string())?;
    if is_max {
        w.unmaximize().map_err(|e| e.to_string())
    } else {
        w.maximize().map_err(|e| e.to_string())
    }
}
#[tauri::command]
fn win_close(app: tauri::AppHandle) -> Result<(), String> {
    app.get_webview_window("main")
        .map(|w| w.close().map_err(|e| e.to_string()))
        .unwrap_or(Err("主窗口未找到".into()))
}
#[tauri::command]
fn win_is_maximized(app: tauri::AppHandle) -> Result<bool, String> {
    app.get_webview_window("main")
        .map(|w| w.is_maximized().map_err(|e| e.to_string()))
        .unwrap_or(Err("主窗口未找到".into()))
}

/// 让主窗的任务栏按钮闪烁（Windows 原生，微信式：闪到用户点开窗口为止）。
///
/// 2026-10-23 老8 实测反馈：「托盘图标会闪，任务栏按钮不闪，微信是两个一起闪」。
/// 此前只走 Tauri 的 `request_user_attention(Critical)`，它经 Tauri → tao 多层转发，
/// 最终也落到 `FlashWindowEx`，但在远程 URL 窗口上实测没有观感效果。
/// 这里改为对主窗 HWND **直接**调 Windows 原生 API，与托盘闪烁同源触发：
///   - `FLASHW_ALL`      = 闪标题栏 + 闪任务栏按钮（缺了它按钮不会亮）
///   - `FLASHW_TIMERNOFG`= 持续闪，直到窗口被激活（不是闪几下就停）
/// 窗口已经是前台时跳过：前台时系统在语义上就不闪，闪了也没意义。
#[cfg(target_os = "windows")]
fn flash_taskbar(w: &tauri::WebviewWindow) {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FlashWindowEx, GetForegroundWindow, GetWindowTextW, GetWindowLongPtrW, IsWindowVisible,
        FLASHWINFO, FLASHW_ALL, FLASHW_TIMERNOFG, GWL_EXSTYLE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    };
    let hwnd = match w.hwnd() {
        Ok(h) => h,
        Err(e) => {
            log_flash(&format!("!! WebviewWindow::hwnd() 失败: {e}"));
            return;
        }
    };
    unsafe {
        // 现场快照：窗口存在 ≠ 有任务栏按钮。把标题/可见性/前台归属一并记下，
        // 才能区分「API 被调用了但被系统忽略」和「根本闪错了窗口」。
        let mut title = [0u16; 256];
        // ⚠️ 统一传裸指针 hwnd.0：windows-sys 0.59 虽然把 HWND 声明成 newtype 结构体，
        //    但 link! 宏展开后这些 API 的实参类型实为 *mut c_void，直接传 HWND 会 E0308。
        let len = GetWindowTextW(hwnd.0, title.as_mut_ptr(), 256);
        let t = String::from_utf16_lossy(&title[..len.max(0) as usize]);
        let visible = IsWindowVisible(hwnd.0) != 0;
        // 任务栏按钮存在的前提：窗口不能带 WS_EX_TOOLWINDOW。记下扩展样式位，
        // 一眼就能判断「窗口看得见但任务栏没按钮」这类根因。
        let ex = GetWindowLongPtrW(hwnd.0, GWL_EXSTYLE);
        let is_tool = (ex as u32 & WS_EX_TOOLWINDOW) != 0;
        let is_appwin = (ex as u32 & WS_EX_APPWINDOW) != 0;
        let fg = GetForegroundWindow();
        // HWND 在 windows-sys 0.59 里是 newtype 结构体（非裸指针别名），
        // 而 GetForegroundWindow() 与 FLASHWINFO.hwnd 都是裸 *mut c_void，故统一取 .0。
        if fg == hwnd.0 {
            log_flash(&format!(
                "[跳过·窗口在前台] hwnd={:p} title={} visible={}",
                hwnd.0, t, visible
            ));
            return;
        }
        let mut f: FLASHWINFO = std::mem::zeroed();
        f.cbSize = std::mem::size_of::<FLASHWINFO>() as u32;
        f.hwnd = hwnd.0;
        f.dwFlags = FLASHW_ALL | FLASHW_TIMERNOFG;
        f.uCount = 0;
        f.dwTimeout = 0;
        let r = FlashWindowEx(&f);
        log_flash(&format!(
            "hwnd={:p} title={} visible={} fg={:p} exStyle=0x{:X}(tool={} appwin={}) cbSize={} flags=0x{:X} FlashWindowEx 返回={} lasterr={}",
            hwnd.0, t, visible, fg, ex as u32, is_tool, is_appwin, f.cbSize, f.dwFlags, r,
            GetLastError()
        ));
    }
}

/// 任务栏闪烁诊断日志（2026-10-23）：两路 API（Tauri 通道 + 原生 FlashWindowEx）都无观感
/// 效果，必须把 API 现场落盘才能区分是「被系统忽略」还是「闪错了窗口」。
/// 写到 %LOCALAPPDATA%\BangXin\flash_debug.log，仅追加、不做轮转（诊断完即可撤掉）。
#[cfg(target_os = "windows")]
fn log_flash(msg: &str) {
    let dir = match std::env::var("LOCALAPPDATA") {
        Ok(v) => std::path::PathBuf::from(v),
        Err(_) => return,
    };
    let p = dir.join("BangXin").join("flash_debug.log");
    let _ = std::fs::create_dir_all(dir.join("BangXin"));
    // 2026-10-24 P2-RS-3：照 si_log 范式加 32KB 上限，防止诊断日志无限增长
    if let Ok(md) = std::fs::metadata(&p) {
        if md.len() > 32 * 1024 {
            let _ = std::fs::remove_file(&p);
        }
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
    {
        let _ = f.write_all(format!("{secs} {msg}\n").as_bytes());
    }
}

/// 新消息提醒（2026-10-14 老8 定）：有消息时右下角要有提示。
/// - 任务栏按钮持续闪烁：Windows 走 `FlashWindowEx(FLASHW_TIMER)`，**闪到窗口获得焦点为止**，
///   拿到焦点由系统自动停，无需手动取消；macOS 是 Dock 图标弹跳一次；Linux tao 未实现 → 自动降级为无动作。
/// - 托盘 tooltip 顺带显示未读数：`邦信 · 3 条未读`（count=0 时还原为默认文案）。
/// - 托盘图标角标（2026-10-15 老8 定）：有未读时整个图标换成「红底 + 未读数」，
///   窗口藏进托盘后任务栏按钮都没了，闪烁看不见 —— 图标数字是唯一常驻可见的提醒。
/// - 托盘图标闪烁（2026-10-22 老8 定）：有新消息且窗口不在前台时，图标每 500ms 在
///   「红底未读数 ↔ 默认 logo」间整图切换（QQ/微信式红白闪）；窗口回前台 / 未读清零即停。
/// 参数：
///   count  当前总未读数（tooltip 文案 + 图标角标）
///   flash  是否要求闪一下。传 None 时按「有未读就闪」处理。
///         显式传 true 用于「消息正好落在当前打开的会话里」——此时未读数不变（不会变红），
///         但窗口在后台时人根本看不见，同样需要任务栏提醒。
/// 窗口当前已是前台可见且聚焦时：任务栏不闪（人就在看着，不该打扰），只更新 tooltip；
/// 托盘图标闪烁循环虽会启动，但首 tick 自查焦点即自停，视觉上不会出现任何闪烁。
///
/// 2026-10-19 老8 追加：任务栏闪烁不够「提醒」——窗口在后台时看不见。改为
///   ①托盘气泡提示（Windows：Shell_NotifyIcon 气泡，Win10/11 渲染成右下角通知卡片，
///     标题=会话名、正文=发送人：内容，带系统提示音）
///   ②主窗置顶抢到最前端 4 秒后自动取消置顶（不永久钉住，不改变用户窗口状态）
///   title/body 由前端传入（会话名 / 发送人：内容），缺省时退化为 N 条未读。
#[tauri::command]
fn notify_unread(
    app: tauri::AppHandle,
    count: i64,
    flash: Option<bool>,
    title: Option<String>,
    body: Option<String>,
    in_current: Option<bool>,
) -> Result<(), String> {
    // 2026-11-16：count = -1 表示「只提醒、不改未读数字」。
    // 场景：消息落在用户正开着的会话里，真实未读不涨；旧前端用 count=1 冒充，
    // 会把托盘 tooltip 写成「1 条未读」且一直挂着（假未读）。传 -1 则完全不动 tooltip。
    let alert_only = count < 0;
    let n = count.max(0);
    let def_icon = Some(tray_icon_image());
    if !alert_only {
        if let Some(tray) = app.tray_by_id("x8im-tray") {
            let tip = if n > 0 {
                format!("邦信 · {n} 条未读")
            } else {
                "邦信即时通讯".to_string()
            };
            let _ = tray.set_tooltip(Some(tip));
            if n > 0 {
                // 新消息：托盘图标**不换**红点角标（老8 2026-10-23：用原图标一暗一亮即好），
                // 保持默认 logo；真正抢眼交给下面的闪烁循环 + 右下角浮窗 + tooltip。
                if let Some(ic) = def_icon.clone() {
                    let _ = tray.set_icon(Some(ic));
                }
            } else {
                // 未读清零：停掉可能在闪的托盘图标循环，再把图标还原默认 logo；
                // 右下角 AI邦 精灵也一并收起（人已经处理完了，精灵再挂着就是噪音）
                stop_tray_flash();
                fde_float_close(&app);
                if let Some(ic) = def_icon {
                    let _ = tray.set_icon(Some(ic));
                }
            }
        }
    }
    let want_flash = flash.unwrap_or(alert_only || n > 0);
    if want_flash {
        if let Some(w) = app.get_webview_window("main") {
            // 真实 OS 窗口状态（Rust 侧权威）：失焦 / 最小化 / 隐藏 任一都算「人没在看」。
            // ⚠️ 前端 document.hasFocus()/visibilityState 反映的是 WebView 内容焦点，
            //    测不到 Tauri 窗口被最小化，绝不能信——离开判据只能以 Rust 的窗口状态为准
            //    （2026-10-22 实测：最小化时前端 hasFocus 仍 true，导致打开着的会话不闪）。
            let away = !w.is_focused().unwrap_or(true)
                || !w.is_visible().unwrap_or(true)
                || w.is_minimized().unwrap_or(false);
            // 2026-11-16 老8「看不到 AI邦」：旧判据只认窗口状态 —— 只要主窗在前台，
            // 任何会话的新消息都被当成「人正在看」，精灵/气泡/闪烁一律不弹，只剩左侧小红点。
            // 改为：消息不属于当前正在看的会话（in_current=false）同样要提醒；
            // 当前会话的消息仍然完全静默（自己正聊的那条不该反复骚扰）。
            let alert = away || !in_current.unwrap_or(true);
            if alert {
                // 提醒用的计数：alert_only 时不碰真实未读，用占位 1 驱动闪烁与精灵
                let cn = if alert_only { 1 } else { n };
                if cn > 0 {
                    // 2026-10-22：托盘图标「一暗一亮」闪烁，闪到窗口回前台 / 已读清零为止。
                    start_tray_flash(app.clone(), cn);
                    // 2026-11-16：AI邦 精灵（原 FDE 浮窗）—— 有未读就停在屏上，不再 9.2s 自动消失
                    let ftip = title
                        .clone()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| {
                            if alert_only {
                                "新消息".to_string()
                            } else {
                                format!("{cn} 条新消息")
                            }
                        });
                    fde_float_show_impl(&app, cn, &ftip, false);
                }
                // 新消息：弹气泡 + 闪任务栏/托盘，但**绝不** bring_to_front 抢前台——
                // 消息到达就抢焦点会让任务栏闪烁（需失焦才闪）失去意义，也违背微信式体验。
                // 2026-11-16：气泡只在人真的离开时弹。主窗在前台（只是不在那个会话）时
                // 精灵已经在右下角占位并响了提示音，再叠一张系统通知卡片会压在一起、
                // 既看不清也吵；声音与文案由精灵承担即可。
                if cn > 0 && away {
                    #[cfg(target_os = "windows")]
                    {
                        let t = title
                            .clone()
                            .filter(|s| !s.trim().is_empty())
                            .unwrap_or_else(|| "邦信新消息".to_string());
                        let b = body
                            .clone()
                            .filter(|s| !s.trim().is_empty())
                            .unwrap_or_else(|| {
                                if alert_only {
                                    "收到一条新消息".to_string()
                                } else {
                                    format!("{cn} 条未读消息")
                                }
                            });
                        win_tray_balloon(&t, &b);
                    }
                    // mac/linux 无托盘气泡 API，靠 Dock 弹跳 / 任务栏闪烁；这里不重复处理
                    #[cfg(not(target_os = "windows"))]
                    let _ = (&title, &body);
                }
                // 2026-10-23 老8「任务栏按钮不闪」：Critical = FLASHW_ALL|FLASHW_TIMER，
                // **一直闪到窗口被激活为止**（微信式，旧 Informational 只闪几下就停）。
                // 实测 Tauri 通道在本机没产生观感效果，故 Windows 上再补一次原生直调
                // （见 flash_taskbar 注释）；两者都指向同一个 FlashWindowEx，重复调用幂等。
                // ⚠️ 这里不能用 `?` 把失败冒泡出去：托盘闪烁与右下角气泡都已生效，
                //    任务栏闪不了不该让整个命令返回失败，前端会误判成「提醒全没做成」。
                // 2026-11-16：只在人真的离开时才抢注意力；主窗在前台时只弹精灵与气泡，
                // 否则窗口明明开着却闪任务栏 / 抢置顶，反而是干扰。
                if away {
                    let _ = w.request_user_attention(Some(UserAttentionType::Critical));
                    #[cfg(target_os = "windows")]
                    flash_taskbar(&w);
                }
            }
        }
    }
    Ok(())
}

/// 返回主窗是否「真正在前台且人在看」（2026-10-23 老8：已读回执不能靠前端
/// document.hasFocus 判定 —— WebView2 在窗口被别的窗口盖住 / 最小化时个别版本仍
/// 返回 true，导致后台窗口也被标已读）。用 OS 级窗口状态为权威口径，与 notify_unread
/// 的 away 判据同源（focused && visible && !minimized）。
#[tauri::command]
fn win_is_focused(app: tauri::AppHandle) -> bool {
    match app.get_webview_window("main") {
        Some(w) => {
            w.is_focused().unwrap_or(false)
                && w.is_visible().unwrap_or(false)
                && !w.is_minimized().unwrap_or(false)
        }
        None => false,
    }
}

/// 「离开提醒」专用命令（2026-10-22 老8 反馈「打开着的会话最小化也不闪」）：
/// 消息正好落在用户**正开着的当前会话**里，未读数不涨（不会变红），可一旦窗口已
/// 失焦/最小化/藏托盘，人根本看不见那条消息——此时也要闪托盘 + 弹气泡。
///
/// 为什么单独一个命令而不复用 notify_unread：当前会话场景未读数是 0，传 0 会被
/// notify_unread 当成「已读清零」去 stop_tray_flash 并还原图标，反而把提醒灭了。
/// 且「窗口是否离开」只有 Rust 的 OS 级窗口状态判得准（前端 hasFocus 测不到最小化），
/// 故判据放这里；前台聚焦时本命令直接返回、什么都不做（人正在看，不打扰）。
#[tauri::command]
fn away_notice(app: tauri::AppHandle, title: Option<String>, body: Option<String>) {
    let away = match app.get_webview_window("main") {
        Some(w) => !w.is_focused().unwrap_or(true)
            || !w.is_visible().unwrap_or(true)
            || w.is_minimized().unwrap_or(false),
        None => true,
    };
    if !away {
        return; // 人在看，不打扰
    }
    // 占位未读 1 驱动闪烁循环（真正停闪靠 show_main / notify_unread(0)）。
    // 只用 logo 一暗一亮，不带角标。
    start_tray_flash(app.clone(), 1);
    // 浮窗同理不带角标，文案优先用消息正文（发送人：内容），缺省退化为会话名
    let ftip = body
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| title.clone())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "收到一条新消息".to_string());
    fde_float_show_impl(&app, 0, &ftip, true);
    // 2026-11-16：当前会话来消息时未读数是 0，fde_float_show_impl 不会自己响，这里补一声
    if !pet_muted() {
        pet_sound();
    }
    #[cfg(target_os = "windows")]
    {
        let t = title
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "邦信新消息".to_string());
        let b = body
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "收到一条新消息".to_string());
        win_tray_balloon(&t, &b);
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.request_user_attention(Some(UserAttentionType::Critical));
        // 同上：Windows 上补一次原生 FlashWindowEx 直调，保证任务栏按钮真的会闪。
        #[cfg(target_os = "windows")]
        flash_taskbar(&w);
    }
}
/// 检测本机微信是否正在运行（2026-10-17 微信快捷登录用）：
/// 本机微信在线时，前端提示「可用 PC 微信确认」并高亮快捷登录按钮。
/// - Windows：查 Weixin.exe / WeChat.exe 进程
/// - macOS：查微信.app 进程
/// - Linux：官方无客户端，恒返回 false（走扫码兜底）
/// 进程查询跑在 spawn_blocking（阻塞线程池）执行阻塞 Command，不占 tokio worker。
/// 2026-10-24 P2-RS-2：旧实现 std::thread::spawn().join() —— 白开一条线程且 join 本身仍阻塞
/// worker，纯属无谓开销；直接 spawn_blocking 跑 tasklist 查询即可。
#[tauri::command]
async fn detect_wechat() -> bool {
    tauri::async_runtime::spawn_blocking(|| {
        #[cfg(target_os = "windows")]
        {
            for name in ["Weixin.exe", "WeChat.exe"] {
                // tasklist.exe 同为控制台程序：CREATE_NO_WINDOW 否则每次检测闪一下黑窗
                // （2026-10-22 与 download_file curl 同批修复）。
                if let Ok(o) = Command::new("tasklist")
                    .args(["/FI", &format!("IMAGENAME eq {name}"), "/NH"])
                    .creation_flags(0x0800_0000)
                    .output()
                {
                    if String::from_utf8_lossy(&o.stdout).contains(name) {
                        return true;
                    }
                }
            }
            false
        }
        #[cfg(target_os = "macos")]
        {
            // 🔴 2026-11-16 Apple 吊销整改：原用完整路径 `WeChat.app/Contents/MacOS/WeChat`
            // 做 pgrep -f 匹配，该字符串会留在主二进制里被公证扫描器读到。
            // 改为按进程名精确匹配（macOS 微信进程名恒为 WeChat）——功能等价，
            // 但不再把第三方 app 的内部路径写进我们的签名包。
            Command::new("pgrep")
                .args(["-x", "WeChat"])
                .output()
                .map(|o| !o.stdout.is_empty())
                .unwrap_or(false)
        }
        #[cfg(target_os = "linux")]
        {
            false
        }
    })
    .await
    .unwrap_or(false)
}

/// Windows 静默自动升级（2026-10-22，用户原话「window也要尝试这种静默升级，自动升级后
/// 启动。不需要任何人为参与」——即零点击、零 UAC、零终端）。
///
/// 全链路（权限视角）：
///   app（普通/管理员态） updater.check() → download() 内部 minisign 验签（updater.rs
///   verify_signature，拿到的字节即可信）→ 落盘 %LOCALAPPDATA%\BangXin\updates\
///   → schtasks /Run 触发安装期注册的计划任务 "BangXin AutoUpdate"（普通权限即可触发
///   自己名下的任务，不弹 UAC）→ 任务以最高权限跑 C:\ProgramData\BangXin\updater-task.cmd
///   → 安装器 /S /UPDATE /R：先 taskkill 本进程，装完经 explorer 中转重启 app
///   （explorer 以交互用户中等完整性令牌运行，重启的 app 拿回普通令牌，不常驻管理员态）。
///
/// 与插件原生 install() 的区别：install() = ShellExecute(安装器) + exit(0)，从普通权限
/// 进程 ShellExecute 本安装器必弹 UAC（用户必须参与）。计划任务把提权点挪到安装期
/// （已过一次 UAC），此后升级全程无人参与。
///
/// 前置条件：安装器 1.5.28+ 已注册计划任务（installer.nsh POSTINSTALL step 9）。
/// 老客户端没有该任务 → 预检直接 Err，前端回落手动通道，绝不白下几十 MB。
///
/// 安全边界：.cmd 只执行「app 自己下载并验签后」放入 updates\ 的最新包；普通用户既不能
/// 写 .cmd（C:\ProgramData\BangXin 继承 ProgramData ACL，仅管理员可写）也不能触发任务
/// （任务属安装时那位管理员）。
#[cfg(windows)]
const X8_TASK_NAME: &str = "BangXin AutoUpdate";
/// 2026-10-25 品牌迁移前的旧任务名（老机器上可能还存在，仅作兜底探测）
const LEGACY_TASK_NAME: &str = "X8.IM AutoUpdate";

/// 更新暂存目录：%ProgramData%\BangXin\updates
/// 🔴 2026-10-25 根因修复：原先用 %LOCALAPPDATA%\BangXin\updates，但计划任务以
///   Highest 权限 + Interactive 登录类型运行时，%LOCALAPPDATA% 会被解析成
///   C:\ProgramData（而非 C:\Users\<user>\AppData\Local）。结果 Rust 客户端（普通用户态）
///   把包落到 C:\Users\<user>\AppData\Local\BangXin\updates\，而 updater-task.cmd
///   （Highest 态）却去 C:\ProgramData\BangXin\updates\ 找包 —— 两边永远对不上，
///   cmd 找不到包 → 升级静默失败（老8 真机「升级 10 多次一直不成功」的根因）。
///   统一改用 %ProgramData%（任何上下文都稳定解析为 C:\ProgramData），Rust 落盘与
///   cmd 读包用同一目录，路径一致。
/// 刻意避开 main() setup 里 Windows 启动即删的 local_data_dir/app_data_dir
/// （identifier com.x8fde.im 那两个目录）——用 BangXin 这个独立目录，包才留得到安装器跑。
#[cfg(windows)]
#[derive(Clone)]
struct TaskCtx {
    /// 计划任务名（触发用）
    name: &'static str,
    /// 更新暂存目录（.cmd 会从这里找包）
    updates: std::path::PathBuf,
    /// 包文件名前缀，须与 .cmd 的 glob 一致
    prefix: &'static str,
}

/// 定位 %ProgramData%（操作系统「已知文件夹」级路径）。
///
/// 🔴 2026-11-05 老8：「`ProgramData 环境变量缺失，无法定位更新目录` 彻底根掉」。
/// 原来只读 `std::env::var("ProgramData")` —— 进程环境块不完整时（计划任务 / 服务上下文 /
/// 精简启动器 / 企业管控清变量）直接 Err → task_ctx() 整体失败 → 自主升级链路整个停摆，
/// 日志里只有一句「环境变量缺失」，既看不出是环境问题还是权限问题，也没有任何自救。
/// 现在三级解析，任一级拿到**真实存在**的目录就收工：
///   ① 环境变量（最常见、零成本，命中即返回）
///   ② SHGetKnownFolderPath(FOLDERID_ProgramData) —— 系统权威来源，完全不看环境块
///   ③ %SystemDrive%\ProgramData、C:\ProgramData（最后兜底）
/// 刻意不回落到 %LOCALAPPDATA%：Highest 权限上下文里它会被解析成 C:\ProgramData，
/// 正是 2026-10-25 那次「Rust 落盘与 .cmd 读包对不上」的老坑。宁可明确报错也不暗中错位。
#[cfg(windows)]
pub(crate) fn programdata_dir() -> Result<std::path::PathBuf, String> {
    // ① 环境变量
    if let Ok(v) = std::env::var("ProgramData") {
        let v = v.trim();
        if !v.is_empty() {
            let p = std::path::PathBuf::from(v);
            if p.is_dir() {
                return Ok(p);
            }
        }
    }
    // ② Shell 已知文件夹（不看环境块）
    if let Some(p) = programdata_via_shell() {
        if p.is_dir() {
            return Ok(p);
        }
    }
    // ③ 系统盘兜底
    let mut cands: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(sd) = std::env::var("SystemDrive") {
        let sd = sd.trim();
        if !sd.is_empty() {
            cands.push(std::path::PathBuf::from(format!("{}\\ProgramData", sd)));
        }
    }
    cands.push(std::path::PathBuf::from(r"C:\ProgramData"));
    for p in cands {
        if p.is_dir() {
            return Ok(p);
        }
    }
    Err("无法定位 %ProgramData%（环境变量 / Shell 已知文件夹 / 系统盘均未命中）".to_string())
}

/// SHGetKnownFolderPath(FOLDERID_ProgramData) 的最小封装。失败返回 None，绝不 panic。
#[cfg(windows)]
fn programdata_via_shell() -> Option<std::path::PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{SHGetKnownFolderPath, FOLDERID_ProgramData, KF_FLAG_DEFAULT};
    unsafe {
        let mut raw: *mut u16 = std::ptr::null_mut();
        // htoken = NULL → 当前用户/进程上下文；KF_FLAG_DEFAULT = 0（不校验注册表重定向）
        let hr = SHGetKnownFolderPath(
            &FOLDERID_ProgramData,
            KF_FLAG_DEFAULT as u32,
            std::ptr::null_mut(),
            &mut raw,
        );
        if hr < 0 || raw.is_null() {
            return None;
        }
        let mut n = 0usize;
        while *raw.add(n) != 0 {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(raw, n));
        CoTaskMemFree(raw as *mut core::ffi::c_void);
        let s = s.trim();
        if s.is_empty() {
            None
        } else {
            Some(std::path::PathBuf::from(s))
        }
    }
}

/// 解析本机升级体系：新品牌（BangXin）优先；若机器上只有品牌迁移前的旧任务，
/// 则整套回落到旧体系（旧任务 → 旧 .cmd → 旧目录 + 旧文件名）。
/// 三种必须配套，只换一半就是「点了没反应」的静默失败。
#[cfg(windows)]
fn task_ctx() -> Result<TaskCtx, String> {
    // 🔴 2026-10-25：改用 %ProgramData%（而非 LOCALAPPDATA），与 updater-task.cmd 的
    //   %ProgramData% 展开结果一致，杜绝 Highest 权限计划任务上下文里路径错位。
    // 🔴 2026-11-05：不再直接读环境变量，改走 programdata_dir() 的三级解析，环境变量缺失也能活。
    let pd = programdata_dir()?;
    let base = pd.as_path();
    let fresh = TaskCtx {
        name: X8_TASK_NAME,
        updates: base.join("BangXin").join("updates"),
        prefix: "BangXin",
    };
    let (c, _) = win_run_capture("schtasks", &["/Query", "/TN", X8_TASK_NAME, "/FO", "LIST"]);
    if c == 0 {
        return Ok(fresh);
    }
    let (c2, _) = win_run_capture("schtasks", &["/Query", "/TN", LEGACY_TASK_NAME, "/FO", "LIST"]);
    if c2 == 0 {
        return Ok(TaskCtx {
            name: LEGACY_TASK_NAME,
            updates: base.join("X8.IM").join("updates"),
            prefix: "X8.IM",
        });
    }
    Ok(fresh)
}

// ─────────────── 升级状态标记 / 半卸自愈（2026-11-17 C）───────────────
//
// 事故原型（老8 新电脑）：自动升级断在「卸掉旧版之后、装上新版之前」，
// `C:\Program Files\BangXin\x8im.exe` 没了、updates 里躺着刚下载的安装包 →
// 桌面快捷方式成死链接、托盘只剩残影、没有任何恢复入口，用户判定「程序死了」。
//
// 三层兜底，缺一不可：
//   ① installer.nsh PREINSTALL 不再整目录 RMDir（中断时旧 exe 还在，程序照样起得来）
//   ② installer.nsh 生成的 updater-task.cmd：装完**校验 x8im.exe 是否回来**，
//      没回来重试一次，仍没有就写 task-failed.txt + 弹提示并打开下载页
//   ③ 这里：触发前落「升级中」标记；启动后自检——成功则收尾清包，
//      连续 3 次升不上去就落 giveup 停止自动重试（防「下载→安装→失败」死循环）

#[cfg(windows)]
fn upgrade_dir() -> Option<std::path::PathBuf> {
    programdata_dir().ok().map(|p| p.join("BangXin"))
}
#[cfg(windows)]
fn upgrade_marker_path() -> Option<std::path::PathBuf> {
    upgrade_dir().map(|d| d.join("updating.json"))
}
#[cfg(windows)]
fn upgrade_giveup_path() -> Option<std::path::PathBuf> {
    upgrade_dir().map(|d| d.join("upgrade-giveup.txt"))
}
#[cfg(windows)]
fn upgrade_task_failed_path() -> Option<std::path::PathBuf> {
    upgrade_dir().map(|d| d.join("updates").join("task-failed.txt"))
}

/// 从 updates 里最新的包名解析目标版本（`BangXin_1.7.10_x64-setup.exe` / `-portable.zip`）。
#[cfg(windows)]
fn newest_update_target(updates: &std::path::Path) -> Option<String> {
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for e in std::fs::read_dir(updates).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("BangXin_")
            || !(name.ends_with("_x64-setup.exe") || name.ends_with("_x64-portable.zip"))
        {
            continue;
        }
        let ver = name.trim_start_matches("BangXin_").split('_').next()?.to_string();
        let t = e
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().map(|(bt, _)| t > *bt).unwrap_or(true) {
            best = Some((t, ver));
        }
    }
    best.map(|(_, v)| v)
}

#[cfg(windows)]
fn parse_ver(s: &str) -> (u32, u32, u32) {
    let mut it = s.trim().split('.');
    let a = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let b = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let c = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    (a, b, c)
}

/// 触发静默安装**之前**落标记：目标版本 + 第几次尝试。只增不减，成功才清。
#[cfg(windows)]
fn upgrade_mark_start(updates: &std::path::Path) {
    let Some(target) = newest_update_target(updates) else {
        return;
    };
    let Some(p) = upgrade_marker_path() else { return };
    let prev = std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let prev_target = prev
        .as_ref()
        .and_then(|v| v.get("target"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // 换了目标版本就重新计数（升到 1.7.11 不该继承 1.7.10 的失败次数）
    let attempts = if prev_target == target {
        prev.as_ref()
            .and_then(|v| v.get("attempts"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            + 1
    } else {
        1
    };
    let body = serde_json::json!({
        "target": target,
        "attempts": attempts,
        "ts": chrono::Local::now().to_rfc3339(),
    });
    let _ = std::fs::write(&p, body.to_string());
    nav_write(&format!("upgrade mark: target={target} attempts={attempts}"));
}

/// 启动后自检上次升级结果（代价是几行文件操作，直接放在后台线程里跑）。
#[cfg(windows)]
fn upgrade_selfcheck(app: &tauri::AppHandle) {
    let cur = app.package_info().version.to_string();
    let Some(marker) = upgrade_marker_path() else { return };
    let Ok(txt) = std::fs::read_to_string(&marker) else {
        return; // 没有标记 = 上次根本没触发过升级，正常
    };
    let v: serde_json::Value = serde_json::from_str(&txt).unwrap_or_default();
    let target = v.get("target").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let attempts = v.get("attempts").and_then(|x| x.as_u64()).unwrap_or(0);
    let (c, t) = (parse_ver(&cur), parse_ver(&target));
    if t.0 == 0 {
        let _ = std::fs::remove_file(&marker);
        return;
    }
    if c >= t {
        // 升级成功：清标记 + 清失败记录 + 清残留安装包（否则下个版本还会拿同一个包重装）
        let _ = std::fs::remove_file(&marker);
        if let Some(g) = upgrade_giveup_path() {
            let _ = std::fs::remove_file(g);
        }
        if let Some(f) = upgrade_task_failed_path() {
            let _ = std::fs::remove_file(f);
        }
        if let Some(u) = upgrade_dir().map(|d| d.join("updates")) {
            if let Ok(rd) = std::fs::read_dir(&u) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().to_string();
                    if n.starts_with("BangXin_") && (n.ends_with(".exe") || n.ends_with(".zip")) {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        }
        nav_write(&format!("upgrade ok: {cur} >= {target}，已清残留安装包"));
    } else {
        nav_write(&format!(
            "upgrade 未完成: 当前 {cur} < 目标 {target}（第 {attempts} 次）"
        ));
        if attempts >= 3 {
            if let Some(g) = upgrade_giveup_path() {
                let _ = std::fs::write(&g, format!("{target}\n"));
            }
            nav_write("upgrade: 连续 3 次未成功 → 落 upgrade-giveup.txt，停止自动重试");
        }
        // 标记**保留**：失败次数要跨重启累计，成功时才清
    }
}

/// 触发「应用补丁」：优先 `schtasks /Run`（普通权限、零 UAC）；
/// 失败则**降级为提权直跑** `updater-task.cmd`（弹一次 UAC，用户点「是」即可）。
///
/// 🔴 2026-10-26 老8 定（原 P1 建议落地）：此前「预检计划任务不存在 → 整条升级链路直接
/// 放弃」，禁用 schtasks / 任务未注册的机器（企业管控机、品牌迁移前的老客户端）
/// 100% 升不了级，表现＝发版了但客户端永远停在老版本。
/// 现在预检失败**只降级、不放弃**：包照样下载、验签、落盘；最后一公里改用提权直跑，
/// 最差情况也只是弹一次 UAC，而不是白下几十 MB 后报「请手动更新」。
#[cfg(windows)]
pub(crate) fn apply_patch_task(ctx: &TaskCtx) -> Result<(), String> {
    // 2026-11-17 C：动手之前先落「升级中」标记。之后本进程多半会被 taskkill 带走，
    // 标记是唯一能把「上次升级到哪个版本、试了几次」带到下一次启动的载体。
    upgrade_mark_start(&ctx.updates);
    let (code, out) = win_run_capture("schtasks", &["/Run", "/TN", ctx.name]);
    upd_log(&format!("触发补丁应用: schtasks /Run code={code} out={out}"));
    if code == 0 {
        return Ok(());
    }
    // 降级通道：提权直跑 updater-task.cmd（内容与计划任务的 /TR 等价，只是这次要 UAC）
    let script = match ctx.updates.parent() {
        Some(p) => p.join("updater-task.cmd"),
        None => return Err(format!("触发自动升级任务失败: {out}")),
    };
    if !script.exists() {
        return Err(format!(
            "自动升级组件未就绪（计划任务缺失且 {} 不存在），请手动重装最新版：{out}",
            script.display()
        ));
    }
    // 优先用随包自带的 elevate.exe 提权无窗启动 updater-task.cmd（SW_HIDE，不弹黑窗）；
    // 仅当该工具缺失（极老版本没带上）时，才回退到 PowerShell Start-Process -WindowStyle Hidden。
    //
    // 🔴 2026-11-13 老8：升级时弹出 DOS 窗口（看着不舒服）。根因在计划任务的动作本身
    // （`cmd /c updater-task.cmd` + /IT → 任务计划器给它一个可见控制台），已在
    // installer.nsh / post-update.cmd 改为 wscript 隐藏启动。降级通道同步改：
    // elevate 起 wscript 跑 updater-silent.vbs（wscript 是 GUI 子系统，零控制台），
    // 只有 vbs 缺失（极老版本没带上）才退回旧的 cmd.exe /c updater-task.cmd。
    let elevate = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.join("elevate.exe")))
        .unwrap_or_else(|| std::path::PathBuf::from("elevate.exe"));
    let silent = ctx.updates.parent().map(|p| p.join("updater-silent.vbs"));
    let (c2, o2) = if elevate.exists() {
        match silent.as_ref().filter(|p| p.exists()) {
            Some(vbs) => win_run_capture(
                &elevate.to_string_lossy(),
                &[
                    "wscript.exe",
                    "//B",
                    "//Nologo",
                    &vbs.display().to_string(),
                ],
            ),
            None => win_run_capture(
                &elevate.to_string_lossy(),
                &["cmd.exe", "/c", &script.display().to_string()],
            ),
        }
    } else {
        let ps = format!(
            "Start-Process -FilePath 'cmd.exe' -ArgumentList '/c','\"{}\"' -Verb RunAs -WindowStyle Hidden",
            script.display()
        );
        win_run_capture(
            "powershell",
            &["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &ps],
        )
    };
    upd_log(&format!("触发补丁应用(提权兜底): code={c2} out={o2}"));
    if c2 != 0 {
        return Err(format!(
            "触发自动升级失败（计划任务与提权通道均不可用）：{out} | {o2}"
        ));
    }
    Ok(())
}

/// 跑一个 Windows 命令并捕获输出（CREATE_NO_WINDOW，不弹黑窗）。
/// 返回 (exit_code, stdout+stderr 拼接)。
///
/// 🔴 2026-10-24 22:5x 事故：`Command::output()` **没有任何超时**，只要子进程不退出
/// （安全软件挂起询问、计划任务服务卡顿、schtasks 被策略拦在半路），这里就永久阻塞，
/// 整条 `win_auto_update` 命令挂死 → 前端横幅永远停在「正在自动升级」，用户观感「没反应」。
/// 老8 真机实测就撞上这个：22:41 触发后 7 分钟无事件、无网络、无 CPU，updates 目录也没有包。
/// 改为 spawn + try_wait 轮询，超时直接 kill 并返回 -2，让上层立刻报错而不是干挂。
#[cfg(windows)]
fn win_run_capture(program: &str, args: &[&str]) -> (i32, String) {
    win_run_capture_to(program, args, 10)
}

/// 同 `win_run_capture`，可指定超时秒数。
///
/// 🔴 2026-10-24 23:0x 二次事故定位：旧实现用 `Command::output()`（内部走管道 +
/// read_to_end 等 EOF）。**Windows 上只要子进程把 stdout 句柄继承给了别的常驻进程
/// （安全软件代理、输入法、计划任务服务中间层都会），子进程早已退出、管道写端却还开着
/// → read_to_end 永远等不到 EOF → `output()` 永久阻塞**。
/// 真机现场完全吻合：22:41 触发后 x8im 0 CPU / 0 网络 / tasklist 里没有 schtasks 残留 /
/// `update.log` 一个字都没写（① 的日志在 output() 之后）——就是卡在这一步。
/// 因此这里**不用管道**：stdout/stderr 重定向到临时文件（句柄独立、不受继承影响），
/// 用 try_wait 轮询 + 超时 kill，最后读文件拿输出。这样任何情况都不会阻塞。
#[cfg(windows)]
fn win_run_capture_to(program: &str, args: &[&str], secs: u64) -> (i32, String) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let log = std::env::temp_dir().join(format!("x8_cap_{}_{}.log", program, std::process::id()));
    let f = match std::fs::File::create(&log) {
        Ok(f) => f,
        Err(e) => return (-1, format!("创建临时输出文件失败: {e}")),
    };
    let f2 = match f.try_clone() {
        Ok(f) => f,
        Err(e) => return (-1, format!("复制输出句柄失败: {e}")),
    };
    let mut child = match Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::from(f2))
        .stderr(std::process::Stdio::from(f))
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return (-1, format!("启动 {program} 失败: {e}")),
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&log);
                return (-1, format!("等待 {program} 失败: {e}"));
            }
        }
    }
    let out = std::fs::read(&log)
        .map(|b| String::from_utf8_lossy(&b).to_string())
        .unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    if timed_out {
        let tail: String = out.chars().rev().take(200).collect::<Vec<_>>().into_iter().rev().collect();
        return (-2, format!("{program} 调用超时（>{secs}s），已强制结束。输出尾段: {tail}"));
    }
    let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
    (code, out)
}

/// 2026-10-23 老8：升级不能只显示一句「正在自动升级」——长时间无进度让人以为卡死。
/// 阶段经 IPC Channel 逐个推给前端：checking → downloading(got/total) →
/// downloaded → installing → waiting（触发计划任务）→ started（安装器真的起来了）。
/// 升级进度：有 Channel 就发事件（老通道）；**没有 Channel（_nc 命令）就直接把阶段推给浮窗**。
///
/// 🔴 2026-10-25 20:0x 事故定论：远程页面（https://im.x8fde.com/app/）上带 Channel 参数的
/// 命令**根本不进函数体**——`__CHANNEL__:<id>` 在远程域下反序列化永久挂住，既不执行也不报错，
/// 前端进度靠本地时间线演满 → 「看着升完了、其实什么都没发生」，进程随后被用户/系统收掉。
/// 所以 1.5.61 起前端一律走 `_nc`，进度改由这里直接 eval 到浮窗，彻底不依赖 IPC Channel。
/// 首次建窗仍由前端 `upd_float_show` 负责（这里只在窗口已存在时更新，绝不在后台线程建窗）。
fn upd_emit(
    app: &tauri::AppHandle,
    ch: &std::sync::Arc<Option<tauri::ipc::Channel<serde_json::Value>>>,
    stage: &str,
    got: u64,
    total: u64,
) {
    if let Some(c) = ch.as_ref() {
        let _ = c.send(serde_json::json!({ "stage": stage, "got": got, "total": total }));
        return;
    }
    // 无 Channel：下载阶段每秒刷新「已用 N 秒」，其它阶段（含结束/失败）立刻停掉
    if stage == "downloading" {
        upd_dl_tick_start(app.clone());
    } else {
        upd_dl_tick_stop();
    }
    upd_float_stage(app, stage, got, total);
}

/// 升级起始时间戳（秒），用于浮窗显示「已用 N 秒」。
static UPD_T0: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// 最近一次前端传进来的目标版本号（Rust 侧推阶段时带回去，浮窗标题才不会丢版本）。
static UPD_VER: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
/// 下载中每秒刷新的开关。
static UPD_DL_TICK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 升级浮窗上次落位坐标（去抖用：坐标没变就不再发 set_position，避免窗口消息风暴）。
static UPD_FLAST_POS: std::sync::OnceLock<std::sync::Mutex<Option<(i32, i32)>>> =
    std::sync::OnceLock::new();

/// 无 Channel 模式下把当前阶段直接推给升级浮窗（仅在浮窗已存在时更新）。
fn upd_float_stage(app: &tauri::AppHandle, stage: &str, got: u64, total: u64) {
    use std::sync::atomic::Ordering;
    if app.get_webview_window("fde-upd").is_none() {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if stage == "checking" {
        UPD_T0.store(now, Ordering::Relaxed);
    }
    let t0 = UPD_T0.load(Ordering::Relaxed);
    let el = if t0 > 0 && now > t0 { now - t0 } else { 0 };
    let pct = if total > 0 {
        (((got as f64 / total as f64) * 100.0).min(99.0)) as u32
    } else {
        0
    };
    let ver = UPD_VER
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default();
    fde_upd_show_impl(app, stage, pct, el, -1, &ver, got, total);
}

/// 下载阶段每秒刷新一次「已用 N 秒」，直到 `UPD_DL_TICK` 被关掉。
/// 只在无 Channel 模式用；此时窗口已由前端建好，这里只有 eval，不涉建窗。
fn upd_dl_tick_start(app: tauri::AppHandle) {
    use std::sync::atomic::Ordering;
    if UPD_DL_TICK.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(move || {
        while UPD_DL_TICK.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if !UPD_DL_TICK.load(Ordering::Relaxed) {
                break;
            }
            upd_float_stage(&app, "downloading", 0, 0);
        }
    });
}

fn upd_dl_tick_stop() {
    UPD_DL_TICK.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// 2026-10-24：升级链路现场日志。静默升级一旦失败，前端通常只收到一句 Err，
/// 真正卡在哪一步（预检/检查/下载/落盘/触发/轮询）无法事后查证——1.5.44「只更新
/// latest.json 没传包 → 客户端点升级静默失败」正是缺现场日志才难定位。
/// 写 LOCALAPPDATA\BangXin\update.log，照抄 si_log 的 32KB 截断防无限增长。
#[cfg(windows)]
fn upd_log(msg: &str) {
    use std::io::Write;
    // 2026-10-25：双写。只写 LOCALAPPDATA 时，一旦进程跑在计划任务/提权上下文
    // （LOCALAPPDATA 可能是 C:\ProgramData 或 systemprofile），日志就"消失"，
    // 现场无法定位（1.5.52/1.5.58/1.5.59 三次「代码有日志、磁盘上没文件」都是这种）。
    // 现在固定再写一份 C:\ProgramData\BangXin\update.log 作地面真值。
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        paths.push(std::path::Path::new(&local).join("BangXin").join("update.log"));
    }
    paths.push(std::path::PathBuf::from(r"C:\ProgramData\BangXin\update.log"));
    // 2026-11-05：系统盘不是 C: 或 ProgramData 被重定向时，上面那条写不进去 → 用真实目录再补一份
    if let Ok(pd) = programdata_dir() {
        let p = pd.join("BangXin").join("update.log");
        if !paths.contains(&p) {
            paths.push(p);
        }
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for path in paths {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(md) = std::fs::metadata(&path) {
            if md.len() > 32 * 1024 {
                let _ = std::fs::remove_file(&path);
            }
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "[{secs}] {msg}");
        }
    }
}

/// 前端命令：Windows 一键静默升级。返回 "installing:<ver>" / "no-update" / Err。
/// ⚠️ 安装器启动后会 taskkill 本进程——本调用的返回值可能永远送不回前端（app 已被杀），
/// 那是预期行为；前端应把「调用即进入自动升级中」当成成功路径展示。
/// `on_event` 为进度通道：Tauri 只给 `Channel<T>` 本身实现 CommandArg（包 Option 不成立），
/// 故设为必填。前端按版本门禁：1.5.32+ 才传（老 Rust 无此参数，传了会 invalid args）；
/// 老客户端跑的是新版远端 JS，但门禁判的是本机 appVer，不会误传。
/// 2026-10-24 稳定性修复：原 win_auto_update 的所有失败分支直接 return Err，前端收不到
/// 任何「收起升级小窗」的信号——小窗停在「正在升级」，用户观感就是「没反应、关不掉」。
/// 重构成：wrapper 统一兜底（任一失败 → 收起小窗 + 落一条 update.log），inner 执行实际步骤。
#[tauri::command]
async fn win_auto_update(
    app: tauri::AppHandle,
    on_event: tauri::ipc::Channel<serde_json::Value>,
) -> Result<String, String> {
    // 2026-10-24：入口即落一行日志。真机排查时「整条链路一条日志都没有」是最难办的现场
    // （分不清是前端没调、IPC 没派发，还是命令内部卡在第一个阻塞点），先钉死这一点。
    #[cfg(windows)]
    upd_log("[入口] win_auto_update 收到调用");
    // 2026-11-17 C：连续 3 次自动升级未成功就停手，别让用户陷入
    // 「下载 10MB → taskkill → 装不上 → 再下载」的死循环。手动装好新版后
    // upgrade_selfcheck() 会自动删掉这个闸门文件。
    #[cfg(windows)]
    if let Some(g) = upgrade_giveup_path() {
        if g.exists() {
            upd_log("[入口] 已存在 upgrade-giveup.txt，拒绝自动升级");
            return Err(
                "自动升级已连续失败 3 次（多半被安全软件拦了）。请手动下载安装最新版：https://im.x8fde.com/app/"
                    .into(),
            );
        }
    }
    let prog = std::sync::Arc::new(Some(on_event));
    #[cfg(windows)]
    {
        let r = win_auto_update_inner(app.clone(), prog).await;
        if let Err(e) = &r {
            upd_log(&format!("升级失败收尾(收起小窗): {e}"));
            upd_float_hide(app.clone());
        }
        r
    }
    #[cfg(not(windows))]
    {
        let _ = &app;
        let _ = prog;
        Err("自动静默升级仅 Windows 可用".into())
    }
}

#[cfg(windows)]
async fn win_auto_update_inner(
    app: tauri::AppHandle,
    prog: std::sync::Arc<Option<tauri::ipc::Channel<serde_json::Value>>>,
) -> Result<String, String> {
    #[cfg(windows)]
    {
        use tauri_plugin_updater::UpdaterExt;

        // ① 预检计划任务（2026-10-26 起**只降级不放弃**：缺失/被禁也照样下载验签，
        //    最后一公里由 apply_patch_task() 改用提权直跑 updater-task.cmd）
        //    schtasks /Query 对不存在任务返回非 0（ERROR: The system cannot find the file specified）。
        // 2026-10-24 23:0x：日志必须写在调用**之前**——这一步正是真机挂死点，
        // 之前日志在调用之后，导致「一条日志都没有」查不出卡在哪。
        upd_log("[①] 开始预检计划任务");
        let ctx = task_ctx()?;
        let (code, out) = win_run_capture("schtasks", &["/Query", "/TN", ctx.name, "/FO", "LIST"]);
        upd_log(&format!("①预检计划任务 name={} code={code} out={out}", ctx.name));
        if code != 0 {
            upd_log("①预检未通过（任务缺失/schtasks 被禁）：降级为提权直跑，继续流程");
        }
        upd_emit(&app, &prog, "checking", 0, 0);

        // ② 检查更新（版本比较由插件的 VersionComparator 负责，默认「大于当前版本」）
        // 2026-10-24：加 20s 超时。此前 check() 无超时，DNS/TLS 卡住即永久挂起，
        // 前端只能干等 10 分钟兜底 —— 又是一种「没反应」。
        let updater = app.updater().map_err(|e| format!("初始化升级组件失败: {e}"))?;
        let checked = tokio::time::timeout(std::time::Duration::from_secs(20), updater.check()).await;
        let update = match checked {
            Err(_) => {
                upd_log("②检查更新: 超时(20s)");
                return Err("检查更新超时（20 秒无响应），请检查网络后重试".into());
            }
            Ok(Err(e)) => return Err(format!("检查更新失败: {e}")),
            Ok(Ok(None)) => {
                upd_log("②检查更新: 无新版(no-update)");
                return Ok("no-update".into());
            }
            Ok(Ok(Some(u))) => u,
        };
        let ver = update.version.clone();
        upd_log(&format!("②检查更新: 有新版 ver={ver}"));

        // ③ 下载——内部完成 minisign 验签，验不过直接报错；拿到 bytes 即可信
        // 2026-10-23：回调改为把进度推给前端（老前端 on_event=None 时 upd_emit 空转，行为不变）；
        // 闭包要求 'static + Send，故先 clone 一份 Arc 进闭包。
        let prog_dl = prog.clone();
        // 闭包要 'static + Send，app 也要 clone 一份；否则下面的 upd_emit(&app, ...) 会借用已 move 的值
        let app_dl = app.clone();
        upd_log("③开始下载更新包(看门狗 900s)");
        // 2026-10-24：给下载加 900s 超时看门狗。此前 download 无任何超时，断流即永久挂起，
        // 前端小窗一直停在「正在升级」→ 用户观感「没反应、关不掉」。
        let dl = update.download(
            move |got, total| {
                upd_emit(&app_dl, &prog_dl, "downloading", got as u64, total.unwrap_or(0));
            },
            || {},
        );
        let bytes = match tokio::time::timeout(std::time::Duration::from_secs(900), dl).await {
            Ok(r) => r.map_err(|e| format!("下载更新包失败: {e}"))?,
            Err(_) => return Err("下载更新包超时（15 分钟无进展），请稍后重试或手动更新".into()),
        };
        upd_log(&format!("③下载完成 bytes={}", bytes.len()));
        upd_emit(&app, &prog, "downloaded", bytes.len() as u64, bytes.len() as u64);

        // ④ 落盘 updates\：先写 .part 再改名，避免半包被 .cmd 的 BangXin_*_x64-setup.exe
        //    glob 命中；顺手清掉更早的旧包（dir /b /o-d 也是最新优先，双保险）
        let ctx = task_ctx()?;
    let dir = ctx.updates.clone();
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建更新目录失败: {e}"))?;
        let final_path = dir.join(format!("{}_{ver}_x64-setup.exe", ctx.prefix));
        let part_path = dir.join(format!("{}_{ver}_x64-setup.exe.part", ctx.prefix));
        std::fs::write(&part_path, &bytes).map_err(|e| format!("写入更新包失败: {e}"))?;
        std::fs::rename(&part_path, &final_path).map_err(|e| format!("更新包就位失败: {e}"))?;
        upd_log(&format!("④更新包就位 {}", final_path.display()));
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for ent in rd.flatten() {
                let p = ent.path();
                if p == final_path {
                    continue;
                }
                let name = p
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                if name.starts_with(ctx.prefix) && name.ends_with("_x64-setup.exe") {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        // 删掉上一轮的启动标记，保证下面轮询读到的是本次任务写的
        let _ = std::fs::remove_file(dir.join("task-started.txt"));

        // ⑤ 触发计划任务（普通权限即可，不弹 UAC）；任务以最高权限运行安装器 /S /UPDATE /R
        upd_emit(&app, &prog, "installing", 0, 0);
        apply_patch_task(&ctx)?;
        upd_log("⑤已触发补丁应用（schtasks 或提权兜底）");

        // ⑥ 轮询任务启动标记（.cmd 第一件事就是写 task-started.txt），最多 30s。
        //    任务一旦启动，安装器很快会 taskkill 本进程——轮询等不到「自然结束」
        //    是预期路径，见到标记即返回成功。
        //    睡眠走 spawn_blocking：tauri::async_runtime 不再导出 sleep（2.12.3 实测
        //    E0425），而本 fn 是 async 命令跑在 tokio worker 上，直接 std::thread::sleep
        //    会堵住 worker（拖累 IPC 等其它任务）。
        for _ in 0..60 {
            tauri::async_runtime::spawn_blocking(|| {
                std::thread::sleep(std::time::Duration::from_millis(500))
            })
            .await
            .ok();
            if dir.join("task-started.txt").exists() {
                upd_log("⑥已见 task-started.txt，安装器接管");
                upd_emit(&app, &prog, "started", 0, 0);
                return Ok(format!("installing:{ver}"));
            }
        }
        upd_log("⑥30 秒内未见 task-started.txt，判定失败");
        Err("自动升级任务未能在 30 秒内启动，请手动更新".into())
    }
}

/// Windows 组件式静默升级（B 方案：不跑 setup.exe）。
/// 拉 latest.json 的 portable 段 → 下载签名 zip → minisign 验签 → 落 updates\ →
/// 触发计划任务解压覆盖 %INSTDIR% 并重拉邦信。全程无安装向导，升级完自动打开，
/// 比「跑 NSIS 安装器」观感更专业、融合度更高。
#[tauri::command]
async fn win_patch_update(
    app: tauri::AppHandle,
    on_event: tauri::ipc::Channel<serde_json::Value>,
) -> Result<String, String> {
    #[cfg(windows)]
    upd_log("[入口] win_patch_update 收到调用");
    let prog = std::sync::Arc::new(Some(on_event));
    #[cfg(windows)]
    {
        let r = win_patch_update_inner(app.clone(), prog).await;
        if let Err(e) = &r {
            upd_log(&format!("组件升级失败收尾: {e}"));
            upd_float_hide(app.clone());
        }
        r
    }
    #[cfg(not(windows))]
    {
        let _ = &app;
        let _ = prog;
        Err("组件式静默升级仅 Windows 可用".into())
    }
}

// ============ 无进度通道兜底版本（2026-10-25 假进度事故修复） ============
// 背景：上面两个命令的 on_event 是**必需**的 Channel 参数。前端 tauriChannel() 构造失败
// （拿不到 __TAURI__.core.Channel 且 __TAURI_INTERNALS__.transformCallback 也不可用）时会传 {}，
// IPC 参数反序列化在进入函数体**之前**就失败 → 命令体一行不执行 → update.log 连入口行都没有，
// 前端只看到 Promise 不 resolve，表现成「进度条走完却没升级」。
// 这里提供不带 Channel 的同名逻辑（_nc = no channel）：前端检测到通道不可用时改调它，
// 升级照常执行，只是没有实时进度回调。
#[tauri::command]
fn win_patch_update_nc(app: tauri::AppHandle) -> Result<String, String> {
    // 2026-10-26 重写：不再自带一套升级逻辑，统一转发给 updater。
    // 同时 async → 同步（远程域下 async 命令的 IPC 会永久 pending）。
    #[cfg(windows)]
    {
        updater::legacy_forward(&app, "win_patch_update_nc")
    }
    #[cfg(not(windows))]
    {
        let _ = app;
        Err("组件式静默升级仅 Windows 可用".into())
    }
}

#[tauri::command]
fn win_auto_update_nc(app: tauri::AppHandle) -> Result<String, String> {
    // 2026-10-26 重写：统一转发给 updater（async → 同步，规避远程域 IPC pending）。
    #[cfg(windows)]
    {
        updater::legacy_forward(&app, "win_auto_update_nc")
    }
    #[cfg(not(windows))]
    {
        let _ = app;
        Err("一键静默升级仅 Windows 可用".into())
    }
}

#[cfg(windows)]
async fn win_patch_update_inner(
    app: tauri::AppHandle,
    prog: std::sync::Arc<Option<tauri::ipc::Channel<serde_json::Value>>>,
) -> Result<String, String> {
    // ① 预检计划任务（与 win_auto_update 同源；2026-10-26 起缺失只降级、不放弃）
    upd_log("[①] 预检计划任务");
    let ctx = task_ctx()?;
    let (code, out) = win_run_capture("schtasks", &["/Query", "/TN", ctx.name, "/FO", "LIST"]);
    upd_log(&format!("①预检计划任务 name={} code={code} out={out}", ctx.name));
    if code != 0 {
        upd_log("①预检未通过（任务缺失/schtasks 被禁）：降级为提权直跑，继续流程");
    }
    upd_emit(&app, &prog, "checking", 0, 0);

    // ② 拉 latest.json，取 portable 段（url + 内联 signature）；blocking 包进 spawn_blocking
    upd_log("②拉取 latest.json 取 portable 段");
    let fetched = tauri::async_runtime::spawn_blocking(|| -> Result<(Vec<u8>, String, String), String> {
        let cli = || {
            reqwest::blocking::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(20))
                .timeout(std::time::Duration::from_secs(900))
                .build()
                .map_err(|e| format!("初始化 HTTP 失败: {e}"))
        };
        let lj = cli()?
            .get("https://im.x8fde.com/app/latest.json")
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.text())
            .map_err(|e| format!("拉取 latest.json 失败: {e}"))?;
        let v: serde_json::Value =
            serde_json::from_str(&lj).map_err(|e| format!("解析 latest.json 失败: {e}"))?;
        let portable = v
            .get("platforms")
            .and_then(|p| p.get("windows-x86_64"))
            .and_then(|w| w.get("portable"))
            .ok_or("latest.json 缺少 portable 段（服务端未发组件包）")?;
        let zip_url = portable
            .get("url")
            .and_then(|x| x.as_str())
            .ok_or("portable.url 缺失")?
            .to_string();
        let sig = portable
            .get("signature")
            .and_then(|x| x.as_str())
            .ok_or("portable.signature 缺失")?
            .to_string();
        let ver = v
            .get("version")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if ver.is_empty() {
            return Err("latest.json 缺少 version".into());
        }
        let bytes = cli()?
            .get(&zip_url)
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.bytes())
            .map(|b| b.to_vec())
            .map_err(|e| format!("下载组件包失败: {e}"))?;
        Ok((bytes, sig, ver))
    })
    .await
    .map_err(|e| format!("下载线程异常: {e}"))?;
    let (bytes, sig, ver) = fetched?;
    upd_log(&format!("②portable ver={ver} bytes={}", bytes.len()));
    upd_emit(&app, &prog, "downloading", 0, 0);

    // ③ 验签（与 tauri 同一把 minisign 公钥，杜绝传输/CDN 被篡改）
    upd_log("③minisign 验签组件包");
    verify_portable(&bytes, &sig).map_err(|e| format!("组件包验签失败: {e}"))?;

    // ④ 落盘 updates\（.part → rename 原子；清理更早的旧包）
    let ctx = task_ctx()?;
    let dir = ctx.updates.clone();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建更新目录失败: {e}"))?;
    let final_path = dir.join(format!("{}_{ver}_x64-portable.zip", ctx.prefix));
    let part_path = dir.join(format!("{}_{ver}_x64-portable.zip.part", ctx.prefix));
    std::fs::write(&part_path, &bytes).map_err(|e| format!("写入组件包失败: {e}"))?;
    std::fs::rename(&part_path, &final_path).map_err(|e| format!("组件包就位失败: {e}"))?;
    upd_log(&format!("④组件包就位 {}", final_path.display()));
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p == final_path {
                continue;
            }
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if name.starts_with(ctx.prefix) && name.ends_with("_x64-portable.zip") {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    let _ = std::fs::remove_file(dir.join("task-started.txt"));

        // ⑤ 触发计划任务（普通权限即可，不弹 UAC）；任务以最高权限解压覆盖 + 重拉
        upd_emit(&app, &prog, "installing", 0, 0);
        apply_patch_task(&ctx)?;
        upd_log("⑤已触发补丁应用（schtasks 或提权兜底）");

    // ⑥ 轮询 task-started.txt（计划任务接管后即返回成功；本进程随后被 taskkill）
    for _ in 0..60 {
        tauri::async_runtime::spawn_blocking(|| std::thread::sleep(std::time::Duration::from_millis(500)))
            .await
            .ok();
        if dir.join("task-started.txt").exists() {
            upd_log("⑥已见 task-started.txt，计划任务接管");
            upd_emit(&app, &prog, "started", 0, 0);
            return Ok(format!("installing:{ver}"));
        }
    }
    upd_log("⑥30 秒内未见 task-started.txt，判定失败");
    Err("自动升级任务未能在 30 秒内启动，请手动更新".into())
}

/// 用与 tauri 同一把 minisign 公钥验签组件包（portable zip）。
///
/// 🔴 2026-10-25 23:2x 实测根治（本机 1.6.5 客户端日志实证）：
///   [启动自检] 发现新版 1.6.6，开始后台自主升级
///   ③自主升级 minisign 验签
///   [启动自检] 升级失败: 组件包验签失败: 签名解析失败: Invalid encoding in minisign data
/// 病根：latest.json 的 signature 存的是 **.sig 文件的内容**（tauri 惯例：base64 一行），
///       而 minisign_verify::Signature::decode 要的是 **解码后的 4 行签名原文**
///       （untrusted comment / base64 sig / trusted comment / global sig）。
///       把 base64 一行直接喂给 decode → 首行不是 "untrusted comment" → Invalid encoding。
///       结果：每台机器都下载完 38MB 组件包，却在验签这一步永远失败 ⇒ 全员卡在旧版本。
/// 修法：验签前先 base64 解码；已解码（以 untrusted comment 开头）的原样放行，两种格式通吃。
#[cfg(windows)]
fn verify_portable(bytes: &[u8], sig_text: &str) -> Result<(), String> {
    use minisign_verify::{PublicKey, Signature};
    const PK: &str = "RWRRYZoqpg5nAMR8xmhdtJgSX4BoGUrjrVPDm0D2gvCovnAov9tJjTo8";
    let pk = PublicKey::from_base64(PK).map_err(|e| format!("公钥解析失败: {e}"))?;
    let raw = decode_sig_text(sig_text)?;
    let sig = Signature::decode(&raw).map_err(|e| format!("签名解析失败: {e}"))?;
    // 第三参 allow_legacy=false：仅接受标准（非 legacy）minisign 签名，与 tauri 同源
    pk.verify(bytes, &sig, false).map_err(|e| format!("验签未通过: {e}"))
}

/// 把 latest.json 的 signature 还原成 minisign 签名原文。
/// 输入两种都接受：① .sig 文件内容（base64 一行，tauri 生成器产出）② 已解码的 4 行原文。
#[cfg(windows)]
fn decode_sig_text(s: &str) -> Result<String, String> {
    let t = s.trim();
    if t.starts_with("untrusted comment") {
        return Ok(t.to_string());
    }
    let bytes = b64_decode(t).map_err(|e| format!("签名 base64 解码失败: {e}"))?;
    String::from_utf8(bytes).map_err(|e| format!("签名不是合法 UTF-8: {e}"))
}

/// 最小 base64 解码：复用上方已有的 b64_decode（1943 行，标准字母表、带体积上限），
/// 这里不再重复实现，避免同名冲突。

// ============================================================================
// Rust 自主升级（2026-10-25 根治「点了没反应」）
// ----------------------------------------------------------------------------
// 背景：此前升级触发是「远程页 → IPC invoke → 异步下载」长链路。1.5.52/58/59/60/61
// 连续复发同一现场：前端 invoke 已发出、Rust 命令体首行 upd_log 都没写、前端
// setInterval（3 分钟轮询 / 120 秒看门狗）全部停摆 —— 即「远程域 IPC 调用在特定
// 条件下永久 pending」，且把前端主线程一起拖死。_nc（无 Channel）只绕开了 Channel
// 反序列化，绕不开这个更底层的 IPC pending。
// 根治：把「检查 + 下载 + 验签 + 落盘 + 触发计划任务」整体搬到 Rust 主进程自主执行，
// 前端退化为纯显示器（只收 app.emit 事件画进度），升级链路不再经过任何远程页 IPC。
// ============================================================================

/// 前端确认后调用的「即返」命令：只记目标版本号并 spawn 一个独立线程执行真实升级。
/// 命令体立刻返回 Ok（不阻塞、不下载），真实升级在后台线程自主跑，
/// 阶段经 `x8-upd-stage` 事件推给前端。这样即使前端 JS / WebView2 抽风，升级照跑。
// win_upd_start 已迁移到 updater 模块（同步 fn，见 updater.rs）

/// 版本号比较：把 "1.6.3" / "1.6.10" 拆成数字段逐段比。
/// 返回 Ordering；无法解析的段按 0 处理（"1.6.x" 之类）。
#[cfg(windows)]
fn cmp_ver(a: &str, b: &str) -> std::cmp::Ordering {
    let seg = |s: &str| -> Vec<u64> {
        s.split(|c: char| !c.is_ascii_digit())
            .filter(|x| !x.is_empty())
            .map(|x| x.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let sa = seg(a);
    let sb = seg(b);
    for i in 0..sa.len().max(sb.len()) {
        let x = sa.get(i).copied().unwrap_or(0);
        let y = sb.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            std::cmp::Ordering::Equal => continue,
            o => return o,
        }
    }
    std::cmp::Ordering::Equal
}

/// 2026-11-02 白屏自愈：WebView2 长挂机 / 系统睡眠后渲染上下文丢失，
/// 主窗客户区变白板（标题栏是系统画的所以照常显示）。唯一稳定恢复手段是 reload 远程页。
/// 前端已做 token 落盘（localStorage `x8_token`）+ 冷启动静默续登（verifySavedToken），
/// 故 reload 不丢登录。两道触发：① 睡眠检测线程发现系统睡过 → 置位；② 主窗隐藏超阈值。
static MAIN_HIDDEN_AT: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
static RESUME_RELOAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 主窗隐藏超过此时长（秒）再次唤起即 reload（兜底非睡眠类长挂机白屏）
const MAIN_IDLE_RELOAD_SECS: u64 = 3600;

// ───────────── 主窗显隐兜底（2026-11-17 A：tao 内部幂等位失同步）─────────────
//
// 🩸 真因（老8 2026-11-17 现场报告 + tao 0.37.1 源码实读）：
//   tao 的 `set_visible()` 只改内部 `WindowFlags::VISIBLE` 位（window.rs:128-136），
//   真正的 ShowWindow 由 `WindowState::apply_diff()` 下发，而它第一句就是
//   `if diff == WindowFlags::empty() { return; }`（window_state.rs:318-320）。
//   于是窗口一旦被 **tao 之外**的力量显示回来 —— 任务栏点击 / Alt-Tab / 双击桌面图标
//   （走 wake_existing_instance_window 的 ShowWindow）/ 安装器外部 ShowWindow ——
//   tao 仍认为它「已隐藏」，此后所有 `hide()` 全部空转：不调系统 API、不报错、
//   不留日志、0.001s 返回 Ok(())。
//   现场表现：点 X 第 1 次有效，从第 2 次起永久失效，重启才恢复。
// 对策：每次 hide/show 之后用 Win32 复核真实窗口状态，不一致就用 Win32 强制纠偏。
//   ⚠️ 判据必须用 IsWindowVisible / IsIconic：tauri 的 `is_visible()` 读的就是那个
//   会失同步的标志位，拿它自查等于拿错题本对答案。

/// 真实可见性（Win32 直查，不信 tao 标志位）。实参统一传裸指针 `hwnd.0`：
/// windows-sys 0.59 的 HWND 是 newtype，link! 展开后 API 实参为 *mut c_void。
#[cfg(target_os = "windows")]
fn w32_visible(h: *mut std::ffi::c_void) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
    unsafe { IsWindowVisible(h) != 0 }
}

/// 真实最小化状态。
#[cfg(target_os = "windows")]
fn w32_iconic(h: *mut std::ffi::c_void) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsIconic;
    unsafe { IsIconic(h) != 0 }
}

/// 强制隐藏（绕开 tao 幂等短路）。
#[cfg(target_os = "windows")]
fn w32_hide(h: *mut std::ffi::c_void) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
    unsafe {
        let _ = ShowWindow(h, SW_HIDE);
    }
}

/// 强制显示到前台（SW_RESTORE 同时解决「隐藏」和「最小化」；SW_SHOW 不还原最小化）。
#[cfg(target_os = "windows")]
fn w32_show_restore(h: *mut std::ffi::c_void) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_RESTORE};
    unsafe {
        let _ = ShowWindow(h, SW_RESTORE);
    }
}

/// 强制最小化。
#[cfg(target_os = "windows")]
fn w32_minimize(h: *mut std::ffi::c_void) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_MINIMIZE};
    unsafe {
        let _ = ShowWindow(h, SW_MINIMIZE);
    }
}

fn show_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        // 是否需要 reload：隐藏过久 或 系统唤醒后置位
        let need_reload = {
            let mut g = MAIN_HIDDEN_AT.lock().unwrap();
            let idle = match *g {
                Some(t) => t.elapsed().as_secs() >= MAIN_IDLE_RELOAD_SECS,
                None => false,
            };
            *g = None; // 消费掉，下次隐藏再记
            idle || RESUME_RELOAD.swap(false, std::sync::atomic::Ordering::SeqCst)
        };
        let _ = w.show();
        let _ = w.unminimize();
        // 2026-11-17 A：show()/unminimize() 同样走 tao 的幂等位 —— 若窗口真实隐藏
        // 而 tao 认为「可见」，这里会静默空转、窗口从此叫不出来（现场「点托盘唤不出」）。
        // 复核真实状态，不一致就用 Win32 兜底。
        #[cfg(target_os = "windows")]
        if let Ok(h) = w.hwnd() {
            if !w32_visible(h.0) || w32_iconic(h.0) {
                w32_show_restore(h.0);
                nav_write("show_main: tao show/unminimize 空转 → Win32 SW_RESTORE 强制显示");
            }
        }
        if need_reload {
            let _ = w.eval("location.reload()");
        }
        let _ = w.set_focus();
    }
    // 回到前台即视为已读：停掉托盘图标闪烁 + tooltip 还原 + 角标图标还原成默认 logo
    // （任务栏闪烁由系统在获得焦点时自动停止；未读数由前端 focus 时的 syncUnreadNotice 再同步一遍）
    stop_tray_flash();
    // 右下角 FDE 浮窗同步收起（点浮窗 / 从托盘唤起都会走到这里）
    fde_float_close(app);
    if let Some(tray) = app.tray_by_id("x8im-tray") {
        let _ = tray.set_tooltip(Some("邦信即时通讯"));
        let _ = tray.set_icon(Some(tray_icon_image()));
    }
}

/// Linux：虚拟显卡 / NVIDIA 下关闭 WebKitGTK 的 DMABUF 渲染器（白屏修复）。
///
/// 🩸 2026-10-19 真机实测（TencentOS 4.2 + Cirrus GD 5446 虚拟显卡 + xrdp）：
/// 默认启动是纯白窗口，日志：
///   libEGL warning: pci id for fd 18: 1013:00b8, driver (null)
///   JIT session error: ... fs_variant_partial / Failed to materialize symbols
/// 原因是 WebKitGTK 的 DMABUF 渲染器向驱动要 3D 缓冲格式，虚拟显卡（无 EGL）
/// 给不出来，llvmpipe 的着色器 JIT 也跟着失败，最终什么都不画。
/// 设 WEBKIT_DISABLE_DMABUF_RENDERER=1 后界面完整渲染（实测截图确认）。
///
/// 只在虚拟显卡 / NVIDIA 上设（普通 Intel/AMD 物理机保留硬件加速）；
/// 用户自己设了该变量时尊重用户（想强制硬件加速：export 成 0/空）。
#[cfg(target_os = "linux")]
fn linux_configure_webkit() {
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_some() {
        return;
    }
    let needs = match std::fs::read_dir("/sys/class/drm") {
        Ok(rd) => rd.flatten().any(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            // 只看 cardN，跳过 cardN-HDMI-A-1 这类连接器
            if !name.starts_with("card") || name.contains('-') {
                return false;
            }
            let v = std::fs::read_to_string(e.path().join("device/vendor")).unwrap_or_default();
            matches!(
                v.trim(),
                // Cirrus / VMware / VirtualBox / QEMU-Bochs / virtio / QXL / NVIDIA
                "0x1013" | "0x15ad" | "0x80ee" | "0x1234" | "0x1af4" | "0x1b36" | "0x10de"
            )
        }),
        // 读不到显卡信息时保守处理：宁可软件渲染，也别给用户一个白窗口
        Err(_) => true,
    };
    if needs {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
}

// ───────── 硬单实例（2026-10-23 老8：同一台电脑只许开一个实例） ─────────
//
// 2026-10-23 起**不再使用** tauri-plugin-single-instance，原因（读 crate 源码 + 本机实测）：
//   ① 它在 ERROR_ALREADY_EXISTS 后靠 FindWindowW 找首个实例的隐藏消息窗，**找到就 exit**——
//      而"隐藏消息窗存在"不等于"用户能看到界面"。本机 9:13 那个高权限残留进程没有主窗、
//      只有隐藏消息窗，结果：每次双击图标的新实例都被插件的逻辑安静地杀掉，用户看到
//      的是「点了完全没反应、程序像没了」。
//   ② 反过来，它找不到消息窗时又会放行第二个实例（见 platform_impl/windows.rs:120-127）。
//      两个方向都不符合我们的要求。
//
// 现在只用自己的判据：**能不能真的把已有实例的主窗唤起来**。
//   有主窗（含托盘隐藏的主窗，EnumWindows 照样枚举得到）→ 唤起并退出自己；
//   没有任何主窗 → 判定对方是僵尸/半死（无界面），放行本次启动，绝不让用户打不开。
#[cfg(target_os = "windows")]
fn wake_existing_instance_window() -> bool {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    static FOUND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    unsafe extern "system" fn cb(hwnd: HWND, _l: LPARAM) -> i32 {
        // ⚠️ 只认主窗：标题形如 "邦信 v1.5.38"。浮窗（fde-float 的 "邦信"、
        // fde-upd 的 "邦信升级中"）也含 "邦信"，不加 "v" 会把 320x108 的小浮窗唤出来。
        let mut buf = [0u16; 256];
        let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        if n <= 0 {
            return 1;
        }
        let title = String::from_utf16_lossy(&buf[..n as usize]);
        if title.contains("邦信 v") {
            // 不看 IsWindowVisible：藏进托盘的窗口也要能还原出来（实测 ShowWindow 对隐藏窗有效）
            unsafe {
                ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd);
                FOUND.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            return 0; // 找到即停止枚举
        }
        1
    }

    FOUND.store(false, std::sync::atomic::Ordering::SeqCst);
    unsafe { EnumWindows(Some(cb), 0) };
    FOUND.load(std::sync::atomic::Ordering::SeqCst)
}

/// 是否还有「另一个」同程序进程活着（2026-11-13）。
///
/// 用途：区分「对方正在冷启动、窗口还没建出来」和「对方是真僵尸残留」。
/// 单靠窗口判据会把前者误判成后者（升级刚结束时尤其明显），于是两个实例都放行。
#[cfg(target_os = "windows")]
fn other_instance_alive() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let me = std::process::id();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            // 拿不到快照时保守判定「有对方」→ 走带超时的等待分支，不会因此双开。
            return true;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        if Process32FirstW(snap, &mut e) != 0 {
            loop {
                let pid = e.th32ProcessID;
                if pid != me && pid != 0 {
                    let len = e
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(e.szExeFile.len());
                    let name = String::from_utf16_lossy(&e.szExeFile[..len]);
                    if name.eq_ignore_ascii_case("x8im.exe") {
                        found = true;
                        break;
                    }
                }
                if Process32NextW(snap, &mut e) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        found
    }
}

#[cfg(target_os = "windows")]
fn si_log(msg: &str) {
    // 单实例判定的现场只写一次文件：这块逻辑没法在页面上打日志，
    // 「第二次双击没反应 / 反而把第一个踢死」只能靠它定位（2026-10-23 实测需要）。
    use std::io::Write;
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let dir = std::path::Path::new(&local).join("BangXin");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("single.log");
        if let Ok(md) = std::fs::metadata(&path) {
            if md.len() > 32 * 1024 {
                let _ = std::fs::remove_file(&path); // 防止无限增长
            }
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = writeln!(f, "[{secs}] {msg}");
        }
    }
}

// 2026-11-03 修稳 in-app 重启：判断「被 --x8-restart 拉起的新进程」要等的上一代老进程
// 是否还活着。用 OpenProcess(SYNCHRONIZE) + WaitForSingleObject(0) 轮询，不靠盲等固定毫秒。
// 打不开句柄（已退出 / 无权限）一律视为已释放，避免卡死。
#[cfg(target_os = "windows")]
fn x8_parent_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
    const SYNCHRONIZE: u32 = 0x0010_0000;
    unsafe {
        let h = OpenProcess(SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return false;
        }
        let r = WaitForSingleObject(h, 0);
        let exited = r == 0; // WAIT_OBJECT_0
        CloseHandle(h);
        !exited
    }
}

// ── macOS / Linux 单实例管控（2026-11-03 老8：三端都只能跑一个邦信）──
// 思路与 Windows 版一致：先上锁 → 已有实例就唤醒主窗并自杀，唤不到（僵尸）就抢占。
// 用 Unix 域套接字同时充当「锁」+「唤醒通道」：首实例 bind 成功即持锁并监听；
// 第二个实例 bind 失败（EADDRINUSE）→ connect 发 1 字节 → 首实例聚焦主窗 → 自己 exit(0)；
// 若 connect 也失败（死进程残留的孤儿 sock）→ 删文件抢占。进程退出后 OS 回收监听，
// 孤儿 sock 文件由下次启动的「connect 失败→删→重绑」自愈。
#[cfg(any(target_os = "linux", target_os = "macos"))]
static APP_HANDLE: std::sync::Mutex<Option<tauri::AppHandle>> =
    std::sync::Mutex::new(None);

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn focus_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn enforce_single_instance() {
    // 应用内重启：跳过锁，等老进程退出再继续（否则新进程会被自己的锁挡死，重启失效）
    if std::env::args().any(|a| a == "--x8-restart") {
        si_log("restart-child -> skip single-instance");
        let parent_pid = std::env::args()
            .find_map(|a| a.strip_prefix("--x8-parent=").and_then(|s| s.parse::<i32>().ok()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            let alive = parent_pid.map(|p| x8_parent_alive(p)).unwrap_or(true);
            if !alive {
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        return;
    }

    // 每用户隔离：优先 XDG_RUNTIME_DIR（Linux 每用户）/ TMPDIR（mac 每用户），
    // 回落 /tmp 时拼 uid 防多用户互锁。
    let uid = unsafe { libc::getuid() };
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    let path = std::path::Path::new(&runtime)
        .join(format!("com.x8fde.im-onlyone-{uid}.sock"));

    match std::os::unix::net::UnixListener::bind(&path) {
        Ok(listener) => {
            si_log("first instance, lock acquired");
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    if conn.is_ok() {
                        if let Some(app) = APP_HANDLE.lock().unwrap().as_ref() {
                            focus_main(app);
                        }
                    }
                }
            });
        }
        // 套接字文件已存在：可能是活实例，也可能是死进程残留
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // 连得上=活实例→聚焦后自杀；连不上=僵尸残留→抢占
            if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                si_log("already-exists woken -> exit(0)");
                wx_sidecar::stop_all_fast();
                std::process::exit(0);
            }
            let _ = std::fs::remove_file(&path);
            match std::os::unix::net::UnixListener::bind(&path) {
                Ok(listener) => {
                    si_log("stale socket taken over");
                    std::thread::spawn(move || {
                        for conn in listener.incoming() {
                            if conn.is_ok() {
                                if let Some(app) = APP_HANDLE.lock().unwrap().as_ref() {
                                    focus_main(app);
                                }
                            }
                        }
                    });
                }
                Err(_) => si_log("bind retry failed -> continue (allow start)"),
            }
        }
        Err(_) => si_log("bind error -> continue (allow start)"),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn x8_parent_alive(pid: i32) -> bool {
    // kill(pid, 0)：返回 0=进程存在，ESRCH=已退出（无权限亦非 0，按「已释放」处理）
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn si_log(msg: &str) {
    use std::io::Write;
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    let dir = std::path::Path::new(&runtime);
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join("com.x8fde.im-single.log");
    if let Ok(md) = std::fs::metadata(&path) {
        if md.len() > 32 * 1024 {
            let _ = std::fs::remove_file(&path);
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{secs}] {msg}");
    }
}

#[cfg(target_os = "windows")]
fn enforce_single_instance() {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    // 2026-10-28 老8：「重启邦信」点了没反应。重启由 x8_restart 派生新进程实现，
    // 新进程带 `--x8-restart`：此时老进程还没退完、互斥体仍被占用，若照常走下面的
    // 「已有实例」分支，它会先唤醒老窗口再自杀，等于重启彻底落空。这里必须直接放行。
    if std::env::args().any(|a| a == "--x8-restart") {
        si_log("restart-child -> skip single-instance");
        // 老进程此刻还在退出：400ms 后 app.exit(0)，最迟 1.2s 硬退兜底。
        // 两个进程同时开 WebView2 会抢同一个用户数据目录（EBWebView）→ 新进程白屏，
        // 所以这里先等老进程彻底让出 UDF，再往下建窗口。只影响重启路径，不拖慢正常启动。
        // 2026-11-03 修稳：原来盲等 1700ms，负载高时老进程还没退完 → 抢锁失败 → 新进程
        // 起不来（表现就是「重启没反应」）。改为按 --x8-parent 传来的老进程 PID 轮询，
        // 确认它真的退出再继续；拿不到 PID 才退化为固定 2.5s 等待。
        let parent_pid = std::env::args()
            .find_map(|a| a.strip_prefix("--x8-parent=").and_then(|s| s.parse::<u32>().ok()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            let alive = parent_pid.map(|p| x8_parent_alive(p)).unwrap_or(true);
            if !alive {
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        return;
    }

    let name: Vec<u16> = std::ffi::OsStr::new("com.x8fde.im-x8im-onlyone")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let h = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
    let err = unsafe { GetLastError() };
    si_log(&format!(
        "pid={} handle={} lasterr={}",
        std::process::id(),
        h as isize,
        err
    ));
    if (h as isize) == 0 {
        si_log("CreateMutexW failed -> continue");
        return; // 建锁失败（权限异常）不阻断启动，宁可多开也别打不开
    }
    if err == ERROR_ALREADY_EXISTS {
        // 已有实例：优先唤起它的主窗。
        //
        // 2026-11-13 修（老8 报「升级后出现两个窗口」）：原逻辑是「唤不到主窗就直接放行」，
        // 理由是「对方是僵尸，不能让你打不开」。但冷启动 WebView2 要 3~10 秒、升级刚结束
        // 又叠加 UAC 与文件替换，这期间对方**进程活着但窗口还没建出来**，于是第二个启动源
        // （安装器拉起的 + 用户点的）被误判成僵尸，两个都放行 → 双开抢同一个 EBWebView
        // 用户数据目录，越抢越慢、越容易白屏。
        //
        // 现在改成「先看有没有同类进程存活」：
        //   · 唤到主窗          → 聚焦并退出本进程（原行为）
        //   · 对方进程已退出    → 立刻放行（不用白等，避免双击没反应）
        //   · 对方存活但没窗口  → 每 250ms 重试唤醒，最多 8 秒；仍无窗才判僵尸放行
        // 这样既堵住双开，也保留老8 的硬要求：僵尸/半死状态绝不影响正常打开。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            if wake_existing_instance_window() {
                si_log("already-exists woken=true -> exit(0): 已聚焦");
                wx_sidecar::stop_all_fast();
                std::process::exit(0);
            }
            if !other_instance_alive() {
                si_log("already-exists peer-gone -> continue（对方已退出，放行本次）");
                break;
            }
            if std::time::Instant::now() >= deadline {
                si_log("already-exists peer-alive no-window 8s -> continue（僵尸放行）");
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    // 首个实例（或对方是僵尸）：句柄故意不关闭，进程存活期间由系统持锁。
    // （HANDLE 是 Copy 类型，无需 forget；只要不调 CloseHandle 就不会提前释放锁。）
    let _ = h;
}

/// 在应用内真正重启自己（2026-10-28 老8：「重启邦信」点了没反应）。
///
/// 原先前端直接调 `plugin:process|restart`（Rust 侧 `app.request_restart()` →
/// `request_exit` → 事件循环 `Exit` → `process::restart`）。本机会话实测：点完之后
/// `x8im.exe` 的 PID 三个半小时不变，前台只多出一条不会消失的 toast —— 请求根本没
/// 让进程退出去，而前端 `.catch(() => {})` 把 reject 吞了，连原因都看不到。
///
/// 这里改成完全自控的链路，不依赖事件循环的退出握手：
///   ① 直接 spawn 自己（继承原参数 + 追加 `--x8-restart`，新进程据此跳过单实例锁）
///   ② 等 600ms 让新进程把窗口/WebView 建起来
///   ③ `app.exit(0)` 正常退出；若事件循环照旧吞掉退出请求，2 秒后硬退兜底
///
/// 返回值只是给前端确认「命令进到函数体了」，真正成功与否看进程有没有退出。
#[tauri::command]
async fn x8_restart(app: tauri::AppHandle) -> Result<String, String> {
    // 派生新进程（与自愈链 ③ 共用同一套 spawn 逻辑，见 spawn_self_restart）
    spawn_self_restart()?;

    // 两个进程短暂并存：给新进程时间把 WebView2 用户数据目录接管好
    let _ = tauri::async_runtime::spawn_blocking(|| {
        std::thread::sleep(std::time::Duration::from_millis(400));
    })
    .await;

    app.exit(0);
    // 兜底：今天这个毛病就是「退出请求发出去了但事件循环没退」，1.2 秒还没走掉就硬退
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(1200));
        // P1-3：硬退兜底也要收插件，不能只靠 stdin EOF。
        wx_sidecar::stop_all_fast();
        std::process::exit(0);
    });
    Ok("restarting".into())
}

/// 2026-11-05 老8「升级后不该再扫码」：登录凭据在宿主侧留一份。
///
/// 病根：自动登录的唯一凭据是 WebView2 里的 localStorage(`x8_token`)。它只活在
/// 浏览器 profile 里 —— 一旦 profile 被重建、或走到 logout()/401 清理路径，凭据就
/// 彻底没了，升级重启后只能扫码。凭据存在宿主目录（app_data_dir）里，与 WebView2
/// 数据、缓存、前端版本完全解耦，升级/清缓存都不会动它。
///
/// 只存 token + 记住我时的账号密码（与原 localStorage `x8_saved_pass` 同口径），
/// 文件落在当前用户自己的 AppData 下；退出登录时前端会显式清掉。
fn x8_auth_file(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    use tauri::Manager;
    let dir = app.path().app_data_dir().map_err(|e| format!("appdata: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
    Ok(dir.join("auth.json"))
}

#[tauri::command]
fn x8_auth_save(app: tauri::AppHandle, blob: String) -> Result<(), String> {
    let p = x8_auth_file(&app)?;
    // 先写临时文件再改名，避免断电/杀进程留下半截 JSON（下次读会解析失败 → 又要扫码）
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, blob.as_bytes()).map_err(|e| format!("write: {e}"))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

#[tauri::command]
fn x8_auth_load(app: tauri::AppHandle) -> Result<String, String> {
    let p = x8_auth_file(&app)?;
    match std::fs::read_to_string(&p) {
        Ok(s) => Ok(s),
        Err(_) => Ok(String::new()),
    }
}

#[tauri::command]
fn x8_auth_clear(app: tauri::AppHandle) -> Result<(), String> {
    let p = x8_auth_file(&app)?;
    let _ = std::fs::remove_file(&p);
    Ok(())
}

/// 2026-11-05 老8「前端为什么老要重启」根治：换版导航从页面 JS 收归宿主。
///
/// 病根：页面里的 `location.replace('/app/index.html?_b=...')` 会被 WebView2 的各种
/// 中间状态吞掉（HTTP 缓存命中、导航被取消、后台/最小化期间被节流、部署窗口期拿到半截
/// 响应）。前端只能靠 15 秒看门狗猜，猜错两次就把提示条升级成「需要重启邦信」——
/// 这才是「经常要重启」的唯一来源。
///
/// 宿主发起的 navigate 是**浏览器级**动作：不经过页面 JS 的作用域、不受缓存策略与
/// 页面生命周期影响，是这套链路上唯一可靠的重载手段。
/// 只放行应用自己的 https 地址，避免远端页面（或注入脚本）把窗口导航到任意站点。
/// 2026-11-12 老8「点版本更新刷新不动，只能关掉邦信重开」：把命令从 async 改回同步。
///
/// x8_nav_log 实证：同一次运行里成功那次留下了 `host-nav`，失败的两次
/// （01:13:56 / 01:42:58）只有 `before-replace`，15 秒后直接 `dog-alive` —— 前端
/// `await invoke('x8_nav')` 的 Promise 一直没 resolve，后面的 location.replace 永远
/// 执行不到。与 2026-10-26 updater 那次同源：**远程域下 async 命令的 IPC response
/// 偶发永久 pending**（不执行完、不 reject）。navigate 本身是同步动作，不需要 async，
/// 走同步返回路径即可绕开这条通道。
#[tauri::command]
fn x8_nav(app: tauri::AppHandle, target: String) -> Result<String, String> {
    let u = tauri::Url::parse(&target).map_err(|e| format!("url: {e}"))?;
    let host_ok = u.host_str().map(|h| h == "im.x8fde.com").unwrap_or(false);
    if u.scheme() != "https" || !host_ok || !u.path().starts_with("/app/") {
        return Err(format!("拒绝导航到非应用地址: {target}"));
    }
    let w = app
        .get_webview_window("main")
        .ok_or_else(|| "主窗口未找到".to_string())?;
    w.navigate(u).map_err(|e| format!("navigate: {e}"))?;
    Ok("nav".into())
}

// ================= 2026-11-13 前端刷新链重做（老8 定性：必须用户点，且由 Rust 执行）=================
// 现场证据（x8_nav_log + leveldb 反推，本地时区）：
//   09:43:36 fire → 09:43:38 bunload（导航真的开始了）→ 09:43:51 dog-alive（无 pagehide、无 boot）
// 同一秒服务端按 WebView2 UA 实测 /app/index.html?_b=… 返回 200，说明既不是网络也不是缓存，
// 而是**导航发起后没有 commit**。而旧链路里 hostNav 超时（1.2s）之后前端还会再 replace 一次，
// 等于两条导航互抢 —— 后发的会把先发的取消，这是 11-13 复盘出来的竞态。
//
// 现在的分工：
//   ① x8_reload_ui  —— 前端只发「信号」，URL 由 Rust 自己拼并 navigate；前端不再 replace。
//   ② nav_probe     —— navigate 后 1/3/8 秒回读主窗真实 URL，落 %ProgramData%\BangXin\nav.log
//                      并上报 /x8/errlog.php。以后不用再 dump WebView2 的 leveldb 猜。
//   ③ x8_rebuild_ui —— 兜底：eval 注入导航，2.2 秒仍未跳就销毁主窗并按同 label 重建
//                      （文档环境坏掉只有重建能治，比让用户「退出重开」体面）。
// 全程只有用户点击才触发，不带任何自动刷新。

fn nav_log_path() -> std::path::PathBuf {
    // 🔴 2026-11-17 真机（mac 1.7.13）：这里原先只有 Windows 路径 —— mac 上 ProgramData
    // 环境变量不存在 → 回落字面量 `C:\ProgramData` → 在 unix 上是个**相对路径**，
    // create_dir_all 一般失败 → nav.log 一行都写不出去。后果是 mac 上「AI邦 不出来 /
    // 托盘点了没反应」这类问题**完全没有证据可查**（这次就是靠读二进制才排到落点问题）。
    // 统一落到 bangxin_user_dir()：mac = ~/Library/Application Support/BangXin/nav.log。
    if cfg!(windows) {
        let base = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
        let dir = std::path::Path::new(&base).join("BangXin");
        let _ = std::fs::create_dir_all(&dir);
        return dir.join("nav.log");
    }
    let dir = bangxin_user_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir.join("nav.log")
}

fn nav_write(msg: &str) {
    use std::io::Write;
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let line = format!("[{ts}] {msg}\n");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(nav_log_path())
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// 上报 /x8/errlog.php（与前端 sendBeacon 同格式，服务端落 /var/lib/x8im/client_err.log）
fn nav_report(msg: &str) {
    let body = format!(
        "{{\"kind\":\"nav\",\"step\":\"host\",\"msg\":\"{}\",\"t\":{},\"href\":\"desktop\"}}",
        msg.replace('\\', "\\\\").replace('"', "\\\""),
        chrono::Utc::now().timestamp_millis()
    );
    if let Ok(c) = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(6))
        .build()
    {
        let _ = c
            .post("https://im.x8fde.com/x8/errlog.php")
            .header("Content-Type", "text/plain")
            .body(body)
            .send();
    }
}

/// 轮询主窗真实 URL 是否已含 marker（= 导航真的 commit 了）。每 120ms 一次，超时返 false。
/// 🔴 2026-11-13 二次现场：`navigate()` 返回 Ok 但 URL 根本没变（10:01 两次 probe 全 false，
/// 而 09:59 那次成功）。所以「调用成功」绝不能当成「已刷新」，必须回读 URL 才算数。
async fn wait_url_contains(app: &tauri::AppHandle, marker: &str, ms: u64) -> bool {
    let mut waited = 0u64;
    while waited < ms {
        if let Some(w) = app.get_webview_window("main") {
            if let Ok(u) = w.url() {
                if u.as_str().contains(marker) {
                    return true;
                }
            }
        } else {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        waited += 120;
    }
    false
}

/// 🔴 2026-11-13 三改：自愈链互斥（当天 11:07 主窗消失事故的根治）。
/// 现场：nav.log 11:07:20.902 与 11:07:21.025 两条命令在同一秒先后进入「③ 销毁重建主窗」，
/// 之后既无 `rebuild done ok` 也无报错 —— 两个任务各自 destroy() 后并发 build 同名窗口 main，
/// 主线程互等死锁。窗口销毁了没重建起来，托盘点击 / 桌面快捷方式全都唤不起
/// （Windows 单实例判据是「能否唤起主窗」，唤不起 = 观感上程序已死）。
/// 做法：整条自愈链（导航 + 重建）只允许一路在跑，后到者立刻返回 "busy"，
/// 既不重复导航互抢，也绝不并发建窗。RAII guard 保证任何 return / panic 都能复位。
static NAV_GUARD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct NavGuard;

impl NavGuard {
    fn acquire() -> Option<NavGuard> {
        use std::sync::atomic::Ordering;
        NAV_GUARD
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| NavGuard)
    }
}

impl Drop for NavGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        NAV_GUARD.store(false, Ordering::SeqCst);
    }
}

/// 🔴 2026-11-15 晚 P0 重构：同进程「销毁重建主窗」整段废弃 → 进程级重启。
///
/// 本机 18:12 事故现场：`nav.log` 走到「③ 销毁重建主窗」后 6s 看门狗触发，此后既无
/// `rebuild done ok` 也无 build err；进程 MainWindowTitle 为空、Responding=False，
/// 而且托盘 `show_main` 兜底完全无效 —— 卡死的是 Tauri 主事件循环。
///
/// 机制（上一版误判）：Tauri 2 的 `WebviewWindow::destroy()` 是向主事件循环投递
/// WindowMessage::Destroy，再在调用线程上阻塞等回执，真正执行 WebView2 `Close()` 的
/// 仍是**主线程**。所以把 destroy 塞进 `spawn_blocking` 只是把「等待」挪走，阻塞点没变，
/// 那一招零收益（旧注释声称「不再阻塞主 async 运行时」与实际不符）。
///
/// 更致命的是逻辑自相矛盾：能走到 ③ 的前提是「navigate 不 commit」= 渲染进程已不健康，
/// 而 `ICoreWebView2Controller::Close()` 恰恰要等渲染进程收尾 —— 渲染进程越坏 destroy 越卡。
/// 实测 16:48 那次 destroy 花 4.5s 才返回、18:12 这次永不返回。
///
/// 结论：换文档在同进程内没有可靠实现，唯一确定的手段是**换进程**，复用已跑通的
/// `x8_restart` 链路（spawn 自身 + `--x8-restart` + `--x8-parent` + DETACHED_PROCESS）。
/// 本次运行只允许重启一次：重启解决不了的问题再重启只会变成反复闪屏，比停在原地更糟。
static RUN_RESTART_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 窗口几何快照文件（重启后新进程据此还原位置/尺寸，避免每次自愈都把窗口弹回默认尺寸）。
/// 仅 Windows 落盘；非 Windows 返回 None，调用方直接跳过（零行为变化）。
#[cfg(target_os = "windows")]
fn win_geom_path() -> Option<std::path::PathBuf> {
    let base = std::env::var("ProgramData").ok()?;
    Some(std::path::Path::new(&base).join("BangXin").join("win.json"))
}

#[cfg(not(target_os = "windows"))]
fn win_geom_path() -> Option<std::path::PathBuf> {
    None
}

/// 重启前把主窗几何写盘。任何一步读不到就走默认值 —— 坏几何宁可还原失败，也绝不能影
/// 响启动。
fn save_win_geom(app: &tauri::AppHandle) {
    let Some(path) = win_geom_path() else { return };
    let mut w = 1180i64;
    let mut h = 800i64;
    let mut x: Option<i64> = None;
    let mut y: Option<i64> = None;
    let mut maxed = false;
    let mut vis = true;
    if let Some(win) = app.get_webview_window("main") {
        let sf = win.scale_factor().unwrap_or(1.0);
        if let Ok(s) = win.inner_size() {
            w = (s.width as f64 / sf).round() as i64;
            h = (s.height as f64 / sf).round() as i64;
        }
        if let Ok(p) = win.outer_position() {
            x = Some((p.x as f64 / sf).round() as i64);
            y = Some((p.y as f64 / sf).round() as i64);
        }
        maxed = win.is_maximized().unwrap_or(false);
        vis = win.is_visible().unwrap_or(true);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let v = serde_json::json!({"w": w, "h": h, "x": x, "y": y, "max": maxed, "vis": vis});
    let _ = std::fs::write(&path, v.to_string());
    nav_write(&format!("win_geom saved: {v}"));
}

/// setup 阶段调用：把上次快照的几何套回主窗，并立刻消费掉快照（只还原一次）。
/// 快照不存在（正常启动）时完全不动 —— 这是重启路径专用开关，不能污染正常启动。
fn restore_win_geom(app: &tauri::AppHandle) -> bool {
    let Some(path) = win_geom_path() else { return true };
    let Ok(txt) = std::fs::read_to_string(&path) else { return true };
    let _ = std::fs::remove_file(&path);
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else { return true };
    let Some(win) = app.get_webview_window("main") else { return true };
    let sf = win.scale_factor().unwrap_or(1.0);
    let w = v.get("w").and_then(|x| x.as_f64()).unwrap_or(1180.0);
    let h = v.get("h").and_then(|x| x.as_f64()).unwrap_or(800.0);
    let _ = win.set_size(tauri::PhysicalSize::new(
        (w * sf) as u32,
        (h * sf) as u32,
    ));
    let px = v.get("x").and_then(|x| x.as_f64());
    let py = v.get("y").and_then(|x| x.as_f64());
    if let (Some(x), Some(y)) = (px, py) {
        let _ = win.set_position(tauri::PhysicalPosition::new((x * sf) as i32, (y * sf) as i32));
    }
    if v.get("max").and_then(|x| x.as_bool()).unwrap_or(false) {
        let _ = win.maximize();
    }
    // 回读落日志：这是几何还原唯一的客观证据（窗口 API 在无 UIA 环境下取不到尺寸）
    if let Ok(s) = win.inner_size() {
        nav_write(&format!(
            "win_geom restored: 实际 {}x{}（文件 {}x{} pos={:?},{:?}）",
            s.width, s.height, w, h, px, py
        ));
    }
    // 快照里的可见性：false = 用户当时把窗口藏在托盘（托盘点「刷新界面」），
    // 那么重启后也不该硬弹到前台。
    v.get("vis").and_then(|x| x.as_bool()).unwrap_or(true)
}

/// 派生一个新进程接管界面（进程级重启的唯一实现，`x8_restart` 与自愈链 ③ 共用）。
///
/// 关键点（每一条都是踩过的坑，别删）：
///   · `--x8-restart` 让新进程跳过单实例互斥（此刻老进程还没退完）；
///   · `--x8-parent=<pid>` 让它精确等老进程退出再建窗，根治两个进程抢 EBWebView 用户数据目录；
///   · DETACHED_PROCESS 脱离老进程的进程组，避免老进程退出把新进程一起带走。
fn spawn_self_restart() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut cmd = std::process::Command::new(&exe);
    for a in std::env::args_os().skip(1) {
        // 之前就是被 --x8-restart 拉起来的，别把旧标记复制过去（否则越滚越多）；
        // --x8-parent 同理（只给一次，指向上一代老进程），避免 PID 链越滚越长。
        let s = a.to_string_lossy();
        if s == "--x8-restart" || s.starts_with("--x8-parent=") {
            continue;
        }
        cmd.arg(a);
    }
    cmd.arg("--x8-restart");
    // 把当前（老）进程 PID 传给子进程，让它精确等到本进程退出再建窗，根治 WebView2 抢锁。
    cmd.arg(format!("--x8-parent={}", std::process::id()));
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    Ok(())
}

/// 分诊探针：`im.x8fde.com:443` 至少能建 TCP 连接。
///
/// 目的不是测服务健康，而是把两类故障分开 —— 这两类的正确处置完全相反：
///   · 探针通、navigate 不 commit  → 渲染进程坏了 → 重启进程有意义；
///   · 探针不通（DNS/出口/断网）   → 重启只会从「窗口不见了」变成「白屏反复重启」，更糟，
///     必须原地放弃，让页面自己显示离线态 / 重试按钮。
fn origin_reachable() -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;
    let Ok(addrs) = "im.x8fde.com:443".to_socket_addrs() else {
        return false;
    };
    for a in addrs.take(3) {
        if TcpStream::connect_timeout(&a, Duration::from_millis(1500)).is_ok() {
            return true;
        }
    }
    false
}

/// ③ 兜底（替代原「销毁重建主窗」）：进程级重启。
///
/// 为什么不再同进程重建：见 `RUN_RESTART_DONE` 上方的机制说明 —— destroy 是主线程的
/// 阻塞调用，而能走到 ③ 的场景恰恰是渲染进程已不健康、最可能卡死的时候。
///
/// 顺序（每一步都不能省）：
///   ① 节流：本次运行只重启一次，第二次放弃并要求手动重启；
///   ② 分诊：网络不可达就别重启（会变成白屏反复重启）；
///   ③ 快照窗口几何 → 派生新进程 → 600ms 后**硬退**（卡死场景下事件循环不可信，
///      绝不能用 `app.exit(0)` 等握手）。
async fn restart_in_place(app: &tauri::AppHandle) -> Result<String, String> {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    // ① 节流
    if RUN_RESTART_DONE.swap(true, Ordering::SeqCst) {
        nav_write("restart aborted: 本次运行已重启过，请用户手动重启邦信");
        nav_report("nav-restart-giveup");
        return Ok("giveup".into());
    }

    // ② 分诊：探针放阻塞线程池（建连最长 4.5s，不能占 async 运行时）
    let reachable = tauri::async_runtime::spawn_blocking(origin_reachable)
        .await
        .unwrap_or(false);
    if !reachable {
        // 没真正重启 → 把配额还回去，等网络恢复后还能再自愈一次（否则一次误判就永久放弃）
        RUN_RESTART_DONE.store(false, Ordering::SeqCst);
        nav_write("restart aborted: im.x8fde.com:443 不可达（网络/出口问题，重启无益）");
        nav_report("nav-restart-offline");
        return Ok("offline".into());
    }

    // ③ 快照 → 派生 → 硬退
    save_win_geom(app);
    nav_write("③ 同进程重建已废弃 → 进程级重启（spawn --x8-restart）");
    nav_report("nav-restart-start");
    if let Err(e) = spawn_self_restart() {
        // 派生失败同样不算「用掉配额」，否则用户点刷新会一直 giveup
        RUN_RESTART_DONE.store(false, Ordering::SeqCst);
        nav_write(&format!("restart spawn err {e}"));
        return Err(format!("spawn: {e}"));
    }
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(600));
        // 插件宿主是独立进程，先收干净再走，避免新进程起来时抢同一批端口/socket。
        wx_sidecar::stop_all_fast();
        nav_write("restart: 硬退 process::exit(0)（不等事件循环）");
        std::process::exit(0);
    });
    Ok("restarting".into())
}

/// ①主通道 + 自愈链（2026-11-13 二改 / 2026-11-15 晚三改）：
///   ① navigate → ② eval 注入导航 → ③ 进程级重启（原「销毁重建主窗」已废弃，
///     原因见 `RUN_RESTART_DONE` 上方机制说明）。
/// 每一步都用「真实 URL 是否变化」判定，而不是信 API 的返回值。
/// 前端只发一次信号、不传 URL，因此不存在双导航竞态，也不需要它再补任何兜底。
#[tauri::command]
async fn x8_reload_ui(app: tauri::AppHandle) -> Result<String, String> {
    // 已有一路自愈在跑（最长约 5.3s）时直接让位：再开一路只会重复导航、甚至并发建窗。
    let _guard = match NavGuard::acquire() {
        Some(g) => g,
        None => {
            nav_write("reload_ui busy（上一次自愈未结束，忽略本次）");
            return Ok("busy".into());
        }
    };
    let ts = chrono::Utc::now().timestamp_millis();
    let target = format!("https://im.x8fde.com/app/?_b=rust{ts}");
    let marker = format!("_b=rust{ts}");
    nav_write(&format!("reload_ui start target={target}"));

    // ① 宿主 navigate
    if let Some(w) = app.get_webview_window("main") {
        match tauri::Url::parse(&target) {
            Ok(u) => {
                if let Err(e) = w.navigate(u) {
                    nav_write(&format!("① navigate err {e}"));
                }
            }
            Err(e) => nav_write(&format!("① url err {e}")),
        }
    }
    if wait_url_contains(&app, &marker, 2600).await {
        nav_write("① navigate commit ✓");
        nav_report("nav-ok step=navigate");
        return Ok("navigate".into());
    }
    nav_write("① navigate 未 commit（URL 没变）→ ② eval 导航");

    // ② 页面内 eval 导航（绕开宿主 navigate 通道）
    if let Some(w) = app.get_webview_window("main") {
        let js = format!(
            "(function(){{try{{location.replace('{t}')}}catch(e){{try{{location.href='{t}'}}catch(e2){{}}}}}})()",
            t = target
        );
        if w.eval(&js).is_ok() {
            nav_write("② eval-nav fired");
        }
    }
    if wait_url_contains(&app, &marker, 2200).await {
        nav_write("② eval commit ✓");
        nav_report("nav-ok step=eval");
        return Ok("eval".into());
    }
    nav_write("② eval 未 commit → ③ 进程级重启（同进程重建已废弃）");

    // ③ 进程级重启
    restart_in_place(&app).await
}

/// ③兜底（前端 15 秒看门狗调用）：eval 注入导航 → 仍未跳则销毁主窗重建。
#[tauri::command]
async fn x8_rebuild_ui(app: tauri::AppHandle) -> Result<String, String> {
    // 与 x8_reload_ui 共用一把锁：看门狗兜底与用户点击撞车时，后到者让位。
    let _guard = match NavGuard::acquire() {
        Some(g) => g,
        None => {
            nav_write("rebuild_ui busy（上一次自愈未结束，忽略本次）");
            return Ok("busy".into());
        }
    };
    let ts = chrono::Utc::now().timestamp_millis();
    let target = format!("https://im.x8fde.com/app/?_b=rb{ts}");
    let marker = format!("_b=rb{ts}");
    nav_write(&format!("rebuild_ui start target={target}"));

    if let Some(w) = app.get_webview_window("main") {
        let js = format!(
            "(function(){{try{{location.replace('{t}')}}catch(e){{try{{location.href='{t}'}}catch(e2){{}}}}}})()",
            t = target
        );
        if w.eval(&js).is_ok() {
            nav_write("rebuild_ui eval-nav fired");
        }
    }
    if wait_url_contains(&app, &marker, 2200).await {
        nav_write("rebuild_ui after-eval commit ✓");
        return Ok("eval-nav".into());
    }
    nav_write("rebuild_ui eval 未 commit → 进程级重启");
    restart_in_place(&app).await
}

fn main() {
    // 2026-10-23 老8：先上硬单实例锁，再初始化任何 GUI/WebView（顺序不能反，
    // 反了会先闪一下登录窗再退出，观感像崩溃）。
    #[cfg(target_os = "windows")]
    enforce_single_instance();

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    enforce_single_instance();

    #[cfg(target_os = "linux")]
    linux_configure_webkit();

    tauri::Builder::default()
        // 2026-10-23 老8「同一台电脑只许一个实例」：**移除** tauri-plugin-single-instance。
        // 它的判据是"首个实例的隐藏消息窗是否存在"，找不到就放行、找到就杀新实例，
        // 与"用户能不能看到界面"无关 —— 本机一个无主窗的高权限残留进程把它骗住，
        // 结果每次双击图标都被安静杀掉（表现为「程序没了」）。改由 main() 开头的
        // enforce_single_instance() 兜：能否唤起主窗才是判据。
        // 自动升级：updater 检查/下载安装 + process 重启（远程页面经 ACL 调用）
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .invoke_handler(tauri::generate_handler![
            get_rustdesk_id,
            open_rustdesk,
            close_rustdesk,
            // ⚠️ 2026-10-16 教训：新增命令**四处都要同步** —— 这里（generate_handler!）、
            // build.rs 的命令名单（生成 permissions/*.toml）、capabilities/remote.json
            // （远端页面 ACL 放行）、以及函数本身的 #[tauri::command]。
            // 本轮首次只补了后三处，漏了这里 → 1.5.5 的 assist_idle_reap 前端调用直接
            // command not found，空闲回收静默失效（1.5.6 修复）。
            assist_idle_reap,
            ensure_rustdesk,
            // 2026-11-06：三端登录后无条件对齐远控服务器配置（只写盘，不提权、不拉进程）
            ensure_assist_server,
            // 2026-10-17：Linux 缺远程协助组件时一键安装（Windows/mac 恒返回 already）
            install_rustdesk,
            // 2026-11-06 Linux P2P 传输内核（Go+pion sidecar；非 Linux 返回 null）
            // 页面 https + WebKitGTK 混合内容拦截 → 本地 WS 不可用，改走下面三条 IPC 命令
            p2p_sidecar_info,
            p2p_sc_open,
            p2p_sc_send,
            p2p_sc_recv,
            p2p_sc_close,
            // 2026-10-26 Linux 应用内升级（下载本架构 deb/rpm → pkexec 安装 → 自动重启）
            linux_self_update,
            // 2026-11-02 macOS 静默升级：Rust 进程内调 tauri-plugin-updater 的原生通道
            //（远程域下前端 async 插件命令 IPC 永久 pending，取不到 rid，故必须走 Rust 侧）
            mac_updater::mac_self_update,
            // 2026-11-04 老8「升级前要在 im 内提示、可取消、有进度」：
            // 状态轮询（同步即返）+ 稍后提醒（落盘冷藏）。三端同名同签名，见 mac_updater.rs。
            mac_updater::mac_upd_status,
            mac_updater::mac_upd_snooze,
            // 2026-11-06 分块落盘：大文件（>48MB）绕开单次 base64 过 IPC 的硬上限
            save_chunk_begin,
            save_chunk_append,
            save_chunk_end,
            open_workbench,
            // 扩展面板（2026-10-24）：小程序独立内嵌窗口单例 ext-<appId>
            ext_open_window,
            ext_close_window,
            ext_focus_window,
            open_in_browser,
            // 桌面端聊天文件下载落盘：带 Bearer 调系统 curl 拉私有文件（WebView <a download> 不生效）
            download_file,
            save_bytes,
            // 2026-10-23 截屏：唤系统截图（ms-screenclip）→ 读剪贴板 → PNG base64 回前端
            screen_clip_grab,
            // 在文件夹中显示（对齐微信）：定位并选中已下载文件
            reveal_file,
            notify_unread,
            away_notice,
            // 2026-10-22 右下角卡通 FDE 浮窗（微信式新消息头像提醒）
            fde_float_show,
            fde_float_hide,
            fde_float_click,
            // 2026-11-16 AI邦 卡通精灵：手动收起 / 拖动 + 位置记忆 / 免打扰 / 状态查询
            pet_close,
            pet_drag_begin,
            pet_pos_save,
            pet_set_muted,
            pet_set_avatar,
            pet_status,
            // 2026-10-23 升级中进度小窗（老8：要看到正在下载 / 已用 / 剩余时间）
            upd_float_show,
            upd_float_hide,
            // 2026-10-24 标题栏原生拖动（RDP/虚拟机下 IPC 延迟导致 startDragging 失效的根治）
            drag_begin,
            // 2026-10-25 标题栏窗控加固：min/max/close/is_maximized 经 core.invoke 通道，
            // 绕开不可靠的 .window 全局命名空间（见 win_minimize 等命令注释）
            win_minimize,
            win_toggle_maximize,
            win_close,
            win_is_maximized,
            // 2026-10-25 升级命令的无进度通道兜底版（前端 Channel 构造失败时改调）
            win_patch_update_nc,
            win_auto_update_nc,
            // 2026-10-26 重写：唯一升级入口（同步 fn）+ 稍后提醒静默。
            // 见 updater.rs：五套并行实现已收敛成一套状态机。
            updater::win_upd_start,
            updater::win_upd_snooze,
            detect_wechat,
            // 2026-10-23 已读回执授权：OS 级窗口焦点，与 notify_unread 同源（四处同步）
            win_is_focused,
            // 2026-10-22 Windows 静默自动升级（零点击/零 UAC）：前端启动检查发现新版
            // 即调用；失败时前端回落到 plugin:updater|download_and_install 手动通道。
            // 四处同步见 build.rs / capabilities/remote.json / permissions/autogenerated/。
            win_auto_update,
            win_patch_update,
            // 2026-10-28 应用内自重启（替代不可靠的 plugin:process|restart）
            x8_restart,
            // 2026-11-05 换版导航收归宿主（页面自己 location.replace 会被 WebView2 吞掉）
            x8_nav,
            // 2026-11-13 刷新链重做：主通道（Rust 自拼 URL 导航）+ 兜底（eval 导航→重建主窗）
            x8_reload_ui,
            x8_rebuild_ui,
            // 2026-11-05 登录凭据宿主侧留底：升级 / 清缓存后仍能自动登录，不必再扫码
            x8_auth_save,
            x8_auth_load,
            x8_auth_clear,
            // 2026-11-07 内置邮件（Rust 原生，四处同步：本表 / build.rs AppManifest /
            //   capabilities/remote.json / permissions/autogenerated/ 自动生成）
            mail::mail_config_get,
            mail::mail_config_set,
            mail::mail_config_clear,
            mail::mail_list,
            mail::mail_sync,
            mail::mail_read,
            mail::mail_attachment,
            mail::mail_mark_seen,
            mail::mail_send,
            mail::mail_unseen,
            mail::mail_secret_get,
            // 2026-11-10 WX 消息（插件 + 本地 wx_* 表；四处同步：本表 / build.rs
            //   AppManifest / capabilities/remote.json / 命令定义处）
            wx_cache::plugins_list,
            wx_cache::wx_plugin_info,
            // 2026-11-17 本机 W 服务探活（127.0.0.1:60999；mac 端 W消息 用它判服务在不在）
            wx_cache::wx_svc_ping,
            wx_cache::wx_plugin_start,
            wx_cache::wx_plugin_prepare,
            wx_cache::wx_plugin_update_now,
            wx_cache::wx_plugin_stop,
            wx_cache::wx_conversations,
            wx_cache::wx_messages,
            wx_cache::wx_contacts,
            wx_cache::wx_follow_get,
            wx_cache::wx_follow_set,
            wx_cache::wx_customer_list,
            wx_cache::wx_customer_set,
            wx_cache::wx_status,
            mac_wx_setup::wx_setup_state,
            mac_wx_setup::wx_setup_run,
            mac_wx_setup::wx_restore_wechat,
            mac_wx_setup::wx_wechat_relogin,
            wx_cache::wx_sync_now,
            wx_cache::wx_set_active,
            wx_cache::wx_pick_messages,
            wx_cache::wx_media_data,
            // 2026-11-13 自动落盘文件保留期（个人资料 → 本地文件自动清理）
            // 四处同步：本表 / build.rs 名单 / capabilities/remote.json / 命令定义处
            file_retention_get,
            file_retention_set,
            file_retention_clean,
            // 2026-11-14 开机自启动（默认开、可关）
            // 四处同步：本表 / build.rs 名单 / capabilities/remote.json / 命令定义处
            autostart_get,
            autostart_set,
            // 2026-11-16 IPC 卡死定性探针：同步/异步双 ping，只回不卡（四处同步）
            ipc_probe::x8_ping,
            ipc_probe::x8_ping_async
        ])
        .setup(|app| {
            // 2026-11-16 IPC 定性探针：日志落 app_data_dir/ipc_probe.log
            if let Ok(dir) = app.path().app_data_dir() {
                crate::ipc_probe::init(dir);
                crate::ipc_probe::mark("SETUP probe ready".to_string());
            }
            // 2026-11-10 插件宿主：挂看门狗（插件被风控杀掉 / 自己 panic 后自愈）。
            // 注意不在这里拉起插件 —— 用户点开「Wx消息」页签时才启动，不占常驻内存。
            plugin_host::init(app.handle());
            // 2026-11-15 晚：自愈链 ③ 改成进程级重启后，新进程据此还原窗口几何。
            // 只有重启前写过快照才生效（一次性消费），正常启动完全不受影响。
            let want_vis = restore_win_geom(app.handle());
            // mac/linux 不走上面那段（那边没有「建窗不置可见」的问题），避免 unused 警告
            #[cfg(not(target_os = "windows"))]
            let _ = want_vis;
            // 🔴 2026-11-15 晚（老8「界面刷新不了」现场）：自愈链 ③ 改成进程级重启后，
            // 20:03 那次重启出现「进程活着、主窗存在、位置尺寸都还原了，但 IsWindowVisible=0」
            // —— 用户眼前什么都没有，只能判程序挂了。EnumWindows 实测证据：
            //   class='Tauri Window'  title='邦信 v1.6.99 … [B10089-庞小刚]'
            //   visible=False  rect=(334,77) 1196x839   ← 几何还原成功，唯独没显示
            // tao 在 Windows 上有极小概率建窗后不置 WS_VISIBLE（与旧进程退出的时序有关），
            // 且没有任何日志能看出它发生过。这里只对重启路径补两道 show：
            //   ① setup 内立刻 show（窗口此刻一定已建好，restore_win_geom 已成功操作过它）
            //   ② 1.5s 后再复查一次（防 Tauri/WebView2 初始化末段又把可见性改回去）
            // 正常启动不走这段，行为零变化。
            #[cfg(target_os = "windows")]
            if want_vis && std::env::args().any(|a| a == "--x8-restart") {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.unminimize();
                    if !w.is_visible().unwrap_or(false) {
                        nav_write("restart-child: setup 内 show 未生效（窗口仍不可见）");
                    } else {
                        nav_write("restart-child: setup 内强制 show ok");
                    }
                }
                let h2 = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    if let Some(w) = h2.get_webview_window("main") {
                        if !w.is_visible().unwrap_or(false) {
                            let _ = w.show();
                            let _ = w.unminimize();
                            let _ = w.set_focus();
                            nav_write("restart-child: 1.5s 复查发现主窗不可见 → 已补 show");
                        }
                    }
                });
            }
            // 2026-11-07 方案 B PoC：本地静态资源拦截（未配置时直接 return，零行为变化）
            #[cfg(windows)]
            local_assets::install(app.handle());
            // 2026-11-06：本地落盘 HTTP 服务（绕开 WebView2 IPC 大包「发了不回」）
            save_http_start(app.handle().clone());
            // 2026-11-16：组件自主更新（不依赖前端点页签触发，见函数注释）
            spawn_wx_autoupdate(app.handle().clone());
            // 2026-11-13：自动落盘文件保留期清理（下载/downfiles，默认 10 天，最少 1 天）
            spawn_retention_cleaner();
            // 2026-11-14 开机自启动：默认开启 + 每次启动对齐系统自启动项（失败只记日志）
            autostart_boot_sync();
            // 2026-11-07 新邮件轮询（30 分钟 / 9-18 点 / 固定相位抖动 / 启动补查）
            mail::spawn_poller(app.handle().clone());
            // 2026-11-07 P2-8 探针：Linux WebKitGTK 默认 enable-webrtc=FALSE，wry 没开。
            // 显式打开；前端黄条上的「🔍 诊断」可复制真机的 RTCPeerConnection 状态用于验收。
            #[cfg(target_os = "linux")]
            enable_linux_webrtc(app.handle());
            // 2026-11-04 回退：主窗口恢复为 tauri.conf.json 配置创建（additionalBrowserArgs
            // 改在 config 注入）。上一轮在此用 WebviewWindowBuilder 代码创建窗口，触发首帧导航
            // 竞态 → 升级后白屏（#app 为空，刷新才好）。config 创建由 Tauri 在 WebView2 环境
            // 就绪后才导航，渲染稳定（1.6.49 验证过）。
            // 2026-10-17 关键修复：升级后清理 WebView2 user data 目录，防止旧版本 bundle 缓存
            // 导致新安装/升级后仍报 `async is not defined` 白屏。identifier = com.x8fde.im。
            // ⚠️ 2026-10-23 重大修正：原实现**每次启动都删**。两个实例并存时，后启动的那个
            //   会把先启动实例的 WebView2 数据目录整个删掉 → 先启动的立刻崩溃退出，
            //   表现为「开第二个，第一个自己没了」「双击图标一会儿有新窗口一会儿又消失」。
            //   改成只在**版本号变化**（刚升级完）时清一次：既保住升级后清缓存的目的，
            //   又不会让同版本的重复启动互相残杀。版本戳存在 %LOCALAPPDATA%\BangXin\ 下
            //   （那是独立目录，不在被删的 com.x8fde.im 里）。
            // 仅 Windows 需要（WebView2 缓存问题）；mac/linux 的 app data 目录存登录态与配置，
            // 每启清理会导致用户每次启动被登出，故用 cfg(windows) 隔离，跟发 mac/linux 时不携带。
            #[cfg(target_os = "windows")]
            {
                // 2026-11-03 修稳 in-app 重启：重启路径（--x8-restart）强制清一次
                // WebView2 user data 目录，保证换版后拉到的是全新 /app/ 而非磁盘缓存的旧界面
                // （版本没变时下面的版本门槛不会触发清理，导致重启完还是旧版）。
                let is_restart = std::env::args().any(|a| a == "--x8-restart");
                let ver = app.package_info().version.to_string();
                let base = std::env::var("LOCALAPPDATA").unwrap_or_default();
                let stamp = std::path::Path::new(&base).join("BangXin").join("webcache_ver.txt");
                let prev = std::fs::read_to_string(&stamp).unwrap_or_default();
                if prev.trim() != ver || is_restart {
                    // 2026-11-04 修：彻底移除 WebView2 用户数据目录清理。
                    // 历史无论「整目录 remove_dir_all」还是「只删缓存子目录」，都会让 Chromium
                    // 在下次启动时判定 profile 损坏而重建整个 profile，连 Local Storage 里的
                    // x8_token 一起清掉 → 升级/重启后必重新扫码（用户反复反馈的根因）。
                    // 现 index.html 是 no-cache、JS 文件名带 contenthash、?v= 每次发版变化，
                    // 旧 bundle 缓存不可能命中，无需清缓存即可避免白屏。清理整体移除。
                    // 仅保留版本戳（供其它逻辑判断，不再触发任何删除）。
                    if let Some(dir) = stamp.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::write(&stamp, &ver);
                }
            }

            // 2026-11-04 兜底：冷启动首屏若因慢加载空白，15s 后自动 reload 一次（等价于手动「刷新」）。
            // 与 web 看门狗共用 sessionStorage 闸门 'x8_blank_reload'，避免重复刷新。web 侧已部署
            // 自动恢复；此处作为客户端兜底，即使远端页面未更新也能自愈白屏。
            #[cfg(target_os = "windows")]
            {
                let h = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(15));
                    if let Some(w) = h.get_webview_window("main") {
                        let _ = w.eval("(function(){var k='x8_blank_reload';if(sessionStorage.getItem(k))return;var a=document.getElementById('app');if(a&&a.children.length===0){sessionStorage.setItem(k,'1');location.reload();}})();");
                    }
                });
            }

            // 2026-11-03 单实例唤醒：把 AppHandle 存进全局，供 mac/linux 监听线程聚焦主窗
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                let _ = APP_HANDLE.lock().unwrap().replace(app.handle().clone());
            }

            let open = MenuItem::with_id(app, "open", "打开邦信", true, None::<&str>)?;
            // 2026-11-17 老8：托盘加「召唤 AI邦」+「开机自启动」勾选开关。
            // 召唤 = 精灵唯一的手动出镜入口（原来只能等一条不属于当前会话的新消息，
            // 主窗开着正看那个会话时永远不出现，用户无从验证它到底在不在）。
            let summon = MenuItem::with_id(app, "summon", "召唤 AI邦", true, None::<&str>)?;
            // 勾选态取系统实际值（用户可能在任务管理器里禁用过），不是配置文件里的意图
            let autostart = CheckMenuItem::with_id(
                app,
                "autostart",
                "开机自启动",
                true,
                autostart_sys::is_on(),
                None::<&str>,
            )?;
            // 2026-11-13：页面里的刷新链在 WebView2 上不可靠（导航发起却不 commit），
            // 托盘这一项由 Rust 直接导航，完全不经过页面 JS/IPC，是最后一道可靠入口。
            let refresh_ui = MenuItem::with_id(app, "refresh_ui", "刷新界面", true, None::<&str>)?;
            // 2026-10-23 托盘菜单完善（A）：加「检查更新」+ 版本只读标签 + 分隔线，与微信式托盘一致
            let check_updates = MenuItem::with_id(app, "check_updates", "检查更新", true, None::<&str>)?;
            let ver_label = MenuItem::with_id(
                app, "ver", &format!("版本 v{}", app.package_info().version), false, None::<&str>,
            )?;
            let sep = PredefinedMenuItem::separator(app)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &open,
                    &summon,
                    &refresh_ui,
                    &check_updates,
                    &autostart,
                    &ver_label,
                    &sep,
                    &quit,
                ],
            )?;
            // TrayIcon 没有 menu() getter（tauri 2.12 只有 set_menu），勾选态只能靠
            // 自己留着这个 item 的句柄来回填 —— clone 一份 move 进菜单事件闭包。
            let autostart_item = autostart.clone();
            TrayIconBuilder::with_id("x8im-tray")
                .icon(tray_icon_image())
                .tooltip("邦信即时通讯")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "open" => show_main(app),
                    // 召唤 AI邦：不看消息、不看 enabled，点了就必须在屏上出现
                    "summon" => pet_summon(app),
                    // 开机自启动：点一下翻转，落盘 + 写系统项；勾选态一律按**写后的系统实际值**回填，
                    // 写失败（注册表被策略锁死等）时不会出现「勾上了其实没生效」的假象
                    "autostart" => {
                        let want = !autostart_sys::is_on();
                        match autostart_set(want) {
                            Ok(_) => nav_write("tray autostart ok"),
                            Err(e) => nav_write(&format!("tray autostart err {e}")),
                        }
                        let now = autostart_sys::is_on();
                        let _ = autostart_item.set_checked(now);
                        // 让页面的「个人资料 → 开机自启动」跟着变（它监听同一个事件）
                        evq::push(
                            &app,
                            "x8-tray-autostart",
                            serde_json::json!({ "enabled": now }),
                        );
                    }
                    // 刷新界面：Rust 直接导航，不经页面 JS/IPC —— 页面刷新链失灵时的可靠入口
                    "refresh_ui" => {
                        let h = app.clone();
                        // 2026-11-13 二改：x8_reload_ui 现在是 async 自愈链（navigate→eval→重建），
                        // 托盘这条必须走 async 运行时，不能再用裸线程同步调。
                        tauri::async_runtime::spawn(async move {
                            match x8_reload_ui(h).await {
                                Ok(m) => nav_write(&format!("tray refresh_ui ok mode={m}")),
                                Err(e) => nav_write(&format!("tray refresh_ui err {e}")),
                            }
                        });
                    }
                    // 检查更新：发事件给前端，复用 checkUpdBoth 的进度条/静默升级 UI（manual=true）
                    "check_updates" => { evq::push(&app, "x8-tray-check-updates", serde_json::json!(null)); }
                    // 2026-11-07 老8 二报「托盘退出仍无反应」：上面的 600ms 硬退兜底
                    // 和 kill_rustdesk_procs() 写在**同一线程**，一旦 kill 卡在管道 EOF
                    // （见 win_run_capture_to 注释），app.exit(0) 与 process::exit(0)
                    // 两条路一起走不到 → 进程永远不退。改为并行看门狗：菜单事件一到
                    // 就起一个独立线程计时，800ms 后无条件硬退（≈原 120ms 让位 +
                    // 600ms 兜底窗口，不再受清理动作阻塞影响）。
                    "quit" => {
                        std::thread::spawn(|| {
                            std::thread::sleep(std::time::Duration::from_millis(800));
                            // P1-3：800ms 硬退兜底同样要收插件。
                            wx_sidecar::stop_all_fast();
                            std::process::exit(0);
                        });
                        let h = app.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(120));
                            kill_rustdesk_procs();
                            h.exit(0);
                        });
                    }
                    _ => {}
                })
                // 2026-11-17 老8 报「托盘有消息一闪一闪的，就是点不开」：
                // 根因是这里原先只绑了 DoubleClick，而上面 show_menu_on_left_click(false)
                // 又让左键不弹菜单 —— 左键单击既不开窗、也无人接管，等于点在空气上
                // （用户看到图标在闪却怎么点都没反应，必被判成「死了」）。
                // 改为左键单击即唤起（与微信/QQ 一致），双击也保留，show_main 幂等不会互相干扰。
                .on_tray_icon_event(|tray, event| {
                    // 2026-11-17 C：托盘点击此前在日志里完全无痕，现场无法区分
                    // 「事件根本没到进程」和「到了但处理无声」。入口无条件落一笔。
                    // ⚠️ Move 必须排除：鼠标划过图标区就发，一次几十条，会把 nav.log 冲爆。
                    match &event {
                        TrayIconEvent::Move { .. } => return,
                        TrayIconEvent::Click {
                            button,
                            button_state,
                            ..
                        } => nav_write(&format!("tray ev: click {button:?}/{button_state:?}")),
                        TrayIconEvent::DoubleClick { button, .. } => {
                            nav_write(&format!("tray ev: dblclick {button:?}"))
                        }
                        // Enter / Leave 是鼠标进出图标区，与「点了没反应」无关，不记
                        _ => {}
                    }
                    match event {
                        TrayIconEvent::Click {
                            button: tauri::tray::MouseButton::Left,
                            button_state: tauri::tray::MouseButtonState::Up,
                            ..
                        } => tray_open_main(tray.app_handle()),
                        TrayIconEvent::DoubleClick { .. } => tray_open_main(tray.app_handle()),
                        _ => {}
                    }
                })
                .build(app)?;
            // 2026-11-17 二改：**不再钉 v4**（那是 1.7.11/1.7.12「点不动」的根因，见 tray_hook 注释）。
            // 改为给 tray_icon_app 挂 wndproc 钩子：点击不再依赖 Shell_NotifyIconGetRect，
            // 且回调消息原样进 nav.log（`tray raw:`），下次再哑掉能一眼分清「没收到」和「收到了没处理」。
            #[cfg(target_os = "windows")]
            tray_hook_install(app.handle().clone());
            // 启动收口（1.7.13）：图标记录版本修回 v3 + 硬写 tooltip + 注册自检。
            // 真机实测（1.7.12）：图标在、szTip 空、点击消息到了窗口却没人接（v4 记录残留）。
            #[cfg(target_os = "windows")]
            {
                let h = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    tray_fixup(&h);
                });
            }
            // 2026-11-17：托盘点击「看得见、点不动」的兜底——常驻自愈线程（见 start_tray_heal）。
            start_tray_heal(app.handle().clone());
            // 2026-11-17 C：上次自动升级到底成没成？5 秒后自检一次（成功清残留包 / 失败计数）。
            // 仅 Windows：整套升级状态机依赖 ProgramData / schtasks / NSIS。
            #[cfg(windows)]
            {
                let h = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    upgrade_selfcheck(&h);
                });
            }
            // 2026-11-17：AI邦 浮窗**取消启动期预建**。
            // 预建走的是 `visible(false)` 建窗 → 真机上窗口 vis=1、rect 正确、非 cloaked，
            // 但 WebView2 一点内容都不画（连不透明档该有的白底都没有）＝隐藏期初始化合成层失败。
            // 现在运行期建窗已能成功（靠独立 data_directory，见 fde_build_window），
            // 所以改为「要显示时就地建一个可见窗口」，最直观也最不容易出这种歪状态。
            // 2026-11-17 晚：AI邦 **默认打开着**（老8：平时不需要自动关，想关有 ✕）。
            // 延迟 3s 等主窗首屏先出来，别跟登录页抢焦点；建的是可见窗口（见 fde_build_window
            // 里「隐藏期建窗不画内容」的坑），出来就钉住，只有 ✕ / 右键收起 / 点精灵才收。
            {
                let h = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    pet_autoshow(&h);
                });
            }
            // 诊断口子：`set X8_PET_DEMO=1` → 4 秒后自动召唤一次；`=2` → 4s 与 16s 各一次
            // （第二次专测「重复召唤」：早先 destroy 竞态就是第二下把窗口搞没的）。
            // 只为「不点托盘也能验证浮窗」用（售后排查、真机验收），不设就完全不触发。
            match std::env::var("X8_PET_DEMO").as_deref() {
                Ok("1") => {
                    let h = app.handle().clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(4));
                        pet_summon(&h);
                        nav_write("[pet] X8_PET_DEMO 自动召唤 #1");
                    });
                }
                Ok("2") => {
                    let h = app.handle().clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(4));
                        pet_summon(&h);
                        nav_write("[pet] X8_PET_DEMO 自动召唤 #1");
                        std::thread::sleep(std::time::Duration::from_secs(12));
                        pet_summon(&h);
                        nav_write("[pet] X8_PET_DEMO 自动召唤 #2（重复召唤）");
                    });
                }
                _ => {}
            }
            // 2026-10-18 问题2 排查结论：boot 路径无启动期 hide（仅 CloseRequested 才 hide 到托盘），
            // 甘真机确认主窗是手动最小化，非代码缺陷。此处加一道保险——启动即把主窗唤到前台：
            // mac 上 Dock/菜单栏若自动隐藏、托盘图标易被忽略，会观感「窗口没出来」。Windows/Linux
            // 本就显示，show+unminimize+focus 为幂等操作，不改变既有行为。
            if let Some(w) = app.get_webview_window("main") {
                // 2026-10-24 老8 报「左/下/右有明显黑线 + 4角要圆润」：decorations:false 的
                // 无边框窗，DWM 默认会画 1px 深色边框且不倒角。两条属性一起设：
                //   attr 33 DWMWA_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND(2) → Win11 圆角
                //   attr 34 DWMWA_BORDER_COLOR = DWMWA_COLOR_NONE(0xFFFFFFFE) → 抹掉黑边
                // Win10 及以下 attr 33/34 不识别，DwmSetWindowAttribute 返回错误码，忽略即可。
                #[cfg(windows)]
                {
                    use windows_sys::Win32::Graphics::Dwm::{
                        DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE,
                        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
                    };
                    if let Ok(hwnd) = w.hwnd() {
                        let corner: i32 = DWMWCP_ROUND;
                        let _ = unsafe {
                            DwmSetWindowAttribute(
                                hwnd.0,
                                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                                &corner as *const _ as *const _,
                                4,
                            )
                        };
                        let none: u32 = DWMWA_COLOR_NONE;
                        let _ = unsafe {
                            DwmSetWindowAttribute(
                                hwnd.0,
                                DWMWA_BORDER_COLOR as u32,
                                &none as *const _ as *const _,
                                4,
                            )
                        };
                    }
                }
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
            // 2026-10-17 老8 定「确保不常驻，节约资源」：启动即回收任何无活跃会话的残留
            // X8Assist 进程（安装升级残留 / 上次异常退出未清）。有会话时 assist_idle_reap 不动。
            let _ = tauri::async_runtime::spawn(async {
                let _ = assist_idle_reap().await;
            });
            // 2026-10-26 重写：升级调度交给 updater 模块——启动 8 秒首检，之后每 15 分钟复查。
            // 完全不经过前端 IPC，IPC 全挂也能升上去（前端点按钮只是「立刻跑一次」的加速器）。
            #[cfg(windows)]
            updater::spawn_scheduler(app.handle().clone());
            // 2026-11-02 macOS：同一套静默口径（启动 60s 首检 + 15 分钟复查），
            // 但走的是独立端点 latest-mac.json，与 Windows 的升级状态机互不干扰。
            #[cfg(target_os = "macos")]
            mac_updater::spawn_scheduler(app.handle().clone());
            // 2026-11-02 白屏自愈：后台线程每 30s 探一次系统是否睡过（墙钟前进远超轮询间隔 =
            // sleep/hibernate，Instant 不可靠故只用 SystemTime）。唤醒后置位 RESUME_RELOAD；
            // 主窗当时正显示则立即 reload 恢复白屏，否则留给下次 show_main（托盘双击/菜单打开）。
            // reload 走前端静默续登，不丢登录。NTP 大跳变可能误触发一次 reload，无害。
            {
                let h = app.handle().clone();
                std::thread::spawn(move || {
                    let mut last_wall = std::time::SystemTime::now();
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(30));
                        let now_wall = std::time::SystemTime::now();
                        if let Ok(d) = now_wall.duration_since(last_wall) {
                            if d.as_secs() > 90 {
                                RESUME_RELOAD.store(true, std::sync::atomic::Ordering::SeqCst);
                                if let Some(w) = h.get_webview_window("main") {
                                    if w.is_visible().unwrap_or(false) {
                                        let _ = w.eval("location.reload()");
                                    }
                                }
                            }
                        }
                        last_wall = now_wall;
                    }
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // 点 ✕ 的语义分两种（2026-10-14 老8 定）：
            //   main      → 不退出，藏到托盘（IM 常规行为）
            //   workbench → 真关闭，并把主窗邦信唤到前台（关闭 = 返回 IM）
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    // 2026-11-17 A：藏到托盘的前提是「托盘那个图标真能点」。
                    // 老8 新机实测：升级断在半路 → 托盘只剩僵尸残影 → 点 X 窗口藏起来，
                    // 然后既叫不回来、托盘也没菜单，整个人以为程序死了。
                    // 先就地重注册一次；仍拿不到矩形（含图标被折进溢出面板）就**降级最小化**：
                    // 窗口留在任务栏，人还能点回来，绝不制造「窗口和托盘同时消失」的死局。
                    let app = window.app_handle();
                    // 2026-11-17 C：这条链此前**一行日志都没有** —— 现场「点 X 没反应」时
                    // 既看不出 CloseRequested 到没到、也看不出 hide 成没成。入口先落一笔：
                    // vis_before 用 Win32 查（tauri 的 is_visible 读的是会失同步的标志位）。
                    #[cfg(target_os = "windows")]
                    let hw: Option<*mut std::ffi::c_void> = window.hwnd().ok().map(|h| h.0);
                    #[cfg(target_os = "windows")]
                    let vis_before = hw.map(w32_visible).unwrap_or(false);
                    #[cfg(not(target_os = "windows"))]
                    let vis_before = window.is_visible().unwrap_or(false);
                    let rect_ok = tray_rect_alive(app);
                    let tray_ok = rect_ok || tray_revive(app);
                    if tray_ok {
                        let _ = window.hide();
                        // 2026-11-17 A：hide() 可能因 tao 幂等位陈旧而**空转**（返回 Ok 但
                        // 窗口纹丝不动，正是「第 1 次有效、第 2 次起永久失效」的现场）。
                        // 复核真实可见性，没藏掉就 Win32 强压一次。
                        #[cfg(target_os = "windows")]
                        if hw.map(w32_visible).unwrap_or(false) {
                            if let Some(h) = hw {
                                w32_hide(h);
                            }
                            nav_write("close: tao hide 空转（标志位陈旧）→ Win32 SW_HIDE 强制隐藏");
                        }
                        // 2026-11-02 白屏自愈：记下藏入托盘的时刻，show_main 据此判断是否需要 reload
                        *MAIN_HIDDEN_AT.lock().unwrap() = Some(std::time::Instant::now());
                        #[cfg(target_os = "windows")]
                        let still = hw.map(w32_visible).unwrap_or(false);
                        #[cfg(not(target_os = "windows"))]
                        let still = window.is_visible().unwrap_or(false);
                        nav_write(&format!(
                            "close: 藏入托盘 vis_before={vis_before} rect_alive={rect_ok} revived={} still_visible={}",
                            !rect_ok, still
                        ));
                    } else {
                        nav_write(&format!(
                            "close: 托盘图标取不到矩形（残影或折进溢出面板）→ 降级为最小化 vis_before={vis_before}"
                        ));
                        let _ = window.minimize();
                        // 最小化的复核判据是 IsIconic —— 最小化窗口 IsWindowVisible 仍为真。
                        #[cfg(target_os = "windows")]
                        if let Some(h) = hw {
                            if !w32_iconic(h) {
                                w32_minimize(h);
                                nav_write("close: tao minimize 空转 → Win32 SW_MINIMIZE 强制最小化");
                            }
                        }
                    }
                } else {
                    show_main(window.app_handle());
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            // 2026-10-16 老8 定「用完就 kill」的兜底：邦信退出时把按需拉起的 X8Assist
            // 一并收掉。必要性：前端的空闲回收心跳随窗口/进程一起消失，若用户会话结束后
            // 没点「结束」就直接退出客户端，X8Assist 会残留（长任务无重载）。
            match event {
                tauri::RunEvent::Exit => {
                    // sidecar 是子进程，主程序退出必须显式收掉，否则残留
                    p2p_sidecar::stop_all();
                    // 2026-11-10 老8 定：邦信关 → WX 消息插件必关
                    wx_sidecar::stop_all();
                    kill_rustdesk_procs();
                }
                // 2026-10-19 修「窗口最小化后无法恢复」：macOS 点 Dock 图标 / LaunchServices
                // 激活会向已运行实例发 reopen 事件，此前无人处理 → 最小化的窗口永远回不来，
                // 用户只剩杀进程重启一条路（本机 Dock 不可见，连重启都得敲命令）。
                // 复用托盘「打开邦信」同款 show_main（show + unminimize + set_focus，
                // 注意 show() 不会还原最小化，unminimize() 是关键一步）。
                // Reopen 在 Tauri 2 里仅 mac 编译，故 match arm 按平台 cfg，防 Windows/Linux 构建报错。
                #[cfg(target_os = "macos")]
                tauri::RunEvent::Reopen { .. } => show_main(&_app),
                _ => {}
            }
        });
}
