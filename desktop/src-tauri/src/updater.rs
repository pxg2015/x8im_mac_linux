//! 自动升级（2026-10-26 重写，替代 main.rs 里并行存在的五套实现）
//!
//! ## 为什么要重写
//! 1.6.0 → 1.6.19 两天发了 18 个版本，几乎全在修升级。病根是结构，不是某个 bug：
//!
//! 1. **五个入口**（`win_auto_update` / `win_auto_update_nc` / `win_patch_update` /
//!    `win_patch_update_nc` / `win_upd_start`）各自带一整套「检查 → 下载 → 验签 → 触发」，
//!    修一处漏四处。
//! 2. **入口全绑在前端 IPC 上**。主窗加载的是远程页面 `https://im.x8fde.com/app/`，
//!    远程域下 `async` 命令的 IPC 会**永久 pending**——不进函数体、不报错、不 reject，
//!    前端 await 干等，用户点「立即升级」实际一个字节都没下载，屏幕上却是编出来的进度。
//! 3. **执行器单绑** `schtasks`，被企业管控/安全软件禁掉就整条链路废掉。
//!
//! ## 新结构（四条铁律，2026-10-26 晚简化）
//! 1. **唯一入口** [`kick`]：启动自检 / 定时复查 / 前端点按钮，全只调它，内部幂等（BUSY 闸门）。
//! 2. **只发 exe，升级就是跑安装程序**（老8 定）：`latest.json` 只下发 windows-x86_64 的
//!    「安装包直链 + 签名」；客户端下载 → 验签 → 直接拉起 NSIS 向导，用户点「下一步」装完。
//!    组件包（portable zip）那套自解压覆盖整条链路已废弃。
//! 3. **进度必须真**：事件只由真实字节数驱动，前端不再自己编百分比。
//! 4. **同版本自动只拉起一次**：用户在向导里点「取消」就是拒绝，60s 复查不会反复弹。
//!
//! 兼容：事件名 `x8-upd-stage` 与 payload 字段（stage/got/total）保持不变，老前端照常能收。

use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tauri::Emitter;

use crate::{cmp_ver, task_ctx, upd_float_hide, upd_float_stage, upd_log, verify_portable};

#[cfg(windows)]
#[link(name = "shell32")]
extern "C" {}

#[cfg(windows)]
extern "system" {
    /// ShellExecuteW —— 以 `runas` 提权拉起安装程序（安装包 manifest 要求管理员，
    /// `CreateProcess` 会直接 ERROR_ELEVATION_REQUIRED，必须走 ShellExecute）。
    fn ShellExecuteW(
        hwnd: *mut std::ffi::c_void,
        lpOperation: *const u16,
        lpFile: *const u16,
        lpParameters: *const u16,
        lpDirectory: *const u16,
        nShowCmd: i32,
    ) -> isize;
}

/// SW_SHOWNORMAL —— 正常显示安装向导（不能隐藏：用户要点「下一步」）
#[cfg(windows)]
const SW_SHOWNORMAL: i32 = 1;

const LATEST_URL: &str = "https://im.x8fde.com/app/latest.json";

/// 启动后首次检查的延迟。60 秒太长（用户打开就想升），8 秒足够等网络栈就绪。
const FIRST_CHECK_SECS: u64 = 8;
/// 定期复查间隔（秒）。这是「用户没点按钮也能升上去」的兜底保险。
/// 🔴 2026-10-26 老8 两次撞上「发完版还显示旧版」：
/// · 900s(15min)：11:33 发版 → 11:44:40 才升；
/// · 180s(3min) ：12:06:10 发版 → 12:05:53 刚查过 → 12:08:53 才升（差 17 秒擦肩）。
/// 结论：只要轮询，发布点与复查点的「相位差」就会制造盲区。收到 **60 秒**，
/// 盲区压到 1 分钟以内；latest.json 约 500 字节，每分钟一次 ≈ 每天 0.7MB，可忽略。
const RECHECK_SECS: u64 = 60;
/// 连续失败退避上限（秒）：失败次数越多间隔越长，避免弱网环境死循环刷请求。
const BACKOFF_MAX_SECS: u64 = 3600;
/// 同一版本「自动拉起安装向导」的最小间隔（秒）。用户取消后 6 小时内不再自动弹，
/// 但不会永久卡死；manual（点版本号）不受限。
const AUTO_RELAUNCH_SECS: u64 = 6 * 3600;

