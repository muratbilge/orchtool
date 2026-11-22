//! The layout survives an unplanned stop: orchd keeps `autosave.json` (the `restore.json`
//! format) next to its socket, written after layout changes (debounced) and on a timer, and
//! once more on SIGTERM / SIGINT while the panes still run. The next orchd restores it when
//! there is no `restore.json` from a planned restart. A plain `orch stop-daemon` removes it.
//!
//! Restore-loop guard: the file is renamed to `autosave.json.restoring` before it is restored
//! and the marker is removed once the restore returned. An orchd that finds the marker at
//! start-up did not survive restoring it: the file is kept as `autosave.json.failed` and the
//! layout does not come back.

use crate::restore::Source;
use crate::server::Daemon;
use crate::ui::UiEvent;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

pub const FILE: &str = "autosave.json";
pub const RESTORING: &str = "autosave.json.restoring";
pub const FAILED: &str = "autosave.json.failed";
/// Quiet time after a layout change before it is written (more changes in it are batched).
pub const DEBOUNCE: Duration = Duration::from_secs(2);
/// For what changes without a layout op: a pane's folder, its agent, session id, `was_working`.
pub const INTERVAL: Duration = Duration::from_secs(60);
/// The final save on SIGTERM / SIGINT gives up after this (a hung snapshot must not hold up
/// the stop; the last autosave then stays).
pub const FINAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Tests shorten the timers with these (milliseconds).
const DEBOUNCE_ENV: &str = "ORCHD_AUTOSAVE_DEBOUNCE_MS";
const INTERVAL_ENV: &str = "ORCHD_AUTOSAVE_INTERVAL_MS";

fn from_env(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(default)
}

#[derive(Default)]
pub struct Autosave {
    path: std::sync::OnceLock<PathBuf>,
    /// Set once the start-up restore is done: before that a snapshot would be the empty
    /// layout and overwrite nothing useful, so nothing is written.
    enabled: AtomicBool,
    /// Set for good once orchd stops (or `stop-daemon` forgot the layout): panes that are
    /// being killed must not be written as "gone".
    closed: AtomicBool,
    /// Held across each write, so `forget` / `close` can't interleave with one.
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The bytes last written: an unchanged snapshot is not written again.
    last: Option<Vec<u8>>,
    /// The last error logged: the same error is logged once, not every minute.
    last_err: Option<String>,
    /// Session version of the layout last written.
    version: u64,
}

impl Autosave {
    pub fn set_path(&self, path: PathBuf) {
        let _ = self.path.set(path);
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.get().map(PathBuf::as_path)
    }

    /// Writes `data` (the layout at session `version`) unless it is what the file already
    /// holds. `close` also ends autosaving (the final save on a signal) in the same critical
    /// section. A snapshot older than the one last written is dropped: snapshots are taken
    /// before this lock, so a slow periodic one must not overwrite a newer one.
    fn write(&self, data: Vec<u8>, version: u64, close: bool) {
        let enabled = self.enabled.load(Ordering::SeqCst);
        let result = if enabled {
            self.write_checked(data, version, close)
        } else {
            Ok(())
        };
        if close {
            self.closed.store(true, Ordering::SeqCst);
        }
        let mut st = self.state.lock().unwrap();
        match result {
            Ok(()) => st.last_err = None,
            Err(e) => {
                let e = e.to_string();
                if st.last_err.as_deref() != Some(&e) {
                    eprintln!("orchd: autosave failed: {e}");
                    st.last_err = Some(e);
                }
            }
        }
    }

    fn write_checked(&self, data: Vec<u8>, version: u64, _close: bool) -> std::io::Result<()> {
        let mut st = self.state.lock().unwrap();
        let Some(path) = self.path.get() else {
            return Ok(());
        };
        if self.closed.load(Ordering::SeqCst) || version < st.version {
            return Ok(());
        }
        if st.last.as_deref() == Some(&data[..]) && path.exists() {
            return Ok(());
        }
        crate::restore::write_atomic(path, &data)?;
        st.last = Some(data);
        st.version = version;
        Ok(())
    }

    fn last_written(&self) -> Option<crate::restore::Saved> {
        let st = self.state.lock().unwrap();
        serde_json::from_slice(st.last.as_deref()?).ok()
    }

