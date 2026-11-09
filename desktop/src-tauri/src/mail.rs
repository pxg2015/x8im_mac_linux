// 邦信 · 内置邮件（2026-11-07，方案 A）
// ─────────────────────────────────────────────────────────────────────────────
// 口径：
//  1) 支持飞书企业邮 / 腾讯企业邮 / 手动配置（custom，IMAP+SMTP 主机端口自定义，全部 SSL）：
//     飞书：IMAP imap.feishu.cn:993     / SMTP smtp.feishu.cn:465
//     腾讯：IMAP imap.exmail.qq.com:993 / SMTP smtp.exmail.qq.com:465
//     custom：参数落 mail.json，仅隐式 TLS；不支持 STARTTLS(143/587) 与 POP3。
//     只走标准 IMAP/SMTP，不接任何私有 OpenAPI；新增预设只需在 endpoints() 加一支。
//  2) 凭据：邮箱地址落 app_data_dir/mail.json（不含密码）；专用密码进系统凭据库
//     （Windows 凭据管理器 / macOS 钥匙串 / Linux Secret Service），**绝不明文落盘、不进日志**。
//     凭据库不可用时直接报错，不静默降级成明文文件。
//  3) 收信用 BODY.PEEK[]（不标已读），前端打开信后显式 mail_mark_seen 标已读。
//  4) 「撤回」按 A 降级：发前撤销窗口 + 发后一键补发更正，全部在前端实现，本协议层无撤回。
//  5) 新邮件提醒：Rust 后台线程每 30 分钟查一次 UNSEEN（9:00–18:00，启动补查 +
//     固定 0~10 分钟相位抖动），emit "x8mail" 给前端；不混进 IM 的 totalUnread。
//  6) 所有 IMAP/SMTP 都是同步阻塞调用，统一丢进 tauri::async_runtime::spawn_blocking，
//     不占 WebView/主线程。
//  7) 2026-11-08 本地 SQLite 缓存：mail_list 只读缓存（进来秒出清单），mail_sync 负责
//     网络增量同步（UIDVALIDITY + 新增 UID），mail_read 正文缓存优先。凭据仍不落盘。
//  8) 文件夹逻辑名只有 inbox / sent；sent 的实际 IMAP 名由 LIST 自动探测（飞书/腾讯不同）。
//     发信成功后 APPEND 到已发送（失败只记日志，不阻断发信）。
//  9) 开 full_encoding 后旧缓存里的乱码主题不会自愈 → 用 CACHE_VER 强制重建一次。
#![allow(dead_code)]

use crate::mail_cache as cache;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use chrono::Timelike as _;
use mail_parser::MimeHeaders as _;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const KEYRING_SERVICE: &str = "com.x8fde.im.mail";
const EVENT_MAIL: &str = "x8mail";
const MAX_ATTACH_BYTES: usize = 8 * 1024 * 1024; // 单附件 8MB，超过走网页版（避免 IPC 大包）
/// 缓存结构 / 解码口径版本；升级后自动重建，避免旧乱码主题一直留在库里
const CACHE_VER: &str = "20261108a";

/// 服务商参数表。custom 的主机来自 MailCfg，故用 String 而非 &'static str。
struct MailEndpoints {
    imap_host: String,
    imap_port: u16,
    smtp_host: String,
    smtp_port: u16,
    secret_hint: &'static str,
}

/// 归一化：未知值一律回落飞书，保证旧 mail.json（无 provider 字段）仍能正常用。
/// 2026-11-08：放行 custom，否则手动配置会被静默改成飞书主机（P0-1）。
fn norm_provider(p: &str) -> &'static str {
    match p {
        "exmail" => "exmail",
        "custom" => "custom",
        _ => "feishu",
    }
}

/// 登录名：自建邮局 / Exchange 常见「登录名 ≠ 邮箱地址」，留空则用地址登录。
fn login_name(cfg: &MailCfg) -> &str {
    if cfg.login.trim().is_empty() { cfg.addr.as_str() } else { cfg.login.trim() }
}

/// 服务器地址只填域名或 IP（不带协议 / 端口），防止把整段内容塞进 TLS SNI / IMAP 命令。
fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && !h.chars().any(char::is_whitespace)
        && h.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
}

/// 网易系（163 / 126 / yeah / 188）第三方客户端登录后必须先发 IMAP ID（RFC 2971），
/// 否则 SELECT 直接回 "Unsafe Login. Please contact kefu@188.com for help"。
fn needs_imap_id(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    ["163.com", "126.com", "yeah.net", "188.com"].iter().any(|k| h.contains(k))
}

fn endpoints(cfg: &MailCfg) -> MailEndpoints {
    match norm_provider(&cfg.provider) {
        "exmail" => MailEndpoints {
            imap_host: "imap.exmail.qq.com".into(),
            imap_port: 993,
            smtp_host: "smtp.exmail.qq.com".into(),
            smtp_port: 465,
            secret_hint: "客户端授权码",
        },
        "custom" => MailEndpoints {
            imap_host: cfg.imap_host.trim().to_string(),
            imap_port: if cfg.imap_port == 0 { 993 } else { cfg.imap_port },
            smtp_host: cfg.smtp_host.trim().to_string(),
            smtp_port: if cfg.smtp_port == 0 { 465 } else { cfg.smtp_port },
            secret_hint: "密码 / 授权码",
        },
        _ => MailEndpoints {
            imap_host: "imap.feishu.cn".into(),
            imap_port: 993,
            smtp_host: "smtp.feishu.cn".into(),
            smtp_port: 465,
            secret_hint: "专用密码",
        },
    }
}