/// 升级线程重入闸门：同一时刻只允许一个 worker。
static BUSY: AtomicBool = AtomicBool::new(false);
/// 用户点「稍后提醒」后的静默截止时间（unix 秒；0 = 未静默）。
static SNOOZE_UNTIL: AtomicI64 = AtomicI64::new(0);
/// 连续失败次数（用于退避）。
static FAILS: AtomicU32 = AtomicU32::new(0);
/// 下一次允许自动复查的时间（unix 秒；0 = 立即可）。
static NEXT_AUTO_AT: AtomicI64 = AtomicI64::new(0);
/// 自动通道已记录过的新版本指纹（保留字段，当前流程不再用它做抑制）。
#[allow(dead_code)]
static SEEN_VER_HASH: AtomicU64 = AtomicU64::new(0);

/// 版本串指纹（FNV-1a）。只用于「这个版本是否已经记过日志」，不做任何安全用途。
#[allow(dead_code)]
fn ver_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 静默（稍后提醒）持久化：重启客户端也记得，避免「点稍后又自己升了」
// ---------------------------------------------------------------------------

fn snooze_file() -> Option<std::path::PathBuf> {
    let pd = std::env::var("ProgramData").ok()?;
    Some(std::path::Path::new(&pd).join("BangXin").join("update-snooze.txt"))
}

fn load_snooze() {
    let Some(p) = snooze_file() else { return };
    let Ok(txt) = std::fs::read_to_string(&p) else { return };
    if let Ok(v) = txt.trim().parse::<i64>() {
        if v > now_secs() {
            SNOOZE_UNTIL.store(v, Ordering::SeqCst);
        }
    }
}

fn save_snooze(until: i64) {
    SNOOZE_UNTIL.store(until, Ordering::SeqCst);
    if let Some(p) = snooze_file() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&p, until.to_string());
    }
}

/// 「稍后提醒」静默检查。2026-10-26 起自动通道不再自行安装，静默只作为语义留档
/// （前端仍会调 `win_upd_snooze`），保留函数以备未来「静默期抑制提醒」使用。
#[allow(dead_code)]
fn snoozed() -> bool {
    SNOOZE_UNTIL.load(Ordering::SeqCst) > now_secs()
}

// ---------------------------------------------------------------------------
// 阶段与进度事件
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Checking,
    Downloading,
    Verifying,
    Staging,
    Applying,
    Done,
    Failed,
}

impl Phase {
    /// 对外事件名：沿用老前端的既有字符串，保证 1.5.x 客户端也能收。
    fn event(self) -> &'static str {
        match self {
            Phase::Checking => "checking",
            Phase::Downloading => "downloading",
            Phase::Verifying => "downloaded",
            Phase::Staging => "downloaded",
            Phase::Applying => "installing",
            Phase::Done => "started",
            Phase::Failed => "failed",
        }
    }
}

/// 进度节流器：阶段切换必发（force），同一阶段内按 300ms / 1% 双闸门。
///
/// 这里同时是「进度必须真」的落点：got/total 只能来自真实读到的字节数，
/// 绝不按时间推算——今天那次误判（浮窗显示 18% 却什么都没下载）就是假进度造成的。
struct Beat {
    last_at: Option<Instant>,
    last_pct: i64,
    n: u64,
}

impl Beat {
    fn new() -> Self {
        Beat { last_at: None, last_pct: -1, n: 0 }
    }

