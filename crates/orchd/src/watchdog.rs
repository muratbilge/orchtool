//! A hang shows in orchd.log: a tokio task bumps a heartbeat every second (more often for a short test `STALL`) and a plain
//! thread (it keeps running when every runtime worker is stuck, e.g. on a lock) writes one
//! line to stderr when the heartbeat is older than `STALL`, at most once a minute, and one
//! when it is fresh again. A SIGTERM / SIGINT that the stuck runtime cannot handle still
//! ends orchd: the signal also sets a flag here, and once it has been pending for
//! `TERM_GRACE` with the runtime stuck the thread logs and exits (status 1, no final save;
//! the last autosave stays, and the panes get SIGHUP when their PTYs close).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// The runtime counts as stuck after this long without a heartbeat.
const STALL: Duration = Duration::from_secs(15);
/// Repeat the "not responding" line at most this often.
const REPEAT: Duration = Duration::from_secs(60);
/// A pending SIGTERM / SIGINT gets this long for the normal stop before the thread exits.
const TERM_GRACE: Duration = Duration::from_secs(5);
/// Tests shorten `STALL` with this (milliseconds).
const STALL_ENV: &str = "ORCHD_WATCHDOG_STALL_MS";
/// Debug builds only: block both runtime workers for this long (ms), one second after start.
#[cfg(debug_assertions)]
const TEST_STALL_ENV: &str = "ORCHD_TEST_STALL_RUNTIME_MS";

fn base() -> Instant {
    static BASE: OnceLock<Instant> = OnceLock::new();
    *BASE.get_or_init(Instant::now)
}

fn now_ms() -> u64 {
    base().elapsed().as_millis() as u64
}

/// Call from inside the runtime, after the signal streams are set up.
pub fn start() {
    let beat = Arc::new(AtomicU64::new(now_ms()));
    let term = Arc::new(AtomicBool::new(false));
    for sig in [libc::SIGTERM, libc::SIGINT] {
        let flag = Arc::clone(&term);
        // SAFETY: the action only stores to an atomic, which is async-signal-safe. It runs
        // next to tokio's own handler (both are registered through signal-hook-registry).
        let r = unsafe {
            signal_hook_registry::register(sig, move || flag.store(true, Ordering::SeqCst))
        };
        if let Err(e) = r {
            eprintln!("orchd: watchdog: signal {sig}: {e}");
        }
    }
    let stall = std::env::var(STALL_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(STALL, Duration::from_millis);
    // Well inside `stall`, also for the short test values.
    let every = Duration::from_secs(1)
        .min(stall / 5)
        .max(Duration::from_millis(10));
    let b = Arc::clone(&beat);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            b.store(now_ms(), Ordering::SeqCst);
        }
    });
    let spawned = std::thread::Builder::new()
        .name("orchd-watchdog".into())
        .spawn(move || watch(&beat, &term, stall));
    if let Err(e) = spawned {
        eprintln!("orchd: watchdog: {e}");
    }
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var(TEST_STALL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        // One blocking task per worker (orchd runs two).
        for _ in 0..2 {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                std::thread::sleep(Duration::from_millis(ms));
            });
        }
    }
}

fn watch(beat: &AtomicU64, term: &AtomicBool, stall: Duration) {
    let mut stuck_since_log: Option<Instant> = None;
    let mut term_seen: Option<Instant> = None;
    loop {
        std::thread::sleep(
            Duration::from_millis(250)
                .min(stall / 4)
                .max(Duration::from_millis(10)),
        );
        let age = Duration::from_millis(now_ms().saturating_sub(beat.load(Ordering::SeqCst)));
        let stuck = age >= stall;
        if stuck {
            if stuck_since_log.is_none_or(|t| t.elapsed() >= REPEAT) {
                eprintln!(
                    "orchd: runtime not responding for {} s (possible deadlock)",
                    age.as_secs()
                );
                stuck_since_log = Some(Instant::now());
            }
        } else if stuck_since_log.take().is_some() {
            eprintln!("orchd: runtime responding again");
        }
        if term.load(Ordering::SeqCst) {
            let since = *term_seen.get_or_insert_with(Instant::now);
            if stuck && since.elapsed() >= TERM_GRACE {
                eprintln!(
                    "orchd: stop signal not handled for {} s, runtime stuck: exiting without the final save",
                    since.elapsed().as_secs()
                );
                // SAFETY: plain exit; nothing here may wait on the stuck threads' locks.
                unsafe { libc::_exit(1) };
            }
        }
    }
}
