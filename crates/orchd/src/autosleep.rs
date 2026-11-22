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

type Sig = Vec<(String, String, u64)>;
type Sample = Result<(Sig, bool), String>;

/// One look at a workspace: its activity signature and whether it is in use right now.
/// `Err` when anything could not be read (then it is not idle).
fn sample(
    d: &Daemon,
    session: &orch_proto::layout::Session,
    ws_id: &str,
    working: &HashSet<String>,
    own_transition: bool,
) -> Sample {
    let w = session
        .workspace(ws_id)
        .ok_or_else(|| "the workspace is gone".to_string())?;
    let mut active = session.raised.as_ref().is_some_and(|r| r.workspace == w.id)
        || (!own_transition && d.ws_in_transition(&w.id));
    let deferred = d.deferred.lock().unwrap().clone();
    let mut sig = Vec::new();
    for id in w.tabs.iter().flat_map(|t| t.root.panes()) {
        let p = d.find(&id).map_err(|_| format!("{id} is gone"))?;
        if p.exit_code().is_some() {
            return Err(format!("{id} has exited"));
        }
        let text = p.visible_text();
        let (inputs, clients) = p.activity();
        // A usage-limit continue still due: it must not be stranded asleep.
        let pending = deferred.contains_key(&id)
            || crate::limits::info(&id).is_some_and(|(reset, on)| reset.is_some() && on);
        if clients > 0
            || pending
            || detect::detect(&text, p.agent(&text).as_deref()) == Detected::Working
            || working.contains(&format!("orch:{id}"))
        {
            active = true;
        }
        sig.push((id, orch_proto::api::text_hash(&text), inputs));
    }
    Ok((sig, active))
}

fn working_panes(d: &Daemon) -> Result<HashSet<String>, String> {
    Ok(d.bus
        .agents()
        .map_err(|e| format!("bus: {e:#}"))?
        .into_iter()
        .filter(|a| a.status == "working")
        .filter_map(|a| a.pane)
        .collect())
}

/// What auto-sleep saw when it decided; checked again right before the sleep commits.
#[derive(Clone)]
pub struct AutoCheck {
    pub period: Duration,
    sig: Sig,
}

impl AutoCheck {
    /// Still idle exactly as sampled, still enabled with the same period, not exempt.
    pub fn still_idle(&self, d: &Daemon, ws_id: &str) -> Result<(), String> {
        if d.auto_sleep_ms() != Some(self.period.as_millis() as u64) {
            return Err("auto-sleep was changed".into());
        }
        let exempt = d
            .bus
            .kv_get(&format!("{KEEP_AWAKE}{ws_id}"))
            .map_err(|e| format!("bus: {e:#}"))?;
        if exempt.is_some() {
            return Err("it is kept awake".into());
        }
        let working = working_panes(d)?;
        let session = d.session();
        // The sleep asking holds the workspace's reservation itself.
        let (sig, active) = sample(d, &session, ws_id, &working, true)?;
        if active {
            return Err("it is in use".into());
        }
        if sig != self.sig {
            return Err("there was activity".into());
        }
        Ok(())
    }
}

async fn check(d: &Arc<Daemon>) {
    let Some(ms) = d.auto_sleep_ms() else {
        d.auto_sleep.track.lock().unwrap().clear();
        return;
    };
    let period = Duration::from_millis(ms);
    let gen = d.auto_sleep.gen.load(Ordering::SeqCst);
    // A read that fails decides nothing: every period starts again.
    let reads = working_panes(d).and_then(|w| {
        d.bus
            .kv_keys(KEEP_AWAKE)
            .map(|k| (w, k.into_iter().collect::<HashSet<String>>()))
            .map_err(|e| format!("bus: {e:#}"))
    });
    let (working, keep) = match reads {
        Ok(r) => r,
        Err(e) => {
            eprintln!("orchd: auto-sleep: {e}");
            d.auto_sleep.track.lock().unwrap().clear();
            return;
        }
    };
    let session = d.session();
    // Read every pane first (pane / terminal locks), then the tracking table (a leaf).
    let seen: Vec<(String, Sample)> = session
        .workspaces
        .iter()
        .map(|w| (w.id.clone(), sample(d, &session, &w.id, &working, false)))
        .collect();
    let now = Instant::now();
    let mut due = Vec::new();
    {
        let mut track = d.auto_sleep.track.lock().unwrap();
        if d.auto_sleep.gen.load(Ordering::SeqCst) != gen {
            return;
        }
        track.retain(|id, _| seen.iter().any(|(w, _)| w == id));
        for (id, r) in seen {
            let (sig, active) = match r {
                Ok(r) => r,
                Err(_) => {
                    track.remove(&id);
                    continue;
                }
            };
            let t = track.entry(id.clone()).or_insert_with(|| Track {
                since: now,
                sig: sig.clone(),
            });
            if active || t.sig != sig {
                t.since = now;
                t.sig = sig;
            } else if now.duration_since(t.since) >= period && !keep.contains(&id) {
                due.push((id, t.sig.clone()));
            }
        }
    }
    for (id, sig) in due {
        if d.auto_sleep.gen.load(Ordering::SeqCst) != gen {
            return;
        }
        // The same checks as a plain `orch ws sleep` (never forced), then this sample again.
        let check = AutoCheck { period, sig };
        let result = d.sleep_workspace(&id, false, false, Some(check)).await;
        let mut track = d.auto_sleep.track.lock().unwrap();
        match result {
            Ok(_) => {
                track.remove(&id);
            }
            Err(e) => {
                // Busy, in use or changed: a new full period.
                if let Some(t) = track.get_mut(&id) {
                    t.since = Instant::now();
                }
                if !matches!(e.code, "ws_busy" | "auto_aborted" | "ws_transition") {
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
