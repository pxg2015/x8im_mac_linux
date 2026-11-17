// ============================================================================
// 本地 HTTP 通用桥（2026-11-16 全量迁移；2026-11-17 从 main.rs 拆出，逻辑零改动）
// ----------------------------------------------------------------------------
// 前端实际会调用的命令尽量都从这里分发（HTTP 回环比 WebView2 的 IPC 回程稳）。
// 不接管的命令返回 Err("UNKNOWN_CMD") → 路由层回 404 → 前端回退 IPC，这条回落必须保留。
// ============================================================================
use super::*;

// ═══════════════════════════════════════════════════════════════════════
// 本地 HTTP 通用桥（2026-11-16 全量迁移）
// ───────────────────────────────────────────────────────────────────────
// 背景：WebView2 的 invoke 桥在部分机器上会整体失效（首条命令 0ms 到达 Rust，
// 回程却回不到 JS，此后所有命令都发不出去，实证见 ipc_probe.log）。而本地 HTTP
// 回环这条通道（主业务、落盘）一直稳。WX 页签先迁过去已验证有效，本轮把前端
// **实际会调用的全部命令**一次迁完，避免「改一个漏一个」。
//
// 约定：
//   · 参数名宽松匹配（appId / app_id / APPID 都认）——Tauri 的 invoke 默认按
//     camelCase 传参，前端各处写法并不统一，这里统一归一化，不做二次约定。
//   · 会等很久的命令（导航自愈、下载安装、IMAP 全量扫描）分两类处理：
//       - 前端要结果 → block_on（HTTP 是独立线程，阻塞不影响界面）
//       - 只要触发   → spawn 后立刻返回 "started"，前端不必等
//   · 不接管的命令返回 Err("UNKNOWN_CMD") → 路由层回 404 → 前端回退 IPC。
//     这条回落必须永远保留：客户机上安全软件偶尔会拦本地回环。
// ═══════════════════════════════════════════════════════════════════════

fn bridge_norm(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// 宽松取参：精确名 → 归一化名（忽略大小写与下划线/连字符）
fn bridge_argv<'a>(a: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    let obj = a.as_object()?;
    if let Some(v) = obj.get(key) {
        return Some(v);
    }
    let nk = bridge_norm(key);
    obj.iter()
        .find(|(k, _)| bridge_norm(k) == nk)
        .map(|(_, v)| v)
}

fn bridge_to_json<T: serde::Serialize>(r: Result<T, String>) -> Result<serde_json::Value, String> {
    let v = r?;
    serde_json::to_value(v).map_err(|e| format!("序列化失败：{e}"))
}