    /// Ends autosaving without writing (orchd is stopping; the panes are about to die).
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// A plain `stop-daemon`: no layout for the next orchd.
    pub fn forget(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let mut st = self.state.lock().unwrap();
        st.last = None;
        if let Some(path) = self.path.get() {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => eprintln!("orchd: removing {}: {e}", path.display()),
            }
        }
    }
}

impl Daemon {
    /// Takes a snapshot and writes it if it changed. Blocking (reads /proc and the bus).
    pub(crate) fn autosave_now(&self, close: bool) {
        let a = &self.autosave;
        if !a.enabled.load(Ordering::SeqCst) || a.closed.load(Ordering::SeqCst) {
            return;
        }
        let mut saved = self.snapshot(false);
        if close {
            keep_panes_that_just_died(&mut saved, a.last_written().as_ref());
        }
        let version = saved.session.version;
        match crate::restore::layout_bytes(&saved) {
            Ok(data) => a.write(data, version, close),
            Err(e) => eprintln!("orchd: autosave: {e}"),
        }
    }

    /// The layout as it is now, written at once and confirmed (a wake commits through this):
    /// errors come back to the caller instead of the log.
    pub(crate) fn autosave_sync(&self) -> std::io::Result<()> {
        let a = &self.autosave;
        if a.closed.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("orchd is stopping"));
        }
        if crate::fault::on("autosave_write") {
            return Err(std::io::Error::other("injected autosave failure"));
        }
        let saved = self.snapshot(false);
        let data = crate::restore::layout_bytes(&saved).map_err(std::io::Error::from)?;
        a.write_checked(data, saved.session.version, false)
    }

    /// SIGTERM / SIGINT: one last snapshot while every pane still runs, then no more writes.
    pub async fn autosave_final(self: &Arc<Self>) {
        let d = Arc::clone(self);
        let job = tokio::task::spawn_blocking(move || d.autosave_now(true));
        match tokio::time::timeout(FINAL_TIMEOUT, job).await {
            Ok(Ok(())) => eprintln!("orchd: final autosave done ({FILE})"),
            Ok(Err(e)) => eprintln!("orchd: final autosave failed: {e}"),
            Err(_) => eprintln!(
                "orchd: final autosave took longer than {}s; keeping the last one",
                FINAL_TIMEOUT.as_secs()
            ),
        }
        self.autosave.close();
    }
}

/// A VM shutdown signals every process of the session at once, so an agent may already be
/// gone when orchd takes its final snapshot. Only for that final save, against the last
/// autosave, for panes still in the layout:
/// - a pane whose program died since then keeps that entry;
/// - a pane whose agent died but whose shell survived (an interactive shell ignores SIGTERM)
///   keeps that entry's agent (argv, agent, bus name, session id, agent argv, was_working),
///   with the folder and size as they are now;
/// - an agent still seen whose session id / real argv are no longer found (its process is
///   gone) keeps the known ones.
///
/// Panes that had exited before the last autosave stay left out, and the periodic autosave
/// never does this (there an agent that is gone was quit on purpose).
fn keep_panes_that_just_died(
    saved: &mut crate::restore::Saved,
    last: Option<&crate::restore::Saved>,
) {
    let Some(last) = last else { return };
    for ws in &saved.session.workspaces {
        for tab in &ws.tabs {
            for id in tab.root.panes() {
                let Some(old) = last.panes.get(&id) else {
                    continue;
                };
                let Some(now) = saved.panes.get_mut(&id) else {
                    saved.panes.insert(id, old.clone());
                    continue;
                };
                if old.agent.is_some() && now.agent.is_none() {
                    *now = crate::restore::SavedPane {
                        cwd: std::mem::take(&mut now.cwd),
                        rows: now.rows,
                        cols: now.cols,
                        ..old.clone()
                    };
                } else if now.agent.is_some() && now.agent == old.agent {
                    if now.session_id.is_none() {
                        now.session_id.clone_from(&old.session_id);
                    }
                    if now.agent_argv.is_none() {
                        now.agent_argv.clone_from(&old.agent_argv);
                    }
                }
            }
        }
    }
}

