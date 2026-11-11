// 邦信 · 通用插件宿主（plugin_host）
// ─────────────────────────────────────────────────────────────
// 2026-11-10 老8 定：插件机制必须与语言无关。
// 宿主只做四件事：找到它 → 拉起它 → 验契约 → 收掉它（含看门狗）。
// 具体插件（Go / Rust / Python / Node 都行）落在 <安装目录>/plugins/<id>/，
// 自带 plugin.json 描述「叫什么、跑什么、怎么验活」，宿主不认任何具体插件。
//
// 目录约定：
//   <安装目录>/plugins/<id>/plugin.json       描述文件
//   <安装目录>/plugins/<id>/<entry>[.exe]     可执行文件
//   <安装目录>/plugins/<id>/<entry>.new.exe   升级包，下次启动前替换（见 apply_pending）
//
// 硬口径（与 wxadapter 一致，任何插件都必须遵守）：
//   1) 插件没有托盘、没有独立界面，一律由邦信触发、由邦信回收；
//   2) 邦信关 → 插件必关（main.rs 的 RunEvent::Exit → stop_all）；
//   3) 端口只绑 127.0.0.1；token 每次启动用 OS CSPRNG 生成，**只经 stdin 首帧
//      传递**（2026-11-10 P0-2：命令行可被 WMI/procfs 任意读取），不落盘；
//   4) 慢作业（一次几秒~十几秒）+ 随时可查状态 → 走 HTTP，不走 stdio 帧。
//
// 占位符（plugin.json 的 args 里可用）：
//   {data}  应用数据目录（app_data_dir）
//   {db}    共用本地库（app_data_dir/mail.db）
//   {port}  分配给该插件的端口
//   {root}  插件自身目录
//
// ⚠️ 2026-11-10 P0-2 起**没有 {token} 占位符**：令牌只经 piped stdin 的首帧
//    "X8TOKEN <hex>\n" 传给 parent_watch 插件，绝不再拼进 argv。
//   {id}    插件 id

use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::AppHandle;

/// 看门狗巡检间隔（秒）。插件被风控杀掉 / 自己 panic，最多 30s 内自愈。
const WATCHDOG_SECS: u64 = 30;
/// 重启失败的退避上限（秒）。2026-11-10 P2-4：不再「3 次失败后永久放弃」，
/// 改为指数退避后继续重试，界面状态可以自己恢复。
const MAX_BACKOFF_SECS: u64 = 300;
/// 拉起后等端口就绪的上限。探测间隔从 200ms 缩到 100ms（P2-3），总时长不变。
const BOOT_TIMEOUT_MS: u64 = 8000;
const PROBE_INTERVAL_MS: u64 = 100;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 复用的 blocking HTTP 客户端（P2-3）。此前每个请求都重建 Client，
/// 既不复用连接池，也要反复读代理配置；超时改成按请求设置。
fn http_client() -> &'static reqwest::blocking::Client {
    static C: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .no_proxy() // 本机回环，绝不能被系统/环境代理接管
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new())
    })
}

// ─────────────────────────── 描述文件 ───────────────────────────

#[derive(Clone, Debug)]
pub struct Spec {
    pub id: String,
    pub name: String,
    pub version: String,
    /// 可执行文件名（不含扩展名）
    pub entry: String,
    /// 启动参数模板
    pub args: Vec<String>,
    pub port: u16,
    /// 验活路径（插件必须返回 200，并带 contract_key）
    pub health: String,
    pub contract_key: String,
    pub contract_min: u64,
    pub capabilities: Vec<String>,
    /// 父进程监护：为真则用 piped stdin 拉起插件，宿主进程一死 stdin 即 EOF，
    /// 插件据此自退。邦信被强杀 / 崩溃时靠这个收尸（SIGTERM 只在正常退出时才有）。
    pub parent_watch: bool,
}