// ── 数据结构 ────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default, Clone)]
struct MailCfg {
    addr: String,
    #[serde(default)]
    provider: String,
    // 以下 5 项仅 provider == "custom" 时生效；全部 serde(default) 保证旧 mail.json 平滑兼容。
    #[serde(default)]
    imap_host: String,
    #[serde(default)]
    imap_port: u16,
    #[serde(default)]
    smtp_host: String,
    #[serde(default)]
    smtp_port: u16,
    /// 登录名与发件地址不同时填；留空 = 用邮箱地址登录
    #[serde(default)]
    login: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct MailHead {
    pub uid: u32,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub date: String,
    pub seen: bool,
    pub snippet: String,
    pub has_attach: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct MailAttachInfo {
    pub name: String,
    pub size: usize,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct MailBody {
    pub uid: u32,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub date: String,
    pub text: String,
    pub html: String,
    pub message_id: String,
    pub references: String,
    pub attachments: Vec<MailAttachInfo>,
}

#[derive(Deserialize, Clone)]
pub struct MailAttachIn {
    name: String,
    mime: String,
    b64: String,
}

#[derive(Serialize, Clone)]
pub struct MailConfigView {
    addr: String,
    configured: bool,
    provider: String,
    /// 能力位：旧客户端(<=1.6.73)不返回此字段，前端据此隐藏「手动配置」入口，
    /// 避免新 web + 旧客户端时 custom 参数被静默忽略、按飞书主机连接（P0-4）。
    custom_supported: bool,
    imap_host: String,
    imap_port: u16,
    smtp_host: String,
    smtp_port: u16,
    login: String,
}

#[derive(Serialize, Clone)]
pub struct MailAttachment {
    name: String,
    mime: String,
    b64: String,
}

// ── 配置与凭据 ──────────────────────────────────────────────────────────────

fn cfg_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("无法定位应用数据目录：{e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建数据目录失败：{e}"))?;
    Ok(dir.join("mail.json"))
}

fn load_cfg(app: &AppHandle) -> MailCfg {
    let Ok(p) = cfg_path(app) else {
        return MailCfg::default();
    };
    match std::fs::read_to_string(&p) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => MailCfg::default(),
    }
}

fn save_cfg(app: &AppHandle, cfg: &MailCfg) -> Result<(), String> {
    let p = cfg_path(app)?;
    let s = serde_json::to_string(cfg).map_err(|e| e.to_string())?;
    // 先写临时文件再改名，避免半截 JSON
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, s.as_bytes()).map_err(|e| format!("写配置失败：{e}"))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("保存配置失败：{e}"))?;
    Ok(())
}

fn secret_entry(addr: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, addr)
        .map_err(|e| format!("系统凭据库不可用（Linux 需 D-Bus 会话）：{e}"))
}

fn get_secret(addr: &str) -> Result<String, String> {
    secret_entry(addr)?
        .get_password()
        .map_err(|_| "读取邮箱专用密码失败，可能已失效，请重新填写".to_string())
}

/// 取「发件地址 + 专用密码 + 完整配置」，未配置时给出可操作提示。
/// 第三个返回值必须是整份 MailCfg：custom 的主机/端口只存在于这里，
/// 只回传 provider 会让自定义参数在 9 处登录调用里全部丢失（P0-2）。
fn creds(app: &AppHandle) -> Result<(String, String, MailCfg), String> {
    let cfg = load_cfg(app);
    if cfg.addr.trim().is_empty() {
        return Err("尚未配置邮箱，请先在「邮箱设置」选择服务商并填写账号".to_string());
    }
    let secret = get_secret(&cfg.addr)?;
    Ok((cfg.addr.clone(), secret, cfg))
}

// ── IMAP 会话 ───────────────────────────────────────────────────────────────

type ImapSession = imap::Session<native_tls::TlsStream<std::net::TcpStream>>;

fn imap_login(cfg: &MailCfg, secret: &str) -> Result<ImapSession, String> {
    let ep = endpoints(cfg);
    let tls = native_tls::TlsConnector::builder()
        .build()
        .map_err(|e| format!("TLS 初始化失败：{e}"))?;
    // 2026-11-08：imap::connect 内部是 TcpStream::connect + read_greeting，两者都没有超时；
    // 任一环节挂起就会让前端 busy 永不复位（表现为「保存并验证连接」按钮长灰 + 一直加载中）。
    // 这里自建 TCP：6s 连接超时 + 8s 读写超时，IPv4 优先，并把失败定位到具体阶段。
    let mut addrs: Vec<_> = (ep.imap_host.as_str(), ep.imap_port)
        .to_socket_addrs()
        .map_err(|e| format!("解析邮件服务器地址失败（{}）：{e}", ep.imap_host))?
        .collect();
    addrs.sort_by_key(|a| if a.is_ipv4() { 0 } else { 1 });
    let mut tcp = None;
    let mut last_err = String::new();
    for a in &addrs {
        match TcpStream::connect_timeout(a, Duration::from_secs(6)) {
            Ok(s) => { tcp = Some(s); break }
            Err(e) => last_err = e.to_string(),
        }
    }
    let tcp = tcp.ok_or_else(|| {
        format!(
            "连接邮件服务器失败（{}:{}）：{}",
            ep.imap_host,
            ep.imap_port,
            if last_err.is_empty() { "未解析到可用地址".to_string() } else { last_err }
        )
    })?;
    tcp.set_read_timeout(Some(Duration::from_secs(8)))
        .map_err(|e| format!("设置读超时失败：{e}"))?;
    tcp.set_write_timeout(Some(Duration::from_secs(8)))
        .map_err(|e| format!("设置写超时失败：{e}"))?;
    let ssl_stream = tls
        .connect(&ep.imap_host, tcp)
        .map_err(|e| format!("TLS 握手失败（{}）：{e:?}", ep.imap_host))?;
    let mut socket = imap::Client::new(ssl_stream);
    socket
        .read_greeting()
        .map_err(|e| format!("邮件服务器未响应（{}）：{e}", ep.imap_host))?;
    let mut sess = socket.login(login_name(cfg), secret).map_err(|(e, _)| {
        format!(
            "邮箱登录失败（请检查{}，以及管理员是否已开启第三方客户端登录权限）：{e}",
            ep.secret_hint
        )
    })?;
    // 网易系个人邮箱强制要求客户端先自报身份，否则 SELECT 报 Unsafe Login。
    // 服务器不支持 ID 时忽略错误（响应已被消费，不影响后续命令）。
    if needs_imap_id(&ep.imap_host) {
        let _ = sess.run_command_and_check_ok(
            "ID (\"name\" \"BangXin\" \"version\" \"1.6.74\" \"vendor\" \"TianJian\" \"contact\" \"support@x8fde.com\")",
        );
    }
    Ok(sess)
}

