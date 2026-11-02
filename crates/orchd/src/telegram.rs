//! Telegram bridge (own bot): questions go to Murat's private chat with option buttons;
//! a button press or a reply answers them. Messages addressed to Murat are forwarded; a reply
//! to one goes back to that agent.
//!
//! Security: only the paired chat is served (pairing = `orch telegram pair` → one-time code →
//! `/pair CODE` in the bot chat, or TG_CHAT in the config). Terminal contents are never sent.
//! The token is never logged (errors are scrubbed).
//!
//! Config: ~/.config/orchtool/telegram.env with TG_TOKEN=… and optionally TG_CHAT=…;
//! the environment overrides it; TG_API overrides the API base (tests).

use crate::bus::{now_ms, Bus};
use anyhow::{anyhow, Result};
use orch_proto::api::{QuestionRow, HUMAN};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct TgConfig {
    pub token: String,
    pub api: String,
    pub chat: Option<i64>,
}

impl TgConfig {
    pub fn load() -> Option<Self> {
        let mut token = None;
        let mut chat = None;
        let mut api = None;
        let path = std::env::var_os("HOME")
            .map(|h| std::path::PathBuf::from(h).join(".config/orchtool/telegram.env"));
        if let Some(text) = path.and_then(|p| std::fs::read_to_string(p).ok()) {
            for line in text.lines() {
                let line = line.trim();
                if let Some((k, v)) = line.split_once('=') {
                    let v = v.trim().trim_matches('"').to_string();
                    match k.trim() {
                        "TG_TOKEN" => token = Some(v),
                        "TG_CHAT" => chat = v.parse().ok(),
                        "TG_API" => api = Some(v),
                        _ => {}
                    }
                }
            }
        }
        if let Ok(v) = std::env::var("TG_TOKEN") {
            token = Some(v);
        }
        if let Ok(v) = std::env::var("TG_CHAT") {
            chat = v.parse().ok();
        }
        if let Ok(v) = std::env::var("TG_API") {
            api = Some(v);
        }
        Some(Self {
            token: token.filter(|t| !t.is_empty())?,
            api: api.unwrap_or_else(|| "https://api.telegram.org".into()),
            chat,
        })
    }
}

struct Api {
    cfg: TgConfig,
    agent: ureq::Agent,
}

impl Api {
    fn new(cfg: TgConfig) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(40)))
            .http_status_as_error(false)
            .build()
            .into();
        Self { cfg, agent }
    }

    fn scrub(&self, e: impl std::fmt::Display) -> anyhow::Error {
        anyhow!("{}", e.to_string().replace(&self.cfg.token, "<token>"))
    }

    fn call(&self, method: &str, body: Value) -> Result<Value> {
        let url = format!("{}/bot{}/{method}", self.cfg.api, self.cfg.token);
        let resp = self
            .agent
            .post(&url)
            .send_json(&body)
            .map_err(|e| self.scrub(e))?;
        let v: Value = resp.into_body().read_json().map_err(|e| self.scrub(e))?;
        if v["ok"].as_bool() != Some(true) {
            return Err(anyhow!(
                "telegram {method}: {}",
                v["description"].as_str().unwrap_or("error")
            ));
        }
        Ok(v["result"].clone())
    }
}

fn chat_id(bus: &Bus, cfg: &TgConfig) -> Option<i64> {
    cfg.chat.or_else(|| {
        bus.kv_get("tg_chat")
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
    })
}

/// A new one-time pairing code (valid 10 minutes).
pub fn new_pair_code(bus: &Bus) -> Result<String> {
    let mut b = [0u8; 8];
    use std::io::Read as _;
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    let n = u64::from_le_bytes(b) % 1_000_000;
    let code = format!("{n:06}");
    bus.kv_set("tg_pair_code", &code)?;
    bus.kv_set("tg_pair_expires", &(now_ms() + 600_000).to_string())?;
    Ok(code)
}

fn question_text(q: &QuestionRow) -> String {
    let mut t = format!("❓ {} asks (#{}):\n{}", q.from, q.id, q.text);
    if q.state == "answered" {
        t.push_str(&format!(
            "\n\n✅ {} (by {})",
            q.answer.as_deref().unwrap_or(""),
            q.answered_by.as_deref().unwrap_or("?")
        ));
    } else if q.options.is_empty() {
        t.push_str("\n\n↩️ Reply to this message to answer.");
    } else {
        t.push_str("\n\nTap an option, or reply with your own answer.");
    }
    t
}

fn question_keyboard(q: &QuestionRow) -> Value {
    if q.state != "open" || q.options.is_empty() {
        return json!({ "inline_keyboard": [] });
    }
    let rows: Vec<Value> = q
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| json!([{ "text": o, "callback_data": format!("a:{}:{i}", q.id) }]))
        .collect();
    json!({ "inline_keyboard": rows })
}

