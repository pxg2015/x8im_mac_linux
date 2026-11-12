// 邦信 · 本地微信消息库（wx_* 表，与邮件同处 app_data_dir/mail.db）
// ─────────────────────────────────────────────────────────────
// 2026-11-10 老8 定：本地微信消息数据并入邦信自己的 sqlite，不再单独一个文件。
// 同一库、同一套打开方式（WAL + busy_timeout 短连接，照 mail_cache.rs），
// 只是表名统一 wx_ 前缀，与 mail_* 互不干扰。
//
// 分工：
//   插件（Go）写 wx_messages / wx_contacts / wx_customers / wx_status
//   客户端（Rust）写 wx_follow（关注名单），并读其它四张表渲染界面
// 两边都是短连接 + busy_timeout，WAL 下并发安全。
//
// 表结构由插件 InitArchive 建（CREATE TABLE IF NOT EXISTS），这里不建表，
// 只做查询；插件没起过时表不存在 → 查询返回空而不是报错（界面显示「未同步过」）。

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

pub fn db_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("无法定位应用数据目录：{e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建数据目录失败：{e}"))?;
    Ok(dir.join("mail.db"))
}

fn open(app: &AppHandle) -> Result<Connection, String> {
    let p = db_path(app)?;
    let c = Connection::open(&p).map_err(|e| format!("打开本地库失败：{e}"))?;
    // 5s：插件一轮可能连写几千条，读要能等。
    let _ = c.busy_timeout(std::time::Duration::from_secs(5));
    Ok(c)
}

fn has_table(c: &Connection, name: &str) -> bool {
    c.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        params![name],
        |_| Ok(()),
    )
    .optional()
    .ok()
    .flatten()
    .is_some()
}

// ── 数据结构 ────────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
pub struct WxConversation {
    pub talker: String,
    pub name: String,
    pub is_group: bool,
    pub customer: String,
    pub last_at: i64,
    pub last_text: String,
    pub count: i64,
    /// 微信头像 URL（wx_contacts.avatar_url）；为空时前端回落首字色块。
    pub avatar_url: String,
}

#[derive(Serialize, Clone)]
pub struct WxMessage {
    pub id: i64,
    pub talker: String,
    pub talker_name: String,
    pub is_group: bool,
    pub sender_id: i64,
    pub kind: String,
    pub text: String,
    pub create_time: i64,
    pub media_path: String,
}

#[derive(Serialize, Clone)]
pub struct WxContact {
    pub wxid: String,
    pub display_name: String,
    pub is_group: bool,
    pub followed: bool,
    pub avatar_url: String,
    /// 关联的客户名（wx_customers.customer_name），未关联为空串。
    pub customer: String,
}

#[derive(Serialize, Clone, Default)]
pub struct WxStatusView {
    pub last_at: i64,
    pub last_count: i64,
    pub last_error: String,
    pub follow_count: i64,
    pub msg_count: i64,
    pub bound_wxid: String,
    /// 本机微信是否在跑。None = 不知道（插件没起，问不到），false = 明确没开。
    /// 前端只在明确 false 时提示「微信未打开」，避免插件没起时误报。
    pub wechat_running: Option<bool>,
    /// 最近一轮采集的密钥结果（插件 /api/status 的 key_status）：
    /// "ok" = 密钥可用；"error" = 本轮失败（含密钥获取失败/解密失败）；
    /// 空串 = 插件刚起、还没跑过一轮，前端按「未知」处理。
    /// 2026-11-12：UI 靠它把「系统权限都齐、只是密钥没取到」单独识别出来，
    /// 才能把 mac 三步向导按「密钥失效」重新顶出来（见 WxMsgPanel.vue）。
    pub key_status: String,
    /// 密钥来源/熔断说明（复用缓存 / 已重新获取 / 熔断中），只用于展示。
    pub key_note: String,
}

// ── 会话列表（按最新消息时间倒序）────────────────────────────────────────────