    fn fire(&mut self, app: &tauri::AppHandle, phase: Phase, got: u64, total: u64, force: bool) {
        let pct = if total > 0 {
            ((got as f64 / total as f64) * 100.0).min(100.0) as i64
        } else {
            0
        };
        let due = match self.last_at {
            None => true,
            Some(p) => p.elapsed() >= Duration::from_millis(300),
        };
        if !force && !due && pct == self.last_pct {
            return;
        }
        self.last_at = Some(Instant::now());
        self.last_pct = pct;
        self.n += 1;
        // 浮窗（1.5.37+）与 Web 事件双通道，两边都喂真实数据。
        upd_float_stage(app, phase.event(), got, total);
        let _ = app.emit(
            "x8-upd-stage",
            serde_json::json!({ "stage": phase.event(), "got": got, "total": total }),
        );
    }
}

/// 单独发一次状态事件（失败 / 收尾这类没有字节数的场景）。
fn fire_plain(app: &tauri::AppHandle, phase: Phase, err: &str) {
    let mut payload = serde_json::json!({ "stage": phase.event(), "got": 0, "total": 0 });
    if !err.is_empty() {
        payload["err"] = serde_json::Value::String(err.to_string());
    }
    let _ = app.emit("x8-upd-stage", payload);
}

// ---------------------------------------------------------------------------
// 唯一入口
// ---------------------------------------------------------------------------

/// 触发一次升级。返回 `true` 表示已接单，`false` 表示已有 worker 在跑（幂等忽略）。
///
/// `why`：`"auto"`（启动自检 / 定时复查）或 `"manual"`（用户点「立即升级」）。
/// 唯一区别：manual 会清掉「稍后提醒」静默并立即执行；auto 在静默期内跳过。
pub fn kick(app: &tauri::AppHandle, why: &str) -> bool {
    if why == "manual" {
        save_snooze(0);
    }
    if BUSY.swap(true, Ordering::SeqCst) {
        upd_log(&format!("[updater] kick({why}) 忽略：已有升级任务在跑"));
        return false;
    }
    upd_log(&format!("[updater] kick({why}) 接单"));
    let a = app.clone();
    let reason = why.to_string();
    std::thread::spawn(move || {
        let r = run(&a, &reason);
        BUSY.store(false, Ordering::SeqCst);
        match r {
            Ok(msg) => {
                upd_log(&format!("[updater] 结束: {msg}"));
            }
            Err(e) => {
                let f = FAILS.fetch_add(1, Ordering::SeqCst) + 1;
                let backoff = std::cmp::min(
                    RECHECK_SECS * (f as u64),
                    BACKOFF_MAX_SECS,
                );
                NEXT_AUTO_AT.store(now_secs() + backoff as i64, Ordering::SeqCst);
                upd_log(&format!("[updater] 失败(第 {f} 次，{backoff}s 后重试): {e}"));
                fire_plain(&a, Phase::Failed, &e);
                upd_float_hide(a.clone());
            }
        }
    });
    true
}

/// 供前端同步命令直接调用：立即升级（清除静默）。
pub fn kick_manual(app: &tauri::AppHandle) -> bool {
    kick(app, "manual")
}

// ---------------------------------------------------------------------------
// 状态机主体
// ---------------------------------------------------------------------------

