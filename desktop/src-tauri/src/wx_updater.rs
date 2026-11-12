// 邦信 · WX 消息组件（wxgo）自动安装 / 自动更新
// ─────────────────────────────────────────────────────────────
// 2026-11-10 老8 定：wxgo 以后会频繁更新，不能每次都重发邦信安装包。
//   打开 W消息 → 若本地没有组件则自动下载安装；有新版则自动更新；
//   页面显示组件最后编译时间，便于一眼确认是不是最新。
//
// 🔴 端点独立：只读 https://im.x8fde.com/app/latest-wxgo.json，
//    绝不碰主 latest.json（抬主 version 会让 Windows 客户端误判主程序有新版）。
// 🔴 主程序 1.6.77 及更早版本根本没有本模块，不会去下载 → 发布顺序必须是
//    「先发含本模块的客户端，再推 latest-wxgo.json」，老客户端天然不受影响。
// 🔴 下载物是可执行文件：HTTPS + minisign 验签 + size + sha256 三重校验，
//    落到 app_data_dir（不是只读的安装目录），握手失败自动回退上一版。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::AppHandle;

const MANIFEST_URL: &str = "https://im.x8fde.com/app/latest-wxgo.json";
/// 2026-11-10 老8 定：微信大版本升级会换 DB 密钥，wxgo 必须能立刻推下去。
/// 因此「打开 W消息」每次都查版本清单（只有几 KB），不再做 6h 节流；
/// 只保留清单拉取失败后 60s 冷却，避免断网时每次打开都白等 5s。
const FAIL_COOLDOWN_SECS: u64 = 60;
const MAX_BIN_BYTES: u64 = 32 * 1024 * 1024;
/// 宿主能吃的插件契约版本；清单要求更高时拒绝更新（新插件不该跑在老宿主上）。
const HOST_CONTRACT: u64 = 1;

#[derive(Deserialize, Clone)]
struct Entry {
    url: String,
    signature: String,
    size: u64,
    sha256: String,
}

#[derive(Deserialize)]
struct Manifest {
    version: String,
    build_time: String,
    contract_min: u64,
    platforms: HashMap<String, Entry>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct Local {
    version: String,
    build_time: String,
}

struct State {
    last_check: u64,
    last_fail: u64,
    phase: &'static str,
    error: String,
    remote: Option<Manifest>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(State {
            last_check: 0,
            last_fail: 0,
            phase: "idle",
            error: String::new(),
            remote: None,
        })
    })
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn set_phase(p: &'static str) {
    if let Ok(mut s) = state().lock() {
        s.phase = p;
    }
}

fn set_error(e: impl Into<String>) {
    if let Ok(mut s) = state().lock() {
        s.error = e.into();
        s.phase = "failed";
    }
}

/// 编译期决定平台键，与 publish-wxgo.sh 的 PLATFORMS 一一对应。
fn platform_key() -> &'static str {
    if cfg!(all(windows, target_arch = "x86_64")) {
        "windows-x86_64"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "darwin-aarch64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "darwin-x86_64"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x86_64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "linux-aarch64"
    } else {
        ""
    }
}

fn plugin_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let d = crate::mail_cache::data_dir(app)?;
    crate::plugin_host::set_data_dir(d.clone());
    Ok(d.join("plugins").join("wx"))
}

fn version_file(dir: &Path) -> PathBuf {
    dir.join("wxgo.version.json")
}

/// 上一版快照：装新版之前写，回退时连版本号一起还原。
/// 只还原二进制不还原版本号的话，下次打开又会判定「有新版本」→ 重装 → 再失败。
fn prev_file(dir: &Path) -> PathBuf {
    dir.join("wxgo.prev.json")
}

