// ============================================================================
// macOS 原生文件选择器（NSOpenPanel）—— 2026-11-18
// ----------------------------------------------------------------------------
// 老8 报的问题：① mac 点「发送图片/发送文件」弹出的面板全英文 ② 点图片按钮反应太慢。
//
// 为什么不用 WebView 的 `<input type="file">`：
//   WKWebView 的文件框是 wry 弹的（wry_web_view_ui_delegate.rs:100-125）：
//     ① 不按 accept 过滤类型 —— 「发送图片」会列出全部文件（wry 只设了 canChooseFiles）
//     ② 拿不到绝对路径 —— 发送方「在文件夹中显示」在 mac 上不可用
//     ③ 起始目录/标题不受控 —— 默认落在「上次用的目录」，撞上 iCloud 云盘目录时
//        macOS 枚举云盘目录会把面板卡住半天（老8 报的「反应太慢」最可能就是这个）
//     ④ 面板语言跟 bundle 走（CFBundleLocalizations，已在 src-tauri/Info.plist 声明）
//
// 本模块自己弹 NSOpenPanel：语言 / 类型过滤 / 起始目录 / 绝对路径全部可控。
//
// 约束：
//   · AppKit 要求面板必须在**主线程**创建并 runModal → 由 http_local.rs 用
//     tauri 的 run_on_main_thread 调度（本函数只能被主线程调用）
//   · 只做「选文件」，不读写文件内容（字节由前端再走 /readfile 取，与 Windows 同链路）
// ============================================================================

use objc2_app_kit::{NSModalResponse, NSModalResponseOK, NSOpenPanel};
use objc2_foundation::{NSArray, NSString, NSURL};
// objc2-foundation 0.3.2 把 MainThreadMarker 挪到了 objc2 根，旧路径留了 deprecated 再导出。
// 与 wry 0.57 用的是同一个类型（就是 objc2::MainThreadMarker），这里显式豁免弃用告警。
#[allow(deprecated)]
use objc2_foundation::MainThreadMarker;

/// 图片扩展名白名单（与 Windows 那条 PowerShell 过滤保持同口径）
const IMG_EXT: [&str; 8] = ["png", "jpg", "jpeg", "gif", "webp", "bmp", "heic", "heif"];

/// 弹系统文件选择框。
///
/// 返回：Ok(选中文件列表 [(绝对路径, 字节数)])；用户取消 = Ok(空列表)。
/// Err = 弹不出来（非主线程等）。
///
/// ⚠️ 必须在主线程调用（AppKit 硬性要求）。
pub(crate) fn pick_files(want_img: bool) -> Result<Vec<(String, u64)>, String> {
    let Some(mtm) = MainThreadMarker::new() else {
        return Err("pick_not_main_thread".to_string());
    };

    // NSOpenPanel 是「共享单例」语义：openPanel 拿到的是 AppKit 内部那一个面板实例，
    // 每次都重新设一遍属性，避免上一次调用残留（如上次的类型过滤）。
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(true);
    panel.setAllowsMultipleSelection(true);
    panel.setCanChooseDirectories(false);
    panel.setShowsHiddenFiles(false);
    panel.setCanCreateDirectories(false);

    let title = NSString::from_str("邦信 - 选择要发送的文件");
    panel.setTitle(Some(&title));

    // 起始目录固定为用户主目录：
    //   不设的话 NSOpenPanel 会用「上次用的目录」，若那目录在 iCloud 云盘里，
    //   macOS 枚举云盘目录会把面板卡住（老8 报的「点图片按钮反应太慢」）。
    //   主目录是本地目录，列一层很快；用户再自己往里走。
    if let Some(home) = std::env::var("HOME").ok().filter(|s| !s.is_empty()) {
        let dir = NSString::from_str(&home);
        let url = NSURL::fileURLWithPath(&dir);
        panel.setDirectoryURL(Some(&url));
    }

    // 类型过滤：只有「发送图片」入口才限制为图片；「发送文件」不限类型。
    if want_img {
        let types: Vec<_> = IMG_EXT.iter().map(|e| NSString::from_str(e)).collect();
        let arr = NSArray::from_retained_slice(&types);
        // allowedFileTypes 在 macOS 12 起被 Apple 标记废弃（推荐 allowedContentTypes/UTType），
        // 但功能仍在、行为一致，且不引 UTType 依赖 → 这里显式豁免废弃告警。
        #[allow(deprecated)]
        panel.setAllowedFileTypes(Some(&arr));
    }

    let ok: NSModalResponse = panel.runModal();
    if ok != NSModalResponseOK {
        // 用户点了取消（不是错误）
        return Ok(Vec::new());
    }

    let urls = panel.URLs();
    let mut out: Vec<(String, u64)> = Vec::new();
    for i in 0..urls.count() {
        let u = urls.objectAtIndex(i);
        if let Some(p) = u.path() {
            let path = p.to_string();
            if path.is_empty() {
                continue;
            }
            // 字节数：前端据此对超大文件回退旧文件框（>300MB 不进内存）
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            out.push((path, size));
        }
    }
    Ok(out)
}