/// Start the bridge: a sync loop (store → Telegram, retried until it succeeds) and a
/// long-poll thread (Telegram → store).
pub fn start(bus: Arc<Bus>, cfg: TgConfig) {
    let api = Arc::new(Api::new(cfg.clone()));
    eprintln!(
        "orchd: telegram enabled ({})",
        if chat_id(&bus, &cfg).is_some() {
            "paired"
        } else {
            "not paired yet: run `orch telegram pair`"
        }
    );
    // Messages to Murat from before the bridge's first start are history, not news.
    if bus.kv_get("tg_msg_cursor").ok().flatten().is_none() {
        let max = bus.max_message_id().unwrap_or(0);
        let _ = bus.kv_set("tg_msg_cursor", &max.to_string());
    }

    // Outgoing: driven by the store, not by transient events. Every bus event (or a 10 s
    // tick) triggers one sync pass that sends whatever is still unsent; failures are retried
    // on the next pass, so outages, restarts and late pairing lose nothing.
    {
        let bus = Arc::clone(&bus);
        let api = Arc::clone(&api);
        let cfg = cfg.clone();
        let mut rx = bus.subscribe();
        tokio::spawn(async move {
            loop {
                let (bus2, api2, cfg2) = (Arc::clone(&bus), Arc::clone(&api), cfg.clone());
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) = sync(&bus2, &api2, &cfg2) {
                        eprintln!("orchd: telegram sync: {e}");
                    }
                })
                .await;
                // Wait for the next hint; drain bursts so one pass covers them.
                let _ = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await;
                while rx.try_recv().is_ok() {}
            }
        });
    }

    // Incoming: long-poll getUpdates. The offset is saved only after an update was applied;
    // a crash in between re-delivers it, and the per-update dedup key makes that harmless.
    std::thread::Builder::new()
        .name("telegram-poll".into())
        .spawn(move || {
            let mut offset: i64 = bus
                .kv_get("tg_offset")
                .ok()
                .flatten()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            loop {
                match api.call(
                    "getUpdates",
                    json!({ "offset": offset, "timeout": 25, "allowed_updates": ["message", "callback_query"] }),
                ) {
                    Ok(updates) => {
                        'batch: for u in updates.as_array().cloned().unwrap_or_default() {
                            let uid = u["update_id"].as_i64().unwrap_or(0);
                            // handle_update only fails on store errors (invalid or foreign
                            // updates are ignored with Ok). Never skip a valid update: stop the
                            // batch without advancing the offset; Telegram re-delivers it.
                            let mut applied = false;
                            for attempt in 1..=3 {
                                match handle_update(&bus, &api, &cfg, &u) {
                                    Ok(()) => {
                                        applied = true;
                                        break;
                                    }
                                    Err(e) => {
                                        eprintln!("orchd: telegram update {uid} (try {attempt}): {e}");
                                        std::thread::sleep(Duration::from_secs(2));
                                    }
                                }
                            }
                            if !applied {
                                std::thread::sleep(Duration::from_secs(10));
                                break 'batch;
                            }
                            offset = offset.max(uid + 1);
                            let _ = bus.kv_set("tg_offset", &offset.to_string());
                        }
                    }
                    Err(e) => {
                        eprintln!("orchd: telegram poll: {e}");
                        std::thread::sleep(Duration::from_secs(5));
                    }
                }
            }
        })
        .expect("telegram thread");
}

/// One pass: open questions not yet shown, answered questions not yet marked, and new
/// messages to Murat. Stops at the first failure (retried next pass).
fn sync(bus: &Bus, api: &Api, cfg: &TgConfig) -> Result<()> {
    let Some(chat) = chat_id(bus, cfg) else {
        return Ok(());
    };
    for q in bus.questions(false)? {
        if bus.tg_msgs_for_question(q.id)?.is_empty() {
            let sent = api.call(
                "sendMessage",
                json!({ "chat_id": chat, "text": question_text(&q),
                        "reply_markup": question_keyboard(&q) }),
            )?;
            if let Some(mid) = sent["message_id"].as_i64() {
                bus.tg_map_set(mid, Some(q.id), Some(&q.from))?;
            }
        }
    }
    for q in bus.questions(true)? {
        if q.state == "open" {
            continue;
        }
        for (mid, edited) in bus.tg_msgs_for_question(q.id)? {
            if !edited {
                api.call(
                    "editMessageText",
                    json!({ "chat_id": chat, "message_id": mid, "text": question_text(&q),
                            "reply_markup": question_keyboard(&q) }),
                )?;
                bus.tg_mark_edited(mid)?;
            }
        }
    }
    let mut cursor: i64 = bus
        .kv_get("tg_msg_cursor")?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    loop {
        let rows = bus.log(Some(cursor), 100)?;
        if rows.is_empty() {
            break;
        }
        for m in rows {
            if m.to == HUMAN && m.kind == "msg" {
                let text = format!(
                    "💬 {}:\n{}\n\n↩️ Reply to answer {}.",
                    m.from, m.text, m.from
                );
                let sent = api.call("sendMessage", json!({ "chat_id": chat, "text": text }))?;
                if let Some(mid) = sent["message_id"].as_i64() {
                    bus.tg_map_set(mid, None, Some(&m.from))?;
                }
            }
            cursor = m.id;
            bus.kv_set("tg_msg_cursor", &cursor.to_string())?;
        }
    }
    Ok(())
}