fn run(app: &tauri::AppHandle, why: &str) -> Result<String, String> {
    let cur = app.package_info().version.to_string();
    let manual = why == "manual";

    // ── 1. 检查 ────────────────────────────────────────────────────────────
    // 2026-10-26 简化：自动通道也会真下载、真拉起安装向导，所以浮窗照常显示。
    let mut beat = Beat::new();
    beat.fire(app, Phase::Checking, 0, 0, true);
    upd_log(&format!("[updater] ①检查 latest.json（当前 {cur}，来源 {why}）"));

    let json = http_text(LATEST_URL)?;
    let v: serde_json::Value =
        serde_json::from_str(&json).map_err(|e| format!("latest.json 解析失败: {e}"))?;
    let ver = v
        .get("version")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if ver.is_empty() {
        return Err("latest.json 缺少 version".into());
    }
    // 🔴 2026-10-26 老8 定（简化升级模式）：「以后只发 exe，升级就下载 exe、下完直接跑安装程序，
    // 下一步下一步装完」——就是 3 天前那套跑得通的老实流程。packaged zip（portable 组件包）
    // 整条链路废弃：不再下发、不再下载、不再解压覆盖。latest.json 只需 windows-x86_64 的
    // url（安装包直链）+ signature（同一份 exe 的 minisign 签名）。
    let win = v
        .get("platforms")
        .and_then(|p| p.get("windows-x86_64"))
        .ok_or("latest.json 缺少 windows-x86_64 段")?;
    let url = win
        .get("url")
        .and_then(|x| x.as_str())
        .ok_or("windows-x86_64.url 缺失")?
        .to_string();
    let sig = win
        .get("signature")
        .and_then(|x| x.as_str())
        .ok_or("windows-x86_64.signature 缺失")?
        .to_string();

    if cmp_ver(&ver, &cur) != std::cmp::Ordering::Greater {
        upd_float_hide(app.clone());
        return Ok(format!("已是最新（{cur} ≥ {ver}）"));
    }

    let ctx = task_ctx()?;
    std::fs::create_dir_all(&ctx.updates).map_err(|e| format!("创建更新目录失败: {e}"))?;
    let marker = ctx.updates.join(format!("setup-launched-{ver}.txt"));

    // 同一版本，自动通道只拉起一次安装向导。用户在向导里点「取消」= 明确拒绝本次升级，
    // 不能让每 60 秒的复查把安装程序再糊他一脸。间隔 6 小时才允许自动再拉一次
    // （既不当场骚扰，也不会永久卡死——手动点版本号走 manual，不受此限）。
    if !manual && marker.exists() {
        let ts = std::fs::read_to_string(&marker)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0);
        if ts > 0 && now_secs() - ts < AUTO_RELAUNCH_SECS as i64 {
            upd_float_hide(app.clone());
            return Ok(format!(
                "发现 {ver}（本版安装向导已在 {} 分钟前拉起过，等用户手动升级）",
                (now_secs() - ts) / 60
            ));
        }
    }

    // ── 1.5 确认（原生消息框，不走 IPC）────────────────────────────────────
    // 2026-10-26 老8：不能「没提示就自己下载、直接弹安装向导」。
    // 远程域下前端确认框等不到 IPC（invoke 永久 pending），所以确认框由客户端内部弹系统消息框。
    // 2026-10-26 二次（点版本号也要问）：**自动通道与手动通道（点版本号）一律先问一句**，
    // 只有用户点「是」才下载。点「否」两条通道都写 marker = 6 小时内自动复查不再骚扰；
    // marker 只挡自动通道（见上方 `if !manual && marker.exists()`），所以用户随时点版本号
    // 仍然会立刻再问一次，不会「点了不提示」。
    upd_log(&format!("[updater] 弹原生确认框（{cur} → {ver}，来源 {why}）"));
    if !crate::upd_ui::confirm_new_version(&ver, &cur) {
        let _ = std::fs::write(&marker, now_secs().to_string());
        upd_float_hide(app.clone());
        upd_log("[updater] 用户选择「稍后」→ 自动复查 6 小时内不再提醒（点版本号仍会立刻问）");
        return Ok(format!("发现 {ver}，用户选择稍后提醒"));
    }
    upd_log("[updater] 用户点「是」→ 开始下载安装包");

    upd_log(&format!(
        "[updater] ①发现新版 {ver}（{cur} → {ver}），开始下载安装包"
    ));
    // ── 2. 下载（流式，真实进度；本地已有同尺寸完整包则跳过）────────────────
    let target = ctx.updates.join(format!("{}_Setup_{ver}.exe", ctx.prefix));
    let total_hint = http_head_len(&url).unwrap_or(0);
    let have = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
    let bytes: Vec<u8> = if total_hint > 0 && have == total_hint {
        beat.fire(app, Phase::Downloading, total_hint, total_hint, true);
        upd_log(&format!("[updater] ②本地已有完整安装包（{have} 字节），跳过下载"));
        std::fs::read(&target).map_err(|e| format!("读取本地安装包失败: {e}"))?
    } else {
        // 原生进度窗：IPC 断了，前端浮窗靠不住，必须让用户看到「在下载、到哪了」。
        let ui = crate::upd_ui::NativeProgress::open(
            &format!("邦信 正在下载更新 v{ver}"),
            "正在连接下载服务器…",
        );
        let res = download(app, &url, &mut beat, Some(&ui));
        ui.finish();
        let b = res?;
        upd_log(&format!("[updater] ②下载完成 {} 字节，进度事件 {} 次", b.len(), beat.n));
        b
    };
    let total = bytes.len() as u64;

    // ── 3. 验签（同一份 exe 的 minisign 签名）───────────────────────────────
    beat.fire(app, Phase::Verifying, total, total, true);
    upd_log("[updater] ③minisign 验签（安装包）");
    verify_portable(&bytes, &sig).map_err(|e| format!("安装包验签失败: {e}"))?;

    // ── 4. 落盘 ────────────────────────────────────────────────────────────
    beat.fire(app, Phase::Staging, total, total, true);
    let path = stage_setup(&ctx, &bytes, &ver)?;
    upd_log(&format!("[updater] ④安装包就位 {}", path.display()));

    // ── 5. 拉起安装向导（下一步 → 下一步）──────────────────────────────────
    // 先写 marker 再启动：即使 UAC 被拒或用户点取消，也不会被 60s 复查反复重弹。
    let _ = std::fs::write(&marker, now_secs().to_string());
    beat.fire(app, Phase::Applying, total, total, true);
    launch_setup(&path)?;
    upd_log("[updater] ⑤已拉起安装程序（NSIS 向导，用户点「下一步」完成安装）");

    FAILS.store(0, Ordering::SeqCst);
    fire_plain(app, Phase::Done, "");
    Ok(format!("已拉起 {ver} 安装向导"))
}