pub(crate) fn x8bridge_dispatch(
    app: &tauri::AppHandle,
    cmd: &str,
    a: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    use serde_json::json;
    let sv = |k: &str| {
        bridge_argv(a, k)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    let osv = |k: &str| {
        bridge_argv(a, k)
            .and_then(|x| x.as_str())
            .map(str::to_string)
    };
    let iv = |k: &str, d: i64| bridge_argv(a, k).and_then(|x| x.as_i64()).unwrap_or(d);
    let ou32 = |k: &str| bridge_argv(a, k).and_then(|x| x.as_u64()).map(|v| v as u32);
    let ou16 = |k: &str| bridge_argv(a, k).and_then(|x| x.as_u64()).map(|v| v as u16);
    let ou64 = |k: &str| bridge_argv(a, k).and_then(|x| x.as_u64());
    let bv = |k: &str, d: bool| bridge_argv(a, k).and_then(|x| x.as_bool()).unwrap_or(d);
    let ob = |k: &str| bridge_argv(a, k).and_then(|x| x.as_bool());

    match cmd {
        // ── 刷新 / 生命周期（P0：老8「点版本更新刷新不动，只能关掉邦信重开」）──
        // 这三条内部要等 2.2~2.6 秒判断导航是否真的提交，绝不能阻塞 HTTP 线程：
        // spawn 后立刻回 started，前端拿到回执就收工，真正的导航由 Rust 自己走。
        "x8_reload_ui" => {
            let h = app.clone();
            tauri::async_runtime::spawn(async move {
                let r = x8_reload_ui(h).await;
                let _ = r;
            });
            Ok(json!("started"))
        }
        "x8_rebuild_ui" => {
            let h = app.clone();
            tauri::async_runtime::spawn(async move {
                let r = x8_rebuild_ui(h).await;
                let _ = r;
            });
            Ok(json!("started"))
        }
        "x8_nav" => bridge_to_json(x8_nav(app.clone(), sv("target"))),
        "x8_restart" => bridge_to_json(tauri::async_runtime::block_on(x8_restart(app.clone()))),

        // ── 落盘 / 文件（7 个）──
        // 单次 base64 可达数 MB，路由层的 body 上限已放到 20MB。
        "save_bytes" => bridge_to_json(tauri::async_runtime::block_on(save_bytes(
            sv("name"),
            sv("b64"),
            osv("subdir"),
        ))),
        "save_chunk_begin" => bridge_to_json(tauri::async_runtime::block_on(save_chunk_begin(
            sv("name"),
            osv("subdir"),
        ))),
        "save_chunk_append" => bridge_to_json(tauri::async_runtime::block_on(save_chunk_append(
            sv("token"),
            sv("b64"),
        ))),
        "save_chunk_end" => bridge_to_json(tauri::async_runtime::block_on(save_chunk_end(
            sv("token"),
            sv("name"),
        ))),
        "download_file" => bridge_to_json(tauri::async_runtime::block_on(download_file(
            app.clone(),
            sv("url"),
            sv("token"),
            sv("name"),
            ob("open"),
            osv("key"),
        ))),
        "reveal_file" => bridge_to_json(tauri::async_runtime::block_on(reveal_file(sv("path")))),
        "file_retention_get" => bridge_to_json(file_retention_get()),
        "file_retention_set" => bridge_to_json(file_retention_set(iv("days", 30) as u32)),
        "file_retention_clean" => bridge_to_json(file_retention_clean(ou32("days"))),

        // ── 窗口 / 外壳（9 个）──
        // drag_begin 要 WebviewWindow 而不是 AppHandle —— 前端只有主窗标题栏用它。
        "drag_begin" => match app.get_webview_window("main") {
            Some(w) => bridge_to_json(tauri::async_runtime::block_on(drag_begin(w))),
            None => Err("no main window".into()),
        },
        "win_minimize" => bridge_to_json(win_minimize(app.clone())),
        "win_toggle_maximize" => bridge_to_json(win_toggle_maximize(app.clone())),
        "win_close" => bridge_to_json(win_close(app.clone())),
        "win_is_maximized" => bridge_to_json(win_is_maximized(app.clone())),
        "win_is_focused" => Ok(json!(win_is_focused(app.clone()))),
        "win_patch_update_nc" => bridge_to_json(win_patch_update_nc(app.clone())),
        "win_auto_update_nc" => bridge_to_json(win_auto_update_nc(app.clone())),
        "open_workbench" => bridge_to_json(tauri::async_runtime::block_on(open_workbench(
            app.clone(),
            sv("url"),
        ))),
        "open_in_browser" => bridge_to_json(tauri::async_runtime::block_on(open_in_browser(
            sv("url"),
        ))),
        "ext_open_window" => bridge_to_json(tauri::async_runtime::block_on(ext_open_window(
            app.clone(),
            sv("appId"),
            sv("url"),
            bridge_argv(a, "w").and_then(|x| x.as_f64()).unwrap_or(900.0),
            bridge_argv(a, "h").and_then(|x| x.as_f64()).unwrap_or(700.0),
        ))),
        "ext_close_window" => bridge_to_json(tauri::async_runtime::block_on(ext_close_window(
            app.clone(),
            sv("appId"),
        ))),
        "ext_focus_window" => bridge_to_json(tauri::async_runtime::block_on(ext_focus_window(
            app.clone(),
            sv("appId"),
        ))),
        "detect_wechat" => Ok(json!(tauri::async_runtime::block_on(detect_wechat()))),

        // ── 登录凭据 / 开机自启（5 个）──
        "x8_auth_save" => bridge_to_json(x8_auth_save(app.clone(), sv("blob"))),
        "x8_auth_load" => bridge_to_json(x8_auth_load(app.clone())),
        "x8_auth_clear" => bridge_to_json(x8_auth_clear(app.clone())),
        "autostart_get" => bridge_to_json(autostart_get()),
        "autostart_set" => bridge_to_json(autostart_set(bv("on", false))),

        // ── 升级（6 个）──
        "mac_upd_status" => Ok(json!(mac_updater::mac_upd_status(app.clone()))),
        "mac_self_update" => bridge_to_json(mac_updater::mac_self_update(app.clone())),
        "mac_upd_snooze" => {
            mac_updater::mac_upd_snooze(app.clone(), ou32("hours"));
            Ok(json!(null))
        }
        "win_upd_start" => bridge_to_json(updater::win_upd_start(app.clone())),
        "win_upd_snooze" => {
            updater::win_upd_snooze(ou32("hours"));
            Ok(json!(null))
        }
        "linux_self_update" => bridge_to_json(tauri::async_runtime::block_on(
            linux_self_update(app.clone()),
        )),
        "screen_clip_grab" => bridge_to_json(tauri::async_runtime::block_on(screen_clip_grab(
            ou64("timeoutMs").or_else(|| ou64("timeout_ms")),
        ))),

        // ── 内置邮件（12 个；IMAP 全量扫描可能跑几十秒，独立线程阻塞可接受）──
        "mail_config_get" => bridge_to_json(tauri::async_runtime::block_on(mail::mail_config_get(
            app.clone(),
        ))),
        "mail_config_set" => bridge_to_json(tauri::async_runtime::block_on(
            mail::mail_config_set(
                app.clone(),
                sv("addr"),
                sv("secret"),
                osv("provider"),
                osv("imapHost"),
                ou16("imapPort"),
                osv("smtpHost"),
                ou16("smtpPort"),
                osv("login"),
            ),
        )),
        "mail_config_clear" => bridge_to_json(tauri::async_runtime::block_on(
            mail::mail_config_clear(app.clone()),
        )),
        "mail_secret_get" => bridge_to_json(tauri::async_runtime::block_on(mail::mail_secret_get(
            app.clone(),
        ))),
        "mail_list" => bridge_to_json(tauri::async_runtime::block_on(mail::mail_list(
            app.clone(),
            ou32("limit"),
            osv("folder"),
        ))),
        "mail_sync" => bridge_to_json(tauri::async_runtime::block_on(mail::mail_sync(
            app.clone(),
            ou32("limit"),
            osv("folder"),
            ob("full"),
        ))),
        "mail_read" => bridge_to_json(tauri::async_runtime::block_on(mail::mail_read(
            app.clone(),
            ou32("uid").unwrap_or(0),
            osv("folder"),
        ))),
        "mail_attachment" => bridge_to_json(tauri::async_runtime::block_on(mail::mail_attachment(
            app.clone(),
            ou32("uid").unwrap_or(0),
            ou32("index").unwrap_or(0),
            osv("folder"),
        ))),
        "mail_mark_seen" => bridge_to_json(tauri::async_runtime::block_on(mail::mail_mark_seen(
            app.clone(),
            ou32("uid").unwrap_or(0),
            osv("folder"),
        ))),
        "mail_send" => {
            let atts: Option<Vec<mail::MailAttachIn>> = bridge_argv(a, "attachments")
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            bridge_to_json(tauri::async_runtime::block_on(mail::mail_send(
                app.clone(),
                sv("to"),
                sv("cc"),
                sv("subject"),
                sv("body"),
                osv("html"),
                ou32("replyUid"),
                ou32("forwardUid"),
                atts,
            )))
        }
        "mail_unseen" => bridge_to_json(tauri::async_runtime::block_on(mail::mail_unseen(
            app.clone(),
        ))),

        // ── Linux P2P 传输内核（4 个；recv 是 800ms 长轮询，block_on 即对齐原语义）──
        "p2p_sidecar_info" => Ok(json!(p2p_sidecar_info())),
        "p2p_sc_open" => bridge_to_json(tauri::async_runtime::block_on(p2p_sc_open())),
        "p2p_sc_send" => bridge_to_json(tauri::async_runtime::block_on(p2p_sc_send(
            ou32("id").unwrap_or(0),
            bridge_argv(a, "op").and_then(|x| x.as_u64()).unwrap_or(0) as u8,
            sv("payload"),
        ))),
        "p2p_sc_recv" => bridge_to_json(tauri::async_runtime::block_on(p2p_sc_recv(
            ou32("id").unwrap_or(0),
            ou64("timeoutMs")
                .or_else(|| ou64("timeout_ms"))
                .unwrap_or(800),
        ))),
        "p2p_sc_close" => bridge_to_json(tauri::async_runtime::block_on(p2p_sc_close(
            ou32("id").unwrap_or(0),
        ))),

        // ── 角标 / 浮窗 / AI邦卡通精灵（IPC 桥失效时角标不刷、进度窗不弹）──
        "notify_unread" => bridge_to_json(notify_unread(
            app.clone(),
            iv("count", 0),
            ob("flash"),
            osv("title"),
            osv("body"),
            ob("inCurrent"),
        )),
        "fde_float_show" => bridge_to_json(fde_float_show(
            app.clone(),
            bridge_argv(a, "count").and_then(|x| x.as_i64()),
            osv("tip"),
            ob("silent"),
            ob("bump"),
        )),
        "fde_float_hide" => {
            fde_float_hide(app.clone());
            Ok(json!(null))
        }
        "upd_float_show" => bridge_to_json(upd_float_show(
            app.clone(),
            osv("stage"),
            ou32("pct"),
            ou64("el"),
            bridge_argv(a, "eta").and_then(|x| x.as_i64()),
            osv("ver"),
            ou64("got"),
            ou64("total"),
        )),
        "upd_float_hide" => {
            upd_float_hide(app.clone());
            Ok(json!(null))
        }
        "pet_status" => Ok(pet_status()),
        "pet_close" => {
            pet_close(app.clone());
            Ok(json!(null))
        }
        "pet_drag_begin" => {
            pet_drag_begin(app.clone());
            Ok(json!(null))
        }
        "pet_pos_save" => {
            pet_pos_save(app.clone());
            Ok(json!(null))
        }
        "pet_set_muted" => Ok(json!(pet_set_muted(bv("on", false)))),
        "pet_set_avatar" => Ok(json!(pet_set_avatar(app.clone(), sv("url")))),
        "away_notice" => {
            away_notice(app.clone(), osv("title"), osv("body"));
            Ok(json!(null))
        }

        // ── 事件补投（P2：IPC 回程失效时前端靠这里拿事件）──
        // after 缺省 0 = 从队列头开始（页面刚加载，拿最近 128 条也无害，前端按 name 分发）
        "x8_poll_events" => Ok(evq::since(ou64("after").unwrap_or(0))),

        // WX 全家族由 wx_cache 自己接管（含 plugins_list / wx_plugin_stop）
        _ => crate::wx_cache::http_dispatch(app, cmd, a),
    }
}