impl Spec {
    fn from_json(v: &Value) -> Option<Spec> {
        let id = v.get("id")?.as_str()?.trim().to_string();
        // id 会拼进路径，禁止分隔符与点点
        if id.is_empty()
            || id.len() > 32
            || id.contains(['/', '\\'])
            || id.contains('.')
            || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return None;
        }
        let entry = v
            .get("entry")
            .and_then(|x| x.as_str())
            .unwrap_or(&id)
            .trim()
            .to_string();
        if entry.is_empty() || entry.contains(['/', '\\']) {
            return None;
        }
        Some(Spec {
            id,
            name: v.get("name").and_then(|x| x.as_str()).unwrap_or("插件").to_string(),
            version: v
                .get("version")
                .and_then(|x| x.as_str())
                .unwrap_or("0.0.0")
                .to_string(),
            entry,
            args: v
                .get("args")
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            port: v.get("port").and_then(|x| x.as_u64()).unwrap_or(0) as u16,
            health: v
                .get("health")
                .and_then(|x| x.as_str())
                .unwrap_or("/api/version")
                .to_string(),
            contract_key: v
                .get("contract_key")
                .and_then(|x| x.as_str())
                .unwrap_or("contract_version")
                .to_string(),
            contract_min: v.get("contract_min").and_then(|x| x.as_u64()).unwrap_or(1),
            capabilities: v
                .get("capabilities")
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            parent_watch: v.get("parent_watch").and_then(|x| x.as_bool()).unwrap_or(false),
        })
    }

    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "version": self.version,
            "entry": self.entry,
            "port": self.port,
            "capabilities": self.capabilities,
        })
    }
}

/// 内置兜底描述：插件目录里没有 plugin.json 时用（开发期 / 老安装包）。
/// 新增插件不需要改这里 —— 丢一个 plugins/<id>/plugin.json 即可。
fn builtin(id: &str) -> Option<Spec> {
    match id {
        "wx" => Some(Spec {
            id: "wx".into(),
            name: "WX 消息".into(),
            version: "1.0.0".into(),
            entry: "wxadapter".into(),
            args: vec![
                "--data-dir={data}".into(),
                "--archive={db}".into(),
                "--port={port}".into(),
                // 2026-11-10 P0-2：令牌改由 stdin 首帧注入，绝不进 argv。
                "--no-kill".into(),
                "--parent-watch".into(),
            ],
            port: 60888,
            health: "/api/version".into(),
            contract_key: "contract_version".into(),
            contract_min: 1,
            capabilities: vec!["wx.messages".into(), "wx.follow".into()],
            parent_watch: true,
        }),
        _ => None,
    }
}

// ─────────────────────────── 目录解析 ───────────────────────────

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(|p| p.to_path_buf())
}

fn plugins_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("X8_PLUGIN_DIR") {
        let pb = PathBuf::from(p);
        if pb.is_dir() {
            return Some(pb);
        }
    }
    Some(exe_dir()?.join("plugins"))
}

fn plugin_dir(id: &str) -> Option<PathBuf> {
    Some(plugins_root()?.join(id))
}

/// 应用数据目录（由 wx_updater / wx_cache 在拿到 AppHandle 后设置一次）。
/// 🔴 插件独立发版的落盘位置：安装目录在 Windows(Program Files) / macOS(.app) /
///    Linux(/usr/lib) 都可能是只读或改了会破坏签名，所以下载的插件一律落这里。
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn set_data_dir(p: PathBuf) {
    let _ = DATA_DIR.set(p);
}

/// 插件在应用数据目录里的落点；首次安装前可能还不存在。
pub fn data_plugin_dir(id: &str) -> Option<PathBuf> {
    Some(DATA_DIR.get()?.join("plugins").join(id))
}

/// 读 <root>/plugin.json；读不到返回 None。
fn read_manifest(root: &PathBuf) -> Option<Spec> {
    let p = root.join("plugin.json");
    let raw = std::fs::read_to_string(&p).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    Spec::from_json(&v)
}

// ─────────────────────────── 运行时句柄 ───────────────────────────

pub struct Plugin {
    pub spec: Spec,
    /// 插件目录（plugin.json 所在处；开发期可能不存在）
    root: PathBuf,
    child: Mutex<Option<Child>>,
    token: Mutex<String>,
    /// 本次拉起的插件 PID（P1-2：握手比对 /api/version 的 pid，防端口串实例）
    pid: AtomicU32,
    /// 连续启动失败次数（用于指数退避）
    fails: AtomicU32,
    /// 下次允许重试的 Unix 秒（P2-4）
    next_retry: AtomicU64,
    /// 曾经成功拉起过 → 看门狗才管它（用户没点开过的插件不去打扰）
    ever_started: AtomicBool,
    last_err: Mutex<String>,
}

impl Plugin {
    fn new(spec: Spec, root: PathBuf) -> Plugin {
        Plugin {
            spec,
            root,
            child: Mutex::new(None),
            token: Mutex::new(String::new()),
            pid: AtomicU32::new(0),
            fails: AtomicU32::new(0),
            next_retry: AtomicU64::new(0),
            ever_started: AtomicBool::new(false),
            last_err: Mutex::new(String::new()),
        }
    }

