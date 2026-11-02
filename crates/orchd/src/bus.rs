//! The agent bus: agents, messages (the timeline), questions for the human.
//!
//! Durability rules (from the phase-2 review):
//! - Every state change and the timeline row that describes it commit in ONE transaction;
//!   notifications go out only after the commit. Broadcast events are hints: readers always
//!   re-query ordered rows from the store, so a late or lagged event never loses a row.
//! - Delivery is acknowledged: reading an inbox does not consume it; the consumer acks the ids it
//!   actually handed over (`ack`). A cancelled wait or a dropped reply therefore loses nothing.
//! - First answer wins atomically (`UPDATE … WHERE state = 'open'`).
//! - Identity never moves between agents: an agent is found again by its pane (pane ids never
//!   repeat across orchd restarts) or by its instance id; an existing name is never re-assigned.
//!
//! SQLite calls are short and run under one mutex; fine for a handful of agents.

use anyhow::{Context, Result};
use orch_proto::api::{normalize_name, AgentRow, MessageRow, QuestionRow, RegisterAgent, HUMAN};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub enum BusEvent {
    Message(MessageRow),
    Question(QuestionRow),
    Agent(AgentRow),
    /// "Something changed, re-check the store" (e.g. Telegram got paired).
    Changed,
}

pub struct Bus {
    db: Mutex<Connection>,
    tx: broadcast::Sender<BusEvent>,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
CREATE TABLE IF NOT EXISTS agents(
    name TEXT PRIMARY KEY, kind TEXT NOT NULL, pane TEXT, cwd TEXT,
    status TEXT NOT NULL DEFAULT 'unknown', status_text TEXT, last_seen INTEGER NOT NULL,
    ident TEXT);
CREATE TABLE IF NOT EXISTS messages(
    id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, sender TEXT NOT NULL, recipient TEXT NOT NULL,
    kind TEXT NOT NULL, text TEXT NOT NULL, thread TEXT, question INTEGER);
CREATE INDEX IF NOT EXISTS messages_recipient ON messages(recipient, id);
CREATE TABLE IF NOT EXISTS message_reads(
    message INTEGER NOT NULL, agent TEXT NOT NULL, PRIMARY KEY(message, agent));
CREATE TABLE IF NOT EXISTS questions(
    id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, sender TEXT NOT NULL, text TEXT NOT NULL,
    options TEXT NOT NULL, state TEXT NOT NULL, answer TEXT, answered_by TEXT, answered_ts INTEGER);
CREATE TABLE IF NOT EXISTS kv(k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS dedup(k TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS tg_map(
    tg_msg INTEGER PRIMARY KEY, question INTEGER, agent TEXT, edited INTEGER NOT NULL DEFAULT 0);
";

/// Message kinds that land in a recipient's inbox (status/event rows are timeline-only).
const INBOX_KINDS: &str = "('msg','ask','answer')";

const MSG_COLS: &str = "id, ts, sender, recipient, kind, text, thread, question";

impl Bus {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let db = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        Self::with_connection(db)
    }

    pub fn in_memory() -> Result<Self> {
        Self::with_connection(Connection::open_in_memory()?)
    }

    fn with_connection(db: Connection) -> Result<Self> {
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(SCHEMA)?;
        // Columns added after the first release of the schema.
        for (table, col, decl) in [
            ("agents", "ident", "TEXT"),
            ("tg_map", "edited", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            let has: bool = db
                .prepare(&format!(
                    "SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"
                ))?
                .exists(params![col])?;
            if !has {
                db.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"))?;
            }
        }
        let (tx, _) = broadcast::channel(1024);
        Ok(Self {
            db: Mutex::new(db),
            tx,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BusEvent> {
        self.tx.subscribe()
    }

    pub fn poke(&self) {
        self.emit(BusEvent::Changed);
    }

    fn emit(&self, e: BusEvent) {
        let _ = self.tx.send(e);
    }

    // ---------- agents ----------

    /// Find the agent again by pane (+kind) or instance id; otherwise a new, never-used name
    /// `<project>-<kind>[-N]`. `$ORCH_AGENT` (explicit name) always wins.
    pub fn register(&self, r: &RegisterAgent) -> Result<AgentRow> {
        let db = self.db.lock().unwrap();
        let now = now_ms();
        let ident = match (&r.pane, &r.instance) {
            (Some(p), _) => Some(format!("pane:{p}:{}", r.kind)),
            (None, Some(i)) if !i.is_empty() => Some(format!("inst:{i}")),
            _ => None,
        };
        let name = if let Some(n) = r.name.as_deref().filter(|n| !n.trim().is_empty()) {
            normalize_name(n)
        } else if let Some(existing) = ident
            .as_ref()
            .map(|i| {
                db.query_row(
                    "SELECT name FROM agents WHERE ident = ?1",
                    params![i],
                    |row| row.get::<_, String>(0),
                )
                .optional()
            })
            .transpose()?
            .flatten()
        {
            existing
        } else {
            let project = r
                .cwd
                .as_deref()
                .and_then(|c| Path::new(c).file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "agent".into());
            let base = sanitize(&format!("{project}-{}", r.kind));
            let mut candidate = base.clone();
            let mut n = 1;
            while db
                .prepare("SELECT 1 FROM agents WHERE name = ?1")?
                .exists(params![candidate])?
            {
                n += 1;
                candidate = format!("{base}-{n}");
            }
            candidate
        };
        if name == HUMAN || name == "all" {
            anyhow::bail!("{name:?} is reserved");
        }
        db.execute(
            "INSERT INTO agents(name, kind, pane, cwd, status, last_seen, ident)
             VALUES (?1, ?2, ?3, ?4, 'unknown', ?5, ?6)
             ON CONFLICT(name) DO UPDATE SET kind = ?2, pane = COALESCE(?3, pane),
               cwd = COALESCE(?4, cwd), last_seen = ?5, ident = COALESCE(?6, ident)",
            params![name, r.kind, r.pane, r.cwd, now, ident],
        )?;
        let row = agent_row(&db, &name)?.context("agent vanished")?;
        drop(db);
        self.emit(BusEvent::Agent(row.clone()));
        Ok(row)
    }

    pub fn set_status(&self, name: &str, status: &str, text: Option<&str>) -> Result<AgentRow> {
        self.set_status_if(name, None, status, text)?
            .with_context(|| format!("no agent named {name:?} (register first)"))
    }

    /// Compare-and-set: only change the status if it currently is one of `expect` (or any,
    /// with `None`). Returns the new row, or `None` if nothing changed.
    pub fn set_status_if(
        &self,
        name: &str,
        expect: Option<&[&str]>,
        status: &str,
        text: Option<&str>,
    ) -> Result<Option<AgentRow>> {
        let status = match status {
            "working" | "idle" | "blocked" | "done" | "unknown" => status,
            other => anyhow::bail!("unknown status {other:?} (working|idle|blocked|done)"),
        };
        let db = self.db.lock().unwrap();
        let changed = match expect {
            None => db.execute(
                "UPDATE agents SET status = ?2, status_text = ?3, last_seen = ?4 WHERE name = ?1",
                params![name, status, text, now_ms()],
            )?,
            Some(expect) => {
                let list = expect
                    .iter()
                    .map(|s| format!("'{}'", s.replace('\'', "")))
                    .collect::<Vec<_>>()
                    .join(",");
                db.execute(
                    &format!(
                        "UPDATE agents SET status = ?2, status_text = ?3 WHERE name = ?1 AND status IN ({list})"
                    ),
                    params![name, status, text],
                )?
            }
        };
        if changed == 0 {
            return Ok(None);
        }
        let row = agent_row(&db, name)?.context("agent vanished")?;
        drop(db);
        self.emit(BusEvent::Agent(row.clone()));
        Ok(Some(row))
    }

    pub fn touch(&self, name: &str) {
        let db = self.db.lock().unwrap();
        let _ = db.execute(
            "UPDATE agents SET last_seen = ?2 WHERE name = ?1",
            params![name, now_ms()],
        );
    }

    pub fn agent(&self, name: &str) -> Result<Option<AgentRow>> {
        agent_row(&self.db.lock().unwrap(), name)
    }

    pub fn agents(&self) -> Result<Vec<AgentRow>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT name, kind, pane, cwd, status, status_text, last_seen FROM agents ORDER BY name",
        )?;
        let rows = st
            .query_map([], map_agent)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---------- messages ----------

    pub fn send(
        &self,
        from: &str,
        to: &str,
        kind: &str,
        text: &str,
        thread: Option<&str>,
        question: Option<i64>,
    ) -> Result<MessageRow> {
        self.send_dedup(None, from, to, kind, text, thread, question)?
            .context("unexpected duplicate")
    }

    /// Like `send`, but at most once per `dedup` key (e.g. a Telegram update id), atomically.
    /// `None` = this key was already applied.
    #[allow(clippy::too_many_arguments)]
    pub fn send_dedup(
        &self,
        dedup: Option<&str>,
        from: &str,
        to: &str,
        kind: &str,
        text: &str,
        thread: Option<&str>,
        question: Option<i64>,
    ) -> Result<Option<MessageRow>> {
        let from = normalize_name(from);
        let to = normalize_name(to);
        if text.trim().is_empty() {
            anyhow::bail!("empty message");
        }
        let mut db = self.db.lock().unwrap();
        if kind == "msg" && to != HUMAN && to != "all" && agent_row(&db, &to)?.is_none() {
            anyhow::bail!("no agent named {to:?} (see list_agents)");
        }
        let tx = db.transaction()?;
        if let Some(key) = dedup {
            if tx.execute("INSERT OR IGNORE INTO dedup(k) VALUES (?1)", params![key])? == 0 {
                return Ok(None);
            }
        }
        let row = insert_message(&tx, &from, &to, kind, text, thread, question)?;
        tx.commit()?;
        drop(db);
        self.emit(BusEvent::Message(row.clone()));
        Ok(Some(row))
    }

    /// Unread inbox rows for `name`. Does not consume them: call `ack` with the ids that were
    /// actually delivered.
    pub fn inbox(&self, name: &str) -> Result<Vec<MessageRow>> {
        let name = normalize_name(name);
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(&format!(
            "SELECT {MSG_COLS} FROM messages m
             WHERE (recipient = ?1 OR (recipient = 'all' AND sender != ?1))
               AND kind IN {INBOX_KINDS}
               AND NOT EXISTS (SELECT 1 FROM message_reads r WHERE r.message = m.id AND r.agent = ?1)
             ORDER BY id LIMIT 200"
        ))?;
        let rows = st
            .query_map(params![name], map_message)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Mark delivered messages read (idempotent).
    pub fn ack(&self, name: &str, ids: &[i64]) -> Result<usize> {
        let name = normalize_name(name);
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let mut n = 0;
        for id in ids {
            n += tx.execute(
                "INSERT OR IGNORE INTO message_reads(message, agent) VALUES (?1, ?2)",
                params![id, name],
            )?;
        }
        tx.execute(
            "UPDATE agents SET last_seen = ?2 WHERE name = ?1",
            params![name, now_ms()],
        )?;
        tx.commit()?;
        Ok(n)
    }

    /// Wait up to `wait` for unread inbox rows (not consumed; see `ack`).
    pub async fn inbox_wait(&self, name: &str, wait: Duration) -> Result<Vec<MessageRow>> {
        let name = normalize_name(name);
        let mut rx = self.subscribe(); // before the first check: no lost wake-up
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let rows = self.inbox(&name)?;
            if !rows.is_empty() || tokio::time::Instant::now() >= deadline {
                return Ok(rows);
            }
            // Any message event is a hint to re-query (we never trust event order or content).
            loop {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Err(_) => return Ok(Vec::new()),
                    Ok(Ok(BusEvent::Message(_))) => break,
                    Ok(Ok(_)) => continue,
                    Ok(Err(broadcast::error::RecvError::Lagged(_))) => break,
                    Ok(Err(broadcast::error::RecvError::Closed)) => return Ok(Vec::new()),
                }
            }
        }
    }

    pub fn log(&self, after: Option<i64>, limit: usize) -> Result<Vec<MessageRow>> {
        let db = self.db.lock().unwrap();
        let rows = if let Some(after) = after {
            let mut st = db.prepare(&format!(
                "SELECT {MSG_COLS} FROM messages WHERE id > ?1 ORDER BY id LIMIT ?2"
            ))?;
            let rows = st
                .query_map(params![after, limit as i64], map_message)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        } else {
            let mut st = db.prepare(&format!(
                "SELECT * FROM (SELECT {MSG_COLS} FROM messages ORDER BY id DESC LIMIT ?1) ORDER BY id"
            ))?;
            let rows = st
                .query_map(params![limit as i64], map_message)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        Ok(rows)
    }

    pub fn max_message_id(&self) -> Result<i64> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT COALESCE(MAX(id), 0) FROM messages",
            [],
            |r| r.get(0),
        )?)
    }

    // ---------- questions ----------

    /// Ask the human. Asking the very same open question again (same sender, text, options)
    /// returns the existing one — a retry after a cancelled call creates no duplicate.
    pub fn ask(&self, from: &str, text: &str, options: &[String]) -> Result<QuestionRow> {
        let from = normalize_name(from);
        if text.trim().is_empty() {
            anyhow::bail!("empty question");
        }
        let options: Vec<String> = options
            .iter()
            .map(|o| o.trim().to_string())
            .filter(|o| !o.is_empty())
            .take(8)
            .collect();
        let options_json = serde_json::to_string(&options)?;
        let mut db = self.db.lock().unwrap();
        // A retry of the same question returns the original: while it is open, and also once
        // answered as long as the asker has not received that answer yet (its answer message
        // is unread). Asking again after reading the answer is a deliberate new question.
        let existing: Option<i64> = db
            .query_row(
                "SELECT q.id FROM questions q
                 WHERE q.sender = ?1 AND q.text = ?2 AND q.options = ?3
                   AND (q.state = 'open' OR EXISTS (
                        SELECT 1 FROM messages m WHERE m.question = q.id AND m.kind = 'answer'
                          AND NOT EXISTS (SELECT 1 FROM message_reads r
                                          WHERE r.message = m.id AND r.agent = q.sender)))
                 ORDER BY q.id DESC LIMIT 1",
                params![from, text, options_json],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return question_row(&db, id)?.context("question vanished");
        }
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO questions(ts, sender, text, options, state) VALUES (?1, ?2, ?3, ?4, 'open')",
            params![now_ms(), from, text, options_json],
        )?;
        let id = tx.last_insert_rowid();
        let msg = insert_message(&tx, &from, HUMAN, "ask", text, None, Some(id))?;
        let q = question_row(&tx, id)?.context("question vanished")?;
        tx.commit()?;
        drop(db);
        self.emit(BusEvent::Message(msg));
        self.emit(BusEvent::Question(q.clone()));
        Ok(q)
    }

    pub fn question(&self, id: i64) -> Result<Option<QuestionRow>> {
        question_row(&self.db.lock().unwrap(), id)
    }

    pub fn questions(&self, all: bool) -> Result<Vec<QuestionRow>> {
        let db = self.db.lock().unwrap();
        let sql = if all {
            "SELECT id FROM questions ORDER BY id DESC LIMIT 100"
        } else {
            "SELECT id FROM questions WHERE state = 'open' ORDER BY id"
        };
        let ids: Vec<i64> = db
            .prepare(sql)?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter()
            .filter_map(|id| question_row(&db, id).transpose())
            .collect()
    }

    /// Returns (won, question). The state change and the answer message commit together.
    pub fn answer(&self, id: i64, answer: &str, by: &str) -> Result<(bool, QuestionRow)> {
        if answer.trim().is_empty() {
            anyhow::bail!("empty answer");
        }
        let by = normalize_name(by);
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let won = tx.execute(
            "UPDATE questions SET state = 'answered', answer = ?2, answered_by = ?3, answered_ts = ?4
             WHERE id = ?1 AND state = 'open'",
            params![id, answer, by, now_ms()],
        )? == 1;
        let q = question_row(&tx, id)?.with_context(|| format!("no question {id}"))?;
        let msg = if won {
            Some(insert_message(
                &tx,
                HUMAN,
                &q.from,
                "answer",
                answer,
                None,
                Some(id),
            )?)
        } else {
            None
        };
        tx.commit()?;
        drop(db);
        if let Some(m) = msg {
            self.emit(BusEvent::Message(m));
            self.emit(BusEvent::Question(q.clone()));
        }
        Ok((won, q))
    }

    /// Wait up to `wait` for question `id` to be answered.
    pub async fn ask_wait(&self, id: i64, wait: Duration) -> Result<QuestionRow> {
        let mut rx = self.subscribe();
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let q = self
                .question(id)?
                .with_context(|| format!("no question {id}"))?;
            if q.state != "open" || tokio::time::Instant::now() >= deadline {
                return Ok(q);
            }
            loop {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Err(_) => break,
                    Ok(Ok(BusEvent::Question(q))) if q.id == id => break,
                    Ok(Ok(_)) => continue,
                    Ok(Err(broadcast::error::RecvError::Lagged(_))) => break,
                    Ok(Err(broadcast::error::RecvError::Closed)) => break,
                }
            }
        }
    }

    // ---------- small key/value + telegram bookkeeping ----------

    pub fn kv_get(&self, k: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT v FROM kv WHERE k = ?1", params![k], |r| r.get(0))
            .optional()?)
    }

    pub fn kv_set(&self, k: &str, v: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO kv(k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = ?2",
            params![k, v],
        )?;
        Ok(())
    }

    pub fn kv_del(&self, k: &str) -> Result<()> {
        self.db
            .lock()
            .unwrap()
            .execute("DELETE FROM kv WHERE k = ?1", params![k])?;
        Ok(())
    }

    /// Atomically take the next value of a persistent counter (pane ids that never repeat).
    pub fn next_counter(&self, k: &str) -> Result<u64> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let cur: u64 = tx
            .query_row("SELECT v FROM kv WHERE k = ?1", params![k], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let next = cur + 1;
        tx.execute(
            "INSERT INTO kv(k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = ?2",
            params![k, next.to_string()],
        )?;
        tx.commit()?;
        Ok(next)
    }

    pub fn tg_map_set(
        &self,
        tg_msg: i64,
        question: Option<i64>,
        agent: Option<&str>,
    ) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT OR REPLACE INTO tg_map(tg_msg, question, agent, edited) VALUES (?1, ?2, ?3, 0)",
            params![tg_msg, question, agent],
        )?;
        Ok(())
    }

    pub fn tg_map_get(&self, tg_msg: i64) -> Result<Option<(Option<i64>, Option<String>)>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT question, agent FROM tg_map WHERE tg_msg = ?1",
                params![tg_msg],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Telegram messages showing question `question`: (message id, already edited to answered).
    pub fn tg_msgs_for_question(&self, question: i64) -> Result<Vec<(i64, bool)>> {
        let db = self.db.lock().unwrap();
        let rows = db
            .prepare("SELECT tg_msg, edited FROM tg_map WHERE question = ?1")?
            .query_map(params![question], |r| {
                Ok((r.get(0)?, r.get::<_, i64>(1)? != 0))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn tg_mark_edited(&self, tg_msg: i64) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE tg_map SET edited = 1 WHERE tg_msg = ?1",
            params![tg_msg],
        )?;
        Ok(())
    }
}