/// 从原始邮件头里取一行（不依赖 mail-parser 的 GetHeader trait，兼容折行）
fn raw_header(raw: &[u8], key: &str) -> String {
    let s = String::from_utf8_lossy(raw);
    let mut out = String::new();
    let mut grabbing = false;
    for line in s.lines() {
        let l = line.trim_end_matches('\r');
        if l.is_empty() {
            break;
        }
        if l.starts_with(' ') || l.starts_with('\t') {
            if grabbing {
                out.push(' ');
                out.push_str(l.trim());
            }
            continue;
        }
        if grabbing {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            if k.eq_ignore_ascii_case(key) {
                out = v.trim().to_string();
                grabbing = true;
            }
        }
    }
    out
}

fn addr_text(a: Option<&mail_parser::Address<'_>>) -> String {
    let one = |x: &mail_parser::Addr<'_>| -> String {
        match (x.name.as_deref(), x.address.as_deref()) {
            (Some(n), Some(ad)) if !n.is_empty() => format!("{n} <{ad}>"),
            (_, Some(ad)) => ad.to_string(),
            _ => String::new(),
        }
    };
    match a {
        Some(mail_parser::Address::List(v)) => v
            .iter()
            .map(one)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        Some(mail_parser::Address::Group(g)) => g
            .iter()
            .flat_map(|gr| gr.addresses.iter())
            .map(one)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        None => String::new(),
    }
}

/// 解析一封原始邮件（raw 可以是「只有头」或「完整邮件」）。
/// mail-parser 默认借用入参，这里转成 owned，避免把 raw 的生命周期一路带出去。
fn parse_mail(raw: &[u8]) -> mail_parser::Message<'static> {
    const EMPTY: &[u8] = b"\r\n\r\n";
    let parsed = mail_parser::MessageParser::default().parse(raw);
    match parsed {
        Some(m) => m.into_owned(),
        None => mail_parser::MessageParser::default()
            .parse(EMPTY)
            .expect("empty mail")
            .into_owned(),
    }
}

/// 文件夹逻辑名归一化：前端只传 inbox / sent，未知值一律收件箱
fn norm_folder(f: Option<&str>) -> &'static str {
    if f == Some("sent") { "sent" } else { "inbox" }
}

/// 探测「已发送」的真实 IMAP 名。飞书 / 腾讯企业邮命名不同（Sent / Sent Messages /
/// 中文 UTF-7 &XfJT0ZAB-），优先看 SPECIAL-USE \Sent，其次名字包含 sent / 已发送。
/// 结果写进 mail_meta，避免每次同步都 LIST 一遍。
fn sent_real_name(
    c: &rusqlite::Connection,
    sess: &mut ImapSession,
    addr: &str,
) -> String {
    if let Some(v) = cache::meta_get(c, addr, "sent", "real") {
        if !v.is_empty() {
            return v;
        }
    }
    let mut best = String::new();
    if let Ok(names) = sess.list(Some(""), Some("*")) {
        for n in names.iter() {
            let nm = n.name().to_string();
            let low = nm.to_lowercase();
            let special = n.attributes().iter().any(|a| match a {
                imap::types::NameAttribute::Custom(x) => {
                    x.as_ref().eq_ignore_ascii_case("\\Sent")
                }
                _ => false,
            });
            let looks = special
                || low.contains("sent")
                || low.contains("&xfjt0zab")
                || nm.contains("已发送");
            if looks && best.is_empty() {
                best = nm.clone();
            }
            if special {
                best = nm;
                break;
            }
        }
    }
    if best.is_empty() {
        // LIST 没给线索时按常见名逐个试探（中文文件夹名在 IMAP 里通常是 UTF-7）
        for cand in ["Sent Messages", "Sent", "Sent Items", "&XfJT0ZAB-", "已发送"] {
            if sess.select(cand).is_ok() {
                best = cand.to_string();
                break;
            }
        }
    }
    let real = if best.is_empty() {
        "Sent Messages".to_string()
    } else {
        best
    };
    let _ = cache::meta_set(c, addr, "sent", "real", &real);
    real
}

fn mailbox_name(
    c: &rusqlite::Connection,
    sess: &mut ImapSession,
    addr: &str,
    folder: &str,
) -> String {
    if folder == "sent" {
        sent_real_name(c, sess, addr)
    } else {
        "INBOX".to_string()
    }
}