/// 「这个版本启动失败过」的封禁记录。2026-11-12：没有它，一个起不来的新版本
/// 会让每次打开 W消息 都重下 11MB 再失败一次，用户看到的就是「永远在更新、
/// 版本号却从不变化」。force（点「重试更新」）时忽略并清除。
fn blocked_file(dir: &Path) -> PathBuf {
    dir.join("wxgo.blocked.json")
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct Blocked {
    version: String,
    error: String,
    at: u64,
}

fn read_blocked(dir: &Path) -> Option<Blocked> {
    let s = std::fs::read_to_string(blocked_file(dir)).ok()?;
    serde_json::from_str::<Blocked>(&s).ok()
}

fn write_blocked(dir: &Path, b: &Blocked) {
    let _ = std::fs::create_dir_all(dir);
    if let Ok(s) = serde_json::to_string(b) {
        let _ = std::fs::write(blocked_file(dir), s);
    }
}

fn clear_blocked(dir: &Path) {
    let _ = std::fs::remove_file(blocked_file(dir));
}

fn read_local(dir: &Path) -> Local {
    std::fs::read_to_string(version_file(dir))
        .ok()
        .and_then(|s| serde_json::from_str::<Local>(&s).ok())
        .unwrap_or_default()
}

fn write_local(dir: &Path, v: &Local) {
    let _ = std::fs::create_dir_all(dir);
    if let Ok(s) = serde_json::to_string(v) {
        let p = version_file(dir);
        let tmp = dir.join("wxgo.version.json.tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(tmp, p);
        }
    }
}

fn http() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("BangXin/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("HTTP 客户端创建失败：{e}"))
}

fn fetch_manifest() -> Result<Manifest, String> {
    // 每次打开 W消息都要查，超时压到 5s：断网时不能拖住页签打开。
    let c = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("HTTP 客户端创建失败：{e}"))?;
    let raw = c
        .get(MANIFEST_URL)
        .header("Cache-Control", "no-cache")
        .send()
        .map_err(|e| format!("取组件版本清单失败：{e}"))?
        .error_for_status()
        .map_err(|e| format!("组件版本清单 HTTP 错误：{e}"))?
        .text()
        .map_err(|e| format!("读组件版本清单失败：{e}"))?;
    serde_json::from_str::<Manifest>(&raw).map_err(|e| format!("组件版本清单格式错误：{e}"))
}

fn download(e: &Entry) -> Result<Vec<u8>, String> {
    if e.size == 0 || e.size > MAX_BIN_BYTES {
        return Err(format!("组件包大小异常：{} 字节", e.size));
    }
    let resp = http()?
        .get(&e.url)
        .send()
        .map_err(|e| format!("下载组件失败：{e}"))?
        .error_for_status()
        .map_err(|e| format!("组件下载 HTTP 错误：{e}"))?;
    let bytes = resp
        .bytes()
        .map_err(|e| format!("读组件内容失败：{e}"))?
        .to_vec();
    if bytes.len() as u64 != e.size {
        return Err(format!(
            "组件大小不符：{} ≠ 清单 {}",
            bytes.len(),
            e.size
        ));
    }
    let mut h = Sha256::new();
    h.update(&bytes);
    let sum = format!("{:x}", h.finalize());
    if !sum.eq_ignore_ascii_case(&e.sha256) {
        return Err("组件 sha256 与清单不符".into());
    }
    crate::signing::verify(&bytes, &e.signature)?;
    Ok(bytes)
}

/// 拉起插件并读回它自报的编译时间；失败原样返回错误（调用方决定是否回退）。
fn start_and_probe(app: &AppHandle) -> Result<String, String> {
    crate::wx_sidecar::ensure_started(app)?;
    let raw = crate::wx_sidecar::http_get("/api/version", 1500).unwrap_or_default();
    let bt = serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|v| v.get("buildTime").and_then(|x| x.as_str()).map(String::from))
        .unwrap_or_default();
    Ok(bt)
}