    /// 可执行文件位置。顺序：env 覆盖 → 插件目录 → bin/ → 主程序同目录 → 开发期 bin/。
    fn resolve(&self) -> Option<PathBuf> {
        let id_up = self.spec.id.to_uppercase();
        for k in [format!("X8_PLUGIN_{id_up}_BIN"), "X8_PLUGIN_BIN".to_string()] {
            if let Ok(p) = std::env::var(&k) {
                let pb = PathBuf::from(p);
                if pb.is_file() {
                    return Some(pb);
                }
            }
        }
        // 兼容老变量
        if self.spec.id == "wx" {
            if let Ok(p) = std::env::var("X8_WX_BIN") {
                let pb = PathBuf::from(p);
                if pb.is_file() {
                    return Some(pb);
                }
            }
        }

        let e = &self.spec.entry;
        let mut cands: Vec<PathBuf> = Vec::with_capacity(8);
        // 2026-11-10：自动更新下载的插件优先于随包插件（新版本覆盖 baseline）。
        if let Some(d) = DATA_DIR.get() {
            let dp = d.join("plugins").join(&self.spec.id);
            cands.push(dp.join(format!("{e}.exe")));
            cands.push(dp.join(e));
        }
        cands.push(self.root.join(format!("{e}.exe")));
        cands.push(self.root.join(e));
        if let Some(dir) = exe_dir() {
            // 打包后 resources 会落到 <安装目录>/plugins/<id>/；
            // 老安装包（resources 写的是 bin/wxadapter.exe）落在 bin/；
            // 开发期 target/debug/x8im.exe → target/bin/wxadapter.exe
            cands.push(dir.join("bin").join(format!("{e}.exe")));
            cands.push(dir.join("bin").join(e));
            cands.push(dir.join(format!("{e}.exe")));
            cands.push(dir.join(e));
            if let Some(p) = dir.parent() {
                cands.push(p.join("bin").join(format!("{e}.exe")));
                cands.push(p.join("bin").join(e));
            }
        }
        cands.into_iter().find(|x| x.is_file())
    }

    pub fn available(&self) -> bool {
        self.resolve().is_some()
    }