/// 从完整原始邮件解析出 MailBody（读信与附件共用）
fn body_from_raw(raw: &[u8], uid: u32) -> MailBody {
    let m = parse_mail(raw);
    let mut attachments = Vec::new();
    let mut i = 0u32; // mail-parser 的 attachment 索引是 u32
    while let Some(p) = m.attachment(i) {
        attachments.push(MailAttachInfo {
            name: p.attachment_name().unwrap_or("附件").to_string(),
            size: p.len(),
        });
        i += 1;
    }
    MailBody {
        uid,
        from: addr_text(m.from()),
        to: addr_text(m.to()),
        cc: addr_text(m.cc()),
        subject: m.subject().unwrap_or("(无主题)").to_string(),
        date: m.date().map(|d| d.to_rfc3339()).unwrap_or_default(),
        text: m.body_text(0).map(|s| s.to_string()).unwrap_or_default(),
        html: m.body_html(0).map(|s| s.to_string()).unwrap_or_default(),
        message_id: raw_header(raw, "Message-ID"),
        references: raw_header(raw, "References"),
        attachments,
    }
}

// ── 命令辅助：把同步阻塞调用丢到 blocking 线程 ─────────────────────────────

async fn blocking<F, R>(f: F) -> Result<R, String>
where
    F: FnOnce() -> Result<R, String> + Send + 'static,
    R: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| format!("任务执行失败：{e}"))?
}

// ── Tauri commands ──────────────────────────────────────────────────────────

#[tauri::command]
pub async fn mail_config_get(app: AppHandle) -> Result<MailConfigView, String> {
    blocking(move || {
        let cfg = load_cfg(&app);
        let configured = !cfg.addr.trim().is_empty()
            && secret_entry(&cfg.addr)
                .and_then(|e| e.get_password().map_err(|e| e.to_string()))
                .is_ok();
        let provider = norm_provider(&cfg.provider).to_string();
        let ep = endpoints(&cfg);
        Ok(MailConfigView {
            addr: cfg.addr,
            configured,
            provider,
            custom_supported: true,
            imap_host: cfg.imap_host,
            imap_port: ep.imap_port,
            smtp_host: cfg.smtp_host,
            smtp_port: ep.smtp_port,
            login: cfg.login,
        })
    })
    .await
}

/// provider: "feishu"（默认，兼容未传的旧前端） | "exmail" | "custom"
/// custom 时前端必须带 imap_host / smtp_host；端口缺省 993 / 465；login 缺省 = 邮箱地址。
/// 旧客户端(<=1.6.73)调用时没有这些参数，Option 全为 None，行为与改动前完全一致。
#[tauri::command]
pub async fn mail_config_set(
    app: AppHandle,
    addr: String,
    secret: String,
    provider: Option<String>,
    imap_host: Option<String>,
    imap_port: Option<u16>,
    smtp_host: Option<String>,
    smtp_port: Option<u16>,
    login: Option<String>,
) -> Result<(), String> {
    blocking(move || {
        let addr = addr.trim().to_string();
        if addr.is_empty() || !addr.contains('@') {
            return Err("请填写正确的邮箱地址".to_string());
        }
        let p = norm_provider(provider.as_deref().unwrap_or("feishu")).to_string();
        let next = MailCfg {
            addr,
            provider: p.clone(),
            imap_host: imap_host.unwrap_or_default().trim().to_string(),
            imap_port: imap_port.unwrap_or(993),
            smtp_host: smtp_host.unwrap_or_default().trim().to_string(),
            smtp_port: smtp_port.unwrap_or(465),
            login: login.unwrap_or_default().trim().to_string(),
        };
        if p == "custom" {
            if !valid_host(&next.imap_host) || !valid_host(&next.smtp_host) {
                return Err("请填写 IMAP / SMTP 服务器地址（只填域名或 IP，不带 http:// 和端口）".to_string());
            }
            if next.imap_port == 0 || next.smtp_port == 0 {
                return Err("端口必须是 1–65535".to_string());
            }
        }
        let ep = endpoints(&next);
        if secret.trim().is_empty() {
            return Err(format!("请填写{}（见上方配置引导）", ep.secret_hint));
        }
        // 先真连一次，确认服务商/地址/密码/管理员开关都通，再落盘
        let mut sess = imap_login(&next, &secret)?;
        let _ = sess.select("INBOX").map_err(|e| format!("收件箱不可读：{e}"))?;
        let _ = sess.logout();
        // 手动配置最容易填错 SMTP：保存当时就用 NOOP + AUTH 验证一次，别等发信才发现。
        // 两家预设参数固定，不额外增加一次网络连接，保持原有保存行为。
        if p == "custom" {
            use lettre::transport::smtp::authentication::Credentials;
            use lettre::SmtpTransport;
            let mailer = SmtpTransport::relay(ep.smtp_host.as_str())
                .map_err(|e| format!("SMTP 服务器地址无效：{e}"))?
                .port(ep.smtp_port)
                .credentials(Credentials::new(login_name(&next).to_string(), secret.clone()))
                .timeout(Some(Duration::from_secs(15)))
                .build();
            match mailer.test_connection() {
                Ok(true) => {}
                Ok(false) => return Err("SMTP 连接失败：请检查主机 / 端口 / 授权码".to_string()),
                Err(e) => return Err(format!("SMTP 验证失败（主机 / 端口 / 授权码）：{e}")),
            }
        }
        secret_entry(&next.addr)?
            .set_password(&secret)
            .map_err(|e| format!("写入系统凭据库失败：{e}"))?;
        save_cfg(&app, &next)
    })
    .await
}

