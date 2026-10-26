// 邦信 Tauri 壳：原生托盘 + 关闭藏托盘
// 2026-10-12：加 windows_subsystem（去 DOS 窗）、tray-icon（右下角图标/菜单/双击）
// 2026-10-14：加 target_os 限定 + macOS 跨平台分支，使同一份源码可在两平台构建
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use std::process::Command;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
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

/// 内置 RustDesk 路径（bundle.resources 捆绑，安装后随包分发）
/// - Windows: `bin/rustdesk.exe`（便携单文件）
/// - macOS:   `bin/mac/RustDesk.app/Contents/MacOS/rustdesk`（官方 dmg 内的 app bundle，保留签名）
/// - Linux:   不内置（官方 deb/rpm 的 rustdesk 强依赖一堆发行版专属 .so，抽单文件跑不起来），
///            回落系统 PATH 里的 `rustdesk`（未装则远程协助退化为手填 ID）
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
        let child = Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|e| format!("拉起外链失败: {e}"))?;
        // 2026-10-24 P2-RS-5：后台收尸，防 Unix 僵尸进程
        reap_in_background(child);
        Ok(())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let child = Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map_err(|e| format!("拉起外链失败: {e}"))?;
        // 2026-10-24 P2-RS-5：同上，后台收尸
        reap_in_background(child);
        Ok(())
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
        let _ = Command::new("taskkill")
            .args(["/F", "/IM", img])
            .creation_flags(0x0800_0000)
            .output();
    }
    // mac/linux：官方 RustDesk 进程名恒为 rustdesk（与 Windows taskkill rustdesk.exe 同语义；
    // 用户自装的官方 RustDesk 一并杀掉，与 Windows 行为一致）。-x 精确匹配进程名，不会误伤。
    #[cfg(not(windows))]
    {
        let _ = Command::new("pkill").args(["-x", "rustdesk"]).output();
    }
}