/// Start-up: `restore.json` from a planned restart if there is one (read once, as before),
/// else the autosave from an unplanned stop, guarded against a restore loop.
pub fn restore_at_start(daemon: &Arc<Daemon>, dir: &Path) {
    // Sleeping workspaces first: they decide which saved workspaces must not come back awake.
    let sleepers = daemon.load_sleepers();
    // In the layout at once: from the first moment their panes count as asleep (their open
    // questions must not expire while the rest starts).
    daemon.preload_sleepers(&sleepers);
    let mut sleepers = Some(sleepers);
    restore_awake(daemon, dir, &mut sleepers);
    if let Some(sl) = sleepers.take() {
        if let Err(e) = daemon.install_sleepers(sl) {
            eprintln!("orchd: {}", e.message);
        }
    }
    retry_unrestored(daemon, dir);
}

/// Panes that could not be started are kept in a file of their own (`unrestored-*.json`, never
/// written by an older orchd or by the autosave) and tried again at every start.
pub const UNRESTORED: &str = "unrestored-";

fn keep_unrestored(dir: &Path, saved: &crate::restore::Saved) -> std::io::Result<PathBuf> {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let mut n = 0;
    let path = loop {
        let p = dir.join(format!("{UNRESTORED}{ms}-{n}.json"));
        if !p.exists() {
            break p;
        }
        n += 1;
    };
    let data = crate::restore::layout_bytes(saved).map_err(std::io::Error::from)?;
    crate::restore::write_atomic(&path, &data)?;
    Ok(path)
}

/// After a layout was restored from `source` (renamed to its loop-guard name already): the
/// source goes only once nothing in it can be lost any more: the sleepers taken from it are
/// in their record, the panes that could not be started are in an `unrestored-*` file, and
/// the restored layout is in the autosave. Otherwise it is kept as `failed`.
fn finish(
    daemon: &Arc<Daemon>,
    dir: &Path,
    source: &Path,
    failed: &Path,
    sleepers: &mut Option<crate::sleep::Sleepers>,
    unrestored: Option<crate::restore::Saved>,
) {
    let keep = |why: String| {
        let _ = std::fs::rename(source, failed);
        eprintln!(
            "orchd: {why}; the layout file is kept as {}",
            failed.display()
        );
    };
    if let Some(sl) = sleepers.take() {
        if let Err(e) = daemon.install_sleepers(sl) {
            return keep(e.message);
        }
    }
    if let Some(u) = unrestored {
        match keep_unrestored(dir, &u) {
            Ok(p) => eprintln!(
                "orchd: {} pane(s) could not be started; kept in {} and tried again at the next start",
                u.panes.len(),
                p.display()
            ),
            Err(e) => return keep(format!("keeping the panes that did not start failed: {e}")),
        }
    }
    if let Err(e) = daemon.autosave_sync() {
        return keep(format!("saving the restored layout failed: {e}"));
    }
    let _ = std::fs::remove_file(source);
}

fn restore_awake(daemon: &Arc<Daemon>, dir: &Path, sleepers: &mut Option<crate::sleep::Sleepers>) {
    let restart = dir.join("restore.json");
    let restart_marker = dir.join("restore.json.restoring");
    let restart_failed = dir.join("restore.json.failed");
    let auto = dir.join(FILE);
    let marker = dir.join(RESTORING);
    let failed = dir.join(FAILED);
    // An orchd that died while restoring a planned restart: not again (kept for inspection).
    if restart_marker.exists() {
        let _ = std::fs::rename(&restart_marker, &restart_failed);
        eprintln!(
            "orchd: the last restore from restore.json did not finish; not restoring it again (kept as {})",
            restart_failed.display()
        );
    }
    let loop_guard = marker.exists();
    if loop_guard {
        let _ = std::fs::rename(&marker, &failed);
        eprintln!(
            "orchd: the last restore from {FILE} did not finish; not restoring it again (kept as {})",
            failed.display()
        );
    }
    if restart.exists() {
        // Read once: a layout that makes orchd fail must not come back on every start. Kept
        // under the marker name until it is no longer needed (see `finish`).
        if let Err(e) = std::fs::rename(&restart, &restart_marker) {
            eprintln!("orchd: not restoring {}: {e}", restart.display());
            return;
        }
        // The planned restart is newer than any autosave.
        let _ = std::fs::remove_file(&auto);
        match std::fs::read(&restart_marker)
            .map_err(|e| e.to_string())
            .and_then(|d| {
                serde_json::from_slice::<crate::restore::Saved>(&d).map_err(|e| e.to_string())
            }) {
            Ok(mut saved) => {
                if let Some(sl) = sleepers.as_mut() {
                    daemon.reconcile_sleepers(sl, Some(&mut saved));
                }
                let (n, unrestored) = daemon.restore_layout(saved, Source::Restart);
                eprintln!("orchd: restored the layout with {n} pane(s) from restore.json (planned restart)");
                finish(
                    daemon,
                    dir,
                    &restart_marker,
                    &restart_failed,
                    sleepers,
                    unrestored,
                );
            }
            Err(e) => {
                let _ = std::fs::rename(&restart_marker, &restart_failed);
                eprintln!(
                    "orchd: ignoring restore.json: {e} (kept as {})",
                    restart_failed.display()
                );
            }
        }
        return;
    }
    if loop_guard || !auto.exists() {
        return;
    }
    if let Err(e) = std::fs::rename(&auto, &marker) {
        eprintln!("orchd: not restoring {}: {e}", auto.display());
        return;
    }
    match std::fs::read(&marker)
        .map_err(|e| e.to_string())
        .and_then(|d| {
            serde_json::from_slice::<crate::restore::Saved>(&d).map_err(|e| e.to_string())
        }) {
        Ok(mut saved) => {
            if let Some(sl) = sleepers.as_mut() {
                daemon.reconcile_sleepers(sl, Some(&mut saved));
            }
            let (n, unrestored) = daemon.restore_layout(saved, Source::Recovery);
            eprintln!(
                "orchd: restored the layout with {n} pane(s) from {FILE} (recovery after an unplanned stop)"
            );
            finish(daemon, dir, &marker, &failed, sleepers, unrestored);
        }
        Err(e) => {
            let _ = std::fs::rename(&marker, &failed);
            eprintln!("orchd: ignoring {FILE}: {e} (kept as {})", failed.display());
        }
    }
}