/// 打开 W消息时调一次：检查 → 缺件/有新版就装 → 拉起插件。
/// force=true 是页面「更新组件」按钮，跳过 6h 节流。
pub fn prepare(app: &AppHandle, force: bool) -> Result<Value, String> {
    let dir = plugin_dir(app)?;
    let local = read_local(&dir);
    let p = crate::plugin_host::get("wx");
    let have_bin = p.available();

    let due = {
        let mut s = state().lock().unwrap();
        let t = now();
        // 每次打开都查；仅「上次清单拉取失败且还在 60s 冷却内」跳过。
        // 缺件 / 没写过版本文件时无论如何都要查（否则永远装不上）。
        let d = force || !have_bin || local.version.is_empty()
            || t.saturating_sub(s.last_fail) >= FAIL_COOLDOWN_SECS;
        if d {
            s.last_check = t;
            s.error.clear();
        }
        d
    };

    if due {
        set_phase("checking");
        match fetch_manifest() {
            Ok(m) => {
                if let Ok(mut s) = state().lock() {
                    s.remote = Some(m);
                }
            }
            Err(e) => {
                // 断网 / CDN 故障：绝不阻断已有插件，页签照常用旧版。
                if let Ok(mut s) = state().lock() {
                    s.last_fail = now();
                }
                let _ = start_and_probe(app);
                set_error(e.clone());
                return Ok(status(&dir, Some(e)));
            }
        }
    }

    let remote = {
        let s = state().lock().unwrap();
        s.remote.as_ref().map(|m| (m.version.clone(), m.build_time.clone(), m.contract_min, m.platforms.clone()))
    };
    if let Some((ver, rbt, cmin, plats)) = remote {
        if cmin > HOST_CONTRACT {
            set_error(format!("组件要求契约版本 {cmin}，当前客户端仅 {HOST_CONTRACT}"));
            let _ = start_and_probe(app);
            return Ok(status(&dir, None));
        }
        let key = platform_key();
        let ent = plats.get(key);
        if key.is_empty() || ent.is_none() {
            set_error(format!("清单没有当前平台的组件（{key}）"));
            let _ = start_and_probe(app);
            return Ok(status(&dir, None));
        }
        let need = !have_bin || local.version != ver;
        // 上一版装过却起不来 → 不再自动重装同一版本（每次重下 11MB、再失败一次，
        // 用户看到的就是「一直在更新但版本从不变化」）。点「重试更新」(force) 才重试。
        let blk = read_blocked(&dir);
        let blocked_here = matches!(&blk, Some(b) if b.version == ver);
        if blocked_here && !force && have_bin {
            let reason = blk.as_ref().map(|b| b.error.clone()).unwrap_or_default();
            let _ = start_and_probe(app);
            set_error(format!(
                "组件 {ver} 上次启动失败，已跳过自动更新：{reason}\n\
                 当前运行的是 {cur}。点「重试更新」可重新下载安装 {ver}；\
                 点「回退到上一版」可清掉这条记录。",
                cur = if local.version.is_empty() { "未知版本".to_string() } else { local.version.clone() }
            ));
            return Ok(status(&dir, None));
        }
        if need {
            set_phase("downloading");
            match download(ent.unwrap()) {
                Ok(bytes) => {
                    set_phase("installing");
                    p.stop(); // 优雅退出，Windows 运行中 exe 才可覆盖
                    // 装之前记下「当前能跑的版本」：回退时连版本号一起还原。
                    if !local.version.is_empty() {
                        if let Ok(s) = serde_json::to_vec(&local) {
                            let _ = std::fs::create_dir_all(&dir);
                            let _ = std::fs::write(prev_file(&dir), s);
                        }
                    }
                    if let Err(e) = p.install_from(&dir, &bytes) {
                        set_error(e.clone());
                        let _ = start_and_probe(app);
                        return Ok(status(&dir, Some(e)));
                    }
                    let next = Local { version: ver.clone(), build_time: String::new() };
                    write_local(&dir, &next);
                    match start_and_probe(app) {
                        Ok(bt) => {
                            clear_blocked(&dir); // 起来了 → 解除封禁
                            write_local(&dir, &Local { version: ver, build_time: bt });
                            if let Ok(mut s) = state().lock() {
                                s.phase = "up_to_date";
                                s.error.clear();
                            }
                        }
                        Err(e) => {
                            // 2026-11-12 老8 定：不再自动回退。
                            // 自动回退会把「组件时间」变回旧值，用户以为没更新成功 →
                            // 反复点更新 → 每次重下 11MB 再失败一次，问题被永久隐藏
                            // （今天下午就是这样反复了一下午）。
                            // 现在：新版留在磁盘上、失败原因摊开、并封禁该版本防止
                            // 反复重下；由用户点「回退到上一版」或「重试更新」决定下一步。
                            // 注意这里刻意不再 start_and_probe：那次注定失败，只会让
                            // 打开页签再多等一个 30s 超时。
                            write_local(&dir, &local);
                            write_blocked(
                                &dir,
                                &Blocked { version: ver.clone(), error: e.clone(), at: now() },
                            );
                            set_error(format!(
                                "新版组件 {ver} 启动失败：{e}\n\
                                 已停止自动回退（新版保留在磁盘上，不会再重复下载）。\
                                 点「回退到上一版」立刻恢复可用，或点「重试更新」重新下载安装。"
                            ));
                            return Ok(status(&dir, None));
                        }
                    }
                }
                Err(e) => {
                    let _ = start_and_probe(app);
                    set_error(e.clone());
                    return Ok(status(&dir, Some(e)));
                }
            }
        } else {
            // 版本文件已是最新，但**跑着的进程未必是磁盘上这份**：
            // mac 上插件由 LaunchAgent 拉起，宿主不持子进程句柄。若「二进制已被覆盖、
            // 旧进程仍占着端口」，start_and_probe 探到端口活着就直接复用了旧实例，
            // 版本文件还照样写新版 —— 于是永远停在旧版、再不触发更新
            // （2026-11-11 实测：诊断文件写不出来正是这个原因）。
            // 这里用进程自报的 buildTime 与清单比对，落后就停掉重启一次（只一次，防抖）。
            match start_and_probe(app) {
                Ok(bt) if !bt.is_empty() => {
                    if bt != local.build_time {
                        write_local(
                            &dir,
                            &Local { version: local.version.clone(), build_time: bt.clone() },
                        );
                    }
                    if !rbt.is_empty() && bt != rbt {
                        p.stop();
                        match start_and_probe(app) {
                            Ok(bt2) if !bt2.is_empty() => {
                                write_local(
                                    &dir,
                                    &Local {
                                        version: local.version.clone(),
                                        build_time: bt2.clone(),
                                    },
                                );
                                set_phase(if bt2 == rbt { "up_to_date" } else { "stale_running" });
                            }
                            Ok(_) => {}
                            Err(e) => set_error(format!("重启组件失败：{e}")),
                        }
                    }
                }
                _ => {}
            }
            if let Ok(mut s) = state().lock() {
                // stale_running：重启后仍跑旧版（如新包起不来），保留该状态供前端提示，
                // 不要被这里无条件改回 up_to_date。
                if s.phase != "failed" && s.phase != "stale_running" {
                    s.phase = "up_to_date";
                }
            }
        }
    } else {
        let _ = start_and_probe(app);
    }
    Ok(status(&dir, None))
}

