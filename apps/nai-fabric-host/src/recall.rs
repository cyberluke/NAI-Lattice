//! Kilo session recall: a read-only view into the local Kilo session store
//! (`~/.local/share/kilo/kilo.db`, SQLite) so the Gas Town dashboard can "pin"
//! an existing Kilo session and bring its message history / context into the UI.
//!
//! Security/operational invariants:
//!   * Always opened READ-ONLY — we never write to the live Kilo DB.
//!   * Never touched while the extension is mid-write is fine: SQLite
//!     (WAL mode) allows concurrent readers.
//!   * Bounded: we never dump a whole 4k-message session; we cap parts/messages.
//!   * Credentials/match tokens are never surfaced (only role/kind/text).

use std::path::PathBuf;

/// Locate the Kilo session DB (read-only target).
pub fn kilo_db_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local").join("share").join("kilo").join("kilo.db")
}

/// A recallable Kilo session (the "pin").
#[derive(serde::Serialize)]
pub struct RecallSession {
    pub id: String,
    pub title: String,
    pub directory: String,
    pub message_count: u64,
    pub last_updated: i64,
}

/// One recalled message turn (role + reconstructed text).
#[derive(serde::Serialize)]
pub struct RecallMessage {
    pub role: String,
    pub kind: String,   // "text" | "tool" | "step-start" | ...
    pub text: String,
    pub time_created: i64,
}
impl RecallMessage {
    fn from_part(part_data: &str, role: &str, time_created: i64) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(part_data).ok()?;
        let kind = v["type"].as_str().unwrap_or("text").to_string();
        let text = match kind.as_str() {
            "text" => v["text"].as_str().unwrap_or("").to_string(),
            // Tool-call/step parts carry little user text; keep a compact marker.
            _ => return None,
        };
        if text.trim().is_empty() { return None; }
        Some(RecallMessage { role: role.to_string(), kind, text, time_created })
    }
}

fn open_readonly(db: &PathBuf) -> Result<rusqlite::Connection, String> {
    rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ).map_err(|e| format!("read-only open failed: {e}"))
}

/// Recent sessions whose location matches the Gas Town repo, newest first.
pub fn list_sessions(repo_like: &str, limit: u64) -> Result<Vec<RecallSession>, String> {
    let db = kilo_db_path();
    if !db.exists() {
        return Err(format!("kilo db not found at {}", db.display()));
    }
    let conn = open_readonly(&db)?;
    let like = format!("%{}%", repo_like.replace('%', "").replace('\\', "/").to_lowercase());
    // `directory DESC` rough recency; count messages per session.
    let mut stmt = conn.prepare(
        "SELECT s.id, s.title, s.directory, \
                (SELECT count(*) FROM message m WHERE m.session_id = s.id) as msg_count, \
                (SELECT MAX(m2.time_updated) FROM message m2 WHERE m2.session_id = s.id) as last_updated \
         FROM session s \
         WHERE s.directory LIKE ?1 OR s.path LIKE ?1 OR s.slug LIKE ?1 \
         ORDER BY last_updated DESC LIMIT ?2",
    ).map_err(|e| format!("prepare: {e}"))?;
    let rows = stmt.query_map(rusqlite::params![like, limit], |r| {
        Ok(RecallSession {
            id: r.get(0)?,
            title: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            directory: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            message_count: r.get(3)?,
            last_updated: r.get(4)?,
        })
    }).map_err(|e| format!("query: {e}"))?;
    let mut out = Vec::new();
    for r in rows.flatten() { out.push(r); }
    Ok(out)
}

/// The transcript/context for ONE pinned session, newest-first reversed so the
/// output is chronological, bounded to `max_messages` turns (each turn = its
/// text part). Never surfaces tokens/credentials.
pub fn session_context(session_id: &str, max_messages: u64) -> Result<Vec<RecallMessage>, String> {
    let db = kilo_db_path();
    if !db.exists() { return Err(format!("kilo db not found at {}", db.display())); }
    let conn = open_readonly(&db)?;
    // Pick the newest N messages for the session, then their text parts.
    let msgs: Vec<(String, String, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT id, data, time_created FROM message WHERE session_id = ?1 ORDER BY time_created DESC LIMIT ?2",
        ).map_err(|e| e.to_string())?;
        let rows = stmt.query_map(rusqlite::params![session_id, max_messages], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        }).map_err(|e| e.to_string())?;
        let mut v = Vec::new();
        for r in rows.flatten() { v.push(r); }
        v
    };
    let mut result: Vec<RecallMessage> = Vec::new();
    for (mid, mdata, mtime) in msgs.iter().rev() {
        // role from message.data JSON
        let role = serde_json::from_str::<serde_json::Value>(mdata)
            .ok().and_then(|v| v["role"].as_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".to_string());
        let parts = {
            let mut stmt = conn.prepare(
                "SELECT data FROM part WHERE message_id = ?1 ORDER BY time_created ASC",
            ).map_err(|e| e.to_string())?;
            let rows = stmt.query_map(rusqlite::params![mid], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
            let mut p = Vec::new();
            for r in rows.flatten() { p.push(r); }
            p
        };
        for pd in &parts {
            if let Some(rm) = RecallMessage::from_part(pd, &role, *mtime) {
                result.push(rm);
            }
        }
    }
    Ok(result)
}