#[tauri::command]
pub async fn mail_config_clear(app: AppHandle) -> Result<(), String> {
    blocking(move || {
        let cfg = load_cfg(&app);
        if !cfg.addr.is_empty() {
            let _ = secret_entry(&cfg.addr).and_then(|e| {
                e.delete_password().map_err(|e| e.to_string())
            });
        }
        if let Ok(p) = cfg_path(&app) {
            let _ = std::fs::remove_file(p);
        }
        // 2026-11-08：本地缓存的邮件内容随配置一起清除，避免换账号后残留他人邮件
        cache::clear(&app);
        Ok(())
    })
    .await
}

/// 取回当前邮箱专用密码（明文）。
/// 仅用于「本机 keyring → 服务端加密同步」，不落盘、不进日志。
/// 未配置 / 凭据缺失时返回 null，前端据此跳过密码上行（旧客户端无此命令亦然）。
#[tauri::command]
pub async fn mail_secret_get(app: AppHandle) -> Result<Option<String>, String> {
    blocking(move || {
        let cfg = load_cfg(&app);
        if cfg.addr.trim().is_empty() {
            return Ok(None);
        }
        match secret_entry(&cfg.addr).and_then(|e| e.get_password().map_err(|e| e.to_string())) {
            Ok(p) => Ok(Some(p)),
            Err(_) => Ok(None),
        }
    })
    .await
}

/// 清单：只读本地 SQLite 缓存，不连 IMAP —— 面板打开即出列表（毫秒级）
#[tauri::command]
pub async fn mail_list(
    app: AppHandle,
    limit: Option<u32>,
    folder: Option<String>,
) -> Result<Vec<MailHead>, String> {
    blocking(move || {
        let cfg = load_cfg(&app);
        if cfg.addr.trim().is_empty() {
            return Ok(Vec::new());
        }
        let f = norm_folder(folder.as_deref());
        let lim = limit.unwrap_or(40).clamp(1, 200) as usize;
        let c = cache::open(&app)?;
        cache::list_heads(&c, &cfg.addr, f, lim)
    })
    .await
}

/// 网络增量同步：登录 → UIDVALIDITY 校验 → 只拉新增 UID 的头 → 回写缓存 → 返回最新清单。
/// 前端先 mail_list 出画面，再异步调这里；full=true（点刷新按钮）才做全量 ALL 扫描。
#[tauri::command]
pub async fn mail_sync(
    app: AppHandle,
    limit: Option<u32>,
    folder: Option<String>,
    full: Option<bool>,
) -> Result<Vec<MailHead>, String> {
    blocking(move || {
        let (addr, secret, cfg) = creds(&app)?;
        let f = norm_folder(folder.as_deref()).to_string();
        let lim = limit.unwrap_or(40).clamp(1, 200) as usize;
        let c = cache::open(&app)?;

        let mut sess = imap_login(&cfg, &secret)?;
        let real = mailbox_name(&c, &mut sess, &addr, &f);
        let label = if f == "sent" { "已发送" } else { "收件箱" };
        let mb = match sess.select(&real) {
            Ok(m) => m,
            Err(e) => {
                // 探测到的名字读不了 → 清掉缓存的文件夹名，下次重新 LIST
                let _ = cache::meta_set(&c, &addr, "sent", "real", "");
                return Err(format!("{}不可读（{}）：{}", label, real, e));
            }
        };
        let uv = mb.uid_validity.unwrap_or(0);
        let old_uv = cache::meta_get(&c, &addr, &f, "uidvalidity")
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        let old_ver = cache::meta_get(&c, &addr, &f, "ver");
        let mut cached = cache::cached_uids(&c, &addr, &f)?;

        // UIDVALIDITY 变了 / 缓存版本升级（例如修了 GBK 解码）→ 整夹重建
        let rebuild = (uv != 0 && old_uv != 0 && uv != old_uv)
            || old_ver.as_deref() != Some(CACHE_VER);
        if rebuild {
            let _ = cache::clear_folder(&c, &addr, &f);
            cached.clear();
        }
        let full_scan = full.unwrap_or(false) || cached.is_empty() || rebuild;

        let unseen: HashSet<u32> = sess
            .uid_search("UNSEEN")
            .map_err(|e| format!("查询未读失败：{e}"))?
            .into_iter()
            .collect();

        let server_uids: Vec<u32> = if full_scan {
            sess.uid_search("ALL")
                .map_err(|e| format!("查询邮件失败：{e}"))?
                .into_iter()
                .collect()
        } else {
            let last = cache::max_uid(&c, &addr, &f);
            // UID 序列集 n:* —— 只问「比已缓存最大的还新」的那些，不再全邮箱扫描
            sess.uid_search(format!("{}:*", last.saturating_add(1)))
                .unwrap_or_default()
                .into_iter()
                .collect()
        };

        // 服务端确实已删除的邮件，只在全量扫描时清理（增量模式不误删老缓存）
        if full_scan {
            let server_set: HashSet<u32> = server_uids.iter().copied().collect();
            let stale: Vec<u32> = cached
                .iter()
                .filter(|u| !server_set.contains(u))
                .copied()
                .collect();
            let _ = cache::delete_uids(&c, &addr, &f, &stale);
        }

        let mut target: Vec<u32> = if full_scan {
            server_uids.clone()
        } else {
            cached
                .iter()
                .copied()
                .chain(server_uids.iter().copied())
                .collect()
        };
        target.sort_unstable_by(|a, b| b.cmp(a));
        target.dedup();
        target.truncate(lim);

        let need: Vec<u32> = target
            .iter()
            .copied()
            .filter(|u| !cached.contains(u))
            .collect();
        if !need.is_empty() {
            let set = need
                .iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let query = "(UID BODY.PEEK[HEADER.FIELDS (FROM TO SUBJECT DATE)])";
            let fetches = sess
                .uid_fetch(set.as_str(), query)
                .map_err(|e| format!("读取邮件头失败：{e}"))?;
            let mut out = Vec::new();
            for ft in fetches.iter() {
                let uid = ft.uid.unwrap_or(0);
                if uid == 0 {
                    continue;
                }
                let mut h = ft.header().unwrap_or(&[]).to_vec();
                h.extend_from_slice(b"\r\n\r\n");
                let m = parse_mail(&h);
                let subject = m.subject().unwrap_or("(无主题)").to_string();
                let mut snippet = m
                    .body_text(0)
                    .map(|s| s.chars().take(120).collect::<String>())
                    .unwrap_or_default();
                if snippet.is_empty() {
                    snippet = subject.clone();
                }
                out.push(MailHead {
                    uid,
                    from: addr_text(m.from()),
                    to: addr_text(m.to()),
                    subject,
                    date: m.date().map(|d| d.to_rfc3339()).unwrap_or_default(),
                    seen: !unseen.contains(&uid),
                    snippet,
                    has_attach: false,
                });
            }
            let _ = cache::upsert_heads(&c, &addr, &f, &out);
        }
        let _ = cache::update_seen(&c, &addr, &f, &unseen, &target);
        let _ = cache::meta_set(&c, &addr, &f, "uidvalidity", &uv.to_string());
        let _ = cache::meta_set(&c, &addr, &f, "ver", CACHE_VER);
        let _ = cache::meta_set(&c, &addr, &f, "last_sync", &now_secs().to_string());
        let _ = sess.logout();
        cache::list_heads(&c, &addr, &f, lim)
    })
    .await
}