fn http_text(url: &str) -> Result<String, String> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| format!("初始化 HTTP 失败: {e}"))?
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.text())
        .map_err(|e| format!("拉取 {url} 失败: {e}"))
}

/// 流式下载组件包，边读边推真实进度。
fn download(
    app: &tauri::AppHandle,
    url: &str,
    beat: &mut Beat,
    ui: Option<&crate::upd_ui::NativeProgress>,
) -> Result<Vec<u8>, String> {
    let mut resp = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        // 大包 + 慢网：给足 15 分钟，避免半路超时白下载
        .timeout(Duration::from_secs(900))
        .build()
        .map_err(|e| format!("初始化 HTTP 失败: {e}"))?
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("下载组件包失败: {e}"))?;

    let total = resp.content_length().unwrap_or(0);
    beat.fire(app, Phase::Downloading, 0, total, true);
    if let Some(u) = ui {
        u.update(
            0,
            &format!("正在下载安装包…  0 MB / {:.1} MB", total as f64 / 1048576.0),
        );
    }

    let mut buf = vec![0u8; 64 * 1024];
    let mut out: Vec<u8> = Vec::with_capacity(total as usize);
    loop {
        let n = resp
            .read(&mut buf)
            .map_err(|e| format!("下载组件包失败: {e}"))?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        beat.fire(app, Phase::Downloading, out.len() as u64, total, false);
        if let Some(u) = ui {
            let pct = if total > 0 {
                ((out.len() as f64 / total as f64) * 100.0) as u32
            } else {
                0
            };
            u.update(
                pct,
                &format!(
                    "正在下载安装包…  {:.1} MB / {:.1} MB  （{pct}%）",
                    out.len() as f64 / 1048576.0,
                    total as f64 / 1048576.0
                ),
            );
        }
    }
    // 收尾强制一次 100%：节流可能把最后一帧吃掉
    beat.fire(app, Phase::Downloading, out.len() as u64, total, true);
    if let Some(u) = ui {
        u.update(100, "下载完成，正在校验签名…");
    }
    if out.is_empty() {
        return Err("下载得到空包".into());
    }
    if total > 0 && out.len() as u64 != total {
        return Err(format!("下载不完整：{}/{} 字节", out.len(), total));
    }
    Ok(out)
}