pub fn conversations(app: &AppHandle, limit: i64) -> Result<Vec<WxConversation>, String> {
    let c = open(app)?;
    if !has_table(&c, "wx_messages") {
        return Ok(vec![]);
    }
    let lim = if limit <= 0 { 200 } else { limit.min(1000) };
    // wx_customers 由插件建表；插件还没起过时表可能不存在，动态拼 join 避免整条 SQL 报错。
    let has_cust = has_table(&c, "wx_customers");
    let join_cust = if has_cust {
        " LEFT JOIN wx_customers cc ON cc.wxid = m.talker"
    } else {
        ""
    };
    let cust_col = if has_cust {
        "COALESCE(cc.customer_name,'')"
    } else {
        "''"
    };
    let sql = format!(
        // 2026-11-10 修正：wx_contacts 没有 customer_name 列（只有 alias/nick_name/remark/
        // display_name），原来写 ct.customer_name 会让整条 SQL 报错 → 前端 catch 吞掉 →
        // 会话列表永远空。备注优先级：remark(自己起的备注) > display_name(插件算好的显示名) > nick_name(微信昵称)。
        // 2026-11-10：末列补头像 URL，前端有图出图、无图回落首字色块。
        "SELECT m.talker,
                COALESCE(NULLIF(ct.remark,''), NULLIF(ct.display_name,''),
                         NULLIF(ct.nick_name,''), m.talker) AS nm,
                COALESCE(ct.is_group,0) AS is_group,
                {cust_col} AS customer,
                MAX(m.create_time) AS last_at,
                COUNT(*) AS cnt,
                COALESCE(MAX(ct.avatar_url),'') AS avatar
           FROM wx_messages m
           LEFT JOIN wx_contacts ct ON ct.wxid = m.talker
           {join_cust}
          GROUP BY m.talker
          ORDER BY last_at DESC
          LIMIT {lim}",
        cust_col = cust_col,
        join_cust = join_cust,
        lim = lim,
    );
    let mut st = c.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| {
            Ok(WxConversation {
                talker: r.get(0)?,
                name: r.get(1)?,
                is_group: r.get::<_, i64>(2)? != 0,
                customer: r.get(3)?,
                last_at: r.get(4)?,
                last_text: String::new(),
                count: r.get(5)?,
                avatar_url: r.get(6)?,
            })
        })
        .map_err(|e| e.to_string())?;
    let mut out: Vec<WxConversation> = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    // 补最后一条消息正文（会话列表要显示摘要）：一条 SQL 取每会话最新一行。
    for cv in out.iter_mut() {
        let t: Option<String> = c
            .query_row(
                "SELECT content FROM wx_messages WHERE talker=?1 ORDER BY create_time DESC, id DESC LIMIT 1",
                params![cv.talker],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        cv.last_text = t.map(|s| clean_text(&s)).unwrap_or_default();
    }
    Ok(out)
}

// ── 消息列表 ────────────────────────────────────────────────────────────────