/// 读信：先查本地正文缓存，命中直接返回；未命中才连 IMAP（BODY.PEEK[] 不改已读态）
#[tauri::command]
pub async fn mail_read(
    app: AppHandle,
    uid: u32,
    folder: Option<String>,
) -> Result<MailBody, String> {
    blocking(move || {
        let (addr, secret, cfg) = creds(&app)?;
        let f = norm_folder(folder.as_deref()).to_string();
        let c = cache::open(&app)?;
        if let Some(b) = cache::body_get(&c, &addr, &f, uid) {
            return Ok(b);
        }
        let mut sess = imap_login(&cfg, &secret)?;
        let real = mailbox_name(&c, &mut sess, &addr, &f);
        sess.select(&real).map_err(|e| format!("打开邮件失败：{e}"))?;
        let fetches = sess
            .uid_fetch(uid.to_string().as_str(), "BODY.PEEK[]")
            .map_err(|e| format!("读取邮件失败：{e}"))?;
        let raw = fetches
            .iter()
            .find(|x| x.uid == Some(uid))
            .and_then(|x| x.body())
            .unwrap_or(&[])
            .to_vec();
        let _ = sess.logout();
        if raw.is_empty() {
            return Err("邮件内容为空或已被删除".to_string());
        }
        let b = body_from_raw(&raw, uid);
        let _ = cache::body_put(&c, &addr, &f, uid, &b);
        Ok(b)
    })
    .await
}

/// 取单个附件的 base64（前端再调 save_bytes 落盘）
#[tauri::command]
pub async fn mail_attachment(
    app: AppHandle,
    uid: u32,
    index: u32,
    folder: Option<String>,
) -> Result<MailAttachment, String> {
    blocking(move || {
        let (addr, secret, cfg) = creds(&app)?;
        let f = norm_folder(folder.as_deref()).to_string();
        let c = cache::open(&app)?;
        let mut sess = imap_login(&cfg, &secret)?;
        let real = mailbox_name(&c, &mut sess, &addr, &f);
        sess.select(&real).map_err(|e| format!("打开邮件失败：{e}"))?;
        let fetches = sess
            .uid_fetch(uid.to_string().as_str(), "BODY.PEEK[]")
            .map_err(|e| format!("读取附件失败：{e}"))?;
        let raw = fetches
            .iter()
            .find(|f| f.uid == Some(uid))
            .and_then(|f| f.body())
            .unwrap_or(&[])
            .to_vec();
        let _ = sess.logout();
        let m = parse_mail(&raw);
        let p = m
            .attachment(index)
            .ok_or_else(|| "附件不存在".to_string())?;
        if p.len() > MAX_ATTACH_BYTES {
            return Err("附件超过 8MB，请到邮箱网页版下载".to_string());
        }
        Ok(MailAttachment {
            name: p.attachment_name().unwrap_or("附件").to_string(),
            mime: "application/octet-stream".to_string(),
            b64: B64.encode(p.contents()),
        })
    })
    .await
}

/// 打开信后显式标已读：先改本地缓存（角标立即消失），再同步到 IMAP
#[tauri::command]
pub async fn mail_mark_seen(
    app: AppHandle,
    uid: u32,
    folder: Option<String>,
) -> Result<(), String> {
    blocking(move || {
        let (addr, secret, cfg) = creds(&app)?;
        let f = norm_folder(folder.as_deref());
        if let Ok(c) = cache::open(&app) {
            cache::set_seen(&c, &addr, f, uid);
        }
        if f != "inbox" {
            return Ok(()); // 已发送里的信 APPEND 时就带 \Seen，不必回写
        }
        let mut sess = imap_login(&cfg, &secret)?;
        sess.select("INBOX").map_err(|e| format!("收件箱不可读：{e}"))?;
        let _ = sess.uid_store(uid.to_string().as_str(), "+FLAGS (\\Seen)");
        let _ = sess.logout();
        Ok(())
    })
    .await
}