    pub fn running(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            // mac 上插件以 root 经提权通道拉起，本进程不持子进程句柄 → 用端口探活
            if let Ok(addr) = format!("127.0.0.1:{}", self.spec.port).parse::<SocketAddr>() {
                return TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok();
            }
            return false;
        }
        #[cfg(not(target_os = "macos"))]
        {
            let mut g = self.child.lock().unwrap();
            match g.as_mut() {
                Some(c) => matches!(c.try_wait(), Ok(None)),
                None => false,
            }
        }
    }

    pub fn last_error(&self) -> String {
        self.last_err.lock().unwrap().clone()
    }

    fn set_err(&self, e: impl Into<String>) {
        *self.last_err.lock().unwrap() = e.into();
    }

    /// 插件单独发版用：把 <entry>.new[.exe] 换成正式文件。
    /// 只在插件没跑的时候做 —— Windows 下运行中的 exe 不能覆盖。
    fn apply_pending(&self) -> Result<(), String> {
        let exe = match self.resolve() {
            Some(p) => p,
            None => return Ok(()),
        };
        let newp = {
            let stem = exe.file_stem().and_then(|s| s.to_str()).unwrap_or(&self.spec.entry);
            let cand = exe.with_file_name(format!("{stem}.new"));
            let cand2 = exe.with_file_name(format!("{stem}.new.exe"));
            if cand2.is_file() {
                cand2
            } else if cand.is_file() {
                cand
            } else {
                return Ok(());
            }
        };
        if self.running() {
            return Ok(()); // 下次启动再说
        }
        std::fs::remove_file(&exe).map_err(|e| format!("替换插件失败（删旧）：{e}"))?;
        std::fs::rename(&newp, &exe).map_err(|e| format!("替换插件失败（改名）：{e}"))?;
        Ok(())
    }

    /// 2026-11-10 插件独立发版：把已验签的二进制落到应用数据目录并原子替换。
    /// 调用方必须先 stop()：Windows 下运行中的 exe 不能覆盖。
    /// 落盘顺序 = 写 .part → 旧版改名 .bak → .part 改名正式名；
    /// 任一步失败都能把 .bak 改回去，绝不出现「插件没了」的中间态。
    pub fn install_from(&self, dir: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建插件目录失败：{e}"))?;
        let name = if cfg!(windows) {
            format!("{}.exe", self.spec.entry)
        } else {
            self.spec.entry.clone()
        };
        let target = dir.join(&name);
        let part = dir.join(format!("{name}.part"));
        let bak = dir.join(format!("{name}.bak"));
        std::fs::write(&part, bytes).map_err(|e| format!("写入插件失败：{e}"))?;

        if target.exists() {
            let _ = std::fs::remove_file(&bak);
            if let Err(e) = std::fs::rename(&target, &bak) {
                return Err(format!("备份旧插件失败：{e}"));
            }
        }
        if let Err(e) = std::fs::rename(&part, &target) {
            if bak.exists() {
                let _ = std::fs::rename(&bak, &target);
            }
            let _ = std::fs::remove_file(&part);
            return Err(format!("替换插件失败：{e}"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755));
        }
        #[cfg(target_os = "macos")]
        {
            // 未签名的 Go 二进制被下载后可能带 com.apple.quarantine，
            // Gatekeeper 会直接拦下；本机自己执行的文件移除该属性即可。
            let _ = std::process::Command::new("/usr/bin/xattr")
                .args(["-d", "com.apple.quarantine"])
                .arg(&target)
                .output();
        }
        Ok(target)
    }

    /// 新插件起不来（握手失败 / 契约不符）时回退到上一版。
    pub fn rollback(&self, dir: &Path) -> bool {
        let name = if cfg!(windows) {
            format!("{}.exe", self.spec.entry)
        } else {
            self.spec.entry.clone()
        };
        let target = dir.join(&name);
        let bak = dir.join(format!("{name}.bak"));
        if !bak.exists() {
            return false;
        }
        let _ = std::fs::remove_file(&target);
        std::fs::rename(&bak, &target).is_ok()
    }

    /// 2026-11-10 P0-2：原实现用 DefaultHasher(时间纳秒+pid+栈地址)，可预测。
    /// 改用 OS CSPRNG 生成 256-bit 令牌；拿不到系统熵就直接失败，绝不降级到弱随机。
    fn gen_token(&self) -> Result<String, String> {
        let mut buf = [0u8; 32];
        getrandom::fill(&mut buf).map_err(|e| format!("系统随机数不可用：{e}"))?;
        let mut out = String::with_capacity(64);
        for b in buf {
            out.push_str(&format!("{b:02x}"));
        }
        Ok(out)
    }

    /// 确保插件在跑；已跑则直接返回端口。
    pub fn ensure(&self, app: &AppHandle) -> Result<u16, String> {
        if SHUTDOWN.load(Ordering::Relaxed) {
            return Err("应用正在退出".to_string());
        }

        // ── macOS：wxgo 以当前用户经 LaunchAgent 拉起（与微信同用户，才能 lldb-attach
        //    提取密钥），本进程不持子进程句柄，状态全靠端口探活 + 握手。 ──
        #[cfg(target_os = "macos")]
        {
            if self.running() {
                let tok = self.token.lock().unwrap().clone();
                if !tok.is_empty() {
                    return Ok(self.spec.port);
                }
            }
            // 闸门：微信未重签（仍带 Hardened Runtime）则 wxgo 根本 attach 不上，
            // 明确报错，避免看门狗徒劳重试刷屏。
            if !crate::mac_wx_setup::wechat_resigned() {
                return Err(
                    "微信尚未重签（Hardened Runtime 未移除），无法读取消息。请在「Wx消息」引导里完成重签后重试。"
                        .into(),
                );
            }
            if let Err(e) = self.apply_pending() {
                self.set_err(e);
            }
            let bin = self
                .resolve()
                .ok_or_else(|| format!("plugin_missing:{}", self.spec.id))?;
            let data_dir = crate::mail_cache::data_dir(app)?;
            let db = crate::mail_cache::db_path(app)?;
            let root = bin.parent().unwrap_or(&self.root).display().to_string();
            let sub = |s: &str| -> String {
                s.replace("{data}", &data_dir.display().to_string())
                    .replace("{db}", &db.display().to_string())
                    .replace("{port}", &self.spec.port.to_string())
                    .replace("{root}", &root)
                    .replace("{id}", &self.spec.id)
            };
            // mac 上插件由 LaunchAgent 以当前用户拉起，无父进程管道：
            // ① 去掉 --parent-watch（否则 wxgo 会因 stdin EOF 误判父进程已死而自退）；
            // ② 令牌改经 X8TOKEN_FILE 临时文件注入（见 mac_wx_setup::launch_user）。
            let args: Vec<String> = self
                .spec
                .args
                .iter()
                .filter(|a| a.as_str() != "--parent-watch")
                .map(|a| sub(a.as_str()))
                .collect();
            let token = self.gen_token()?;
            *self.token.lock().unwrap() = token.clone();
            // 以当前用户经 LaunchAgent 拉起（无需管理员），函数内已等端口就绪；握手时
            // pid==0 会跳过 pid 校验（用户态子进程 pid 本进程不知道），安全。
            crate::mac_wx_setup::launch_user(&bin, &args, &token, self.spec.port)?;
            return self.handshake();
        }

        // ── 非 macOS（Windows / Linux）：本进程直接 spawn 子进程，持句柄、靠 stdin 传令牌。 ──
        #[cfg(not(target_os = "macos"))]
        {
        // 已在跑 → 复用
        {
            let mut g = self.child.lock().unwrap();
            if let Some(c) = g.as_mut() {
                match c.try_wait() {
                    Ok(None) => {
                        let tok = self.token.lock().unwrap().clone();
                        if !tok.is_empty() {
                            return Ok(self.spec.port);
                        }
                    }
                    _ => *g = None,
                }
            }
        }

        if let Err(e) = self.apply_pending() {
            self.set_err(e);
        }

        let bin = self
            .resolve()
            .ok_or_else(|| format!("plugin_missing:{}", self.spec.id))?;

        let data_dir = crate::mail_cache::data_dir(app)?;
        let db = crate::mail_cache::db_path(app)?;

        let token = self.gen_token()?;
        *self.token.lock().unwrap() = token.clone();

        // 2026-11-10 P1-2：启动前探测端口归属。只把「端口被占」记进 last_error，
        // 不直接拒绝启动 —— 上一轮残留实例可能正在退出，硬拦会误伤正常重启；
        // 真正的判据是握手时的 token + pid 双重校验（见 handshake/healthy）。
        if let Ok(addr) = format!("127.0.0.1:{}", self.spec.port).parse::<SocketAddr>() {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                self.set_err(format!(
                    "插件 {} 端口 {} 已被占用，正在确认是否为残留实例…",
                    self.spec.id, self.spec.port
                ));
            }
        }

        let root = bin.parent().unwrap_or(&self.root).display().to_string();
        let sub = |s: &str| -> String {
            s.replace("{data}", &data_dir.display().to_string())
                .replace("{db}", &db.display().to_string())
                .replace("{port}", &self.spec.port.to_string())
                .replace("{root}", &root)
                .replace("{id}", &self.spec.id)
        };

        let mut cmd = Command::new(&bin);
        for a in &self.spec.args {
            cmd.arg(sub(a));
        }
        // parent_watch 的插件用 piped stdin：宿主进程消失后管道关闭，
        // 插件读到 EOF 自行退出；句柄由 Child 持有，只要插件还在跑就一直开着。
        cmd.stdin(if self.spec.parent_watch {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("启动插件 {} 失败：{e}", self.spec.id))?;
        self.pid.store(child.id(), Ordering::Relaxed);

        // 2026-11-10 P0-2：令牌作为 stdin 首帧写入，不出现在 argv / 环境变量。
        // 管道继续由 Child 持有（parent_watch 靠它读到 EOF 自退）；老插件不认识
        // 这一行，io.Copy 会直接丢掉，行为不受影响。
        if self.spec.parent_watch {
            if let Some(si) = child.stdin.as_mut() {
                let _ = writeln!(si, "X8TOKEN {token}");
                let _ = si.flush();
            }
        }
        *self.child.lock().unwrap() = Some(child);

        // 等端口就绪（探测间隔 100ms，总上限仍为 8s）
        let tries = BOOT_TIMEOUT_MS / PROBE_INTERVAL_MS;
        for _ in 0..tries {
            std::thread::sleep(Duration::from_millis(PROBE_INTERVAL_MS));
            if self.get(&self.spec.health, 800).is_ok() {
                return self.handshake();
            }
            // 进程已经死了就别傻等
            if !self.running() {
                break;
            }
        }
        self.stop();
        Err(format!("插件 {} 启动超时（端口未就绪）", self.spec.id))
        }
    }

    /// 契约校验：健康接口必须带 contract_key，且不低于 contract_min。
    /// 2026-11-10 P1-2：额外校验 /api/version 的 pid 必须等于本次拉起的子进程，
    /// 否则端口上响应的是残留实例 / 用户自己开的独立版（旧逻辑会误判成插件就绪）。
    fn handshake(&self) -> Result<u16, String> {
        let raw = self.get(&self.spec.health, 1500)?;
        let v: Value = serde_json::from_str(&raw).ok().unwrap_or(Value::Null);
        if let Some(pid) = v.get("pid").and_then(|x| x.as_u64()) {
            let mine = self.pid.load(Ordering::Relaxed) as u64;
            if mine > 0 && pid != mine {
                self.stop();
                let msg = format!(
                    "插件 {} 端口被其它实例占用（响应 pid {pid} ≠ 本次 {mine}），已拒绝握手",
                    self.spec.id
                );
                self.set_err(msg.clone());
                return Err(msg);
            }
        }
        let cv = v
            .get(&self.spec.contract_key)
            .and_then(|x| x.as_u64());
        match cv {
            Some(v) if v >= self.spec.contract_min => {
                self.ever_started.store(true, Ordering::Relaxed);
                self.fails.store(0, Ordering::Relaxed);
                self.set_err("");
                Ok(self.spec.port)
            }
            Some(v) => {
                self.stop();
                let msg = format!(
                    "插件 {} 契约版本过旧（{v} < {}），请升级客户端",
                    self.spec.id, self.spec.contract_min
                );
                self.set_err(msg.clone());
                Err(msg)
            }
            None => {
                // 没报 contract_key：只提示，不拦（兼容早期插件）
                self.ever_started.store(true, Ordering::Relaxed);
                self.fails.store(0, Ordering::Relaxed);
                Ok(self.spec.port)
            }
        }
    }

    pub fn stop(&self) {
        self.stop_with(true);
    }

    /// 硬退路径专用：跳过 HTTP 优雅退出，直接 kill（进程马上要没了，不能等 2s）。
    pub fn stop_fast(&self) {
        self.stop_with(false);
    }

    /// 2026-11-10 P1-1：先请插件优雅收尾，等 2s，超时才 TerminateProcess。
    fn stop_with(&self, graceful: bool) {
        // ── macOS：插件是 root 子进程，本进程不持句柄，只能走 HTTP 优雅退出 + 端口反杀。 ──
        #[cfg(target_os = "macos")]
        {
            let tok = self.token.lock().unwrap().clone();
            if graceful && !tok.is_empty() {
                let _ = self.post("/api/app/shutdown", "{}", 600);
                std::thread::sleep(Duration::from_millis(300));
            }
            crate::mac_wx_setup::kill_by_port(self.spec.port);
            self.pid.store(0, Ordering::Relaxed);
            return;
        }
        #[cfg(not(target_os = "macos"))]
        {
            let mut g = self.child.lock().unwrap();
            let Some(mut c) = g.take() else { return };
            let tok = self.token.lock().unwrap().clone();
            if graceful && !tok.is_empty() {
                let _ = self.post("/api/app/shutdown", "{}", 600);
                // 最多等 3s（报告建议 2~3s）：正常无在途采集时插件百毫秒内就能退干净。
                for _ in 0..30 {
                    if matches!(c.try_wait(), Ok(Some(_))) {
                        self.pid.store(0, Ordering::Relaxed);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            let _ = c.kill();
            let _ = c.wait();
            self.pid.store(0, Ordering::Relaxed);
        }
    }

    /// 看门狗健康判据（P1-2）：进程存活 + 健康接口能带正确 token 应答 +
    /// pid 是本次拉起的那个。比原来的「try_wait 看进程还在不在」strict 得多，
    /// 能识别「界面显示在跑、实际连到别人实例」的假活。
    fn healthy(&self) -> bool {
        if !self.running() {
            return false;
        }
        let raw = match self.get(&self.spec.health, 800) {
            Ok(v) => v,
            Err(_) => return false,
        };
        let v: Value = serde_json::from_str(&raw).ok().unwrap_or(Value::Null);
        if let Some(pid) = v.get("pid").and_then(|x| x.as_u64()) {
            let mine = self.pid.load(Ordering::Relaxed) as u64;
            if mine > 0 && pid != mine {
                return false;
            }
        }
        true
    }

    pub fn get(&self, path: &str, timeout_ms: u64) -> Result<String, String> {
        self.request("GET", path, None, timeout_ms)
    }

    pub fn post(&self, path: &str, body: &str, timeout_ms: u64) -> Result<String, String> {
        self.request("POST", path, Some(body), timeout_ms)
    }

    /// 取二进制（图片 / 表情）。与 request 同口径（token 头、回环 no_proxy），
    /// 但**按字节读**：图片被 resp.text() 解成 UTF-8 会直接损坏。
    /// 返回 (字节, Content-Type)；插件用 200 + application/json 报「wxgf 私有格式」，
    /// 由调用方按 Content-Type 判定成败。
    pub fn get_bytes(&self, path: &str, timeout_ms: u64) -> Result<(Vec<u8>, String), String> {
        let tok = self.token.lock().unwrap().clone();
        let url = format!("http://127.0.0.1:{}{path}", self.spec.port);
        let mut rb = http_client()
            .get(&url)
            .timeout(Duration::from_millis(timeout_ms));
        if !tok.is_empty() {
            rb = rb.header("X-X8-Token", tok);
        }
        let resp = rb.send().map_err(|e| format!("插件无响应：{e}"))?;
        let code = resp.status().as_u16();
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = resp
            .bytes()
            .map_err(|e| format!("读取媒体失败：{e}"))?
            .to_vec();
        if code == 401 {
            return Err("插件令牌不匹配（端口可能被外部实例占用）".to_string());
        }
        if code >= 400 {
            return Err(format!("插件返回 {code}"));
        }
        Ok((bytes, ct))
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        timeout_ms: u64,
    ) -> Result<String, String> {
        let tok = self.token.lock().unwrap().clone();
        let url = format!("http://127.0.0.1:{}{path}", self.spec.port);
        // 2026-11-10 P2-3：复用 Client（连接池 + no_proxy 只解析一次），
        // 超时按请求设置，语义与原来每个请求新建 Client 完全一致。
        let mut rb = if method == "POST" {
            http_client().post(&url).body(body.unwrap_or("{}").to_string())
        } else {
            http_client().get(&url)
        };
        rb = rb.timeout(Duration::from_millis(timeout_ms));
        if !tok.is_empty() {
            rb = rb.header("X-X8-Token", tok);
        }
        let resp = rb.send().map_err(|e| format!("插件无响应：{e}"))?;
        let code = resp.status().as_u16();
        let text = resp.text().unwrap_or_default();
        if code == 401 {
            return Err("插件令牌不匹配（端口可能被外部实例占用）".to_string());
        }
        if code >= 400 {
            return Err(format!("插件返回 {code}：{text}"));
        }
        Ok(text)
    }
}

// ─────────────────────────── 注册表 ───────────────────────────

static REG: OnceLock<Mutex<HashMap<String, Arc<Plugin>>>> = OnceLock::new();
static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static WATCHDOG: OnceLock<()> = OnceLock::new();

fn reg() -> &'static Mutex<HashMap<String, Arc<Plugin>>> {
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取插件句柄（惰性构造）。目录里有 plugin.json 就用它，否则用内置兜底。
pub fn get(id: &str) -> Arc<Plugin> {
    if let Some(p) = reg().lock().unwrap().get(id) {
        return p.clone();
    }
    let root = plugin_dir(id).unwrap_or_else(|| PathBuf::from(id));
    let spec = read_manifest(&root)
        .or_else(|| builtin(id))
        .unwrap_or_else(|| {
            // 完全不认识：用目录名当 entry，参数为空，只做「能起来就算」的兜底
            Spec {
                id: id.to_string(),
                name: id.to_string(),
                version: "0.0.0".into(),
                entry: id.to_string(),
                args: vec![],
                port: 0,
                health: "/api/version".into(),
                contract_key: "contract_version".into(),
                contract_min: 0,
                capabilities: vec![],
                parent_watch: false,
            }
        });
    let p = Arc::new(Plugin::new(spec, root));
    reg().lock().unwrap().insert(id.to_string(), p.clone());
    p
}

/// 已安装插件清单（含未启动的）。前端将来按 capabilities 决定显示哪些页签。
pub fn list() -> Vec<Value> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();

    if let Some(root) = plugins_root() {
        if let Ok(rd) = std::fs::read_dir(&root) {
            for e in rd.flatten() {
                if !e.path().is_dir() {
                    continue;
                }
                let id = e.file_name().to_string_lossy().to_string();
                let p = get(&id);
                if !p.available() {
                    continue;
                }
                seen.insert(p.spec.id.clone());
                out.push(json_of(&p));
            }
        }
    }
    // 内置插件（老安装包 / 开发期没有 plugins 目录）
    for id in ["wx"] {
        if seen.contains(id) {
            continue;
        }
        let p = get(id);
        if p.available() {
            out.push(json_of(&p));
        }
    }
    out
}

fn json_of(p: &Plugin) -> Value {
    let mut v = p.spec.to_json();
    if let Some(o) = v.as_object_mut() {
        o.insert("running".into(), Value::Bool(p.running()));
        o.insert("last_error".into(), Value::String(p.last_error()));
    }
    v
}

/// 应用启动时调一次：把看门狗挂上。
pub fn init(app: &AppHandle) {
    WATCHDOG.get_or_init(|| {
        let app = app.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(WATCHDOG_SECS));
            if SHUTDOWN.load(Ordering::Relaxed) {
                break;
            }
            let items: Vec<Arc<Plugin>> = reg().lock().unwrap().values().cloned().collect();
            for p in items {
                // 只在「曾经起来过」的插件上做自愈，用户没点开过的不去打扰
                if !p.ever_started.load(Ordering::Relaxed) {
                    continue;
                }
                // 2026-11-10 P1-2：判据从「进程还活着」升级为健康接口 + pid 校验。
                if p.healthy() {
                    p.fails.store(0, Ordering::Relaxed);
                    p.next_retry.store(0, Ordering::Relaxed);
                    continue;
                }
                let now = now_secs();
                if now < p.next_retry.load(Ordering::Relaxed) {
                    continue;
                }
                // 进程还在但健康不过：先收掉再重拉，避免一直连到残留实例。
                if p.running() {
                    p.stop_fast();
                }
                match p.ensure(&app) {
                    Ok(_) => {
                        p.fails.store(0, Ordering::Relaxed);
                        p.next_retry.store(0, Ordering::Relaxed);
                    }
                    Err(e) => {
                        let n = p.fails.fetch_add(1, Ordering::Relaxed) + 1;
                        // P2-4：指数退避到 5 分钟封顶，不再「3 次失败后永久锁死」。
                        let back = WATCHDOG_SECS
                            .saturating_mul(1u64 << n.min(4))
                            .min(MAX_BACKOFF_SECS);
                        p.next_retry.store(now + back, Ordering::Relaxed);
                        p.set_err(format!("{e}（第 {n} 次，{back}s 后重试）"));
                    }
                }
            }
        });
        ()
    });
}

