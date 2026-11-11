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
    if let Some((ver, _bt, cmin, plats)) = remote {
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
        if need {
            set_phase("downloading");
            match download(ent.unwrap()) {
                Ok(bytes) => {
                    set_phase("installing");
                    p.stop(); // 优雅退出，Windows 运行中 exe 才可覆盖
                    if let Err(e) = p.install_from(&dir, &bytes) {
                        set_error(e.clone());
                        let _ = start_and_probe(app);
                        return Ok(status(&dir, Some(e)));
                    }
                    let next = Local { version: ver.clone(), build_time: String::new() };
                    write_local(&dir, &next);
                    match start_and_probe(app) {
                        Ok(bt) => {
                            write_local(&dir, &Local { version: ver, build_time: bt });
                            if let Ok(mut s) = state().lock() {
                                s.phase = "up_to_date";
                                s.error.clear();
                            }
                        }
                        Err(e) => {
                            // 新包起不来 → 回退上一版，再把插件拉起来，绝不留死页签
                            let rolled = p.rollback(&dir);
                            write_local(&dir, &local);
                            let _ = start_and_probe(app);
                            set_error(format!("新版组件启动失败（{}），已{}", e,
                                if rolled { "回退旧版" } else { "保留原状" }));
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
            match start_and_probe(app) {
                Ok(bt) if !bt.is_empty() => {
                    if bt != local.build_time {
                        write_local(&dir, &Local { version: local.version, build_time: bt });
                    }
                }
                _ => {}
            }
            if let Ok(mut s) = state().lock() {
                if s.phase != "failed" {
                    s.phase = "up_to_date";
                }
            }
        }
    } else {
        let _ = start_and_probe(app);
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
    })
}

/// 页面首屏探测（不联网）：只回本地状态 + 宿主能力。
pub fn info(app: &AppHandle) -> Value {
    match plugin_dir(app) {
        Ok(dir) => status(&dir, None),
        Err(e) => json!({ "auto_update": true, "update_error": e }),
    }
}