fn reply(api: &Api, chat: i64, text: &str) {
    let _ = api.call("sendMessage", json!({ "chat_id": chat, "text": text }));
}

/// Only Murat may answer: a private chat whose id is the paired one, and the sender is that
/// same user (in a private chat the chat id equals the user id). Groups never qualify, even
/// when TG_CHAT is configured.
fn authorized(paired: Option<i64>, chat: &Value, from: &Value) -> bool {
    let Some(paired) = paired else { return false };
    chat["id"].as_i64() == Some(paired)
        && from["id"].as_i64() == Some(paired)
        && chat["type"].as_str().is_none_or(|t| t == "private")
}

fn handle_update(bus: &Bus, api: &Api, cfg: &TgConfig, u: &Value) -> Result<()> {
    let paired = chat_id(bus, cfg);
    let uid = u["update_id"].as_i64().unwrap_or(0);
    if let Some(cb) = u.get("callback_query") {
        let _ = api.call(
            "answerCallbackQuery",
            json!({ "callback_query_id": cb["id"] }),
        );
        if !authorized(paired, &cb["message"]["chat"], &cb["from"]) {
            return Ok(());
        }
        let chat = paired.unwrap();
        let data = cb["data"].as_str().unwrap_or("");
        let mut parts = data.split(':');
        if let (Some("a"), Some(q), Some(i)) = (parts.next(), parts.next(), parts.next()) {
            // Malformed or stale buttons are ignored (Ok), only store errors propagate.
            let (Ok(q), Ok(i)) = (q.parse::<i64>(), i.parse::<usize>()) else {
                return Ok(());
            };
            let Some(question) = bus.question(q)? else {
                return Ok(());
            };
            let Some(answer) = question.options.get(i).cloned() else {
                return Ok(());
            };
            // Idempotent: a re-delivered update finds the question answered already.
            let (won, q) = bus.answer(q, &answer, "telegram")?;
            if !won && q.answer.as_deref() != Some(answer.as_str()) {
                reply(
                    api,
                    chat,
                    &format!(
                        "#{} was already answered: {}",
                        q.id,
                        q.answer.unwrap_or_default()
                    ),
                );
            }
        }
        return Ok(());
    }
    let Some(msg) = u.get("message") else {
        return Ok(());
    };
    let chat_v = &msg["chat"];
    let chat = chat_v["id"].as_i64().unwrap_or(0);
    let text = msg["text"].as_str().unwrap_or("").trim().to_string();
    if let Some(code) = text.strip_prefix("/pair") {
        let ok = bus.kv_get("tg_pair_code")?.as_deref() == Some(code.trim())
            && bus
                .kv_get("tg_pair_expires")?
                .and_then(|v| v.parse::<i64>().ok())
                .is_some_and(|exp| now_ms() < exp)
            && chat_v["type"].as_str() == Some("private")
            && msg["from"]["id"].as_i64() == Some(chat);
        if ok {
            bus.kv_set("tg_chat", &chat.to_string())?;
            bus.kv_del("tg_pair_code")?;
            reply(api, chat, "✅ Paired. Agents' questions will arrive here.");
            bus.poke(); // send what was waiting for pairing right away
        }
        return Ok(());
    }
    if !authorized(paired, chat_v, &msg["from"]) {
        return Ok(()); // strangers get no answer at all
    }
    if text == "/open" || text == "/start" {
        let open = bus.questions(false)?;
        if open.is_empty() {
            reply(api, chat, "No open questions.");
        }
        for q in open {
            let sent = api.call(
                "sendMessage",
                json!({ "chat_id": chat, "text": question_text(&q), "reply_markup": question_keyboard(&q) }),
            )?;
            if let Some(mid) = sent["message_id"].as_i64() {
                bus.tg_map_set(mid, Some(q.id), Some(&q.from))?;
            }
        }
        return Ok(());
    }
    let replied = msg["reply_to_message"]["message_id"].as_i64();
    match replied.map(|id| bus.tg_map_get(id)).transpose()?.flatten() {
        Some((Some(q), _)) => {
            let (won, q) = bus.answer(q, &text, "telegram")?;
            if !won && q.answer.as_deref() != Some(text.as_str()) {
                reply(
                    api,
                    chat,
                    &format!(
                        "#{} was already answered: {}",
                        q.id,
                        q.answer.unwrap_or_default()
                    ),
                );
            }
        }
        Some((None, Some(agent))) => {
            // At most once per Telegram update, atomically with the message itself.
            let key = format!("tg:{uid}");
            if bus
                .send_dedup(Some(&key), HUMAN, &agent, "msg", &text, None, None)?
                .is_some()
            {
                reply(api, chat, &format!("→ sent to {agent}"));
            }
        }
        _ => reply(
            api,
            chat,
            "Reply to a question or message to answer it. /open lists open questions.",
        ),
    }
    Ok(())
}