/// Start-up, last: every `unrestored-*` file is tried again; each goes only once what came
/// back is in the autosave and what still did not start is in a new file.
fn retry_unrestored(daemon: &Arc<Daemon>, dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(UNRESTORED) && n.ends_with(".json"))
        })
        .collect();
    files.sort();
    for f in files {
        let saved = match std::fs::read(&f).map_err(|e| e.to_string()).and_then(|d| {
            serde_json::from_slice::<crate::restore::Saved>(&d).map_err(|e| e.to_string())
        }) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("orchd: {} is not usable ({e}); left alone", f.display());
                continue;
            }
        };
        let (n, still) = daemon.restore_more(saved);
        let mut successor = None;
        if let Some(u) = still {
            match keep_unrestored(dir, &u) {
                Ok(p) => successor = Some(p),
                Err(e) => {
                    eprintln!("orchd: keeping {} failed: {e}; it stays", f.display());
                    continue;
                }
            }
        }
        if let Err(e) = daemon.autosave_sync() {
            eprintln!(
                "orchd: saving the layout failed ({e}); {} stays",
                f.display()
            );
            if let Some(p) = successor {
                let _ = std::fs::remove_file(p);
            }
            continue;
        }
        eprintln!("orchd: {n} pane(s) of {} came back", f.display());
        let _ = std::fs::remove_file(&f);
    }
}

