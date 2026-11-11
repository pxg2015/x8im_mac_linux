// 邦信 · WX 消息插件（wxadapter）—— 通用插件宿主的薄封装
// ─────────────────────────────────────────────────────────────
// 2026-11-10 老8 定：插件机制必须与语言无关，宿主通用化（见 plugin_host.rs）。
// 本文件只剩「把 wx 这个 id 转过去」的胶水，不再持有任何进程/端口/token 逻辑。
//
// 三条硬口径（不变）：
//   1) 插件没有托盘、没有设置页 —— 界面全部在邦信的「Wx消息」页签里；
//   2) 邦信关 → 插件必关（RunEvent::Exit 里 stop_all，见 main.rs）；
//   3) 本地消息数据并入邦信自己的 sqlite（app_data_dir/mail.db 的 wx_* 表），
//      插件只写库，客户端直接读库，控制动作走 loopback API。
//
// 留下的这层壳是为了不改 wx_cache.rs 的 11 个命令签名 —— 谁调都还是这几个函数。

use tauri::AppHandle;

/// 插件 id（对应 plugins/wx/plugin.json 的 id）。
const ID: &str = "wx";

/// 本平台有没有插件（没有则前端显示「请升级客户端」而不是报错）。
pub fn available() -> bool {
    crate::plugin_host::get(ID).available()
}

/// 确保插件在跑。已在跑则直接返回端口；否则拉起并等端口就绪 + 契约校验。
pub fn ensure_started(app: &AppHandle) -> Result<u16, String> {
    crate::plugin_host::get(ID).ensure(app)
}

/// 停止插件（应用退出时调用，优雅收尾）。
pub fn stop_all() {
    crate::plugin_host::stop_all();
}

/// 硬退路径（process::exit 兜底）：立即收掉插件，不等 HTTP 优雅退出。
pub fn stop_all_fast() {
    crate::plugin_host::stop_all_fast();
}

/// 插件是否在跑（前端用来显示「同步引擎：运行中 / 未启动」）。
pub fn running() -> bool {
    crate::plugin_host::get(ID).running()
}

/// 最近一次错误（看门狗/握手写进去的，界面上给一句人话）。
pub fn last_error() -> String {
    crate::plugin_host::get(ID).last_error()
}

/// 调用插件控制 API（GET）。
pub fn http_get(path: &str, timeout_ms: u64) -> Result<String, String> {
    crate::plugin_host::get(ID).get(path, timeout_ms)
}

/// 调用插件控制 API（GET，按字节返回）。用于图片 / 表情这类二进制媒体。
pub fn http_get_bytes(path: &str, timeout_ms: u64) -> Result<(Vec<u8>, String), String> {
    crate::plugin_host::get(ID).get_bytes(path, timeout_ms)
}

/// 调用插件控制 API（POST，body 为 JSON 字符串）。
pub fn http_post(path: &str, body: &str, timeout_ms: u64) -> Result<String, String> {
    crate::plugin_host::get(ID).post(path, body, timeout_ms)
}
