// 邦信 · IPC 卡死定性探针（2026-11-16）
// ─────────────────────────────────────────────────────────────
// 目的：一次性区分「IPC 通道断」/「blocking 线程池占满」/「某条命令内部卡」/「回程丢」。
// 这不是补丁，是诊断工具：只加观测，不改任何现有业务逻辑。
//
// 两个对照命令：
//   x8_ping        —— 同步命令，走 Tauri v2 的 blocking 线程池
//   x8_ping_async  —— async 命令，走 tokio runtime，不占 blocking 线程池
// 判读矩阵（复现卡死时点「复制诊断」看四条命令谁回）：
//   两个 ping 都不回            → IPC 通道/WebView 断（不是业务代码）
//   x8_ping 不回、async 回      → blocking 线程池被某条同步命令占满
//   两个 ping 都回、wx_plugin_info 不回 → 那条命令内部卡（看 ipc_probe.log 的 EXIT 缺口）
//   四条全回但前端仍 timeout    → 回程/JS 回调问题
//
// 埋点日志：<app_data_dir>/ipc_probe.log，每行 [时:分:秒.毫秒] ENTER/EXIT + 线程 + 耗时。
// 全部 fire-and-forget：写盘失败绝不影响命令本身、绝不 panic。

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

static PROBE_LOG: OnceLock<PathBuf> = OnceLock::new();

/// setup 时调用一次：定位日志文件并建立目录。
pub fn init(dir: PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    let _ = PROBE_LOG.set(dir.join("ipc_probe.log"));
}

fn stamp() -> String {
    chrono::Local::now().format("%H:%M:%S%.3f").to_string()
}

/// 写一行埋点。任何失败都静默吞掉。
pub fn mark(line: String) {
    let Some(path) = PROBE_LOG.get() else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "[{}] {}", stamp(), line);
    }
}

/// 计时守卫：构造时写 ENTER，Drop 时写 EXIT + 实际耗时（毫秒）。
/// 用法：命令体第一行 `let _probe = crate::ipc_probe::Guard::enter("命令名");`
/// 即使命令中途 ? 提前返回，Drop 仍会记录 EXIT，能看到真实耗时。
pub struct Guard {
    name: &'static str,
    started: Instant,
    tid: String,
}

impl Guard {
    pub fn enter(name: &'static str) -> Self {
        let tid = format!("{:?}", std::thread::current().id());
        mark(format!("ENTER {:<24} tid={}", name, tid));
        Self { name, started: Instant::now(), tid }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        mark(format!(
            "EXIT  {:<24} cost={}ms tid={}",
            self.name,
            self.started.elapsed().as_millis(),
            self.tid
        ));
    }
}

/// 同步 ping —— 测 blocking 线程池是否还接活。
/// 刻意不碰任何锁、文件、网络、插件，只回一个带时间戳的对象。
#[tauri::command]
pub fn x8_ping() -> serde_json::Value {
    serde_json::json!({
        "pong": true,
        "kind": "sync",
        "thread": format!("{:?}", std::thread::current().id()),
        "ts": chrono::Local::now().timestamp_millis(),
    })
}

/// 异步 ping —— 走 tokio runtime，不占 blocking 线程池。
/// 与同步 ping 对照即可定位「通道断」还是「线程池满」。
#[tauri::command]
pub async fn x8_ping_async() -> serde_json::Value {
    serde_json::json!({
        "pong": true,
        "kind": "async",
        "ts": chrono::Local::now().timestamp_millis(),
    })
}
