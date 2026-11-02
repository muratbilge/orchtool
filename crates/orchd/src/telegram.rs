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

use crate::bus::{now_ms, Bus, BusEvent};
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

/// Start the bridge: one task forwards bus events, one blocking thread long-polls updates.
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

    // Outgoing: bus → Telegram.
    {
        let bus = Arc::clone(&bus);
        let api = Arc::clone(&api);
        let cfg = cfg.clone();
        let mut rx = bus.subscribe();
        tokio::spawn(async move {
            loop {
                let ev = match rx.recv().await {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                let (bus, api, cfg) = (Arc::clone(&bus), Arc::clone(&api), cfg.clone());
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) = forward(&bus, &api, &cfg, ev) {
                        eprintln!("orchd: telegram send: {e}");
                    }
                })
                .await;
            }
        });
    }

    // Incoming: long-poll getUpdates.
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
                        for u in updates.as_array().cloned().unwrap_or_default() {
                            offset = offset.max(u["update_id"].as_i64().unwrap_or(0) + 1);
                            let _ = bus.kv_set("tg_offset", &offset.to_string());
                            if let Err(e) = handle_update(&bus, &api, &cfg, &u) {
                                eprintln!("orchd: telegram update: {e}");
                            }
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

fn forward(bus: &Bus, api: &Api, cfg: &TgConfig, ev: BusEvent) -> Result<()> {
    let Some(chat) = chat_id(bus, cfg) else {
        return Ok(());
    };
    match ev {
        BusEvent::Question(q) if q.state == "open" => {
            let sent = api.call(
                "sendMessage",
                json!({ "chat_id": chat, "text": question_text(&q),
                        "reply_markup": question_keyboard(&q) }),
            )?;
            if let Some(mid) = sent["message_id"].as_i64() {
                bus.tg_map_set(mid, Some(q.id), Some(&q.from))?;
            }
        }
        BusEvent::Question(q) => {
            // Answered (from anywhere): update every Telegram copy of the question.
            for mid in bus.tg_msgs_for_question(q.id)? {
                let _ = api.call(
                    "editMessageText",
                    json!({ "chat_id": chat, "message_id": mid, "text": question_text(&q),
                            "reply_markup": question_keyboard(&q) }),
                );
            }
        }
        BusEvent::Message(m) if m.to == HUMAN && m.kind == "msg" => {
            let text = format!(
                "💬 {}:\n{}\n\n↩️ Reply to answer {}.",
                m.from, m.text, m.from
            );
            let sent = api.call("sendMessage", json!({ "chat_id": chat, "text": text }))?;
            if let Some(mid) = sent["message_id"].as_i64() {
                bus.tg_map_set(mid, None, Some(&m.from))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn reply(api: &Api, chat: i64, text: &str) {
    let _ = api.call("sendMessage", json!({ "chat_id": chat, "text": text }));
}

fn handle_update(bus: &Bus, api: &Api, cfg: &TgConfig, u: &Value) -> Result<()> {
    let paired = chat_id(bus, cfg);
    if let Some(cb) = u.get("callback_query") {
        let chat = cb["message"]["chat"]["id"].as_i64();
        let _ = api.call(
            "answerCallbackQuery",
            json!({ "callback_query_id": cb["id"] }),
        );
        if chat.is_none() || chat != paired {
            return Ok(());
        }
        let data = cb["data"].as_str().unwrap_or("");
        let mut parts = data.split(':');
        if let (Some("a"), Some(q), Some(i)) = (parts.next(), parts.next(), parts.next()) {
            let (q, i): (i64, usize) = (q.parse()?, i.parse()?);
            let question = bus.question(q)?.ok_or_else(|| anyhow!("no question {q}"))?;
            let answer = question
                .options
                .get(i)
                .cloned()
                .ok_or_else(|| anyhow!("no option {i}"))?;
            let (won, q) = bus.answer(q, &answer, "telegram")?;
            if !won {
                reply(
                    api,
                    chat.unwrap(),
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
    let chat = msg["chat"]["id"].as_i64().unwrap_or(0);
    let text = msg["text"].as_str().unwrap_or("").trim().to_string();
    if let Some(code) = text.strip_prefix("/pair") {
        let ok = bus.kv_get("tg_pair_code")?.as_deref() == Some(code.trim())
            && bus
                .kv_get("tg_pair_expires")?
                .and_then(|v| v.parse::<i64>().ok())
                .is_some_and(|exp| now_ms() < exp)
            && msg["chat"]["type"].as_str() == Some("private");
        if ok {
            bus.kv_set("tg_chat", &chat.to_string())?;
            bus.kv_del("tg_pair_code")?;
            reply(api, chat, "✅ Paired. Agents' questions will arrive here.");
        }
        return Ok(());
    }
    if Some(chat) != paired {
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
            if !won {
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
            bus.send(HUMAN, &agent, "msg", &text, None, None)?;
            reply(api, chat, &format!("→ sent to {agent}"));
        }
        _ => reply(
            api,
            chat,
            "Reply to a question or message to answer it. /open lists open questions.",
        ),
    }
    Ok(())
}
