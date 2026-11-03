//! Continue after a usage-limit reset (herdr needed a cron script for this; orchd sees every
//! screen itself). When an agent's screen ends with its limit notice ("You've hit your usage
//! limit … try again at 5:03 AM"), the pane's status becomes `limited` and Murat gets a
//! message; one minute after the reset time orchd types a continue message into the pane,
//! once per pause. On by default; `orch auto-continue <pane> off` turns it off for a pane.

use crate::detect::{self, Detected};
use crate::server::Daemon;
use orch_proto::api::HUMAN;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// How often screens are checked for limit notices.
const CHECK_EVERY: Duration = Duration::from_secs(10);
/// Typed this long after the reset time, so the agent's own clock has surely passed it.
const GRACE_SECS: i64 = 60;
/// Between the text and its Enter (Codex takes a CR right after text for part of a paste).
const ENTER_AFTER: Duration = Duration::from_millis(300);

pub const CONTINUE_TEXT: &str = "Your usage limit has reset. Continue exactly where you stopped: \
re-read your task file (HANDOFF.md / the CODEX-*.md brief), check git status, finish the current \
step and go on with the plan. Same rules as before.";

/// One pause of one pane.
#[derive(Debug, Clone)]
struct Pause {
    /// The reset time as the agent wrote it ("" if it gave none).
    when: String,
    /// That time as unix seconds, when it could be read.
    reset: Option<i64>,
    told: bool,
    continued: bool,
}

#[derive(Default)]
struct State {
    pauses: HashMap<String, Pause>,
    /// Panes with auto-continue switched off.
    off: HashMap<String, bool>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// What `pane.info()` reports: (reset time as unix secs if known, auto-continue on).
pub fn info(pane: &str) -> Option<(Option<i64>, bool)> {
    let s = state().lock().unwrap();
    let on = !s.off.get(pane).copied().unwrap_or(false);
    s.pauses.get(pane).map(|p| (p.reset, on))
}

/// Auto-continue for a pane: set it (Some) and/or read it.
pub fn auto_continue(pane: &str, set: Option<bool>) -> bool {
    let mut s = state().lock().unwrap();
    if let Some(on) = set {
        if on {
            s.off.remove(pane);
        } else {
            s.off.insert(pane.to_string(), true);
        }
    }
    !s.off.get(pane).copied().unwrap_or(false)
}

fn clock(t: i64) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = t as libc::time_t;
    // SAFETY: plain libc time conversion on stack values.
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub fn start(daemon: Arc<Daemon>) {
    tokio::spawn(async move {
        // Tests check faster (ORCHD_LIMIT_CHECK_MS).
        let every = std::env::var("ORCHD_LIMIT_CHECK_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .map_or(CHECK_EVERY, Duration::from_millis);
        let mut tick = tokio::time::interval(every);
        loop {
            tick.tick().await;
            check(&daemon).await;
        }
    });
}

/// One pass over every pane: note new pauses, forget ended ones, continue due ones.
pub async fn check(daemon: &Daemon) {
    let t = now();
    let agents = daemon.bus.agents().unwrap_or_default();
    let name_of = |pane: &str, kind: &str| {
        agents
            .iter()
            .find(|a| a.pane.as_deref() == Some(format!("orch:{pane}").as_str()))
            .map(|a| a.name.clone())
            .unwrap_or_else(|| format!("{kind} in {pane}"))
    };
    let mut due = Vec::new();
    let mut tell = Vec::new();
    {
        let mut s = state().lock().unwrap();
        let mut seen = Vec::new();
        for pane in daemon.all_panes() {
            if pane.exit_code().is_some() {
                continue;
            }
            let text = pane.visible_text();
            let Some(kind) = pane.agent(&text) else {
                continue;
            };
            if detect::detect(&text, Some(&kind)) != Detected::Limited {
                continue;
            }
            let when = detect::limit_notice(&text).unwrap_or_default();
            seen.push(pane.id.clone());
            let on = !s.off.get(&pane.id).copied().unwrap_or(false);
            let p = s.pauses.entry(pane.id.clone()).or_insert_with(|| Pause {
                when: when.clone(),
                reset: detect::reset_unix(&when, t),
                told: false,
                continued: false,
            });
            if p.when != when {
                // A new notice (the limit came back with another reset time): a new pause.
                *p = Pause {
                    when: when.clone(),
                    reset: detect::reset_unix(&when, t),
                    told: false,
                    continued: false,
                };
            }
            let name = name_of(&pane.id, &kind);
            if !p.told {
                p.told = true;
                tell.push((
                    name.clone(),
                    match (p.reset, on) {
                        (Some(r), true) => format!(
                            "{name} hit its usage limit; orchd types 'continue' at {} (orch auto-continue {} off to stop that)",
                            clock(r + GRACE_SECS),
                            pane.id
                        ),
                        (Some(r), false) => format!(
                            "{name} hit its usage limit; it resets at {} (auto-continue is off for {})",
                            clock(r),
                            pane.id
                        ),
                        (None, _) => format!(
                            "{name} hit its usage limit; no reset time on its screen, so continue it yourself"
                        ),
                    },
                ));
            }
            if on && !p.continued && p.reset.is_some_and(|r| t >= r + GRACE_SECS) {
                p.continued = true;
                due.push((pane, name));
            }
        }
        // Panes no longer limited: their pause is over.
        s.pauses.retain(|id, _| seen.contains(id));
    }
    for (from, text) in tell {
        let _ = daemon
            .bus
            .send(&from, HUMAN, "msg", &text, Some("limits"), None);
    }
    for (pane, name) in due {
        let sent = pane.write(CONTINUE_TEXT.as_bytes().to_vec()) && {
            tokio::time::sleep(ENTER_AFTER).await;
            pane.write(b"\r".to_vec())
        };
        let text = if sent {
            format!("limit reset: typed 'continue' into {name} ({})", pane.id)
        } else {
            format!(
                "limit reset, but {name} ({}) does not read its input: continue it yourself",
                pane.id
            )
        };
        let _ = daemon
            .bus
            .send(&name, HUMAN, "msg", &text, Some("limits"), None);
    }
}