/// Autosaving, from now on (call after the start-up restore). The first save runs at once.
pub fn start(daemon: Arc<Daemon>) {
    if daemon.autosave.path().is_none() {
        return;
    }
    daemon.autosave.enabled.store(true, Ordering::SeqCst);
    let mut events = daemon.ui_events.subscribe();
    let debounce = from_env(DEBOUNCE_ENV, DEBOUNCE);
    let interval = from_env(INTERVAL_ENV, INTERVAL);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                ev = events.recv() => match ev {
                    // Status / title changes come often; the timer covers what they change.
                    Ok(UiEvent::Panes) => continue,
                    Ok(UiEvent::Session) | Err(RecvError::Lagged(_)) => {
                        tokio::time::sleep(debounce).await;
                        loop {
                            match events.try_recv() {
                                Ok(_) | Err(TryRecvError::Lagged(_)) => {}
                                Err(TryRecvError::Empty) => break,
                                Err(TryRecvError::Closed) => return,
                            }
                        }
                    }
                    Err(RecvError::Closed) => return,
                },
            }
            if daemon.autosave.closed.load(Ordering::SeqCst) {
                return;
            }
            // Off the request path: the snapshot reads /proc and the bus.
            let d = Arc::clone(&daemon);
            if let Err(e) = tokio::task::spawn_blocking(move || d.autosave_now(false)).await {
                eprintln!("orchd: autosave: {e}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::restore::{Saved, SavedPane};
    use orch_proto::layout::{Node, Session, Tab, Workspace};
    use std::collections::HashMap;

    fn pane(argv: &str) -> SavedPane {
        serde_json::from_value(serde_json::json!({
            "argv": [argv], "cwd": "/tmp", "rows": 20, "cols": 80
        }))
        .unwrap()
    }

    fn saved(layout: &[&str], panes: &[(&str, &str)]) -> Saved {
        let mut session = Session::default();
        for (n, id) in layout.iter().enumerate() {
            session.workspaces.push(Workspace {
                id: format!("w{n}"),
                label: format!("w{n}"),
                cwd: "/tmp".into(),
                tabs: vec![Tab {
                    id: format!("t{n}"),
                    label: String::new(),
                    root: Node::Pane {
                        pane: id.to_string(),
                    },
                    focus: id.to_string(),
                    zoomed: false,
                }],
                active_tab: 0,
            });
        }
        Saved {
            session,
            panes: panes
                .iter()
                .map(|(id, argv)| (id.to_string(), pane(argv)))
                .collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn the_final_save_keeps_panes_that_died_during_the_shutdown_only() {
        // p1 runs, p2 died with the shutdown (alive at the last autosave), p3 had exited
        // before it, p4 was closed (not in the layout any more).
        let last = saved(
            &["p1", "p2", "p3"],
            &[("p1", "a"), ("p2", "claude"), ("p4", "x")],
        );
        let mut now = saved(&["p1", "p2", "p3"], &[("p1", "a2")]);
        keep_panes_that_just_died(&mut now, Some(&last));
        let mut ids: Vec<_> = now.panes.keys().cloned().collect();
        ids.sort();
        assert_eq!(ids, ["p1", "p2"]);
        assert_eq!(
            now.panes["p1"].argv,
            ["a2"],
            "a running pane is taken as it is now"
        );
        assert_eq!(now.panes["p2"].argv, ["claude"]);

        // The shell survived the shutdown, the agent in it did not.
        let mut agent = pane("bash");
        agent.agent = Some("claude".into());
        agent.bus_name = Some("x-claude".into());
        agent.session_id = Some("11111111-1111-4111-8111-111111111111".into());
        agent.agent_argv = Some(vec!["claude".into(), "--model".into(), "m".into()]);
        agent.was_working = true;
        let mut last = saved(&["p1", "p2"], &[]);
        last.panes.insert("p1".into(), agent.clone());
        last.panes.insert("p2".into(), agent.clone());
        let mut now = saved(&["p1", "p2"], &[("p1", "bash")]);
        {
            let p1 = now.panes.get_mut("p1").unwrap();
            p1.cwd = "/srv".into();
            p1.rows = 40;
        }
        // p2: the agent is still seen (registered kind), but its process is gone.
        let mut seen = pane("bash");
        seen.agent = Some("claude".into());
        seen.bus_name = Some("x-claude".into());
        now.panes.insert("p2".into(), seen);
        keep_panes_that_just_died(&mut now, Some(&last));
        let p1 = &now.panes["p1"];
        assert_eq!(p1.agent.as_deref(), Some("claude"));
        assert_eq!(p1.bus_name.as_deref(), Some("x-claude"));
        assert_eq!(p1.session_id, agent.session_id);
        assert_eq!(p1.agent_argv, agent.agent_argv);
        assert!(p1.was_working);
        assert_eq!(
            (p1.cwd.as_str(), p1.rows),
            ("/srv", 40),
            "folder and size as now"
        );
        let p2 = &now.panes["p2"];
        assert_eq!(p2.session_id, agent.session_id);
        assert_eq!(p2.agent_argv, agent.agent_argv);
        assert!(!p2.was_working, "taken as it is now otherwise");
        // A different agent now (or a session id found now) is not overwritten.
        let mut now = saved(&["p1"], &[]);
        let mut other = pane("bash");
        other.agent = Some("codex".into());
        now.panes.insert("p1".into(), other);
        keep_panes_that_just_died(&mut now, Some(&last));
        assert!(now.panes["p1"].session_id.is_none());

        let mut first = saved(&["p1"], &[]);
        keep_panes_that_just_died(&mut first, None);
        assert!(first.panes.is_empty());
    }
}