fn insert_message(
    db: &Connection,
    from: &str,
    to: &str,
    kind: &str,
    text: &str,
    thread: Option<&str>,
    question: Option<i64>,
) -> Result<MessageRow> {
    let ts = now_ms();
    db.execute(
        "INSERT INTO messages(ts, sender, recipient, kind, text, thread, question)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![ts, from, to, kind, text, thread, question],
    )?;
    Ok(MessageRow {
        id: db.last_insert_rowid(),
        ts_ms: ts,
        from: from.to_string(),
        to: to.to_string(),
        kind: kind.to_string(),
        text: text.to_string(),
        thread: thread.map(str::to_string),
        question,
    })
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

fn map_agent(r: &rusqlite::Row) -> rusqlite::Result<AgentRow> {
    Ok(AgentRow {
        name: r.get(0)?,
        kind: r.get(1)?,
        pane: r.get(2)?,
        cwd: r.get(3)?,
        status: r.get(4)?,
        status_text: r.get(5)?,
        last_seen_ms: r.get(6)?,
    })
}

fn agent_row(db: &Connection, name: &str) -> Result<Option<AgentRow>> {
    Ok(db
        .query_row(
            "SELECT name, kind, pane, cwd, status, status_text, last_seen FROM agents WHERE name = ?1",
            params![name],
            map_agent,
        )
        .optional()?)
}

fn map_message(r: &rusqlite::Row) -> rusqlite::Result<MessageRow> {
    Ok(MessageRow {
        id: r.get(0)?,
        ts_ms: r.get(1)?,
        from: r.get(2)?,
        to: r.get(3)?,
        kind: r.get(4)?,
        text: r.get(5)?,
        thread: r.get(6)?,
        question: r.get(7)?,
    })
}

fn question_row(db: &Connection, id: i64) -> Result<Option<QuestionRow>> {
    Ok(db
        .query_row(
            "SELECT id, ts, sender, text, options, state, answer, answered_by, answered_ts
             FROM questions WHERE id = ?1",
            params![id],
            |r| {
                let options: String = r.get(4)?;
                Ok(QuestionRow {
                    id: r.get(0)?,
                    ts_ms: r.get(1)?,
                    from: r.get(2)?,
                    text: r.get(3)?,
                    options: serde_json::from_str(&options).unwrap_or_default(),
                    state: r.get(5)?,
                    answer: r.get(6)?,
                    answered_by: r.get(7)?,
                    answered_ts_ms: r.get(8)?,
                })
            },
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg_full(
        bus: &Bus,
        kind: &str,
        pane: Option<&str>,
        inst: Option<&str>,
        cwd: &str,
    ) -> String {
        bus.register(&RegisterAgent {
            kind: kind.into(),
            pane: pane.map(str::to_string),
            cwd: Some(cwd.into()),
            name: None,
            instance: inst.map(str::to_string),
        })
        .unwrap()
        .name
    }

    fn reg(bus: &Bus, kind: &str, pane: &str, cwd: &str) -> String {
        reg_full(bus, kind, Some(pane), None, cwd)
    }

    fn read_all(bus: &Bus, name: &str) -> Vec<MessageRow> {
        let rows = bus.inbox(name).unwrap();
        bus.ack(name, &rows.iter().map(|r| r.id).collect::<Vec<_>>())
            .unwrap();
        rows
    }

    #[test]
    fn names_follow_panes_and_never_move_between_agents() {
        let bus = Bus::in_memory().unwrap();
        assert_eq!(
            reg(&bus, "claude", "orch:p1", "/projects/Paperscanner"),
            "paperscanner-claude"
        );
        // Same pane again → same name.
        assert_eq!(
            reg(&bus, "claude", "orch:p1", "/projects/paperscanner"),
            "paperscanner-claude"
        );
        // Another pane in the same project → numbered.
        assert_eq!(
            reg(&bus, "claude", "herdr:w1:p1", "/projects/paperscanner"),
            "paperscanner-claude-2"
        );
        assert_eq!(
            reg(&bus, "codex", "orch:p2", "/projects/paperscanner"),
            "paperscanner-codex"
        );
        // Two pane-less agents in the same cwd are NOT merged; each keeps its instance.
        let a = reg_full(&bus, "codex", None, Some("i-a"), "/x/proj");
        let b = reg_full(&bus, "codex", None, Some("i-b"), "/x/proj");
        assert_ne!(a, b);
        assert_eq!(reg_full(&bus, "codex", None, Some("i-a"), "/x/proj"), a);
        // No pane, no instance → always a fresh name, never someone else's mailbox.
        let c = reg_full(&bus, "codex", None, None, "/x/proj");
        assert!(c != a && c != b);
    }

    #[test]
    fn inbox_is_consumed_only_by_ack() {
        let bus = Bus::in_memory().unwrap();
        let a = reg(&bus, "claude", "orch:p1", "/x/proj");
        let b = reg(&bus, "codex", "orch:p2", "/x/proj");
        bus.send(&a, &b, "msg", "hello b", None, None).unwrap();
        bus.send(&b, "all", "msg", "hello everyone", None, None)
            .unwrap();
        assert!(bus.send(&a, "nobody", "msg", "x", None, None).is_err());
        assert_eq!(
            bus.inbox(&b).unwrap().len(),
            1,
            "own broadcast is not in own inbox"
        );
        // Reading without ack (a cancelled wait, a lost reply) consumes nothing.
        assert_eq!(bus.inbox(&b).unwrap().len(), 1);
        assert_eq!(read_all(&bus, &b).len(), 1);
        assert!(bus.inbox(&b).unwrap().is_empty());
        let inbox_a = read_all(&bus, &a);
        assert_eq!(inbox_a.len(), 1);
        assert_eq!(inbox_a[0].text, "hello everyone");
        assert_eq!(bus.log(None, 10).unwrap().len(), 2);
    }

    #[test]
    fn first_answer_wins_and_reaches_the_asker() {
        let bus = Bus::in_memory().unwrap();
        let a = reg(&bus, "claude", "orch:p1", "/x/proj");
        let q = bus
            .ask(&a, "Deploy now?", &["yes".into(), "no".into()])
            .unwrap();
        // Asking the same open question again returns the same one (retry after cancel).
        assert_eq!(
            bus.ask(&a, "Deploy now?", &["yes".into(), "no".into()])
                .unwrap()
                .id,
            q.id
        );
        assert_eq!(bus.questions(false).unwrap().len(), 1);
        assert_eq!(read_all(&bus, HUMAN)[0].question, Some(q.id));
        let (won, _) = bus.answer(q.id, "yes", "telegram").unwrap();
        assert!(won);
        let (won2, q2) = bus.answer(q.id, "no", "cli").unwrap();
        assert!(!won2);
        assert_eq!(q2.answer.as_deref(), Some("yes"));
        assert!(bus.questions(false).unwrap().is_empty());
        // Retried before the asker read the answer: the original (answered) question.
        let retry = bus
            .ask(&a, "Deploy now?", &["yes".into(), "no".into()])
            .unwrap();
        assert_eq!((retry.id, retry.state.as_str()), (q.id, "answered"));
        let inbox = read_all(&bus, &a);
        assert_eq!(inbox.len(), 1, "exactly one answer message");
        assert_eq!(
            (inbox[0].kind.as_str(), inbox[0].text.as_str()),
            ("answer", "yes")
        );
        // Once answered, the same text is a new question.
        assert_ne!(
            bus.ask(&a, "Deploy now?", &["yes".into(), "no".into()])
                .unwrap()
                .id,
            q.id
        );
    }

    #[test]
    fn dedup_and_compare_and_set() {
        let bus = Bus::in_memory().unwrap();
        let a = reg(&bus, "claude", "orch:p1", "/x/proj");
        assert!(bus
            .send_dedup(Some("tg:5"), HUMAN, &a, "msg", "hi", None, None)
            .unwrap()
            .is_some());
        assert!(bus
            .send_dedup(Some("tg:5"), HUMAN, &a, "msg", "hi", None, None)
            .unwrap()
            .is_none());
        assert_eq!(bus.inbox(&a).unwrap().len(), 1);
        bus.set_status(&a, "working", None).unwrap();
        assert!(bus
            .set_status_if(&a, Some(&["idle"]), "working", Some("nudged"))
            .unwrap()
            .is_none());
        bus.set_status(&a, "idle", None).unwrap();
        assert!(bus
            .set_status_if(&a, Some(&["idle"]), "working", Some("nudged"))
            .unwrap()
            .is_some());
        assert_eq!(bus.next_counter("next_pane").unwrap(), 1);
        assert_eq!(bus.next_counter("next_pane").unwrap(), 2);
    }

    #[tokio::test]
    async fn waits_wake_up_on_new_rows() {
        let bus = std::sync::Arc::new(Bus::in_memory().unwrap());
        let a = reg(&bus, "claude", "orch:p1", "/x/proj");
        let b = reg(&bus, "codex", "orch:p2", "/x/proj");
        let q = bus.ask(&a, "ok?", &[]).unwrap();
        let (b2, bus2) = (b.clone(), bus.clone());
        let waiter =
            tokio::spawn(
                async move { bus2.inbox_wait(&b2, Duration::from_secs(5)).await.unwrap() },
            );
        let bus3 = bus.clone();
        let qwait =
            tokio::spawn(async move { bus3.ask_wait(q.id, Duration::from_secs(5)).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(50)).await;
        bus.send(&a, &b, "msg", "ping", None, None).unwrap();
        bus.answer(q.id, "fine", "cli").unwrap();
        assert_eq!(waiter.await.unwrap()[0].text, "ping");
        assert_eq!(qwait.await.unwrap().answer.as_deref(), Some("fine"));
        let q2 = bus.ask(&a, "later?", &[]).unwrap();
        let r = bus
            .ask_wait(q2.id, Duration::from_millis(50))
            .await
            .unwrap();
        assert_eq!(r.state, "open");
    }

    #[test]
    fn survives_reopen_and_migrates() {
        let dir = std::env::temp_dir().join(format!("orch-bus-test-{}", std::process::id()));
        let path = dir.join("bus.db");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // An old-schema database (no agents.ident, no tg_map.edited) opens and migrates.
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch(
                "CREATE TABLE agents(name TEXT PRIMARY KEY, kind TEXT NOT NULL, pane TEXT, cwd TEXT,
                 status TEXT NOT NULL DEFAULT 'unknown', status_text TEXT, last_seen INTEGER NOT NULL);
                 CREATE TABLE tg_map(tg_msg INTEGER PRIMARY KEY, question INTEGER, agent TEXT);",
            )
            .unwrap();
        }
        let id = {
            let bus = Bus::open(&path).unwrap();
            let a = reg(&bus, "claude", "orch:p1", "/x/proj");
            bus.ask(&a, "persist me", &[]).unwrap().id
        };
        let bus = Bus::open(&path).unwrap();
        assert_eq!(bus.question(id).unwrap().unwrap().text, "persist me");
        assert!(bus.answer(id, "done", "cli").unwrap().0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
