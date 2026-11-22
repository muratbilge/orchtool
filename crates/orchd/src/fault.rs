//! Failure injection for the integration tests: `ORCHD_FAULT=name[,name=value…]`. Without the
//! variable nothing here does anything.

use std::sync::OnceLock;

fn faults() -> &'static Vec<(String, Option<String>)> {
    static F: OnceLock<Vec<(String, Option<String>)>> = OnceLock::new();
    F.get_or_init(|| {
        std::env::var("ORCHD_FAULT")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| match s.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (s.to_string(), None),
            })
            .collect()
    })
}

pub fn on(name: &str) -> bool {
    faults().iter().any(|(k, _)| k == name)
}

pub fn value(name: &str) -> Option<&'static str> {
    faults()
        .iter()
        .find(|(k, _)| k == name)
        .and_then(|(_, v)| v.as_deref())
}

/// Dies here like `kill -9` would (no cleanup, no final save) when the fault is set.
pub fn crash_at(name: &str) {
    if on(name) {
        eprintln!("orchd: fault {name}: SIGKILL");
        // SAFETY: plain signal to ourselves.
        unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
    }
}

/// Holds a transition here for the given milliseconds (to race it in a test).
pub async fn pause_at(name: &str) {
    if let Some(ms) = value(name).and_then(|v| v.parse().ok()) {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
}