/// 停止全部插件（应用退出时调；此后不再自愈）。走优雅退出。
pub fn stop_all() {
    SHUTDOWN.store(true, Ordering::Relaxed);
    let items: Vec<Arc<Plugin>> = reg().lock().unwrap().values().cloned().collect();
    for p in items {
        p.stop();
    }
}

/// 硬退路径（process::exit 兜底）专用：立刻收掉子进程，不等 HTTP 优雅收尾。
/// 2026-11-10 P1-3：让所有绕过 RunEvent::Exit 的 process::exit(0) 都显式收尸，
/// 不再只依赖 stdin EOF 兜底。
pub fn stop_all_fast() {
    SHUTDOWN.store(true, Ordering::Relaxed);
    let items: Vec<Arc<Plugin>> = reg().lock().unwrap().values().cloned().collect();
    for p in items {
        p.stop_fast();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_rejects_path_traversal() {
        let bad = serde_json::json!({"id": "../evil", "entry": "x"});
        assert!(Spec::from_json(&bad).is_none());
        let good = serde_json::json!({"id": "ocr", "entry": "ocr-svc", "port": 60900});
        let s = Spec::from_json(&good).unwrap();
        assert_eq!(s.id, "ocr");
        assert_eq!(s.contract_min, 1);
    }

    #[test]
    fn builtin_wx_matches_manifest_shape() {
        let s = builtin("wx").unwrap();
        assert_eq!(s.entry, "wxadapter");
        assert_eq!(s.port, 60888);
        // P0-2：内置兜底描述也不能再经 argv 传令牌。
        assert!(!s.args.iter().any(|a| a.contains("{token}")));
        assert!(s.parent_watch);
    }

    #[test]
    fn token_is_cryptographically_random_and_long() {
        let p = Plugin::new(builtin("wx").unwrap(), PathBuf::from("."));
        let a = p.gen_token().unwrap();
        let b = p.gen_token().unwrap();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }
}