/// 落盘安装包：先写 .part 再 rename，并清掉旧的安装包 / 旧版本 marker。
fn stage_setup(
    ctx: &crate::TaskCtx,
    bytes: &[u8],
    ver: &str,
) -> Result<std::path::PathBuf, String> {
    let dir = ctx.updates.clone();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建更新目录失败: {e}"))?;
    let file = format!("{}_Setup_{ver}.exe", ctx.prefix);
    let final_path = dir.join(&file);
    let part_path = dir.join(format!("{file}.part"));
    std::fs::write(&part_path, bytes).map_err(|e| format!("写入安装包失败: {e}"))?;
    std::fs::rename(&part_path, &final_path).map_err(|e| format!("安装包就位失败: {e}"))?;

    let keep_marker = format!("setup-launched-{ver}.txt");
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
            // 只动本品牌自建的安装包与标记，别碰目录里别的东西
            let is_old_exe = name.starts_with(ctx.prefix)
                && name.ends_with(".exe")
                && name.contains("_Setup_");
            let is_old_marker = name.starts_with("setup-launched-") && name != keep_marker;
            if is_old_exe || is_old_marker {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    Ok(final_path)
}

/// HEAD 请求取安装包大小；失败返回 None（不影响主流程，只是少一次「跳过下载」的机会）。
fn http_head_len(url: &str) -> Option<u64> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .ok()?
        .head(url)
        .send()
        .ok()?
        .error_for_status()
        .ok()?
        .content_length()
}

/// 以管理员身份拉起安装程序，**正常显示 NSIS 向导**（用户点「下一步」）。
///
/// 必须用 `ShellExecuteW("runas")`：安装包 manifest 要求管理员，`CreateProcess`
/// 会直接返回 ERROR_ELEVATION_REQUIRED(740)。`nShowCmd` 用 SW_SHOWNORMAL ——
/// 不能隐藏，否则用户看不到「下一步」也点不了。
#[cfg(windows)]
fn launch_setup(path: &std::path::Path) -> Result<(), String> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0u16)).collect()
    }
    let op = wide(OsStr::new("runas"));
    let file = wide(path.as_os_str());
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if r > 32 {
        Ok(())
    } else {
        Err(format!(
            "启动安装程序失败（ShellExecute 返回 {r}；5 = 用户取消了 UAC 授权）"
        ))
    }
}

/// 清理 updates 目录里已经用完的升级文件（2026-10-26 老8：装完自动删，别占地方）。
///
/// 只删「版本 ≤ 当前运行版本」的安装包与标记：
///   当前 1.6.31 在跑 → `*_Setup_1.6.31.exe`、`*_Setup_1.6.30.exe` 都删；
///   但 `*_Setup_1.6.32.exe`（下过、UAC 被拒/点了取消）保留，下次直接复用不重下。
/// 中断的 `.part` 碎片、1.6.29 之前遗留的 portable 组件包，无条件删。
///
/// 时机：调度线程每轮复查前扫一次（启动 60s 后才开始），此时安装器进程已退出，
/// 不会出现「文件被占用删不掉」；万一被占用（用户正手动跑安装包）只是本次跳过，
/// 60 秒后的下一轮会再删。
fn sweep_updates(app: &tauri::AppHandle) {
    let Ok(ctx) = task_ctx() else { return };
    let Ok(rd) = std::fs::read_dir(&ctx.updates) else {
        return;
    };
    let cur = app.package_info().version.to_string();
    let (mut n, mut freed) = (0u32, 0u64);

    for ent in rd.flatten() {
        let p = ent.path();
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);

        // 下载中断的碎片 / 老 portable 组件包：无条件清
        let always = name.ends_with(".part")
            || name.contains("_x64-portable.zip")
            || name == "task-started.txt";

        // 安装包名字：{prefix}_Setup_{ver}.exe；标记：setup-launched-{ver}.txt
        let ver_in_name = name
            .strip_suffix(".exe")
            .and_then(|s| s.find("_Setup_").map(|i| s[i + 7..].to_string()))
            .or_else(|| {
                name.strip_prefix("setup-launched-")
                    .and_then(|s| s.strip_suffix(".txt"))
                    .map(|s| s.to_string())
            });

        let stale = ver_in_name
            .map(|v| !matches!(cmp_ver(&v, &cur), std::cmp::Ordering::Greater))
            .unwrap_or(false);

        if (always || stale) && std::fs::remove_file(&p).is_ok() {
            n += 1;
            freed += size;
        }
    }

    if n > 0 {
        upd_log(&format!(
            "[updater] 清理已用完的升级文件 {n} 个，释放 {:.1} MB（当前版本 {cur}）",
            freed as f64 / 1048576.0
        ));
    }
}

