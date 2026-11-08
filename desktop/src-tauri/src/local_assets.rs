// 邦信 · 本地静态资源（方案 B / PoC，仅 Windows）
//
// 目标：主窗仍加载 https://im.x8fde.com/app/（origin 不变），但 /app/assets/* 这类
// 带 hash 的静态资源改由随包 dist 直接响应，做到「只传数据」。
//
// 设计要点（2026-11-07）：
//  1. 只拦 /app/assets/*，不拦 index.html / version.json / latest.json —— 这三个是
//     前端热更的入口，必须继续走网络。
//  2. 只按「文件名」在本地 dist/assets/ 里找同名文件；**命中才响应，未命中直接放行网络**。
//     vite 产物带 hash，服务器换版后 index.html 引用的是新 hash，本地没有 -> 自动走网络，
//     热更照常生效。因此不需要版本协商、不需要额外请求、不需要改前端/后端/ACL/CORS。
//  3. origin 不变 -> capabilities/remote.json 的 80+ 命令、SSO 回跳、Cookie、
//     /api 相对路径、x8_nav 白名单全部不受影响。
//  4. 天然 kill switch：X8_LOCAL_ASSETS_DIR 未设置且 resource_dir()/app-assets 不存在时
//     整个模块直接 return，行为与改造前逐字节一致。回滚 = 删掉 main.rs 的两行引用。
//
// 状态：PoC。构建链未接（_build.sh / tauri.conf.json resources 未改），默认不激活。

use std::path::PathBuf;
use tauri::{Manager, Runtime};
use webview2_com::{
    take_pwstr, Microsoft::Web::WebView2::Win32::*, WebResourceRequestedEventHandler,
};
use windows::{
    core::{HSTRING, PWSTR},
    Win32::UI::Shell::SHCreateMemStream,
};

const URL_PREFIX: &str = "https://im.x8fde.com/app/assets/";
const MAX_BYTES: usize = 5 * 1024 * 1024;

/// 本地 dist 目录（其下须有 assets/）。找不到 -> None -> 不激活。
fn asset_dir<R: Runtime>(app: &tauri::AppHandle<R>) -> Option<PathBuf> {
    // PoC / 真机验证用：X8_LOCAL_ASSETS_DIR=<web/dist 绝对路径>
    if let Ok(p) = std::env::var("X8_LOCAL_ASSETS_DIR") {
        let d = PathBuf::from(p);
        if d.join("assets").is_dir() {
            return Some(d);
        }
    }
    // 正式落地时：bundle.resources 把 dist 放到 <resource_dir>/app-assets/
    let d = app.path().resource_dir().ok()?.join("app-assets");
    if d.join("assets").is_dir() {
        Some(d)
    } else {
        None
    }
}

/// WebView2 对 JS/CSS 做严格 MIME 检查，给错会直接拒绝执行（PoC 主要风险点）。
fn mime_of(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1;
    Some(match ext {
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "json" => "application/json; charset=utf-8",
        _ => return None,
    })
}

/// 从 URI 取纯文件名；任何路径穿越 / 非白名单字符 / 非白名单扩展名一律放行网络。
fn safe_name(uri: &str) -> Option<&str> {
    let rest = uri.strip_prefix(URL_PREFIX)?;
    let name = rest.split(['?', '#']).next()?;
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return None;
    }
    mime_of(name)?;
    Some(name)
}

pub fn install<R: Runtime>(app: &tauri::AppHandle<R>) {
    let Some(dir) = asset_dir(app) else {
        return; // 未配置本地资源 = 零行为变化
    };
    let Some(win) = app.get_webview_window("main") else {
        eprintln!("[x8][local-assets] main 窗口未找到，跳过（走网络）");
        return;
    };
    let dir_for_log = dir.clone();
    let ret = win.with_webview(move |w| unsafe {
        let controller = w.controller();
        let core = match controller.CoreWebView2() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[x8][local-assets] CoreWebView2 取不到: {e}（走网络）");
                return;
            }
        };
        let env = w.environment();

        let filter = HSTRING::from(format!("{URL_PREFIX}*"));
        if let Err(e) = core.AddWebResourceRequestedFilter(
            &filter,
            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
        ) {
            eprintln!("[x8][local-assets] 注册 filter 失败: {e}（走网络）");
            return;
        }

        let mut token: i64 = 0;
        let r = core.add_WebResourceRequested(
            &WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()); };
                let req = args.Request()?;
                let mut raw = PWSTR::null();
                req.Uri(&mut raw)?;
                let uri = take_pwstr(raw);
                let Some(name) = safe_name(&uri) else { return Ok(()); };

                let path = dir.join("assets").join(name);
                let Ok(bytes) = std::fs::read(&path) else {
                    return Ok(()); // 本地没有该 hash -> 放行网络（热更照常）
                };
                if bytes.len() > MAX_BYTES {
                    return Ok(());
                }
                let mime = mime_of(name).unwrap_or("application/octet-stream");
                let headers = HSTRING::from(format!(
                    "Content-Type: {mime}\r\nCache-Control: public, max-age=31536000, immutable\r\n"
                ));
                let status = HSTRING::from("OK");
                let stream = SHCreateMemStream(Some(&bytes));
                let resp = env.CreateWebResourceResponse(
                    stream.as_ref(),
                    200,
                    &status,
                    &headers,
                )?;
                args.SetResponse(&resp)?;
                eprintln!("[x8][local-assets] 命中 {name} ({} B)", bytes.len());
                Ok(())
            })),
            &mut token,
        );
        match r {
            Ok(()) => eprintln!(
                "[x8][local-assets] 已启用 dir={dir_for_log:?} token={token}"
            ),
            Err(e) => eprintln!("[x8][local-assets] 注册 handler 失败: {e}（走网络）"),
        }
    });
    if let Err(e) = ret {
        eprintln!("[x8][local-assets] with_webview 失败: {e}（走网络）");
    }
}
