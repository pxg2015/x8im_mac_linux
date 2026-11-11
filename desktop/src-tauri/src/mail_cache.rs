// 邦信 · 客户端 SQLite 邮件缓存（2026-11-08）
// ─────────────────────────────────────────────────────────────────────────────
// 口径：
//  1) 只缓存邮件内容（头 / 正文 / 文件夹元信息），**绝不缓存邮箱密码或授权码**；
//     凭据仍只在系统凭据库里，mail.json 仍只存地址 + 服务商。
//  2) 库文件 = app_data_dir/mail.db（WAL）。按 (addr, folder, uid) 隔离，换账号不串数据；
//     清除邮箱配置时整库删除（含 -wal / -shm）。
//  3) 每次命令单独 open 一个短连接，不持全局锁 —— 便于删库、避免跨线程共享状态。
//  4) 「进来先读缓存」由 mail_list 完成；「网络增量同步」由 mail_sync 完成，
//     两者分离，界面不等 IMAP 就能出清单。
#![allow(dead_code)]

use crate::mail::{MailBody, MailHead};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
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

/// 应用数据目录（app_data_dir），确保存在。
///
/// 同时是 wxgo 插件的 --data-dir：插件把 config.json / wxgo.log / 密钥放这里，
/// 归档库指向同目录的 mail.db（见 wx_sidecar.rs）。
pub fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("无法定位应用数据目录：{e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建数据目录失败：{e}"))?;
    Ok(dir)
}

pub fn open(app: &AppHandle) -> Result<Connection, String> {
    let p = db_path(app)?;
    let c = Connection::open(&p).map_err(|e| format!("打开邮件缓存失败：{e}"))?;
    let _ = c.busy_timeout(std::time::Duration::from_secs(3));
    c.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS mail_heads (
            addr TEXT NOT NULL, folder TEXT NOT NULL, uid INTEGER NOT NULL,
            from_addr TEXT NOT NULL DEFAULT '', to_addr TEXT NOT NULL DEFAULT '',
            subject TEXT NOT NULL DEFAULT '', date TEXT NOT NULL DEFAULT '',
            seen INTEGER NOT NULL DEFAULT 0, snippet TEXT NOT NULL DEFAULT '',
            has_attach INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (addr, folder, uid));
         CREATE TABLE IF NOT EXISTS mail_bodies (
            addr TEXT NOT NULL, folder TEXT NOT NULL, uid INTEGER NOT NULL,
            data TEXT NOT NULL, PRIMARY KEY (addr, folder, uid));
         CREATE TABLE IF NOT EXISTS mail_meta (
            addr TEXT NOT NULL, folder TEXT NOT NULL, k TEXT NOT NULL, v TEXT NOT NULL,
            PRIMARY KEY (addr, folder, k));",
    )
    .map_err(|e| format!("初始化邮件缓存失败：{e}"))?;
    Ok(c)
}

/// 清除邮箱配置时调用：删整库（WAL 附属文件一并删）
pub fn clear(app: &AppHandle) {
    if let Ok(p) = db_path(app) {
        for suffix in ["", "-wal", "-shm"] {
            let mut q = p.clone().into_os_string();
            q.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(q));
        }
    }
}

// ── meta（uidvalidity / last_max_uid / last_sync / 实际文件夹名）────────────