// ---------------------------------------------------------------------------
// 调度器：启动首检 + 定期复查（不依赖前端 IPC 的兜底保险）
// ---------------------------------------------------------------------------

/// 启动后台调度线程。整个进程生命周期只调一次（setup 里）。
///
/// 设计取舍（2026-10-26 老8 定）：自动复查**只发现、不安装**。命中新版只写一行日志，
/// 真正下载安装必须走 manual（前端弹确认框、用户点「立即升级」）。
/// 换句话说：定时器只保证「提醒不会漏」，装不装永远由用户说了算。
pub fn spawn_scheduler(app: tauri::AppHandle) {
    load_snooze();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(FIRST_CHECK_SECS));
        loop {
            // 每轮复查前先清掉已用完的安装包（60s 一次，安装器进程此时早已退出）
            sweep_updates(&app);
            let now = now_secs();
            let next = NEXT_AUTO_AT.load(Ordering::SeqCst);
            if next > now {
                std::thread::sleep(Duration::from_secs((next - now) as u64));
                continue;
            }
            if !BUSY.load(Ordering::SeqCst) {
                kick(&app, "auto");
            }
            std::thread::sleep(Duration::from_secs(RECHECK_SECS));
        }
    });
    upd_log(&format!(
        "[updater] 调度器已启动：{FIRST_CHECK_SECS}s 首检，之后每 {RECHECK_SECS}s 复查"
    ));
}

// ---------------------------------------------------------------------------
// 前端命令（全部同步 fn —— 远程域下 async 命令的 IPC 会永久 pending）
// ---------------------------------------------------------------------------

/// 用户点「立即升级」。同步、即返：真实工作在后台线程里跑。
#[tauri::command]
pub fn win_upd_start(app: tauri::AppHandle) -> Result<String, String> {
    upd_log("[入口] win_upd_start（同步）→ updater::kick(manual)");
    let accepted = kick_manual(&app);
    Ok(if accepted { "accepted" } else { "busy" }.to_string())
}

/// 用户点「稍后提醒」。hours 缺省 2 小时，上限 24 小时。
#[tauri::command]
pub fn win_upd_snooze(hours: Option<u32>) {
    let h = hours.unwrap_or(2).clamp(1, 24) as i64;
    let until = now_secs() + h * 3600;
    save_snooze(until);
    upd_log(&format!("[入口] win_upd_snooze {h}h（至 unix {until}）"));
}

/// 老入口统一转发（`win_patch_update_nc` / `win_auto_update_nc` 用）：
/// 历史上有两套命令各带一整套升级逻辑，现在只留这一个实现。
pub fn legacy_forward(app: &tauri::AppHandle, tag: &str) -> Result<String, String> {
    upd_log(&format!("[入口] {tag}（旧入口转发）→ updater::kick(manual)"));
    let accepted = kick_manual(app);
    Ok(if accepted { "accepted" } else { "busy" }.to_string())
}
