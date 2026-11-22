//! Auto-sleep (Murat, 2026-11-22): a workspace idle for the set time is put to sleep, exactly
//! as `orch ws sleep` without `--force` would (a busy one is refused and gets a new period).
//! Off by default; `orch auto-sleep <duration>|off` sets it, kept in the bus database (`kv`).
//! `orch ws keep-awake <ws> on` exempts a workspace.
//!
//! "Idle" for the whole period means: no agent in it working (screen detection or bus status),
//! no change of any of its panes' screens (the same normalised hash `orch read --hash` uses, so
//! spinners don't count), no input written into any of its panes, no client attached to any of
//! its panes (an open TUI showing it, `orch attach`), and it is not the workspace last raised
//! (`orch focus`, Kabuk, a TUI's own switch). The period starts again on any of these, and for
//! a workspace that was just created or woken. After an orchd start every workspace starts a
//! full period (nothing is tracked yet), and so it does when the setting changes.

use crate::detect::{self, Detected};
use crate::server::Daemon;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// `kv` key of the period in ms (absent = off).
pub const KEY: &str = "auto_sleep_ms";
/// `kv` key prefix of exempt workspaces (`keep_awake:<workspace id>`).
pub const KEEP_AWAKE: &str = "keep_awake:";
/// How often workspaces are looked at (tests: ORCHD_AUTO_SLEEP_TICK_MS).
const TICK: Duration = Duration::from_secs(15);

#[derive(Default)]
pub struct State {
    /// Bumped when the setting changes: every workspace starts a new period.
    gen: AtomicU64,
    /// Leaf lock (nothing else is taken under it).
    track: Mutex<HashMap<String, Track>>,
}

struct Track {
    since: Instant,
    /// (pane, screen hash, inputs written): any change is activity.
    sig: Vec<(String, String, u64)>,
}

/// "1h30m", "45s", "500ms".
pub fn duration_words(d: Duration) -> String {
    let ms = d.as_millis() as u64;
    if !ms.is_multiple_of(1000) {
        return format!("{ms}ms");
    }
    let mut s = ms / 1000;
    let mut out = String::new();
    for (unit, n) in [("d", 86_400), ("h", 3600), ("m", 60), ("s", 1)] {
        if s >= n {
            out.push_str(&format!("{}{unit}", s / n));
            s %= n;
        }
    }
    if out.is_empty() {
        "0s".into()
    } else {
        out
    }
}

impl Daemon {
    pub fn auto_sleep_ms(&self) -> Option<u64> {
        self.bus
            .kv_get(KEY)
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .filter(|ms| *ms > 0)
    }

    /// 0 = off.
    pub fn set_auto_sleep(&self, ms: u64) -> anyhow::Result<()> {
        let v = ms.to_string();
        self.bus.kv_set(KEY, (ms > 0).then_some(v.as_str()))?;
        self.auto_sleep.gen.fetch_add(1, Ordering::SeqCst);
        self.auto_sleep.track.lock().unwrap().clear();
        Ok(())
    }
}

pub fn start(daemon: Arc<Daemon>) {
    let tick = std::env::var("ORCHD_AUTO_SLEEP_TICK_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(TICK, Duration::from_millis);
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(tick);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            iv.tick().await;
            if daemon.stopping.load(Ordering::SeqCst) {
                return;
            }
            check(&daemon).await;
        }
    });
}

async fn check(d: &Arc<Daemon>) {
    let Some(ms) = d.auto_sleep_ms() else {
        d.auto_sleep.track.lock().unwrap().clear();
        return;
    };
    let period = Duration::from_millis(ms);
    let gen = d.auto_sleep.gen.load(Ordering::SeqCst);
    let session = d.session();
    let raised = session.raised.as_ref().map(|r| r.workspace.clone());
    let working: HashSet<String> = d
        .bus
        .agents()
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.status == "working")
        .filter_map(|a| a.pane)
        .collect();
    let keep: HashSet<String> = d
        .bus
        .kv_keys(KEEP_AWAKE)
        .unwrap_or_default()
        .into_iter()
        .collect();
    // Read every pane first (pane / terminal locks), then the tracking table (a leaf).
    let mut seen = Vec::new();
    for w in &session.workspaces {
        let mut active = raised.as_deref() == Some(w.id.as_str());
        let mut sig = Vec::new();
        for id in w.tabs.iter().flat_map(|t| t.root.panes()) {
            match d.find(&id) {
                Ok(p) if p.exit_code().is_none() => {
                    let text = p.visible_text();
                    let (inputs, clients) = p.activity();
                    if clients > 0
                        || detect::detect(&text, p.agent(&text).as_deref()) == Detected::Working
                        || working.contains(&format!("orch:{id}"))
                    {
                        active = true;
                    }
                    sig.push((id, orch_proto::api::text_hash(&text), inputs));
                }
                _ => sig.push((id, String::new(), 0)),
            }
        }
        seen.push((w.id.clone(), sig, active));
    }
    let now = Instant::now();
    let mut due = Vec::new();
    {
        let mut track = d.auto_sleep.track.lock().unwrap();
        if d.auto_sleep.gen.load(Ordering::SeqCst) != gen {
            return;
        }
        track.retain(|id, _| seen.iter().any(|(w, _, _)| w == id));
        for (id, sig, active) in seen {
            let t = track.entry(id.clone()).or_insert_with(|| Track {
                since: now,
                sig: sig.clone(),
            });
            if active || t.sig != sig {
                t.since = now;
                t.sig = sig;
            } else if now.duration_since(t.since) >= period && !keep.contains(&id) {
                due.push(id);
            }
        }
    }
    for id in due {
        if d.auto_sleep.gen.load(Ordering::SeqCst) != gen {
            return;
        }
        // The same checks as a plain `orch ws sleep`: never forced.
        let result = d.sleep_workspace(&id, false, false, Some(period)).await;
        let mut track = d.auto_sleep.track.lock().unwrap();
        match result {
            Ok(_) => {
                track.remove(&id);
            }
            Err(e) => {
                // Busy (a dialog, a question, a task, a job): try again after a full period.
                if let Some(t) = track.get_mut(&id) {
                    t.since = Instant::now();
                }
                if e.code != "ws_busy" {
                    eprintln!("orchd: auto-sleep {id}: {}", e.message);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_back() {
        assert_eq!(duration_words(Duration::from_secs(5400)), "1h30m");
        assert_eq!(duration_words(Duration::from_secs(45)), "45s");
        assert_eq!(duration_words(Duration::from_millis(1500)), "1500ms");
        assert_eq!(duration_words(Duration::from_secs(86_400)), "1d");
    }
}