fn ensure_rustdesk_server_config() -> bool {
    // Windows：品牌客户端 X8Assist 的配置（实测读写处）
    #[cfg(windows)]
    let (dir, fname) = match x8assist_config_dir() {
        Some(d) => (d, "X8Assist2.toml"),
        None => return false,
    };
    // mac/linux：邦信内嵌/依赖的是官方 RustDesk（app-name = RustDesk）
    #[cfg(not(windows))]
    let (dir, fname) = match rustdesk_user_config_dir() {
        Some(d) => (d, "RustDesk2.toml"),
        None => return false,
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
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
        return false;
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
    std::fs::write(&path, content).is_ok()
}

fn rustdesk_get_id(app: &tauri::AppHandle) -> Result<String, String> {
    let exe = rustdesk_path(app).ok_or("内置 RustDesk 缺失")?;
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

/// mac：尽力清掉**资源包内那份** bundle 的 quarantine（无提权、失败静默）。
///
/// 为什么还要清：dmg 下载会把 `com.apple.quarantine` 带给包内所有文件，
/// Tauri 资源目录里那份 `bin/mac/RustDesk.app` 因此一直带隔离标记。
/// 主链路已改为不启动它（见 assist_exe），但 `--get-id` 等 CLI 仍会 exec 它；
/// 万一哪天有路径以 GUI 方式拉起，就会重演「RustDesk 已损坏」。
/// 清不掉（属主是 root）也无所谓 —— 直接 exec 不走 Gatekeeper。
#[cfg(target_os = "macos")]
fn macos_clear_bundle_quarantine(exe: &std::path::Path) {
    if let Some(bundle) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
        let _ = Command::new("/usr/bin/xattr")
            .args(["-dr", "com.apple.quarantine"])
            .arg(bundle)
            .output();
    }
}

/// mac：修复被打包管线弄坏的 RustDesk.app 框架符号链接时用的脚本（写入 /tmp 后由
/// 提权 osascript 调用）。内容对应 `甘/repair-rustdesk-bundle.sh` 已实测的逻辑。
#[cfg(target_os = "macos")]
const RD_REPAIR_SH: &str = r#"#!/bin/bash
# 修复被打包管线弄坏的 RustDesk.app 框架符号链接（2026-10-18 甘真机根因）
set -u
B="$1"
FW="$B/Contents/Frameworks"
for f in "$FW"/*.framework; do
  [ -d "$f/Versions/A" ] || continue
  if [ ! -L "$f/Versions/Current" ]; then
    [ -e "$f/Versions/Current" ] && rm -rf "$f/Versions/Current"
    ln -s A "$f/Versions/Current"
  fi
  for e in "$f/Versions/A"/*; do
    [ -e "$e" ] || continue
    n=$(basename "$e")
    [ "$n" = "Versions" ] && continue
    if [ -L "$f/$n" ]; then :
    elif [ -e "$f/$n" ]; then rm -rf "$f/$n"; ln -s "Versions/Current/$n" "$f/$n"
    else ln -s "Versions/Current/$n" "$f/$n"; fi
  done
done
/usr/bin/xattr -dr com.apple.quarantine "$B" 2>/dev/null
/usr/bin/codesign --force --deep --sign - "$B" 2>&1 | grep -v '^$' || true
"#;

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

/// mac：提权修复某 RustDesk.app 被打包管线弄坏的框架符号链接结构
/// （重建 `Versions/Current -> A` 与根级符号链接，清 quarantine，ad-hoc 重签）。
/// 仅当 `macos_bundle_symlinks_ok` 返回 false 时才调用 —— 健康包不会被打扰。
#[cfg(target_os = "macos")]
fn macos_repair_bundle_symlinks(bundle: &std::path::Path) -> Result<(), String> {
    let script_path = std::path::Path::new("/tmp/x8_repair_rd.sh");
    if !script_path.exists() {
        std::fs::write(script_path, RD_REPAIR_SH)
            .map_err(|e| format!("写入修复脚本失败: {e}"))?;
    }
    let b = bundle.to_string_lossy().to_string();
    let script = format!(
        "do shell script \"bash '{}' '{}'\" with administrator privileges",
        script_path.to_string_lossy(),
        b
    );
    let out = Command::new("osascript")
        .args(["-e", &script])
        .output()
        .map_err(|e| format!("提权修复框架符号链接失败: {e}"))?;
    if !out.status.success() {
        let se = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if se.contains("-128") || se.to_lowercase().contains("cancel") {
            return Err(
                "你取消了系统授权，远程协助组件修复未完成（重试并输入本机开机密码）".into(),
            );
        }
        return Err(format!("远程协助组件框架修复失败：{se}"));
    }
    if macos_bundle_symlinks_ok(bundle) {
        Ok(())
    } else {
        Err("远程协助组件框架符号链接修复后仍不完整，请重新安装或升级邦信客户端".into())
    }
}

/// mac：把「启动用的」和「`--password` 用的」统一成**同一个 bundle**（A 方案，2026-10-17 老8 定）。
///
/// 🩸 事故链（甘真机 2026-10-17）：`rustdesk_path()` 返回邦信资源包里的
/// `BangXin.app/Contents/Resources/bin/mac/RustDesk.app/...`，而 `--password` 硬编码跑
/// `/Applications/RustDesk.app/...` —— 用户机器上两处**各有一份**（启动台里两个图标），
/// 版本还可能不同；落位逻辑又是「已存在就跳过」，于是永远用 A 启动、拿 B 设密码。
/// 跨版本 IPC 的 `Data::Config` 结构会变（ACK 是 1.5 才加的）→ 服务端反序列化失败 →
/// 1s 无 ACK → 只报一句「未获得系统授权」，无从定位。
///
/// 现在（2026-10-21 方案A，用户实证「升级后没复用之前已启用的 rustdesk」）：
/// 已装的 `/Applications/RustDesk.app` **只要存在且结构完好就永远复用**，
/// 不再按版本字符串去覆盖它 —— 否则 macOS「屏幕录制」TCC 授权会因二进制哈希变化而失效，
/// 升级后被控端（要截自己屏）每次都得重设。缺失 / 结构损坏才提权把**内置这份**落位到
/// `/Applications/RustDesk.app`，旧的那份改名成 `RustDesk.app.x8bak-<时间戳>` 留作回滚点（不做 rm -rf）。
/// 复用已装副本还能顺带保证「启动实例」与「--password 实例」是同一 bundle（IPC 版本天然一致，
/// 正是 2026-10-17 事故要达成的目标），比原版本号比对更稳。
/// 返回 (canonical exe, 是否发生了替换)。
#[cfg(target_os = "macos")]
fn macos_canonical_exe(raw: &std::path::Path) -> Result<(std::path::PathBuf, bool), String> {
    // raw = .../RustDesk.app/Contents/MacOS/rustdesk → 上三级即 .app bundle
    let src = raw
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("app"))
        .ok_or("内置 RustDesk 路径异常（不是 .app bundle）")?;
    // 2026-10-21 方案A：不再用版本号比对决定复用（见下方逻辑）。
    // 内置这份 RustDesk.app 的框架符号链接可能已被打包管线解引用弄坏（🩸2026-10-18 甘真机），
    // 但**只在真正要 ditto 落位时才修它**（见下方替换分支）—— 健康已装副本被复用时绝不碰
    // 内置这份的签名，避免任何不必要的 ad-hoc 重签。
    let _ = macos_bundle_version(src);
    let inst_dir = std::path::Path::new("/Applications/RustDesk.app");
    // 已经就是 /Applications 那份（调用方传错/未来重构）→ 直接返回，绝不 ditto 自己到自己
    if src == inst_dir {
        return macos_installed_exe().map(|p| (p, false)).ok_or_else(|| {
            "远程协助组件异常（/Applications/RustDesk.app 内无可执行文件）".to_string()
        });
    }
    let inst_exe = macos_installed_exe().unwrap_or_else(|| inst_dir.join("Contents/MacOS/rustdesk"));
    if inst_exe.is_file() {
        // 2026-10-21 方案A：已装副本**健康即永远复用**，绝不碰它的签名/内容 → TCC 授权跨版本保留。
        if macos_bundle_symlinks_ok(inst_dir) {
            return Ok((inst_exe, false));
        }
        // 已装副本结构损坏（框架符号链接断）才尝试修复；修复会 ad-hoc 重签（必然丢 TCC），
        // 但损坏副本本就无法启动，修好比保留授权更优先。修复后仍完好则复用，否则落到下方替换。
        if macos_repair_bundle_symlinks(inst_dir).is_ok() && macos_bundle_symlinks_ok(inst_dir) {
            return Ok((inst_exe, false));
        }
        // 修复失败 → 落到下方整包替换（TCC 必然失效，但已是「无可用副本」时的唯一出路）
    }
    // 进入替换分支：先确保内置这份 RustDesk.app 结构完好（框架符号链接断会导致启动即崩），
    // 坏了就提权自愈（ad-hoc 重签，必然丢 TCC，但这是「无可用副本」时的唯一出路，可接受）。
    if !macos_bundle_symlinks_ok(src) {
        let _ = macos_repair_bundle_symlinks(src);
    }
    // 先 ditto 到隐藏的临时目录，校验可执行后再换位 —— 换位失败时用户的旧 app 仍可用
    // 顺手清掉**源**（资源包那份）的隔离标记：否则以后任何以 GUI 方式拉起它的路径
    // 都会重演「RustDesk 已损坏」（直接 exec 不受影响，故失败即忽略）。
    macos_clear_bundle_quarantine(raw);
    let mk = format!(
        "do shell script \"/bin/rm -rf /Applications/.X8RD.incoming ; \
         /usr/bin/ditto '{}' /Applications/.X8RD.incoming ; \
         [ -x /Applications/.X8RD.incoming/Contents/MacOS/rustdesk ] || exit 3 ; \
         BAK=/Applications/RustDesk.app.x8bak-`/bin/date +%s` ; \
         /bin/mv /Applications/RustDesk.app $BAK 2>/dev/null ; \
         /bin/mv /Applications/.X8RD.incoming /Applications/RustDesk.app ; \
         /usr/bin/xattr -dr com.apple.quarantine /Applications/RustDesk.app || true\" \
         with administrator privileges",
        src.to_string_lossy()
    );
    let out = Command::new("osascript")
        .args(["-e", &mk])
        .output()
        .map_err(|e| format!("提权执行失败: {e}"))?;
    if !out.status.success() {
        let se = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if se.contains("-128") || se.to_lowercase().contains("cancel") {
            return Err("你取消了系统授权，远程协助组件未安装（重试并输入本机开机密码）".into());
        }
        return Err(format!("远程协助组件落位失败（/Applications 写入被拒或包不完整）：{se}"));
    }
    // 落位后重新按 CFBundleExecutable 取（名字可能是 RustDesk 而非 rustdesk）
    let landed = macos_installed_exe().ok_or_else(|| {
        "远程协助组件落位失败（/Applications/RustDesk.app 内无可执行文件）".to_string()
    })?;
    Ok((landed, true))
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
#[cfg(not(windows))]
fn unix_set_permanent_password(exe: &std::path::Path, pw: &str) -> Result<(), String> {
    // 兜底校验：pw 会被拼进 osascript/pkexec 的 shell 字符串（无引号包裹转义），
    // 只放行前端生成的 [A-Za-z0-9] 随机密码（与服务端 setid 校验同口径），
    // 任何可疑字符直接判失败，杜绝经命令注入把任意命令以 root 跑起来。
    if pw.len() < 8 || !pw.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("密码格式不合法".into());
    }
    #[cfg(target_os = "macos")]
    {
        // A 方案（2026-10-17 老8 定）：落位已前置到 macos_canonical_exe()，
        // 这里拿到的 exe 必须已经在 /Applications/RustDesk.app 内 —— 与启动实例是同一份 bundle、
        // 同一版本，否则跨版本 IPC 拿不到 ACK（见 macos_canonical_exe 的注释）。
        let p = exe.to_string_lossy().to_string();
        if !p.starts_with("/Applications/RustDesk.app/") {
            return Err(format!(
                "远程协助组件未就位（{p}），请重试以完成安装到 /Applications"
            ));
        }
        let script = format!(
            "do shell script \"'{}' --password {}\" with administrator privileges",
            p, pw
        );
        let out = Command::new("osascript")
            .args(["-e", &script])
            .output()
            .map_err(|e| format!("提权执行失败: {e}"))?;
        let so = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let se = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !out.status.success() {
            if se.contains("-128") || se.to_lowercase().contains("cancel") {
                return Err("你取消了系统授权，远程协助密码未写入（重试并在弹框输入本机开机密码）".into());
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
            // is_installed() 不过：路径对了但 APP_NAME 不符，或落到别的副本上
            return Err(format!(
                "RustDesk 未通过安装位置校验（{p}），远程协助组件必须是 /Applications/RustDesk.app"
            ));
        }
        if so.is_empty() {
            return Err(
                "远程协助密码写入无响应（RustDesk 未在运行或版本不匹配），请重试".into()
            );
        }
        return Err(format!("远程协助密码写入失败：{so}"));
    }
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
    let exe = rustdesk_path(&app).ok_or("内置 RustDesk 缺失")?;
    // 先幂等写自建服务器配置（可能结束在跑的 RustDesk），再取 ID（新起进程即读新配置）
    let _ = ensure_rustdesk_server_config();
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
        unix_set_permanent_password(&exe, pw)?;
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
        #[cfg(not(target_os = "linux"))]
        {
            Ok("already".to_string())
        }
    })
    .await
    .map_err(|e| format!("远程协助组件安装线程异常：{e}"))?
}

/// Linux：定位系统安装的 rustdesk 可执行文件（rustdesk_path / 安装后校验共用同一判据）。
/// 实时扫目录、不是启动时缓存，所以刚装完立刻调用也能发现。
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
            .filter(|p| std::path::Path::new(*p).exists())
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
#[tauri::command]
async fn linux_self_update(app: tauri::AppHandle) -> Result<String, String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = app;
        Err("应用内升级仅在 Linux 版支持".to_string())
    }
    #[cfg(target_os = "linux")]
    {
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
    #[cfg(target_os = "macos")]
    if let Some(p) = macos_installed_exe() {
        return Ok(p);
    }
    rustdesk_path(app).ok_or_else(|| "内置 RustDesk 缺失".to_string())
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
        let _ = ensure_rustdesk_server_config();
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
        let _ = ensure_rustdesk_server_config();
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
///   mac：osascript 让 System Events 把 rustdesk 进程的所有窗口设为不可见。需邦信在
///        「系统设置-隐私与安全性-辅助功能」获授权；未授权时静默失败（窗口照常显示，
///        远程功能本身不受影响）。
///   linux：wmctrl 可用时把 rustdesk 类窗口置 hidden（发行版未装 wmctrl 则跳过）。
/// 被控端待控期间可反复调用（见 hide_assist_windows_while_running）——官方客户端无
/// hide_cm，对端连入时连接管理器窗口会晚于启动期出现，只有持续隐藏才盖得住。
#[cfg(not(windows))]
fn hide_assist_windows() {
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("osascript")
            .args([
                "-e",
                "tell application \"System Events\" to set visible of process \"rustdesk\" to false",
            ])
            .output();
    }
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
fn open_in_browser(url: String) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("无效地址".into());
    }
    open_external(&url)
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

/// 文件名消毒（2026-10-23 S3）。只收敛「本机不合法 / 会出事」的部分，其余尽量保留原貌
/// （用户要靠文件名认文件，不做拼音化、不改扩展名大小写）。
///   · 路径分隔与 Windows 非法字符 → 去掉（否则可跑出下载目录，或建出子路径）
///   · cmd 元字符（& % ^）→ 去掉：Windows 侧「打开」走 `cmd /C start "" <路径>`，
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

/// P2P 直传收到的文件落盘（2026-10-23）。
///
/// 为什么需要它：WebView 里 `<a download>` 不生效，网页端能靠浏览器保存，桌面端只能走命令。
/// 为什么用 base64 而不是 Vec<u8>：Tauri 把 Vec<u8> 当普通数组逐元素序列化，几十 MB 会卡死主线程；
/// 字符串一次过桥虽然多占 1/3 体积，但可控且不会卡死。
/// 落盘走「.part → rename」原子写，与 download_file 同款，目录里不会出现半截文件。
#[tauri::command]
async fn save_bytes(name: String, b64: String) -> Result<String, String> {
    // 2026-10-24 P2-RS-1：base64 解码 + 写盘都是阻塞 CPU/IO，跑在 async 体里会占住 tokio worker
    //（几十 MB 解码可达数百 ms）——整段移入 spawn_blocking，与 download_file 一致。
    let result = tauri::async_runtime::spawn_blocking(move || {
    let bytes = b64_decode(&b64)?;
    if bytes.is_empty() {
        return Err("文件内容为空".into());
    }
    let downloads = downloads_dir();
    std::fs::create_dir_all(&downloads).map_err(|e| format!("无法创建下载目录：{}", e))?;
    let safe = sanitize_filename(&name);
    let dest = downloads.join(&safe);
    let tmp = downloads.join(format!("{}.{}.part", safe, std::process::id()));
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
async fn download_file(url: String, token: String, name: String, open: Option<bool>) -> Result<String, String> {
    // 2026-10-24 稳定性修复（AppHangB1）：原为同步命令，reqwest blocking 下载 + 写盘全跑在主线程，
    // 大文件/慢网时整窗冻结（表现为「没反应、关不掉、移不动」）。改 async + spawn_blocking：
    // 阻塞 IO 移出主线程，await 期间 UI 正常响应；.part → rename 原子落盘逻辑不变。
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
    let copied = {
        let mut body = resp;
        std::io::copy(&mut body, &mut f)
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
#[tauri::command]
fn reveal_file(path: String) -> Result<(), String> {
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
        Shell_NotifyIconW, NIF_INFO, NIIF_INFO, NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW,
        NOTIFYICON_VERSION_4,
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
        // 关键修复（2026-10-22 排查「托盘闪烁正常、气泡永不弹出」）：
        // tray-icon 0.25.1 注册图标时从不调用 NIM_SETVERSION，通知图标停留在默认版本（v3）。
        // Windows 要求图标处于 NOTIFYICON_VERSION_4 才会渲染 NIF_INFO 气球——否则
        // Shell_NotifyIconW(NIM_MODIFY, NIF_INFO) 返回 SUCCESS 却静默丢弃，无报错、图标照常。
        // 每次发气泡前把版本钉到 4（幂等）：既修当前问题，也顺带覆盖「资源管理器重启→图标被
        // 重建→版本被打回默认」后气泡再次失效的场景（无需监听 TaskbarCreated）。
        let mut v: NOTIFYICONDATAW = std::mem::zeroed();
        v.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        v.hWnd = hwnd;
        v.uID = uid;
        v.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &v);

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

/// 闪烁半周期（毫秒）。500ms = 每秒切两次，QQ/微信同级速率：明显但不躁。
const TRAY_FLASH_INTERVAL_MS: u64 = 500;

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
        loop {
            use std::sync::atomic::Ordering;
            if TRAY_FLASH_GEN.load(Ordering::SeqCst) != my_gen {
                // ③ 被新消息接替：不碰图标，新一代全权负责
                return;
            }
            let n = TRAY_FLASH_UNREAD.load(Ordering::SeqCst);
            if n <= 0 {
                // ① 已清完：自己还原默认 logo（与调用方的还原幂等，踩中竞态也只是同写一张图）
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

/// 浮窗尺寸（逻辑像素）：正好装下 84px 圆形头像 + 8px 间距 + 21px 文案胶囊。
const FDE_FLOAT_W: f64 = 132.0;
const FDE_FLOAT_H: f64 = 148.0;
/// 自动收起延时：页面内 8s 开始淡出，这里再多留 1.2s 兜住淡出动画。
const FDE_FLOAT_AUTO_HIDE_MS: u64 = 9_200;
/// 浮窗世代号：每次显示 +1，上一轮的自动隐藏计时发现自己过期就放弃（同 TRAY_FLASH_GEN 套路）。
static FDE_FLOAT_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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

/// 浮窗落点：主屏工作区右下角，距右 16、距底 8 逻辑像素。
fn fde_float_pos(app: &tauri::AppHandle) -> (f64, f64) {
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
            return (w - FDE_FLOAT_W - 16.0, h - FDE_FLOAT_H - 8.0);
        }
    }
    // 非 Windows / 取不到工作区：退回显示器整块，额外让出 56px 当任务栏
    if let Ok(Some(m)) = app.primary_monitor() {
        let sf = m.scale_factor();
        let s = m.size();
        let p = m.position();
        return (
            (p.x as f64 + s.width as f64) / sf - FDE_FLOAT_W - 16.0,
            (p.y as f64 + s.height as f64) / sf - FDE_FLOAT_H - 8.0 - 56.0,
        );
    }
    (0.0, 0.0)
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
fn fde_float_close(app: &tauri::AppHandle) {
    use std::sync::atomic::Ordering;
    FDE_FLOAT_GEN.fetch_add(1, Ordering::SeqCst);
    if let Some(w) = app.get_webview_window("fde-float") {
        let _ = w.hide();
    }
}

/// 显示/刷新浮窗。count=0 时只显示头像不显示角标（消息落在已打开会话里，未读数没涨）。
fn fde_float_show_impl(app: &tauri::AppHandle, count: i64, tip: &str) {
    use std::sync::atomic::Ordering;
    let n = count.max(0);
    let (x, y) = fde_float_pos(app);

    if let Some(w) = app.get_webview_window("fde-float") {
        // 已存在：原地更新数字/文案 + 重播入场动画。不整窗 reload——reload 会白闪一下
        let js = format!(
            "window.__fdeUpdate && window.__fdeUpdate({}, {})",
            n,
            js_str(tip)
        );
        let _ = w.eval(&js);
        let _ = w.set_position(tauri::LogicalPosition::new(x, y));
        let _ = w.show();
    } else {
        let url = format!(
            "https://im.x8fde.com/app/fde-float.html?n={}&t={}",
            n,
            url_encode(tip)
        );
        let parsed = match url.parse() {
            Ok(u) => u,
            Err(_e) => {
                return;
            }
        };
        match tauri::WebviewWindowBuilder::new(app, "fde-float", tauri::WebviewUrl::External(parsed))
            .title("邦信")
            .inner_size(FDE_FLOAT_W, FDE_FLOAT_H)
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
            Ok(w) => {
                let _ = w.show();
            }
            Err(_) => {}
        }
    }

    // 到点自动收起（被再次显示 / 被已读 / 被点击 → 世代号变了，本轮自行放弃）
    let my_gen = FDE_FLOAT_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let app2 = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(FDE_FLOAT_AUTO_HIDE_MS));
        use std::sync::atomic::Ordering;
        if FDE_FLOAT_GEN.load(Ordering::SeqCst) == my_gen {
            if let Some(w) = app2.get_webview_window("fde-float") {
                let _ = w.hide();
            }
        }
    });
}

/// 前端接口：显示右下角 FDE 浮动提醒（由 notify_unread / away_notice 内部调用，
/// 也允许页面直接触发，便于以后扩展「任务提醒」等场景）。
#[tauri::command]
fn fde_float_show(app: tauri::AppHandle, count: Option<i64>, tip: Option<String>) -> Result<(), String> {
    let t = tip.unwrap_or_default();
    fde_float_show_impl(&app, count.unwrap_or(0), &t);
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
    fde_float_close(&app);
    show_main(&app);
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
) -> Result<(), String> {
    let n = count.max(0);
    let def_icon = Some(tray_icon_image());
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
            // 右下角 FDE 浮窗也一并收起（人已经处理完了，浮窗再挂着就是噪音）
            stop_tray_flash();
            fde_float_close(&app);
            if let Some(ic) = def_icon {
                let _ = tray.set_icon(Some(ic));
            }
        }
    }
    let want_flash = flash.unwrap_or(n > 0);
    if want_flash {
        if let Some(w) = app.get_webview_window("main") {
            // 真实 OS 窗口状态（Rust 侧权威）：失焦 / 最小化 / 隐藏 任一都算「人没在看」。
            // ⚠️ 前端 document.hasFocus()/visibilityState 反映的是 WebView 内容焦点，
            //    测不到 Tauri 窗口被最小化，绝不能信——离开判据只能以 Rust 的窗口状态为准
            //    （2026-10-22 实测：最小化时前端 hasFocus 仍 true，导致打开着的会话不闪）。
            let away = !w.is_focused().unwrap_or(true)
                || !w.is_visible().unwrap_or(true)
                || w.is_minimized().unwrap_or(false);
            if away {
                if n > 0 {
                    // 2026-10-22：托盘图标「一暗一亮」闪烁，闪到窗口回前台 / 已读清零为止。
                    start_tray_flash(app.clone(), n);
                    // 2026-10-22：屏幕右下角卡通 FDE 浮窗（微信式），窗在托盘上方浮现，
                    // 8s 后自动淡出，点一下直接唤起主窗。
                    let ftip = title
                        .clone()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| format!("{n} 条新消息"));
                    fde_float_show_impl(&app, n, &ftip);
                }
                // 新消息：弹气泡 + 闪任务栏/托盘，但**绝不** bring_to_front 抢前台——
                // 消息到达就抢焦点会让任务栏闪烁（需失焦才闪）失去意义，也违背微信式体验。
                if n > 0 {
                    #[cfg(target_os = "windows")]
                    {
                        let t = title
                            .clone()
                            .filter(|s| !s.trim().is_empty())
                            .unwrap_or_else(|| "邦信新消息".to_string());
                        let b = body
                            .clone()
                            .filter(|s| !s.trim().is_empty())
                            .unwrap_or_else(|| format!("{n} 条未读消息"));
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
                let _ = w.request_user_attention(Some(UserAttentionType::Critical));
                #[cfg(target_os = "windows")]
                flash_taskbar(&w);
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
    fde_float_show_impl(&app, 0, &ftip);
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
            Command::new("pgrep")
                .args(["-f", "WeChat.app/Contents/MacOS/WeChat"])
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

/// 解析本机升级体系：新品牌（BangXin）优先；若机器上只有品牌迁移前的旧任务，
/// 则整套回落到旧体系（旧任务 → 旧 .cmd → 旧目录 + 旧文件名）。
/// 三种必须配套，只换一半就是「点了没反应」的静默失败。
#[cfg(windows)]
fn task_ctx() -> Result<TaskCtx, String> {
    // 🔴 2026-10-25：改用 %ProgramData%（而非 LOCALAPPDATA），与 updater-task.cmd 的
    //   %ProgramData% 展开结果一致，杜绝 Highest 权限计划任务上下文里路径错位。
    let pd = std::env::var("ProgramData")
        .map_err(|_| "ProgramData 环境变量缺失，无法定位更新目录".to_string())?;
    let base = std::path::Path::new(&pd);
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

/// 触发「应用补丁」：优先 `schtasks /Run`（普通权限、零 UAC）；
/// 失败则**降级为提权直跑** `updater-task.cmd`（弹一次 UAC，用户点「是」即可）。
///
/// 🔴 2026-10-26 老8 定（原 P1 建议落地）：此前「预检计划任务不存在 → 整条升级链路直接
/// 放弃」，禁用 schtasks / 任务未注册的机器（企业管控机、品牌迁移前的老客户端）
/// 100% 升不了级，表现＝发版了但客户端永远停在老版本。
/// 现在预检失败**只降级、不放弃**：包照样下载、验签、落盘；最后一公里改用提权直跑，
/// 最差情况也只是弹一次 UAC，而不是白下几十 MB 后报「请手动更新」。
#[cfg(windows)]
fn apply_patch_task(ctx: &TaskCtx) -> Result<(), String> {
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
    let elevate = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.join("elevate.exe")))
        .unwrap_or_else(|| std::path::PathBuf::from("elevate.exe"));
    let (c2, o2) = if elevate.exists() {
        win_run_capture(
            &elevate.to_string_lossy(),
            &["cmd.exe", "/c", &script.display().to_string()],
        )
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


fn show_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
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

#[cfg(target_os = "windows")]
fn enforce_single_instance() {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

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
        // 已有实例：先把它唤到前台，唤得到才退出本进程。
        // 唤不到主窗 = 对方是僵尸（无界面）→ 放行，否则用户双击图标永远「没反应」。
        let woken = wake_existing_instance_window();
        si_log(&format!("already-exists woken={woken}"));
        if woken {
            si_log("exit(0): 已有主窗，已聚焦");
            std::process::exit(0);
        }
        si_log("no main window -> continue");
    }
    // 首个实例（或对方是僵尸）：句柄故意不关闭，进程存活期间由系统持锁。
    // （HANDLE 是 Copy 类型，无需 forget；只要不调 CloseHandle 就不会提前释放锁。）
    let _ = h;
}

fn main() {
    // 2026-10-23 老8：先上硬单实例锁，再初始化任何 GUI/WebView（顺序不能反，
    // 反了会先闪一下登录窗再退出，观感像崩溃）。
    #[cfg(target_os = "windows")]
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
            // 2026-10-17：Linux 缺远程协助组件时一键安装（Windows/mac 恒返回 already）
            install_rustdesk,
            // 2026-10-26 Linux 应用内升级（下载本架构 deb/rpm → pkexec 安装 → 自动重启）
            linux_self_update,
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
            win_patch_update
        ])
        .setup(|app| {
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
                let ver = app.package_info().version.to_string();
                let base = std::env::var("LOCALAPPDATA").unwrap_or_default();
                let stamp = std::path::Path::new(&base).join("BangXin").join("webcache_ver.txt");
                let prev = std::fs::read_to_string(&stamp).unwrap_or_default();
                if prev.trim() != ver {
                    if let Ok(local_data) = app.path().local_data_dir() {
                        let _ = std::fs::remove_dir_all(&local_data);
                    }
                    if let Ok(roaming_data) = app.path().app_data_dir() {
                        let _ = std::fs::remove_dir_all(&roaming_data);
                    }
                    if let Some(dir) = stamp.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::write(&stamp, &ver);
                }
            }

            let open = MenuItem::with_id(app, "open", "打开邦信", true, None::<&str>)?;
            // 2026-10-23 托盘菜单完善（A）：加「检查更新」+ 版本只读标签 + 分隔线，与微信式托盘一致
            let check_updates = MenuItem::with_id(app, "check_updates", "检查更新", true, None::<&str>)?;
            let ver_label = MenuItem::with_id(
                app, "ver", &format!("版本 v{}", app.package_info().version), false, None::<&str>,
            )?;
            let sep = PredefinedMenuItem::separator(app)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &check_updates, &ver_label, &sep, &quit])?;
            TrayIconBuilder::with_id("x8im-tray")
                .icon(tray_icon_image())
                .tooltip("邦信即时通讯")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show_main(app),
                    // 检查更新：发事件给前端，复用 checkUpdBoth 的进度条/静默升级 UI（manual=true）
                    "check_updates" => { let _ = app.emit("x8-tray-check-updates", ()); }
                    // 2026-10-23 老8 实测「托盘退出关不掉」：app.exit(0) 直接在菜单回调里
                    // 同步调用 —— 退出时要销毁托盘（含正在响应的这个菜单对象），在 muda 的
                    // 菜单消息处理中重入，退出请求被吞掉，进程与托盘图标都留着。
                    // 改到独立线程：先让菜单回调返回、菜单关闭，再走正常退出；
                    // 若 2 秒后仍未退（Exit 分支的 taskkill 被拖住 / 事件循环卡住），强制结束兜底。
                    "quit" => {
                        let h = app.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(120));
                            kill_rustdesk_procs();
                            h.exit(0);
                            std::thread::sleep(std::time::Duration::from_millis(2000));
                            std::process::exit(0);
                        });
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::DoubleClick { .. } = event {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;
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
            Ok(())
        })
        .on_window_event(|window, event| {
            // 点 ✕ 的语义分两种（2026-10-14 老8 定）：
            //   main      → 不退出，藏到托盘（IM 常规行为）
            //   workbench → 真关闭，并把主窗邦信唤到前台（关闭 = 返回 IM）
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    let _ = window.hide();
                    api.prevent_close();
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
                tauri::RunEvent::Exit => kill_rustdesk_procs(),
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