/// 单页消息。`talker` 为空 = 全部会话；`q` 非空 = 全文模糊（内容 / 会话名）。
/// `since` > 0 = 只取 create_time >= since 的增量（页面 60s 轮询用，最多 500 条）。
/// 用 >= 而不是 >：微信消息时间是秒级，同一秒可能有多条，用 > 会漏；
/// 前端按 id 去重消化边界重复。
pub fn messages(
    app: &AppHandle,
    talker: &str,
    q: &str,
    before: i64,
    limit: i64,
    since: i64,
) -> Result<Vec<WxMessage>, String> {
    let c = open(app)?;
    if !has_table(&c, "wx_messages") {
        return Ok(vec![]);
    }
    let lim = if limit <= 0 { 100 } else { limit.min(500) };
    let mut sql = String::from(
        // 2026-11-10 修正：插件 INSERT 时 sender_id 传的是 nil（archive.go:1206），
        // 全表都是 NULL；原来直接 r.get::<i64>(4) 会让每一行读取失败，而
        // rows.flatten() 又把失败静默丢掉 → 前端拿到空数组显示「没有消息」。
        // 这里统一 COALESCE 成 0，content 也兜底空串，避免以后再被 NULL 卡住。
        "SELECT m.id, m.talker,
                COALESCE(NULLIF(ct.display_name,''), m.talker) AS nm,
                COALESCE(ct.is_group,0), COALESCE(m.sender_id,0), m.local_type,
                COALESCE(m.content,''),
                m.create_time, COALESCE(m.media_path,'')
           FROM wx_messages m LEFT JOIN wx_contacts ct ON ct.wxid=m.talker WHERE 1=1",
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if !talker.is_empty() {
        sql.push_str(" AND m.talker=?");
        args.push(Box::new(talker.to_string()));
    }
    if before > 0 {
        sql.push_str(" AND m.create_time<?");
        args.push(Box::new(before));
    }
    if since > 0 {
        sql.push_str(" AND m.create_time>=?");
        args.push(Box::new(since));
    }
    if !q.trim().is_empty() {
        sql.push_str(" AND m.content LIKE ?");
        args.push(Box::new(format!("%{}%", q.trim())));
    }
    sql.push_str(&format!(
        " ORDER BY m.create_time DESC, m.id DESC LIMIT {lim}"
    ));

    let mut st = c.prepare(&sql).map_err(|e| e.to_string())?;
    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let rows = st
        .query_map(refs.as_slice(), |r| {
            let raw: String = r.get(6)?;
            let (kind, text) = classify(&raw, &r.get::<_, String>(8)?);
            Ok(WxMessage {
                id: r.get(0)?,
                talker: r.get(1)?,
                talker_name: r.get(2)?,
                is_group: r.get::<_, i64>(3)? != 0,
                sender_id: r.get(4)?,
                kind,
                text,
                create_time: r.get(7)?,
                media_path: r.get(8)?,
            })
        })
        .map_err(|e| e.to_string())?;
    // 不再用 rows.flatten()：它把「某行字段读不出来」悄悄吞掉，界面只显示空列表，
    // 排查极难。改成整批 Result，任何一行有问题都把错误返回给前端。
    Ok(rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?)
}

// ── 关注名单 ────────────────────────────────────────────────────────────────

pub fn follow_list(app: &AppHandle) -> Result<Vec<String>, String> {
    let c = open(app)?;
    if !has_table(&c, "wx_follow") {
        return Ok(vec![]);
    }
    let mut st = c.prepare("SELECT wxid FROM wx_follow ORDER BY added_at").map_err(|e| e.to_string())?;
    let rows = st.query_map([], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

/// 覆盖写关注名单（客户端是唯一写方）。
pub fn follow_set(app: &AppHandle, list: &[String]) -> Result<usize, String> {
    let mut c = open(app)?;
    if !has_table(&c, "wx_follow") {
        // 插件还没起过：建最小表，避免用户点了关注却没地方存。
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS wx_follow(
                wxid TEXT PRIMARY KEY, added_at INTEGER NOT NULL DEFAULT 0);",
        )
        .map_err(|e| format!("初始化关注表失败：{e}"))?;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let tx = c.transaction().map_err(|e| e.to_string())?;
    tx.execute("DELETE FROM wx_follow", []).map_err(|e| e.to_string())?;
    for w in list {
        let w = w.trim();
        if w.is_empty() {
            continue;
        }
        tx.execute(
            "INSERT OR IGNORE INTO wx_follow(wxid,added_at) VALUES(?1,?2)",
            params![w, now],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(list.len())
}

// ── 客户关联（wx_customers，与插件 /api/customer/set 同口径）─────────────────

/// 已有客户名去重字典，供「设客户」输入框联想 / 复用，避免同一个客户写错好几种叫法。
pub fn customer_list(app: &AppHandle) -> Result<Vec<String>, String> {
    let c = open(app)?;
    if !has_table(&c, "wx_customers") {
        return Ok(vec![]);
    }
    let mut st = c
        .prepare(
            "SELECT DISTINCT TRIM(customer_name) FROM wx_customers
              WHERE TRIM(customer_name)<>'' ORDER BY 1",
        )
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    Ok(rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?)
}

/// 给某个会话（好友/群）关联客户名；传空串 = 取消关联。
/// 插件是这张表的原作者，客户端这里只做 UPSERT / DELETE，表结构与 archive.go:52 保持一致。
pub fn customer_set(app: &AppHandle, wxid: &str, name: &str) -> Result<(), String> {
    let c = open(app)?;
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS wx_customers(
            wxid TEXT PRIMARY KEY, customer_name TEXT NOT NULL DEFAULT '');
         CREATE INDEX IF NOT EXISTS idx_cc_name ON wx_customers(customer_name);",
    )
    .map_err(|e| format!("初始化客户表失败：{e}"))?;
    let n = name.trim();
    if wxid.trim().is_empty() {
        return Err("会话 ID 为空".into());
    }
    if n.is_empty() {
        c.execute("DELETE FROM wx_customers WHERE wxid=?1", params![wxid.trim()])
            .map_err(|e| format!("取消客户关联失败：{e}"))?;
    } else {
        c.execute(
            "INSERT INTO wx_customers(wxid,customer_name) VALUES(?1,?2)
             ON CONFLICT(wxid) DO UPDATE SET customer_name=excluded.customer_name",
            params![wxid.trim(), n],
        )
        .map_err(|e| format!("保存客户关联失败：{e}"))?;
    }
    Ok(())
}

// ── 联系人（供「设置关注」挑选）──────────────────────────────────────────────

/// 联系人分页。`offset` 用于「关注设置」弹层的懒加载（本机 2000+ 个会话，
/// 一次全拉会卡住渲染线程，所以每页 120 条、滚到底再要下一页）。
pub fn contacts(
    app: &AppHandle,
    q: &str,
    only_followed: bool,
    kind: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<WxContact>, String> {
    let c = open(app)?;
    if !has_table(&c, "wx_contacts") {
        return Ok(vec![]);
    }
    let lim = if limit <= 0 { 120 } else { limit.min(1000) };
    let off = if offset < 0 { 0 } else { offset };
    // 2026-11-10：补头像 + 客户名（与插件 /api/conversations 同口径）。
    // 表可能尚未由插件建出，动态拼 join，避免老库上整条 SQL 报错。
    let has_cust = has_table(&c, "wx_customers");
    let join_cust = if has_cust {
        " LEFT JOIN wx_customers cc ON cc.wxid=ct.wxid"
    } else {
        ""
    };
    let cust_col = if has_cust {
        "COALESCE(cc.customer_name,'')"
    } else {
        "''"
    };
    let mut sql = format!(
        "SELECT ct.wxid, COALESCE(NULLIF(ct.display_name,''), ct.wxid) AS nm, COALESCE(ct.is_group,0),
                CASE WHEN f.wxid IS NULL THEN 0 ELSE 1 END,
                COALESCE(ct.avatar_url,''),
                {cust_col}
           FROM wx_contacts ct LEFT JOIN wx_follow f ON f.wxid=ct.wxid
           {join_cust} WHERE 1=1",
        cust_col = cust_col,
        join_cust = join_cust,
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if !q.trim().is_empty() {
        sql.push_str(" AND (ct.display_name LIKE ? OR ct.remark LIKE ? OR ct.wxid LIKE ?");
        if has_cust {
            sql.push_str(" OR COALESCE(cc.customer_name,'') LIKE ?");
        }
        sql.push_str(")");
        let like = format!("%{}%", q.trim());
        args.push(Box::new(like.clone()));
        args.push(Box::new(like.clone()));
        args.push(Box::new(like.clone()));
        if has_cust {
            args.push(Box::new(like));
        }
    }
    if only_followed {
        sql.push_str(" AND f.wxid IS NOT NULL");
    }
    // 2026-11-10 B 方案：群/好友在 SQL 层过滤，与「只看已关注」可任意叠加。
    // 放前端整页筛会让页内条数虚低、分页错乱（老8 反馈的根因之一）。
    match kind {
        "群" => sql.push_str(" AND COALESCE(ct.is_group,0)=1"),
        "好友" => sql.push_str(" AND COALESCE(ct.is_group,0)=0"),
        _ => {}
    }
    sql.push_str(&format!(
        " ORDER BY f.wxid IS NOT NULL DESC, ct.is_group DESC, nm LIMIT {lim} OFFSET {off}"
    ));
    let mut st = c.prepare(&sql).map_err(|e| e.to_string())?;
    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let rows = st
        .query_map(refs.as_slice(), |r| {
            Ok(WxContact {
                wxid: r.get(0)?,
                display_name: r.get(1)?,
                is_group: r.get::<_, i64>(2)? != 0,
                followed: r.get::<_, i64>(3)? != 0,
                avatar_url: r.get(4)?,
                customer: r.get(5)?,
            })
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

// ── 同步状态 ────────────────────────────────────────────────────────────────

pub fn status(app: &AppHandle) -> Result<WxStatusView, String> {
    let c = open(app)?;
    let mut st = WxStatusView::default();
    if has_table(&c, "wx_status") {
        let _ = c.query_row(
            "SELECT last_at,last_count,last_error,follow_count,msg_count,bound_wxid FROM wx_status WHERE id=1",
            [],
            |r| {
                st.last_at = r.get(0)?;
                st.last_count = r.get(1)?;
                st.last_error = r.get(2)?;
                st.follow_count = r.get(3)?;
                st.msg_count = r.get(4)?;
                st.bound_wxid = r.get(5)?;
                Ok(())
            },
        );
    }
    if st.msg_count == 0 && has_table(&c, "wx_messages") {
        st.msg_count = c
            .query_row("SELECT count(*) FROM wx_messages", [], |r| r.get(0))
            .unwrap_or(0);
    }
    if st.follow_count == 0 && has_table(&c, "wx_follow") {
        st.follow_count = c
            .query_row("SELECT count(*) FROM wx_follow", [], |r| r.get(0))
            .unwrap_or(0);
    }
    // 微信进程是否在跑：库里的 last_error 是「上一轮同步时」的快照，可能几分钟前；
    // 状态条要立刻反映「微信刚被关掉/刚打开」，所以直接问插件一句。
    if crate::wx_sidecar::running() {
        if let Ok(raw) = crate::wx_sidecar::http_get("/api/status", 1500) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                st.wechat_running = v.get("wechat_running").and_then(|x| x.as_bool());
                // 2026-11-12：密钥结果透传给界面。wx_status 表里没有这两列（插件不改表结构），
                // 只能问插件内存里的最近一轮状态；插件重启后为空 = 未知，不据此弹向导。
                st.key_status = v
                    .get("key_status")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                st.key_note = v
                    .get("key_note")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
            }
        }
    }
    Ok(st)
}

// ── 文本清洗 ────────────────────────────────────────────────────────────────

/// 去掉微信 XML 包裹与「昵称:\n」前缀，返回可读文本。
/// 与插件 archive.ExtractText / classifyMessage 的口径一致（简版）：
/// 客户端只负责列表展示，媒体型消息统一显示占位文案。
fn clean_text(raw: &str) -> String {
    let s = raw.trim();
    if !s.starts_with('<') {
        return truncate(strip_prefix(s), 200);
    }
    let kind = if s.contains("<img") {
        "[图片]"
    } else if s.contains("<videomsg") {
        "[视频]"
    } else if s.contains("<voicemsg") {
        "[语音]"
    } else if s.contains("<emoji") {
        "[表情]"
    } else if s.contains("<appmsg") {
        "[链接/文件]"
    } else if s.contains("<location") {
        "[位置]"
    } else if s.contains("<sysmsg") {
        "[系统消息]"
    } else {
        "[消息]"
    };
    kind.to_string()
}

fn classify(raw: &str, media_path: &str) -> (String, String) {
    let s = raw.trim();
    if !s.starts_with('<') {
        return ("text".to_string(), truncate(strip_prefix(s), 4000));
    }
    // 媒体类只给占位文案：原件在本机微信目录，进度里不展示也不上传。
    let _ = media_path;
    let (kind, label) = if s.contains("<img") {
        ("image", "[图片]")
    } else if s.contains("<videomsg") {
        ("video", "[视频]")
    } else if s.contains("<voicemsg") {
        ("voice", "[语音]")
    } else if s.contains("<emoji") {
        ("emoji", "[表情]")
    } else if s.contains("<appmsg") {
        if s.contains("<type>6<") {
            ("file", "[文件]")
        } else if s.contains("<type>57<") {
            ("quote", "[引用]")
        } else {
            ("link", "[链接]")
        }
    } else if s.contains("<location") {
        ("location", "[位置]")
    } else if s.contains("<sysmsg") {
        ("system", "[系统消息]")
    } else {
        ("other", "[消息]")
    };
    (kind.to_string(), label.to_string())
}

/// 微信群聊文本会带「发送者昵称:\n正文」前缀，展示时压成单行并保留昵称。
fn strip_prefix(s: &str) -> String {
    let one_line = s.replace(['\r', '\n'], " ");
    one_line.trim().to_string()
}

fn truncate(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    s.chars().take(max).collect::<String>() + "…"
}

// ── Tauri commands ──────────────────────────────────────────────────────────
//
// 四处同步（项目口径）：① 本文件 #[tauri::command] ② main.rs generate_handler!
// ③ build.rs AppManifest ④ capabilities/remote.json

async fn blocking<F, R>(f: F) -> Result<R, String>
where
    F: FnOnce() -> Result<R, String> + Send + 'static,
    R: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| format!("任务执行失败：{e}"))?
}

/// 已安装插件清单（通用，与具体插件无关）。
/// 2026-11-10：将来前端按 capabilities 决定显示哪些页签；现在只有 wx 一个，
/// WxMsgPanel 仍是硬编码页签，这里先把入口留着。
#[tauri::command]
pub fn plugins_list() -> serde_json::Value {
    serde_json::json!({ "items": crate::plugin_host::list() })
}

/// 插件是否随包存在 + 是否在跑 + 组件版本状态。
/// 2026-11-10：前端判据改为 available || auto_update —— 没随包插件的新客户端
/// 也要显示 W消息页签，否则「打开页面自动下载」永远没机会触发。
#[tauri::command]
pub fn wx_plugin_info(app: tauri::AppHandle) -> serde_json::Value {
    let mut v = serde_json::json!({
        "available": crate::wx_sidecar::available(),
        "running": crate::wx_sidecar::running(),
        "last_error": crate::wx_sidecar::last_error(),
    });
    let extra = crate::wx_updater::info(&app);
    if let (Some(o), Some(src)) = (v.as_object_mut(), extra.as_object()) {
        for (k, val) in src {
            o.insert(k.clone(), val.clone());
        }
    }
    v
}

/// 打开 W消息时调一次：缺件自动安装、有新版自动更新，然后拉起插件。
/// 每次打开都查版本清单（微信大版本换密钥后要能立刻推），失败后 60s 冷却；
/// 断网 / CDN 故障时保留旧版并照常启动，绝不阻断页面。
#[tauri::command]
pub async fn wx_plugin_prepare(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    blocking(move || crate::wx_updater::prepare(&app, false)).await
}

/// 页面「更新组件」按钮：跳过节流强制复查并安装。
#[tauri::command]
pub async fn wx_plugin_update_now(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    blocking(move || crate::wx_updater::prepare(&app, true)).await
}

/// 组件逃生口（2026-11-12）：新版起不来 / 新版本身有问题时，手动退回上一版。
/// 宿主不再自动回退（自动回退会掩盖问题、让用户以为「一直没更新成功」），
/// 由用户显式点「回退到上一版」；这里顺带解除该版本的封禁记录。
#[tauri::command]
pub async fn wx_plugin_rollback(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    blocking(move || crate::wx_updater::rollback_to_prev(&app)).await
}

/// 确保插件已启动（点「Wx消息」页签时调一次；已启动则秒回）。
#[tauri::command]
pub async fn wx_plugin_start(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    blocking(move || {
        let port = crate::wx_sidecar::ensure_started(&app)?;
        // 握手：比对契约版本，不匹配只提示、不阻塞
        let ver = crate::wx_sidecar::http_get("/api/version", 1500).unwrap_or_default();
        let cv = serde_json::from_str::<serde_json::Value>(&ver)
            .ok()
            .and_then(|v| v.get("contract_version").and_then(|x| x.as_u64()))
            .unwrap_or(0);
        Ok(serde_json::json!({ "port": port, "contract_version": cv, "expect": 1 }))
    })
    .await
}

/// 停止插件（默认不用，退出时由 RunEvent::Exit 统一收；留给排障）。
#[tauri::command]
pub fn wx_plugin_stop() {
    // 只停 wx，别误伤其它插件（stop_all 是应用退出才用的）
    crate::plugin_host::get("wx").stop();
}

/// 会话列表（按最新消息倒序）。
#[tauri::command]
pub async fn wx_conversations(app: tauri::AppHandle, limit: Option<i64>) -> Result<Vec<WxConversation>, String> {
    blocking(move || conversations(&app, limit.unwrap_or(200))).await
}

/// 消息列表（可按会话 / 关键词 / 时间游标过滤）。
#[tauri::command]
pub async fn wx_messages(
    app: tauri::AppHandle,
    talker: Option<String>,
    q: Option<String>,
    before: Option<i64>,
    limit: Option<i64>,
    since: Option<i64>,
) -> Result<Vec<WxMessage>, String> {
    blocking(move || {
        messages(
            &app,
            &talker.unwrap_or_default(),
            &q.unwrap_or_default(),
            before.unwrap_or(0),
            limit.unwrap_or(100),
            since.unwrap_or(0),
        )
    })
    .await
}

/// 联系人 / 群（供「设置关注」挑选）。
#[tauri::command]
pub async fn wx_contacts(
    app: tauri::AppHandle,
    q: Option<String>,
    only_followed: Option<bool>,
    kind: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Vec<WxContact>, String> {
    blocking(move || {
        contacts(
            &app,
            &q.unwrap_or_default(),
            only_followed.unwrap_or(false),
            &kind.unwrap_or_default(),
            limit.unwrap_or(120),
            offset.unwrap_or(0),
        )
    })
    .await
}

/// 读关注名单。
#[tauri::command]
pub async fn wx_follow_get(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    blocking(move || follow_list(&app)).await
}

/// 写关注名单（覆盖式）。
#[tauri::command]
pub async fn wx_follow_set(app: tauri::AppHandle, list: Vec<String>) -> Result<usize, String> {
    blocking(move || follow_set(&app, &list)).await
}

/// 客户名去重字典（「设客户」输入框联想用）。
#[tauri::command]
pub async fn wx_customer_list(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    blocking(move || customer_list(&app)).await
}

/// 关联 / 取消关联会话的客户名（name 为空 = 取消关联）。
#[tauri::command]
pub async fn wx_customer_set(
    app: tauri::AppHandle,
    wxid: String,
    name: String,
) -> Result<(), String> {
    blocking(move || customer_set(&app, &wxid, &name)).await
}

/// 同步状态（上次同步时间 / 本轮新增 / 本地总条数 / 错误）。
#[tauri::command]
pub async fn wx_status(app: tauri::AppHandle) -> Result<WxStatusView, String> {
    blocking(move || status(&app)).await
}

/// 立即同步一次（转给插件 /api/capture，可能要跑几秒~十几秒）。
/// 插件用 `{ok:false,error:"..."}` 表达「微信未打开」这类业务失败 —— 一律转成
/// Err，让界面原样弹出来，不要显示「已同步」骗人。
#[tauri::command]
pub async fn wx_sync_now(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    blocking(move || {
        // 先确保插件在跑（mac 上插件由 LaunchAgent 拉起，可能因崩溃/被停而死掉；
        // 直接 POST 会得到「插件无响应」这种毫无信息的错误）。
        crate::wx_sidecar::ensure_started(&app)?;
        // 超时对齐 keyext：mac 密钥捕获要在微信「退出登录 → 重新登录」时挂断点，
        // 窗口 180s；这里留 240s，否则用户慢一步就被宿主判超时、白抓一轮。
        let raw = crate::wx_sidecar::http_post("/api/capture", "{}", 240_000)?;
        let v = serde_json::from_str::<serde_json::Value>(&raw)
            .unwrap_or_else(|_| serde_json::json!({ "ok": true }));
        if v.get("ok").and_then(|x| x.as_bool()) == Some(false) {
            let msg = v
                .get("error")
                .and_then(|x| x.as_str())
                .unwrap_or("同步失败")
                .to_string();
            return Err(msg);
        }
        Ok(v)
    })
    .await
}

/// 宿主页签活跃信号（2026-11-10 B 方案）：只改插件内存开关，不写配置、不落盘。
/// 打开「Wx消息」页签下发 true，切走下发 false；插件侧自动轮才读微信库。
/// 老版本插件没有 /api/active 时静默降级，不影响消息浏览。
#[tauri::command]
pub async fn wx_set_active(active: bool) -> Result<serde_json::Value, String> {
    blocking(move || {
        let body = format!("{{\"active\":{}}}", active);
        let raw = crate::wx_sidecar::http_post("/api/active", &body, 5_000)?;
        Ok(serde_json::from_str::<serde_json::Value>(&raw)
            .unwrap_or_else(|_| serde_json::json!({ "ok": false, "error": "插件版本过旧" })))
    })
    .await
}

/// 取一条消息的原始结构（引用到任务时做快照，避免前端拼装）。
#[tauri::command]
pub async fn wx_pick_messages(
    app: tauri::AppHandle,
    ids: Vec<i64>,
) -> Result<Vec<WxMessage>, String> {
    blocking(move || {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let c = open(&app)?;
        if !has_table(&c, "wx_messages") {
            return Ok(vec![]);
        }
        // 逐条按 id 取：勾选数很少（口径 ≤50），逐条比拼 IN 更省事且无注入面。
        let mut out = Vec::new();
        for id in ids.iter().take(50) {
            let row = c.query_row(
                "SELECT m.id, m.talker, COALESCE(NULLIF(ct.display_name,''), m.talker),
                        COALESCE(ct.is_group,0), COALESCE(m.sender_id,0), m.local_type,
                        COALESCE(m.content,''),
                        m.create_time, COALESCE(m.media_path,'')
                   FROM wx_messages m LEFT JOIN wx_contacts ct ON ct.wxid=m.talker
                  WHERE m.id=?1",
                params![id],
                |r| {
                    let raw: String = r.get(6)?;
                    let media: String = r.get(8)?;
                    let (kind, text) = classify(&raw, &media);
                    Ok(WxMessage {
                        id: r.get(0)?,
                        talker: r.get(1)?,
                        talker_name: r.get(2)?,
                        is_group: r.get::<_, i64>(3)? != 0,
                        sender_id: r.get(4)?,
                        kind,
                        text,
                        create_time: r.get(7)?,
                        media_path: media,
                    })
                },
            );
            match row {
                Ok(m) => out.push(m),
                // 同 messages()：勾了消息却「没取到」时，要把真实原因报出来
                Err(e) => return Err(format!("读取消息 {id} 失败：{e}")),
            }
        }
        Ok(out)
    })
    .await
}

// ── 消息里的图片 / 表情（2026-11-10）────────────────────────────────────────
// 媒体本体留在本机微信目录，插件负责 .dat 解密（AES+XOR）与表情 CDN 代理。
// 页面在云端 https：既不能直连 127.0.0.1（混合内容被拦），也不该拿到插件 token。
// 所以由 Rust 取字节、转 base64 data URL 交给前端 <img>，插件令牌不出宿主。
// 微信 4.x 高清原图是私有 wxgf 格式，插件回 200 + application/json 说明；
// 同目录 `_t.dat` 是能解密的缩略图，按顺序回落即可覆盖本机全部图片。

/// 只保留 URL unreserved 字符，其余逐字节 %XX（& 不编码会把 query 截断）。
fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(*b, b'-' | b'_' | b'.' | b'~') {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// 从 <emoji> XML 里取 cdnurl（属性等号两侧可能有空格）。
fn emoji_cdn_url(xml: &str) -> Option<String> {
    let rest = &xml[xml.find("cdnurl")?..];
    let a = rest.find('"')?;
    let tail = &rest[a + 1..];
    let b = tail.find('"')?;
    let u = tail[..b].replace("&amp;", "&");
    if u.starts_with("http://") || u.starts_with("https://") {
        Some(u)
    } else {
        None
    }
}

/// 取一条消息的可显示媒体，返回 `data:image/...;base64,...`；失败给人话原因。
#[tauri::command]
pub async fn wx_media_data(app: tauri::AppHandle, id: i64) -> Result<String, String> {
    blocking(move || {
        // 图片请求可能早于页面 probe（或插件刚被看门狗重启）：先确保插件在跑。
        crate::wx_sidecar::ensure_started(&app)?;
        let c = open(&app)?;
        let (lt, content, media): (i64, String, String) = c
            .query_row(
                "SELECT local_type, COALESCE(content,''), COALESCE(media_path,'') FROM wx_messages WHERE id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|e| format!("找不到消息 {id}：{e}"))?;

        let mut apis: Vec<String> = Vec::new();
        if lt == 47 || content.contains("<emoji") {
            if let Some(u) = emoji_cdn_url(&content) {
                apis.push(format!("/api/emoji?url={}", pct_encode(&u)));
            }
        }
        if !media.is_empty() {
            apis.push(format!("/api/media/view?path={}", pct_encode(&media)));
            if media.to_ascii_lowercase().ends_with(".dat") {
                let stem = &media[..media.len() - 4];
                for suf in ["_t.dat", "_h.dat"] {
                    let p = format!("{stem}{suf}");
                    if std::path::Path::new(&p).exists() {
                        apis.push(format!("/api/media/view?path={}", pct_encode(&p)));
                    }
                }
            }
        }
        if apis.is_empty() {
            return Err("这条消息没有可显示的图片".to_string());
        }
        for api in apis {
            let (bytes, ct) = match crate::wx_sidecar::http_get_bytes(&api, 25_000) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if !ct.starts_with("image/") || bytes.is_empty() {
                continue; // 插件用 200 + application/json 说明 wxgf / 解密失败
            }
            if bytes.len() > 3 * 1024 * 1024 {
                return Err("图片过大，已跳过预览".to_string());
            }
            use base64::Engine;
            let mime = ct.split(';').next().unwrap_or("image/jpeg").trim();
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            return Ok(format!("data:{mime};base64,{b64}"));
        }
        Err("该图片暂无法显示（微信私有格式或原文件已清理）".to_string())
    })
    .await
}