/// 手动回退到上一版（新版起不来、或新版本身有问题时的逃生口）。
/// 与已移除的「自动回退」的区别：这一步由用户点，且会把版本号一起还原、
/// 解除版本封禁 —— 否则下次打开又判定「有新版本」→ 重装 → 再失败。
pub fn rollback_to_prev(app: &AppHandle) -> Result<Value, String> {
    let dir = plugin_dir(app)?;
    let p = crate::plugin_host::get("wx");
    p.stop();
    // 等旧进程彻底放开 exe / 端口（Windows 下文件还被占用时改名会失败）。
    std::thread::sleep(Duration::from_millis(400));
    if !p.rollback(&dir) {
        return Err("没有可回退的上一版（插件从未更新过，或备份已被后续安装覆盖）".into());
    }
    let prev = std::fs::read_to_string(prev_file(&dir))
        .ok()
        .and_then(|s| serde_json::from_str::<Local>(&s).ok())
        .unwrap_or_default();
    let local = Local {
        version: prev.version,
        build_time: start_and_probe(app).unwrap_or_else(|_| prev.build_time.clone()),
    };
    write_local(&dir, &local);
    clear_blocked(&dir);
    if let Ok(mut s) = state().lock() {
        s.phase = "up_to_date";
        s.error.clear();
    }
    Ok(status(&dir, None))
}

/// 给前端的组件状态快照。
pub fn status(dir: &Path, err: Option<String>) -> Value {
    let local = read_local(dir);
    let s = state().lock().unwrap();
    let remote_ver = s.remote.as_ref().map(|m| m.version.clone()).unwrap_or_default();
    let remote_bt = s.remote.as_ref().map(|m| m.build_time.clone()).unwrap_or_default();
    let has_update = !remote_ver.is_empty() && remote_ver != local.version;
    let error = err.unwrap_or_else(|| s.error.clone());
    json!({
        "auto_update": true,               // 宿主具备自动安装能力（页签显隐判据）
        "build_time": local.build_time,    // 组件最后编译时间，页面直接显示
        "version": local.version,
        "remote_version": remote_ver,
        "remote_build_time": remote_bt,
        "has_update": has_update,
        "update_state": s.phase,
        "update_error": error,
        // 2026-11-12：有过「某版本启动失败」的封禁记录 → 前端才亮出「回退到上一版」。
        // 不让前端去猜文案，判据只在这里。
        "need_rollback": read_blocked(dir).is_some(),
    })
}

/// 页面首屏探测（不联网）：只回本地状态 + 宿主能力。
pub fn info(app: &AppHandle) -> Value {
    match plugin_dir(app) {
        Ok(dir) => status(&dir, None),
        Err(e) => json!({ "auto_update": true, "update_error": e }),
    }
}