/// 发信 / 回复 / 转发。reply_uid / forward_uid 用于自动组装线程头与引用原文。
#[tauri::command]
pub async fn mail_send(
    app: AppHandle,
    to: String,
    cc: String,
    subject: String,
    body: String,
    // 2026-11-08 B：富文本正文（HTML）。前端同时传 body 纯文本版，
    // 旧客户端没有这个参数、会忽略多余字段，自动退回纯文本邮件。
    html: Option<String>,
    reply_uid: Option<u32>,
    forward_uid: Option<u32>,
    attachments: Option<Vec<MailAttachIn>>,
) -> Result<(), String> {
    blocking(move || {
        use lettre::message::header::{ContentDisposition, ContentType};
        use lettre::message::{Mailbox, Message, MultiPart, SinglePart};
        use lettre::transport::smtp::authentication::Credentials;
        use lettre::{SmtpTransport, Transport};

        let (addr, secret, cfg) = creds(&app)?;
        let mut builder = Message::builder()
            .from(addr.parse::<Mailbox>().map_err(|e| format!("发件人无效：{e}"))?);
        for t in to.split([',', ';', '，', '；']) {
            let t = t.trim();
            if !t.is_empty() {
                builder = builder.to(t.parse::<Mailbox>().map_err(|e| format!("收件人无效：{e}"))?);
            }
        }
        for c in cc.split([',', ';', '，', '；']) {
            let c = c.trim();
            if !c.is_empty() {
                builder = builder.cc(c.parse::<Mailbox>().map_err(|e| format!("抄送人无效：{e}"))?);
            }
        }
        // 回复：补 In-Reply-To / References，保持邮件线程
        if let Some(ruid) = reply_uid.or(forward_uid) {
            if ruid > 0 {
                let mut sess = imap_login(&cfg, &secret)?;
                sess.select("INBOX").map_err(|e| format!("收件箱不可读：{e}"))?;
                let fetches = sess
                    .uid_fetch(ruid.to_string().as_str(), "BODY.PEEK[HEADER.FIELDS (MESSAGE-ID REFERENCES)]")
                    .map_err(|e| format!("读取原邮件头失败：{e}"))?;
                let raw = fetches
                    .iter()
                    .find(|f| f.uid == Some(ruid))
                    .and_then(|f| f.header())
                    .unwrap_or(&[])
                    .to_vec();
                let _ = sess.logout();
                let mid = raw_header(&raw, "Message-ID");
                if reply_uid.is_some() && !mid.is_empty() {
                    builder = builder.in_reply_to(mid.clone());
                    let old = raw_header(&raw, "References");
                    let refs = if old.is_empty() {
                        mid
                    } else {
                        format!("{old} {mid}")
                    };
                    builder = builder.references(refs);
                }
            }
        }
        builder = builder.subject(subject);

        let atts = attachments.unwrap_or_default();
        // 2026-11-08 B：有 html 就发 multipart/alternative（纯文本 + HTML 两版），
        // 没有 html 时完全保持原来的单段纯文本结构，旧前端 / 旧客户端零影响。
        let html_body = html.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let html_ct = || {
            ContentType::parse("text/html; charset=utf-8").unwrap_or(ContentType::TEXT_HTML)
        };
        let email = if atts.is_empty() {
            if let Some(h) = html_body.clone() {
                let alt = MultiPart::alternative()
                    .singlepart(
                        SinglePart::builder()
                            .header(ContentType::TEXT_PLAIN)
                            .body(body.clone()),
                    )
                    .singlepart(SinglePart::builder().header(html_ct()).body(h));
                builder
                    .multipart(alt)
                    .map_err(|e| format!("组装邮件失败：{e}"))?
            } else {
                builder
                    .header(ContentType::TEXT_PLAIN)
                    .body(body.clone())
                    .map_err(|e| format!("组装邮件失败：{e}"))?
            }
        } else {
            let mut mixed = if let Some(h) = html_body {
                MultiPart::mixed().multipart(
                    MultiPart::alternative()
                        .singlepart(
                            SinglePart::builder()
                                .header(ContentType::TEXT_PLAIN)
                                .body(body.clone()),
                        )
                        .singlepart(SinglePart::builder().header(html_ct()).body(h)),
                )
            } else {
                MultiPart::mixed().singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_PLAIN)
                        .body(body.clone()),
                )
            };
            for a in atts {
                let bytes = B64
                    .decode(a.b64.as_bytes())
                    .map_err(|e| format!("附件解码失败：{e}"))?;
                let ct = ContentType::parse(&a.mime)
                    .unwrap_or_else(|_| ContentType::parse("application/octet-stream").unwrap());
                let part = SinglePart::builder()
                    .header(ct)
                    .header(ContentDisposition::attachment(a.name.as_str()))
                    .body(bytes);
                mixed = mixed.singlepart(part);
            }
            builder
                .multipart(mixed)
                .map_err(|e| format!("组装邮件失败：{e}"))?
        };

        let raw = email.formatted();
        let ep = endpoints(&cfg);
        let mailer = SmtpTransport::relay(ep.smtp_host.as_str())
            .map_err(|e| format!("SMTP 配置失败：{e}"))?
            .port(ep.smtp_port)
            // clone：下面还要用同一份凭据把原文 APPEND 到「已发送」
            .credentials(Credentials::new(login_name(&cfg).to_string(), secret.clone()))
            .timeout(Some(Duration::from_secs(30)))
            .build();
        mailer
            .send(&email)
            .map_err(|e| format!("发送失败（请检查{} / 网络）：{e}", ep.secret_hint))?;

        // 2026-11-08：投递成功后把原文 APPEND 到「已发送」，本机与网页版都能看到。
        // 这一步失败只静默降级，绝不回滚已成功发出的信。
        if let Ok(mut sess) = imap_login(&cfg, &secret) {
            if let Ok(c) = cache::open(&app) {
                let real = sent_real_name(&c, &mut sess, &addr);
                if sess
                    .append_with_flags(&real, &raw, &[imap::types::Flag::Seen])
                    .is_err()
                {
                    // 探测到的名字不可写时再试常见名，并让下次同步重新探测
                    let _ = sess.append_with_flags(
                        "Sent Messages",
                        &raw,
                        &[imap::types::Flag::Seen],
                    );
                    let _ = cache::meta_set(&c, &addr, "sent", "real", "");
                }
                // 清掉已发送缓存，下次切过去由 mail_sync 重拉，新信就会出现
                let _ = cache::clear_folder(&c, &addr, "sent");
            }
            let _ = sess.logout();
        }
        Ok(())
    })
    .await
}