pub fn meta_get(c: &Connection, addr: &str, folder: &str, k: &str) -> Option<String> {
    c.query_row(
        "SELECT v FROM mail_meta WHERE addr=?1 AND folder=?2 AND k=?3",
        params![addr, folder, k],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

pub fn meta_set(c: &Connection, addr: &str, folder: &str, k: &str, v: &str) -> Result<(), String> {
    c.execute(
        "INSERT INTO mail_meta(addr,folder,k,v) VALUES(?1,?2,?3,?4)
         ON CONFLICT(addr,folder,k) DO UPDATE SET v=excluded.v",
        params![addr, folder, k, v],
    )
    .map_err(|e| format!("写缓存元信息失败：{e}"))?;
    Ok(())
}

// ── 清单 ────────────────────────────────────────────────────────────────────

pub fn list_heads(
    c: &Connection,
    addr: &str,
    folder: &str,
    limit: usize,
) -> Result<Vec<MailHead>, String> {
    let mut st = c
        .prepare(
            "SELECT uid,from_addr,to_addr,subject,date,seen,snippet,has_attach
             FROM mail_heads WHERE addr=?1 AND folder=?2 ORDER BY uid DESC LIMIT ?3",
        )
        .map_err(|e| format!("查询缓存清单失败：{e}"))?;
    let rows = st
        .query_map(params![addr, folder, limit as i64], |r| {
            Ok(MailHead {
                uid: r.get::<_, i64>(0)? as u32,
                from: r.get(1)?,
                to: r.get(2)?,
                subject: r.get(3)?,
                date: r.get(4)?,
                seen: r.get::<_, i64>(5)? != 0,
                snippet: r.get(6)?,
                has_attach: r.get::<_, i64>(7)? != 0,
            })
        })
        .map_err(|e| format!("读取缓存清单失败：{e}"))?;
    let mut out = Vec::new();
    for h in rows {
        out.push(h.map_err(|e| format!("读取缓存清单失败：{e}"))?);
    }
    Ok(out)
}

pub fn upsert_heads(
    c: &Connection,
    addr: &str,
    folder: &str,
    heads: &[MailHead],
) -> Result<(), String> {
    let tx = c
        .unchecked_transaction()
        .map_err(|e| format!("开启缓存写入事务失败：{e}"))?;
    {
        let mut st = tx
            .prepare(
                "INSERT INTO mail_heads
                 (addr,folder,uid,from_addr,to_addr,subject,date,seen,snippet,has_attach)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                 ON CONFLICT(addr,folder,uid) DO UPDATE SET
                   from_addr=excluded.from_addr, to_addr=excluded.to_addr,
                   subject=excluded.subject, date=excluded.date,
                   seen=excluded.seen, snippet=excluded.snippet,
                   has_attach=excluded.has_attach",
            )
            .map_err(|e| format!("准备缓存写入失败：{e}"))?;
        for h in heads {
            st.execute(params![
                addr,
                folder,
                h.uid as i64,
                h.from,
                h.to,
                h.subject,
                h.date,
                h.seen as i64,
                h.snippet,
                h.has_attach as i64,
            ])
            .map_err(|e| format!("写缓存清单失败：{e}"))?;
        }
    }
    tx.commit().map_err(|e| format!("提交缓存清单失败：{e}"))
}

pub fn cached_uids(
    c: &Connection,
    addr: &str,
    folder: &str,
) -> Result<HashSet<u32>, String> {
    let mut st = c
        .prepare("SELECT uid FROM mail_heads WHERE addr=?1 AND folder=?2")
        .map_err(|e| format!("查询缓存 UID 失败：{e}"))?;
    let rows = st
        .query_map(params![addr, folder], |r| r.get::<_, i64>(0))
        .map_err(|e| format!("读取缓存 UID 失败：{e}"))?;
    let mut out = HashSet::new();
    for u in rows {
        out.insert(u.map_err(|e| format!("读取缓存 UID 失败：{e}"))? as u32);
    }
    Ok(out)
}

pub fn max_uid(c: &Connection, addr: &str, folder: &str) -> u32 {
    c.query_row(
        "SELECT COALESCE(MAX(uid),0) FROM mail_heads WHERE addr=?1 AND folder=?2",
        params![addr, folder],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0) as u32
}

pub fn delete_uids(
    c: &Connection,
    addr: &str,
    folder: &str,
    uids: &[u32],
) -> Result<(), String> {
    if uids.is_empty() {
        return Ok(());
    }
    let tx = c
        .unchecked_transaction()
        .map_err(|e| format!("开启缓存清理事务失败：{e}"))?;
    {
        let mut st = tx
            .prepare("DELETE FROM mail_heads WHERE addr=?1 AND folder=?2 AND uid=?3")
            .map_err(|e| format!("准备缓存清理失败：{e}"))?;
        for u in uids {
            st.execute(params![addr, folder, *u as i64])
                .map_err(|e| format!("清理缓存清单失败：{e}"))?;
        }
    }
    {
        let mut st = tx
            .prepare("DELETE FROM mail_bodies WHERE addr=?1 AND folder=?2 AND uid=?3")
            .map_err(|e| format!("准备缓存清理失败：{e}"))?;
        for u in uids {
            st.execute(params![addr, folder, *u as i64])
                .map_err(|e| format!("清理缓存正文失败：{e}"))?;
        }
    }
    tx.commit().map_err(|e| format!("提交缓存清理失败：{e}"))
}

/// UIDVALIDITY 变化 / 全量刷新时重建该文件夹
pub fn clear_folder(c: &Connection, addr: &str, folder: &str) -> Result<(), String> {
    c.execute(
        "DELETE FROM mail_heads WHERE addr=?1 AND folder=?2",
        params![addr, folder],
    )
    .map_err(|e| e.to_string())?;
    c.execute(
        "DELETE FROM mail_bodies WHERE addr=?1 AND folder=?2",
        params![addr, folder],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 用服务器 UNSEEN 集合刷新本地已读态；只动本次同步范围内的 UID，
/// 不把缓存里更老的未读邮件误标成已读（否则角标会偏小）。
pub fn update_seen(
    c: &Connection,
    addr: &str,
    folder: &str,
    unseen: &HashSet<u32>,
    scope: &[u32],
) -> Result<(), String> {
    let tx = c
        .unchecked_transaction()
        .map_err(|e| format!("开启已读态更新事务失败：{e}"))?;
    {
        let mut st = tx
            .prepare("UPDATE mail_heads SET seen=?4 WHERE addr=?1 AND folder=?2 AND uid=?3")
            .map_err(|e| format!("准备已读态更新失败：{e}"))?;
        for u in scope {
            let seen = if unseen.contains(u) { 0i64 } else { 1i64 };
            st.execute(params![addr, folder, *u as i64, seen])
                .map_err(|e| format!("更新缓存已读态失败：{e}"))?;
        }
    }
    tx.commit().map_err(|e| format!("提交已读态更新失败：{e}"))
}

pub fn set_seen(c: &Connection, addr: &str, folder: &str, uid: u32) {
    let _ = c.execute(
        "UPDATE mail_heads SET seen=1 WHERE addr=?1 AND folder=?2 AND uid=?3",
        params![addr, folder, uid as i64],
    );
}

pub fn unseen_cached(c: &Connection, addr: &str) -> u32 {
    c.query_row(
        "SELECT COUNT(*) FROM mail_heads WHERE addr=?1 AND folder='inbox' AND seen=0",
        params![addr],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0) as u32
}

// ── 正文（整封 MailBody 存 JSON，避免和结构体字段一一绑死）─────────────────

pub fn body_get(c: &Connection, addr: &str, folder: &str, uid: u32) -> Option<MailBody> {
    let s = c
        .query_row(
            "SELECT data FROM mail_bodies WHERE addr=?1 AND folder=?2 AND uid=?3",
            params![addr, folder, uid as i64],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()?;
    serde_json::from_str::<MailBody>(&s).ok()
}

pub fn body_put(
    c: &Connection,
    addr: &str,
    folder: &str,
    uid: u32,
    b: &MailBody,
) -> Result<(), String> {
    let s = serde_json::to_string(b).map_err(|e| format!("序列化邮件正文失败：{e}"))?;
    c.execute(
        "INSERT INTO mail_bodies(addr,folder,uid,data) VALUES(?1,?2,?3,?4)
         ON CONFLICT(addr,folder,uid) DO UPDATE SET data=excluded.data",
        params![addr, folder, uid as i64, s],
    )
    .map_err(|e| format!("写缓存正文失败：{e}"))?;
    Ok(())
}