/// 未读数：只读本地缓存（前端挂载不再为角标多连一次 IMAP）
#[tauri::command]
pub async fn mail_unseen(app: AppHandle) -> Result<u32, String> {
    blocking(move || {
        let cfg = load_cfg(&app);
        if cfg.addr.trim().is_empty() {
            return Ok(0);
        }
        let c = cache::open(&app)?;
        Ok(cache::unseen_cached(&c, &cfg.addr))
    })
    .await
}

/// 后台轮询用：真实查服务器（30 分钟一次，不占面板打开路径）
fn unseen_count(app: &AppHandle) -> Result<u32, String> {
    let (_, secret, cfg) = creds(app)?;
    let mut sess = imap_login(&cfg, &secret)?;
    sess.select("INBOX").map_err(|e| format!("收件箱不可读：{e}"))?;
    let set: std::collections::HashSet<u32> = sess
        .uid_search("UNSEEN")
        .map_err(|e| format!("查询未读失败：{e}"))?
        .into_iter()
        .collect();
    let n = set.len() as u32;
    let _ = sess.logout();
    Ok(n)
}

// ── 后台新邮件轮询 ──────────────────────────────────────────────────────────

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn in_work_window() -> bool {
    // chrono::Local 已按本机时区取小时；9:00–18:00（含 9 点，不含 18 点之后）
    let h = chrono::Local::now().hour();
    (9..18).contains(&h)
}

fn emit_unseen(app: &AppHandle, n: u32, reason: &str) {
    let payload = serde_json::json!({ "unseen": n, "reason": reason });
    let _ = app.emit(EVENT_MAIL, payload);
}

/// 在 setup() 里调用一次。固定相位（0~10min）+ 每 30 分钟一次 + 启动补查 + 失败退避。
pub fn spawn_poller(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last: Option<u32> = None;
        let mut fail_streak: u32 = 0;
        // 启动时先补查一次（覆盖 18:00 → 次日 9:00 的积压）
        poll_once(&app, &mut last, &mut fail_streak);
        // 固定相位：按启动时刻取 0~600s，之后每轮固定 30 分钟，不让相邻间隔缩到 10 分钟
        let phase = now_secs() % 600;
        std::thread::sleep(Duration::from_secs(phase));
        loop {
            std::thread::sleep(Duration::from_secs(30 * 60));
            if in_work_window() {
                poll_once(&app, &mut last, &mut fail_streak);
            }
        }
    });
}

fn poll_once(app: &AppHandle, last: &mut Option<u32>, fail_streak: &mut u32) {
    match unseen_count(app) {
        Ok(n) => {
            *fail_streak = 0;
            // 只在数量变化或首次时推，避免每 30 分钟骚扰一次
            if last.map(|v| v != n).unwrap_or(true) {
                emit_unseen(app, n, "poll");
                *last = Some(n);
            }
        }
        Err(_) => {
            // 凭据没配 / 网络断：静默跳过，指数退避，不弹错、不紧连重试
            *fail_streak = (*fail_streak + 1).min(6);
        }
    }
    let _ = fail_streak; // 退避留给后续接入；当前固定间隔已足够稀疏
}

// 2026-11-08 回归测试：GBK / GB2312 主题必须解出中文，不能变成 U+FFFD（界面上的实心 ◆）。
// 这条测试同时守住 Cargo.toml 里的 full_encoding feature —— 删了它测试立刻失败。
#[cfg(test)]
mod encoding_tests {
    use super::*;

    #[test]
    fn gbk_subject_is_readable() {
        // "中文测试" 的 GBK 字节
        let gbk: &[u8] = &[0xD6, 0xD0, 0xCE, 0xC4, 0xB2, 0xE2, 0xCA, 0xD4];
        let raw = format!("Subject: =?GBK?B?{}?=\r\n\r\n", B64.encode(gbk));
        let m = parse_mail(raw.as_bytes());
        let s = m.subject().unwrap_or("").to_string();
        assert!(!s.contains('\u{fffd}'), "主题仍含替换符：{s}");
        assert!(s.contains("中文测试"), "主题未解出中文：{s}");
    }
}
