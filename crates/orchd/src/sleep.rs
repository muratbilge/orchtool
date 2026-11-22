//! Workspaces that sleep and wake (`orch ws sleep|wake`). A sleeping workspace keeps its
//! layout (tabs, splits, sizes, labels, ids) and, per pane, what is needed to start it again;
//! its programs end and their memory is free.
//!
//! Durability (see `sleepstore.rs` for the files):
//! - **Sleep** reserves the workspace (no layout change, input, task or nudge reaches it), checks
//!   it, moves it into `Session::sleeping`, and writes the sleeper record durably. Only once that
//!   write is confirmed are its programs ended; a failed write puts the workspace back and kills
//!   nothing. A crash after the write: the next orchd keeps it asleep (the sleeper record wins
//!   over a stale awake layout).
//! - **Wake** starts every pane, checks the agents really started, writes the old→new pane map
//!   into the record (`wake`), moves the owners (running tasks, bus names) to the new panes,
//!   writes the awake layout durably, marks the record `committed`, and only then drops the
//!   sleeper. A crash before `committed`: the owners move back to the old pane ids and it stays
//!   asleep; after: the awake layout wins. Any failure before the commit undoes the steps done.
//!
//! What waits for a sleeping agent is kept: unread mail (it is told on wake), queued tasks (no
//! agent there to dispatch to), running tasks (they follow the pane), open questions (they don't
//! expire), its bus name (reserved for it), and a due usage-limit continue. Nothing wakes a
//! workspace by itself; `--wake` on a message or task asks for it.

use crate::detect::{self, Detected};
use crate::pane::Pane;
use crate::server::{err, ApiError, Daemon};
use orch_proto::api::{NewPane, RegisterAgent, SleepPlan, HUMAN};
use orch_proto::layout::{
    Node, SavedPane, Session, Sleeping, SleptPane, Tab, WakeState, Workspace,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

/// Typed into an agent that was put to sleep with `--force` while it was busy.
pub const SLEEP_NOTE: &str = "Your orch workspace was put to sleep while you were busy and has \
just been woken; your conversation was resumed. Your last tool call, any running command or \
subagent, and any open permission dialog were lost: check their outcome, then continue where \
you stopped. If you were waiting for Murat's answer to an orch question, call wait_for_answer \
with its id.";

/// The agent's "continue" after its usage limit reset, the same delay as `limits.rs`.
const LIMIT_GRACE_SECS: i64 = 60;
/// After starting a woken agent, how long it must stay up to count as started.
const WAKE_GRACE: Duration = Duration::from_millis(1500);
/// Stands for "no verified conversation" while building an agent's resume command.
const PICK: &str = "\u{1}orch-pick";

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn hhmm(t: i64) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = t as libc::time_t;
    // SAFETY: plain libc time conversion on stack values.
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

fn shell_pane(sp: &SavedPane) -> bool {
    sp.argv.is_empty() || crate::restore::is_shell(&sp.argv[0])
}

/// What a woken pane runs. Agents resume only a **verified** conversation (the session id the
/// agent's hook or process reported); without one Codex and Claude come back at their resume
/// picker, never at "the latest conversation in this folder" (another agent may have made a
/// newer one while this one slept) and never by replaying a saved command line.
pub fn wake_argv(sp: &SavedPane) -> (Vec<String>, Vec<(String, String)>) {
    let Some(kind) = sp
        .agent
        .as_deref()
        .filter(|a| matches!(*a, "claude" | "codex"))
    else {
        return (sp.argv.clone(), Vec::new());
    };
    let in_shell = shell_pane(sp);
    let base = sp
        .agent_argv
        .clone()
        .or_else(|| (!in_shell).then(|| sp.argv.clone()))
        .or_else(|| {
            in_shell
                .then(|| crate::resume_argv::shell_agent(&sp.argv, kind))
                .flatten()
        })
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| vec![kind.to_string()]);
    let target = sp.session_id.as_deref().unwrap_or(PICK);
    let mut argv = crate::resume_argv::rewrite(&base, kind, Some(target))
        .or_else(|| crate::resume_argv::rewrite(&base[..1], kind, Some(target)))
        .unwrap_or_else(|| {
            let flag = if kind == "claude" {
                "--resume"
            } else {
                "resume"
            };
            vec![kind.to_string(), flag.to_string(), target.to_string()]
        });
    argv.retain(|a| a != PICK);
    if in_shell {
        let shell = sp
            .argv
            .first()
            .cloned()
            .unwrap_or_else(|| "/bin/bash".into());
        let name = sp
            .bus_name
            .as_ref()
            .map(|n| format!("ORCH_AGENT={} ", shell_words::quote(n)))
            .unwrap_or_default();
        let script = format!(
            "{name}{}; exec {}",
            shell_words::join(&argv),
            shell_words::quote(&shell)
        );
        return (vec![shell, "-lc".into(), script], Vec::new());
    }
    let env = sp
        .bus_name
        .as_ref()
        .map(|n| vec![("ORCH_AGENT".to_string(), n.clone())])
        .unwrap_or_default();
    (argv, env)
}

/// How an agent will come back, in words (for the plan).
fn resume_words(sp: &SavedPane) -> String {
    let kind = sp.agent.as_deref().unwrap_or("agent");
    match &sp.session_id {
        Some(id) => format!("{kind} resumes conversation {id}"),
        None => format!(
            "{kind} comes back at its resume picker (no verified session id: someone has to pick)"
        ),
    }
}

/// The program a woken agent pane runs as its agent (inside its shell wrapper).
fn agent_exe(sp: &SavedPane) -> String {
    let argv = wake_argv(sp).0;
    let cmd = if shell_pane(sp) {
        argv.get(2)
            .and_then(|script| {
                shell_words::split(script.split("; exec").next().unwrap_or_default()).ok()
            })
            .and_then(|w| w.into_iter().find(|w| !w.contains('=')))
            .unwrap_or_default()
    } else {
        argv.first().cloned().unwrap_or_default()
    };
    std::path::Path::new(&cmd)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or(cmd)
}

/// The pane's shell has the agent itself as a child (its process name is the agent's
/// program, as the kernel shortens it to 15 bytes): a shell that started something else does
/// not count.
fn has_agent_child(pid: u32, exe: &str) -> bool {
    let want: String = exe.chars().take(15).collect();
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .any(|c| {
            std::fs::read_to_string(format!("/proc/{c}/comm")).is_ok_and(|comm| comm.trim() == want)
        })
}

/// `ws` with its panes renamed by `map`; panes not in it are dropped (a tab without any goes,
/// None when nothing is left).
pub(crate) fn mapped(ws: &Workspace, map: &HashMap<String, String>) -> Option<Workspace> {
    fn node(n: &Node, map: &HashMap<String, String>) -> Node {
        match n {
            Node::Pane { pane } => Node::Pane {
                pane: map.get(pane).cloned().unwrap_or_default(),
            },
            Node::Split { dir, ratio, a, b } => Node::Split {
                dir: *dir,
                ratio: *ratio,
                a: Box::new(node(a, map)),
                b: Box::new(node(b, map)),
            },
        }
    }
    let mut s = Session {
        workspaces: vec![ws.clone()],
        ..Default::default()
    };
    for t in &mut s.workspaces[0].tabs {
        t.root = node(&t.root, map);
        t.focus = map.get(&t.focus).cloned().unwrap_or_default();
    }
    while s.remove_pane("") {}
    let mut w = s.workspaces.pop()?;
    for t in &mut w.tabs {
        if t.focus.is_empty() || !t.root.contains(&t.focus) {
            t.focus = t.root.panes().first().cloned().unwrap_or_default();
        }
    }
    Some(w)
}

fn unique_label(s: &Session, base: &str) -> String {
    let mut label = base.to_string();
    let mut n = 2;
    while s.label_taken(&label, None) {
        label = format!("{base}-{n}");
        n += 1;
    }
    label
}

/// A sleep's plan, each pane's record, and the pane ids the workspace held.
type Inspected = (SleepPlan, HashMap<String, SleptPane>, Vec<String>);

/// The sleepers read at start-up, until they are installed in the layout.
#[derive(Default)]
pub struct Sleepers {
    pub list: Vec<Sleeping>,
    next_id: u64,
    reconciled: bool,
    dirty: bool,
}

/// What a commit changes: the whole new sleeper list, awake workspaces taken out (by id) and
/// put in (at a position).
pub(crate) struct Change {
    pub sleeping: Vec<Sleeping>,
    pub remove: Vec<String>,
    pub add: Vec<(usize, Workspace)>,
}

impl Change {
    pub fn sleepers(sleeping: Vec<Sleeping>) -> Self {
        Change {
            sleeping,
            remove: Vec::new(),
            add: Vec::new(),
        }
    }
}

/// A workspace going to sleep or waking.
pub(crate) struct Transit {
    /// Its panes (old ones, and the new ones of a wake): no input, kill or attach reaches them
    /// until the transition is over, also after the workspace left the awake layout.
    panes: HashSet<String>,
    /// An automatic sleep: a person arriving (attach, input, a layout change) stops it instead
    /// of being refused, as long as it has not started to commit.
    auto: bool,
    aborted: bool,
    committing: bool,
}

/// A request writing into a pane (see `admit_op`).
pub(crate) struct OpGuard {
    d: Arc<Daemon>,
    pane: String,
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        let mut m = self.d.inflight.lock().unwrap();
        if let Some(n) = m.get_mut(&self.pane) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&self.pane);
            }
        }
    }
}

/// Held while a workspace goes to sleep or wakes; released when dropped.
pub(crate) struct Reservation {
    d: Arc<Daemon>,
    id: String,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.d.transitions.lock().unwrap().remove(&self.id);
    }
}

fn busy_err() -> ApiError {
    err(
        "ws_transition",
        "its workspace is going to sleep or waking; try again",
    )
}

impl Daemon {
    // ---------- reservations ----------

    /// Reserves `ws_id` with its current panes. Admission is decided under the same lock a
    /// stop uses to wait for transitions, so none starts after a stop saw none running.
    pub(crate) fn reserve(
        self: &Arc<Self>,
        ws_id: &str,
        auto: bool,
    ) -> Result<Reservation, ApiError> {
        let panes: HashSet<String> = {
            let s = self.session();
            s.workspace(ws_id)
                .map(|w| w.tabs.iter().flat_map(|t| t.root.panes()).collect())
                .unwrap_or_default()
        };
        let mut t = self.transitions.lock().unwrap();
        if self.stopping_soon.load(std::sync::atomic::Ordering::SeqCst)
            || self.stopping.load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(err("ws_transition", "orchd is stopping"));
        }
        if t.contains_key(ws_id) {
            return Err(err(
                "ws_transition",
                "that workspace is already going to sleep or waking",
            ));
        }
        t.insert(
            ws_id.to_string(),
            Transit {
                panes,
                auto,
                aborted: false,
                committing: false,
            },
        );
        Ok(Reservation {
            d: Arc::clone(self),
            id: ws_id.to_string(),
        })
    }

    /// New panes of a wake join its reservation.
    pub(crate) fn reserve_panes(&self, ws_id: &str, panes: &[String]) {
        if let Some(t) = self.transitions.lock().unwrap().get_mut(ws_id) {
            t.panes.extend(panes.iter().cloned());
        }
    }

    pub(crate) fn ws_in_transition(&self, ws_id: &str) -> bool {
        self.transitions.lock().unwrap().contains_key(ws_id)
    }

    /// The pane belongs to a workspace going to sleep or waking (tasks and nudges wait).
    pub(crate) fn in_transition(&self, pane: &str) -> bool {
        self.transitions
            .lock()
            .unwrap()
            .values()
            .any(|t| t.panes.contains(pane))
    }

    /// Someone wants to use the pane (input, kill, attach): refused during a transition, except
    /// that it stops an automatic sleep that has not started to commit (and is then let in).
    /// Decided under the transitions lock, which the commit also takes: atomic with it.
    pub(crate) fn admit(&self, pane: &str) -> Result<(), ApiError> {
        let mut map = self.transitions.lock().unwrap();
        match map.values_mut().find(|t| t.panes.contains(pane)) {
            None => Ok(()),
            Some(t) if t.auto && !t.committing => {
                t.aborted = true;
                Ok(())
            }
            Some(_) => Err(busy_err()),
        }
    }

    /// The same for a change of the workspace's layout.
    pub(crate) fn admit_ws(&self, ws_id: &str) -> Result<(), ApiError> {
        let mut map = self.transitions.lock().unwrap();
        match map.get_mut(ws_id) {
            None => Ok(()),
            Some(t) if t.auto && !t.committing => {
                t.aborted = true;
                Ok(())
            }
            Some(_) => Err(busy_err()),
        }
    }

    /// `admit` for a request that writes into the pane: counted as in flight until the guard
    /// is dropped. Counted under the transitions lock, so a reservation sees every one that got
    /// in before it and waits for them (`drain`).
    pub(crate) fn admit_op(self: &Arc<Self>, pane: &str) -> Result<OpGuard, ApiError> {
        let mut map = self.transitions.lock().unwrap();
        match map.values_mut().find(|t| t.panes.contains(pane)) {
            Some(t) if t.auto && !t.committing => t.aborted = true,
            Some(_) => return Err(busy_err()),
            None => {}
        }
        *self
            .inflight
            .lock()
            .unwrap()
            .entry(pane.to_string())
            .or_default() += 1;
        drop(map);
        Ok(OpGuard {
            d: Arc::clone(self),
            pane: pane.to_string(),
        })
    }

    /// After reserving: waits (≤ 10 s) until no admitted request still writes into its panes.
    async fn drain(&self, ws_id: &str) -> Result<(), ApiError> {
        let panes: HashSet<String> = self
            .transitions
            .lock()
            .unwrap()
            .get(ws_id)
            .map(|t| t.panes.clone())
            .unwrap_or_default();
        for _ in 0..500 {
            let busy = self
                .inflight
                .lock()
                .unwrap()
                .iter()
                .any(|(p, n)| *n > 0 && panes.contains(p));
            if !busy {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Err(err(
            "ws_transition",
            "input into it did not finish within 10 s; try again",
        ))
    }

    /// From here on nothing more is admitted; an automatic sleep somebody interrupted stops.
    fn begin_commit(&self, ws_id: &str) -> Result<(), ApiError> {
        let mut map = self.transitions.lock().unwrap();
        let Some(t) = map.get_mut(ws_id) else {
            return Ok(());
        };
        if t.aborted {
            return Err(err("auto_aborted", "somebody started using it"));
        }
        t.committing = true;
        Ok(())
    }

    fn commit_failed(&self, ws_id: &str) {
        if let Some(t) = self.transitions.lock().unwrap().get_mut(ws_id) {
            t.committing = false;
        }
    }

    /// An explicit bus name of an agent in a sleeping workspace is reserved for it: only the
    /// new pane of **that** agent's own pane (during its wake) may register it.
    pub(crate) fn name_free_for(&self, r: &RegisterAgent) -> Result<(), ApiError> {
        let Some(name) = r
            .name
            .as_deref()
            .filter(|n| !n.trim().is_empty())
            .map(orch_proto::api::normalize_name)
        else {
            return Ok(());
        };
        let s = self.session.lock().unwrap();
        for x in &s.sleeping {
            let Some(old) = x
                .panes
                .iter()
                .find(|(_, p)| p.saved.bus_name.as_deref() == Some(name.as_str()))
                .map(|(id, _)| id)
            else {
                continue;
            };
            let own = x
                .wake
                .as_ref()
                .and_then(|w| w.map.get(old))
                .is_some_and(|n| r.pane.as_deref() == Some(format!("orch:{n}").as_str()));
            if !own {
                return Err(err(
                    "name_reserved",
                    format!(
                        "{name} belongs to an agent in sleeping workspace {}; wake it instead",
                        x.workspace.label
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Pane ids of every sleeping workspace (old ones, and the new ones of a wake in progress).
    pub fn asleep_panes(&self) -> HashSet<String> {
        self.session
            .lock()
            .unwrap()
            .sleeping
            .iter()
            .flat_map(|s| {
                s.workspace
                    .tabs
                    .iter()
                    .flat_map(|t| t.root.panes())
                    .chain(s.panes.keys().cloned())
                    // During (and after a crash in) a wake both ids belong to it.
                    .chain(s.wake.iter().flat_map(|w| w.map.values().cloned()))
            })
            .collect()
    }

    // ---------- the record ----------

    /// Changes the sleepers durably, in one pass: `f` looks at the layout as it is and returns
    /// the change (the whole new sleeper list, awake workspaces to take out and to put in); new
    /// workspace / tab ids come from `alloc`, which takes them from the live counter (no other
    /// allocation can hand them out again). That change is written, and exactly that change is
    /// published to the live layout afterwards: nothing sees it before it is on disk, and what is
    /// live is what was written. Commits are serialized by the store's lock; layout snapshots
    /// wait while one is in flight. A failure after the new slot was renamed into place counts
    /// as written (the slot is what the next start reads; the folder sync of the next write
    /// makes it durable): there is no second, compensating write that could itself fail.
    /// Blocking (disk I/O): from async code use [`Self::commit_off`].
    pub(crate) fn commit<R>(
        &self,
        f: impl FnOnce(&Session, &dyn Fn(&str) -> String) -> Result<(Change, R), ApiError>,
    ) -> Result<R, ApiError> {
        let mut st = self.sleep_store.lock();
        let _gate = self.publish_gate.write().unwrap();
        let snapshot = self.session();
        let alloc = |prefix: &str| {
            self.edit(|s| Ok(s.new_id(prefix)))
                .unwrap_or_else(|_| format!("{prefix}0"))
        };
        let (change, r) = f(&snapshot, &alloc)?;
        let next_id = self.session.lock().unwrap().next_id;
        if let Err(e) = self.sleep_store.write(&mut st, &change.sleeping, next_id) {
            if !e.published {
                return Err(err(
                    "save_failed",
                    format!("saving the sleeping workspaces failed: {e}"),
                ));
            }
            eprintln!("orchd: the sleeper record was written but its folder could not be synced ({e}); it counts as written");
        }
        let _ = self.edit(|s| {
            s.sleeping = change.sleeping;
            s.workspaces.retain(|w| !change.remove.contains(&w.id));
            for (at, w) in change.add {
                let at = at.min(s.workspaces.len());
                s.workspaces.insert(at, w);
            }
            Ok(())
        });
        Ok(r)
    }

    /// [`Self::commit`] on a blocking thread.
    pub(crate) async fn commit_off<R: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&Session, &dyn Fn(&str) -> String) -> Result<(Change, R), ApiError>
            + Send
            + 'static,
    ) -> Result<R, ApiError> {
        let d = Arc::clone(self);
        tokio::task::spawn_blocking(move || d.commit(f))
            .await
            .map_err(|e| err("save_failed", e.to_string()))?
    }

    /// A commit that only changes one sleeper (by id) in place.
    pub(crate) async fn commit_sleeper(
        self: &Arc<Self>,
        ws_id: &str,
        f: impl FnOnce(&mut Sleeping) + Send + 'static,
    ) -> Result<(), ApiError> {
        let id = ws_id.to_string();
        self.commit_off(move |s, _| {
            let mut list = s.sleeping.clone();
            let x = list
                .iter_mut()
                .find(|x| x.workspace.id == id)
                .ok_or_else(|| err("no_workspace", "it was closed meanwhile"))?;
            f(x);
            Ok((Change::sleepers(list), ()))
        })
        .await
    }

    /// Writes the sleepers as they are now.
    pub(crate) fn persist_sleepers(&self) -> Result<(), ApiError> {
        self.commit(|s, _| Ok((Change::sleepers(s.sleeping.clone()), ())))
    }

    pub fn load_sleepers(&self) -> Sleepers {
        match self.sleep_store.load() {
            crate::sleepstore::Loaded::Found { record, .. } => Sleepers {
                list: record.sleeping,
                next_id: record.next_id,
                ..Default::default()
            },
            _ => Sleepers::default(),
        }
    }

    /// Moves the owners of `x`'s panes (running tasks, bus names) along `map` (old → new), or
    /// back (`back`). Each step is idempotent, so a partly done move can be repeated.
    fn move_owners(
        &self,
        x: &Sleeping,
        map: &HashMap<String, String>,
        back: bool,
    ) -> anyhow::Result<()> {
        for (i, (old, new)) in map.iter().enumerate() {
            if back && crate::fault::on("owners_back_fail") {
                anyhow::bail!("injected failure moving owners back");
            }
            if !back && i > 0 && crate::fault::on("owners_fail") {
                anyhow::bail!("injected failure moving owners");
            }
            let (from, to) = if back { (new, old) } else { (old, new) };
            self.bus.task_rebind(from, to)?;
            let Some(sp) = x.panes.get(old) else { continue };
            if let (Some(kind), Some(name)) = (&sp.saved.agent, &sp.saved.bus_name) {
                self.bus.register(&RegisterAgent {
                    kind: kind.clone(),
                    pane: Some(format!("orch:{to}")),
                    cwd: Some(sp.saved.cwd.clone()),
                    name: Some(name.clone()),
                    instance: None,
                })?;
            }
        }
        Ok(())
    }

    /// Start-up, before anything is started: decides which source owns each pane, by its
    /// lineage (see `Pane::origin`), so that replaying after a crash at any point can neither
    /// start a pane twice nor lose one. Idempotent.
    ///
    /// Authority, highest first:
    /// 1. a sleeper (its wake did not commit: the owners of an interrupted wake are moved back;
    ///    if that fails the map stays for the next start);
    /// 2. the awake layout file (`primary`); a sleeper whose wake committed and whose panes are
    ///    in it is dropped;
    /// 3. the retry file (`retry`, panes that could not be started before).
    ///
    /// Each pane is removed from every lower source. Sleepers found in an older layout file are
    /// taken over.
    pub fn reconcile_sleepers(
        &self,
        sl: &mut Sleepers,
        mut primary: Option<&mut crate::restore::Saved>,
        retry: Option<&mut crate::restore::Saved>,
    ) {
        if let Some(saved) = primary.as_deref_mut() {
            for x in saved.session.sleeping.drain(..) {
                if !sl.list.iter().any(|y| y.workspace.id == x.workspace.id) {
                    sl.list.push(x);
                    sl.dirty = true;
                }
            }
            sl.next_id = sl.next_id.max(saved.session.next_id);
        }
        let primary_lines = primary
            .as_deref()
            .map(crate::restore::lineages)
            .unwrap_or_default();
        let mut keep = Vec::new();
        for mut x in std::mem::take(&mut sl.list) {
            let mine = Self::lineages(&x);
            match x.wake.clone() {
                Some(w) if w.committed && mine.iter().any(|l| primary_lines.contains(l)) => {
                    eprintln!(
                        "orchd: the wake of {} had committed; it is awake",
                        x.workspace.label
                    );
                    sl.dirty = true;
                    continue;
                }
                Some(w) => match self.move_owners(&x, &w.map, true) {
                    Ok(()) => {
                        x.wake = None;
                        sl.dirty = true;
                        eprintln!(
                            "orchd: the wake of {} was interrupted; it stays asleep",
                            x.workspace.label
                        );
                    }
                    // Kept (with its map) for the next start to try again.
                    Err(e) => eprintln!(
                        "orchd: undoing the interrupted wake of {} failed: {e:#}; tried again at the next start",
                        x.workspace.label
                    ),
                },
                None => {}
            }
            keep.push(x);
        }
        sl.list = keep;
        let asleep: HashSet<String> = sl.list.iter().flat_map(Self::lineages).collect();
        if let Some(saved) = primary.as_deref_mut() {
            crate::restore::prune(saved, &asleep);
        }
        let awake = primary
            .as_deref()
            .map(crate::restore::lineages)
            .unwrap_or_default();
        if let Some(saved) = retry {
            let owned: HashSet<String> = asleep.union(&awake).cloned().collect();
            crate::restore::prune(saved, &owned);
        }
        sl.reconciled = true;
    }

    /// Start-up, first: the sleepers as read, so their panes count as asleep right away.
    pub fn preload_sleepers(&self, sl: &Sleepers) {
        let _ = self.edit(|s| {
            s.sleeping = sl.list.clone();
            Ok(())
        });
    }

    /// Start-up, after the awake layout is back: the sleepers join the layout (ids that an
    /// older orchd handed out meanwhile are renumbered) and the record is rewritten if needed.
    /// `Err` when that write failed: the caller keeps the layout file they came from.
    pub fn install_sleepers(&self, mut sl: Sleepers) -> Result<(), ApiError> {
        if !sl.reconciled {
            self.reconcile_sleepers(&mut sl, None, None);
        }
        let mut dirty = sl.dirty;
        let num = |id: &str| id.get(1..).and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
        let _ = self.edit(|s| {
            s.next_id = s.next_id.max(sl.next_id);
            for x in &sl.list {
                s.next_id = s.next_id.max(num(&x.workspace.id));
                for t in &x.workspace.tabs {
                    s.next_id = s.next_id.max(num(&t.id));
                }
            }
            let awake_tabs: HashSet<String> = s
                .workspaces
                .iter()
                .flat_map(|w| w.tabs.iter().map(|t| t.id.clone()))
                .collect();
            for x in &mut sl.list {
                if s.workspaces.iter().any(|w| w.id == x.workspace.id) {
                    x.workspace.id = s.new_id("w");
                    dirty = true;
                }
                for t in &mut x.workspace.tabs {
                    if awake_tabs.contains(&t.id) {
                        t.id = s.new_id("t");
                        dirty = true;
                    }
                }
                if s.workspaces.iter().any(|w| w.label == x.workspace.label) {
                    x.workspace.label = unique_label(s, &x.workspace.label);
                    dirty = true;
                }
            }
            s.sleeping = std::mem::take(&mut sl.list);
            Ok(())
        });
        if dirty {
            self.persist_sleepers()?;
        }
        Ok(())
    }

    // ---------- sleep ----------

    /// What sleeping `ws_id` would do, each pane's record and the pane ids it held. Reads
    /// the screens, the bus and /proc; changes nothing.
    fn sleep_inspect(&self, ws_id: &str, auto: bool) -> Result<Inspected, ApiError> {
        let session = self.session();
        let w = session
            .workspace(ws_id)
            .ok_or_else(|| err("no_workspace", format!("no workspace {ws_id:?}")))?;
        let bus = |e: anyhow::Error| err("bus", format!("{e:#}"));
        let agents = self.bus.agents().map_err(bus)?;
        let questions = self.bus.questions(false).map_err(bus)?;
        let running: Vec<_> = self
            .bus
            .tasks(false)
            .map_err(bus)?
            .into_iter()
            .filter(|t| t.state == "running")
            .collect();
        let deferred = self.deferred.lock().unwrap().clone();
        let mut plan = SleepPlan {
            workspace: w.id.clone(),
            label: w.label.clone(),
            ..Default::default()
        };
        let mut panes = HashMap::new();
        let ids: Vec<String> = w.tabs.iter().flat_map(|t| t.root.panes()).collect();
        for id in &ids {
            let Some(p) = self.find(id).ok().filter(|p| p.exit_code().is_none()) else {
                plan.notes
                    .push(format!("{id} has ended already: it is left out"));
                continue;
            };
            let mut sp = crate::restore::saved_pane(&p, &agents, Some(questions.as_slice()));
            sp.sole_codex_in_cwd = false;
            let screen = p.visible_text();
            let status = detect::detect(&screen, sp.agent.as_deref());
            let who = match (&sp.bus_name, &sp.name) {
                (Some(n), _) | (None, Some(n)) => format!("{n} ({id})"),
                _ => id.clone(),
            };
            let mut busy = Vec::new();
            if status == Detected::Blocked {
                busy.push("waits on a dialog (a permission prompt or a menu)".to_string());
            }
            if sp.agent.is_some() && status == Detected::Working {
                busy.push("is working: its turn would be interrupted".into());
            } else if agents.iter().any(|a| {
                a.pane.as_deref() == Some(format!("orch:{id}").as_str()) && a.status == "working"
            }) {
                busy.push("reports working (its hooks): its turn would be interrupted".into());
            }
            if let Some(q) = questions
                .iter()
                .find(|q| sp.bus_name.as_ref() == Some(&q.from))
            {
                busy.push(format!("waits for Murat's answer to question #{}", q.id));
            }
            for t in running
                .iter()
                .filter(|t| t.pane.as_deref() == Some(id.as_str()))
            {
                busy.push(format!("runs task #{}", t.id));
            }
            let limited = sp.agent.is_some() && status == Detected::Limited;
            let limit_reset = crate::limits::info(id).and_then(|(reset, _)| reset);
            let auto_continue = crate::limits::auto_continue(id, None);
            let pending =
                (limited && limit_reset.is_some() && auto_continue) || deferred.contains_key(id);
            if auto && pending {
                busy.push("has a usage-limit continue due (auto-sleep leaves it awake)".into());
            }
            if sp.agent.is_some() {
                plan.notes.push(format!("{who}: {}", resume_words(&sp)));
                if limited {
                    plan.notes.push(match limit_reset {
                        Some(r) if auto_continue => format!(
                            "{who} is paused on its usage limit: once awake it gets \"continue\" at {}",
                            hhmm(r + LIMIT_GRACE_SECS)
                        ),
                        Some(r) => format!(
                            "{who} is paused on its usage limit (resets {}; auto-continue is off)",
                            hhmm(r)
                        ),
                        None => format!(
                            "{who} is paused on its usage limit with no reset time on screen: continue it yourself"
                        ),
                    });
                }
            } else if shell_pane(&sp) {
                if let Some((_, job)) = p.foreground_job() {
                    busy.push(format!(
                        "runs {job} in its shell: that program ends and does not come back"
                    ));
                }
                // A `bash -lc '<program>; exec bash'` pane comes back as the plain shell, not
                // running the program again.
                if sp
                    .argv
                    .iter()
                    .skip(1)
                    .any(|a| a.starts_with('-') && a.contains('c'))
                {
                    sp.argv.truncate(1);
                }
                plan.notes.push(format!(
                    "{who}: the shell comes back fresh in {} (its screen and history are not kept)",
                    sp.cwd
                ));
            } else {
                busy.push(format!(
                    "runs {}, which cannot be resumed: it comes back as a fresh shell in {}",
                    sp.argv[0], sp.cwd
                ));
                sp.argv = Vec::new();
            }
            // A deferred continue goes with the pane (its time is kept).
            sp.continue_at = deferred.get(id).copied();
            let interrupted = !busy.is_empty();
            plan.blockers
                .extend(busy.into_iter().map(|b| format!("{who} {b}")));
            panes.insert(
                id.clone(),
                SleptPane {
                    saved: sp,
                    limited,
                    limit_reset: limit_reset.or(deferred.get(id).copied()),
                    auto_continue,
                    interrupted,
                },
            );
        }
        Ok((plan, panes, ids))
    }

    /// `orch ws sleep` (and auto-sleep, with `auto`).
    pub async fn sleep_workspace(
        self: &Arc<Self>,
        key: &str,
        force: bool,
        dry_run: bool,
        auto: Option<crate::autosleep::AutoCheck>,
    ) -> Result<Value, ApiError> {
        let session = self.session();
        if let Some(s) = session.sleeping(key) {
            return Ok(json!({
                "session": session, "slept": false, "already": true,
                "plan": SleepPlan { workspace: s.workspace.id.clone(), label: s.workspace.label.clone(), ..Default::default() },
            }));
        }
        let ws_id = session
            .workspace(key)
            .ok_or_else(|| err("no_workspace", format!("no workspace {key:?}")))?
            .id
            .clone();
        drop(session);
        let _reserved = self.reserve(&ws_id, auto.is_some())?;
        // A request, task or nudge already typing into it finishes first; later ones see the
        // reservation.
        self.drain(&ws_id).await?;
        drop(self.task_gate.lock().await);
        crate::fault::pause_at("sleep_pause_ms").await;
        let d = Arc::clone(self);
        let id = ws_id.clone();
        let is_auto = auto.is_some();
        let (plan, panes, ids) = tokio::task::spawn_blocking(move || d.sleep_inspect(&id, is_auto))
            .await
            .map_err(|e| err("bus", e.to_string()))??;
        if dry_run {
            return Ok(json!({ "session": self.session(), "plan": plan, "slept": false }));
        }
        if !plan.blockers.is_empty() && !force {
            return Err(err(
                "ws_busy",
                format!(
                    "{} is busy, nothing was done (--force puts it to sleep anyway):\n  {}",
                    plan.label,
                    plan.blockers.join("\n  ")
                ),
            ));
        }
        crate::fault::pause_at("auto_pause_ms").await;
        if let Some(a) = &auto {
            let d = Arc::clone(self);
            let (a2, id) = (a.clone(), ws_id.clone());
            tokio::task::spawn_blocking(move || a2.still_idle(&d, &id))
                .await
                .map_err(|e| err("auto_aborted", e.to_string()))?
                .map_err(|why| err("auto_aborted", why))?;
        }
        let agents: Vec<String> = panes
            .values()
            .filter(|p| p.saved.agent.is_some())
            .filter_map(|p| p.saved.bus_name.clone())
            .collect();
        let n_agents = panes.values().filter(|p| p.saved.agent.is_some()).count();
        let interrupted: Vec<String> = panes
            .iter()
            .filter(|(_, p)| p.interrupted)
            .map(|(id, p)| p.saved.bus_name.clone().unwrap_or_else(|| id.clone()))
            .collect();
        // The commit. From here nothing more is admitted (an automatic sleep somebody
        // interrupted stops here); the layout must still hold exactly the panes inspected; the
        // record is written and confirmed before the workspace leaves the live layout, and
        // before anything ends. A failed write changes nothing.
        self.begin_commit(&ws_id)?;
        let check_questions = auto.is_some();
        let bus_names = agents.clone();
        let (id, ids2, panes2) = (ws_id.clone(), ids.clone(), panes);
        let bus = Arc::clone(&self.bus);
        let committed = self
            .commit_off(move |s, _| {
                let at = s
                    .workspaces
                    .iter()
                    .position(|w| w.id == id)
                    .ok_or_else(|| err("no_workspace", "the workspace is gone"))?;
                let now: Vec<String> = s.workspaces[at]
                    .tabs
                    .iter()
                    .flat_map(|t| t.root.panes())
                    .collect();
                if now != ids2 {
                    return Err(err(
                        "ws_changed",
                        "its panes changed while it was checked; nothing was done, try again",
                    ));
                }
                // An automatic sleep never takes an agent with a question open, nor a
                // workspace that was exempted meanwhile.
                if check_questions {
                    let qs = bus
                        .questions(false)
                        .map_err(|e| err("auto_aborted", format!("bus: {e:#}")))?;
                    if qs.iter().any(|q| bus_names.contains(&q.from)) {
                        return Err(err("auto_aborted", "an agent in it asked a question"));
                    }
                    let exempt = bus
                        .kv_get(&format!("{}{id}", crate::autosleep::KEEP_AWAKE))
                        .map_err(|e| err("auto_aborted", format!("bus: {e:#}")))?;
                    if exempt.is_some() {
                        return Err(err("auto_aborted", "it is kept awake"));
                    }
                }
                let mut list = s.sleeping.clone();
                list.push(Sleeping {
                    workspace: s.workspaces[at].clone(),
                    position: at,
                    slept_ms: now_ms(),
                    panes: panes2,
                    wake: None,
                    stuck: Vec::new(),
                });
                Ok((
                    Change {
                        sleeping: list,
                        remove: vec![id.clone()],
                        add: Vec::new(),
                    },
                    (),
                ))
            })
            .await;
        if let Err(e) = committed {
            self.commit_failed(&ws_id);
            return Err(match e.code {
                "save_failed" => err(
                    "save_failed",
                    format!("{}; nothing was put to sleep", e.message),
                ),
                _ => e,
            });
        }
        crate::fault::pause_at("sleep_before_kill_ms").await;
        crate::fault::crash_at("sleep_after_commit");
        let d = Arc::clone(self);
        let _ = tokio::task::spawn_blocking(move || d.autosave_now(false)).await;
        let failures = self.kill_panes(&ids).await;
        let mut warnings: Vec<String> = failures.iter().map(|(_, e)| e.clone()).collect();
        if !failures.is_empty() {
            warnings.push(self.keep_survivors(&ws_id, &plan.label, &failures).await);
        }
        let _ = self.ui_events.send(crate::ui::UiEvent::Panes);
        for name in &agents {
            let _ = self.bus.set_status(
                name,
                "idle",
                Some(&format!("asleep · orch ws wake {}", plan.label)),
            );
        }
        let why = auto.as_ref().map_or(String::new(), |a| {
            format!(
                " (auto-sleep: idle for {})",
                crate::autosleep::duration_words(a.period)
            )
        });
        let text = format!(
            "{} is asleep{why}: {} pane(s), {n_agents} agent(s){}; wake it with: orch ws wake {}",
            plan.label,
            ids.len(),
            if interrupted.is_empty() {
                String::new()
            } else {
                format!(", forced while busy: {}", interrupted.join(", "))
            },
            plan.label
        );
        if let Err(e) = self
            .bus
            .send("orchd", HUMAN, "msg", &text, Some("sleep"), None)
        {
            eprintln!("orchd: sleep summary: {e}");
        }
        Ok(json!({
            "session": self.session(), "plan": plan, "slept": true, "warnings": warnings,
        }))
    }

    /// Ends panes in parallel; returns those that could not be ended.
    async fn kill_panes(self: &Arc<Self>, ids: &[String]) -> Vec<(String, String)> {
        let mut jobs = tokio::task::JoinSet::new();
        for id in ids {
            let (d, id) = (Arc::clone(self), id.clone());
            jobs.spawn(async move {
                let r = d.kill(&id).await;
                (id, r)
            });
        }
        let mut failed = Vec::new();
        while let Some(r) = jobs.join_next().await {
            match r {
                Ok((_, Ok(()))) => {}
                Ok((_, Err(e))) if e.code == "no_pane" => {}
                Ok((id, Err(e))) => failed.push((id, e.message)),
                Err(e) => failed.push((String::new(), e.to_string())),
            }
        }
        failed
    }

    /// Programs that outlived SIGKILL stay visible in an awake workspace "<label>-stuck". They
    /// keep the lineage of the pane they belonged to, so the sleeper can't wake (no second copy)
    /// while any of them runs; that guard is derived from the running panes, not from a record
    /// that could fail to be written.
    async fn keep_survivors(
        self: &Arc<Self>,
        ws_id: &str,
        label: &str,
        failed: &[(String, String)],
    ) -> String {
        let ids: Vec<String> = failed
            .iter()
            .map(|(id, _)| id.clone())
            .filter(|id| !id.is_empty())
            .collect();
        let cwd = self
            .session()
            .sleeping(ws_id)
            .map(|x| x.workspace.cwd.clone())
            .unwrap_or_default();
        let stuck_label = self.edit(|s| {
            let stuck_label = unique_label(s, &format!("{label}-stuck"));
            let mut tabs = Vec::new();
            for (i, id) in ids.iter().enumerate() {
                tabs.push(Tab {
                    id: s.new_id("t"),
                    label: (i + 1).to_string(),
                    root: Node::leaf(id),
                    focus: id.clone(),
                    zoomed: false,
                });
            }
            let id = s.new_id("w");
            s.workspaces.push(Workspace {
                id,
                label: stuck_label.clone(),
                cwd: cwd.clone(),
                tabs,
                active_tab: 0,
            });
            Ok(stuck_label)
        });
        match stuck_label {
            Ok(l) => format!(
                "{} pane(s) survived SIGKILL: they are in workspace {l}; {label} wakes once they are gone",
                ids.len()
            ),
            Err(_) => format!(
                "{} pane(s) survived SIGKILL ({}); {label} wakes once they are gone",
                ids.len(),
                ids.join(", ")
            ),
        }
    }

    /// The lineages a sleeper's panes belong to.
    fn lineages(sl: &Sleeping) -> HashSet<String> {
        sl.panes
            .iter()
            .map(|(k, p)| p.saved.origin.clone().unwrap_or_else(|| k.clone()))
            .collect()
    }

    // ---------- wake ----------

    /// `orch ws wake`: the layout comes back with fresh panes, agents resume. All or nothing
    /// unless `partial`.
    pub async fn wake_workspace(
        self: &Arc<Self>,
        key: &str,
        partial: bool,
    ) -> Result<Value, ApiError> {
        let session = self.session();
        let Some(ws_id) = session.sleeping(key).map(|x| x.workspace.id.clone()) else {
            return match session.workspace(key) {
                Some(w) => Ok(json!({
                    "session": session, "workspace": w.id, "already": true,
                    "panes": [], "failed": [],
                })),
                None => Err(err("no_workspace", format!("no workspace {key:?}"))),
            };
        };
        drop(session);
        let _reserved = self.reserve(&ws_id, false)?;
        let sl = self
            .session()
            .sleeping(&ws_id)
            .cloned()
            .ok_or_else(|| err("no_workspace", "it was closed meanwhile"))?;
        let label = sl.workspace.label.clone();
        // A program of one of its panes still runs (it survived being ended): no second copy.
        let mine = Self::lineages(&sl);
        let stuck: Vec<String> = self
            .all_panes()
            .iter()
            .filter(|p| p.exit_code().is_none() && mine.contains(&p.origin()))
            .map(|p| p.id.clone())
            .collect();
        if !stuck.is_empty() {
            return Err(err(
                "ws_stuck",
                format!(
                    "{label} can't wake while its old program(s) still run: {} (close them first)",
                    stuck.join(", ")
                ),
            ));
        }
        if sl.wake.is_some() {
            return Err(err(
                "ws_transition",
                format!("an earlier wake of {label} is not undone yet (its tasks / names could not be moved back); restart orchd to retry that"),
            ));
        }
        // A bus name held by a live agent elsewhere: report it, start nothing.
        let agents = self
            .bus
            .agents()
            .map_err(|e| err("bus", format!("{e:#}")))?;
        for (old, p) in &sl.panes {
            let Some(name) = &p.saved.bus_name else {
                continue;
            };
            let holder = agents
                .iter()
                .find(|a| &a.name == name)
                .and_then(|a| a.pane.clone());
            if let Some(h) = holder.filter(|h| *h != format!("orch:{old}")) {
                let live = h
                    .strip_prefix("orch:")
                    .is_some_and(|id| self.find(id).is_ok_and(|p| p.exit_code().is_none()));
                if live {
                    return Err(err(
                        "name_conflict",
                        format!("{name} is in use by {h}; {label} stays asleep"),
                    ));
                }
            }
        }
        // Start every pane (off the runtime), in tab order.
        let order: Vec<(String, SleptPane)> = sl
            .workspace
            .tabs
            .iter()
            .flat_map(|t| t.root.panes())
            .filter_map(|old| sl.panes.get(&old).map(|p| (old, p.clone())))
            .collect();
        // Nothing starts unless its record can be written (a blocked store would only be found
        // at the first commit, after processes were started).
        if let Err(e) = self.sleep_store.writable() {
            return Err(err("save_failed", format!("{label} stays asleep: {e}")));
        }
        let d = Arc::clone(self);
        let wsid = ws_id.clone();
        let started = tokio::task::spawn_blocking(move || {
            order
                .into_iter()
                .map(|(old, sp)| {
                    let r = d.spawn_slept(&old, &sp.saved, &wsid);
                    (old, sp, r)
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|e| err("spawn_failed", e.to_string()))?;
        let spawned: Vec<String> = started
            .iter()
            .filter_map(|(_, _, r)| r.as_ref().ok().map(|p| p.id.clone()))
            .collect();
        let _ = &spawned;
        // An agent counts as started once it is still up after a moment; inside a shell
        // wrapper the agent itself must be the shell's child (not just any process).
        if started
            .iter()
            .any(|(_, sp, r)| r.is_ok() && sp.saved.agent.is_some())
        {
            let grace = std::env::var("ORCHD_WAKE_GRACE_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .map_or(WAKE_GRACE, Duration::from_millis);
            tokio::time::sleep(grace).await;
        }
        let mut ok: Vec<(String, SleptPane, Arc<Pane>)> = Vec::new();
        let mut failed: Vec<(String, String)> = Vec::new();
        let mut dead: Vec<String> = Vec::new();
        for (old, sp, r) in started {
            match r {
                Ok(p) => {
                    let up = p.exit_code().is_none()
                        && match sp.saved.agent.as_deref() {
                            None => true,
                            Some(_) if !shell_pane(&sp.saved) => true,
                            Some(_) => p
                                .pid
                                .is_some_and(|pid| has_agent_child(pid, &agent_exe(&sp.saved))),
                        };
                    if up {
                        ok.push((old, sp, p));
                    } else {
                        failed.push((old, format!("{} did not start", p.id)));
                        dead.push(p.id.clone());
                    }
                }
                Err(e) => failed.push((old, e)),
            }
        }
        let new_panes: Vec<String> = ok.iter().map(|(_, _, p)| p.id.clone()).collect();
        if ok.is_empty() || (!failed.is_empty() && !partial) {
            let all: Vec<String> = new_panes.iter().chain(&dead).cloned().collect();
            let mut msg = format!(
                "{label} stays asleep: {}{}",
                failed
                    .iter()
                    .map(|(o, e)| format!("{o}: {e}"))
                    .collect::<Vec<_>>()
                    .join("; "),
                if ok.is_empty() {
                    ""
                } else {
                    " (retry once that is fixed, or wake with --partial)"
                }
            );
            let survivors = self.kill_panes(&all).await;
            if !survivors.is_empty() {
                msg = format!(
                    "{msg}; {}",
                    self.keep_survivors(&ws_id, &label, &survivors).await
                );
            }
            return Err(err("wake_failed", msg));
        }
        let survivors = self.kill_panes(&dead).await;
        if !survivors.is_empty() {
            self.keep_survivors(&ws_id, &label, &survivors).await;
        }
        let map: HashMap<String, String> = ok
            .iter()
            .map(|(o, _, p)| (o.clone(), p.id.clone()))
            .collect();
        // 1. The map, durably, before any owner moves.
        let m = map.clone();
        let written = self
            .commit_sleeper(&ws_id, move |x| {
                x.wake = Some(WakeState {
                    map: m,
                    committed: false,
                });
            })
            .await;
        if let Err(e) = written {
            self.undo_wake(&ws_id, &sl, &map, false, &new_panes).await;
            return Err(e);
        }
        crate::fault::pause_at("wake_pause_ms").await;
        // 2. Owners: running tasks and bus names follow the new panes.
        if let Err(e) = self.move_owners(&sl, &map, false) {
            self.undo_wake(&ws_id, &sl, &map, true, &new_panes).await;
            return Err(err("bus", format!("moving tasks / names failed: {e:#}")));
        }
        crate::fault::crash_at("wake_after_owners");
        // 3. The awake layout, durably (with the usage-limit continues still due, so a crash
        //    after this keeps them).
        let Some(mut w) = mapped(&sl.workspace, &map) else {
            self.undo_wake(&ws_id, &sl, &map, true, &new_panes).await;
            return Err(err("wake_failed", "nothing of it came back"));
        };
        for (_, sp, p) in &ok {
            let due = sp
                .saved
                .continue_at
                .or(sp.limited.then_some(sp.limit_reset).flatten());
            if let (Some(reset), true) = (due, sp.auto_continue) {
                self.deferred.lock().unwrap().insert(p.id.clone(), reset);
            }
        }
        let rest = (!failed.is_empty()).then(|| {
            let back: HashMap<String, String> =
                failed.iter().map(|(o, _)| (o.clone(), o.clone())).collect();
            mapped(&sl.workspace, &back).map(|ws| Sleeping {
                workspace: ws,
                position: sl.position + 1,
                slept_ms: sl.slept_ms,
                panes: failed
                    .iter()
                    .filter_map(|(o, _)| sl.panes.get(o).map(|p| (o.clone(), p.clone())))
                    .collect(),
                wake: None,
                stuck: Vec::new(),
            })
        });
        let placed = self.edit(|s| {
            if s.sleeping(&ws_id).is_none() {
                return Err(err("no_workspace", "it was closed meanwhile"));
            }
            if s.workspaces.iter().any(|x| x.label == w.label) {
                w.label = unique_label(s, &w.label);
            }
            let at = sl.position.min(s.workspaces.len());
            s.workspaces.insert(at, w.clone());
            Ok(())
        });
        if let Err(e) = placed {
            self.undo_wake(&ws_id, &sl, &map, true, &new_panes).await;
            return Err(e);
        }
        let d = Arc::clone(self);
        let saved = tokio::task::spawn_blocking(move || d.autosave_sync())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))
            .and_then(|r| r);
        if let Err(e) = saved {
            self.unplace_woken(&ws_id);
            self.undo_wake(&ws_id, &sl, &map, true, &new_panes).await;
            return Err(err(
                "save_failed",
                format!("saving the layout failed ({e}); {label} stays asleep"),
            ));
        }
        crate::fault::crash_at("wake_after_layout");
        // 4. Committed: the sleeper (and the panes that did not come back, kept asleep).
        let (id, lbl, rest2) = (ws_id.clone(), label.clone(), rest.clone().flatten());
        let committed = self
            .commit_off(move |s, alloc| {
                let mut list = s.sleeping.clone();
                let x = list
                    .iter_mut()
                    .find(|x| x.workspace.id == id)
                    .ok_or_else(|| err("no_workspace", "it was closed meanwhile"))?;
                if let Some(wk) = &mut x.wake {
                    wk.committed = true;
                }
                let mut rest_label = None;
                if let Some(mut r) = rest2 {
                    // Allocated once, from the live counter; published exactly as written.
                    r.workspace.id = alloc("w");
                    for t in &mut r.workspace.tabs {
                        t.id = alloc("t");
                    }
                    r.workspace.label = unique_label(s, &format!("{lbl}-rest"));
                    rest_label = Some(r.workspace.label.clone());
                    list.push(r);
                }
                Ok((Change::sleepers(list), rest_label))
            })
            .await;
        let rest_label = match committed {
            Ok(l) => l,
            Err(e) => {
                self.unplace_woken(&ws_id);
                self.undo_wake(&ws_id, &sl, &map, true, &new_panes).await;
                let d = Arc::clone(self);
                let _ = tokio::task::spawn_blocking(move || d.autosave_sync()).await;
                return Err(e);
            }
        };
        crate::fault::crash_at("wake_after_commit");
        // 5. The sleeper goes.
        let id = ws_id.clone();
        if let Err(e) = self
            .commit_off(move |s, _| {
                let list = s
                    .sleeping
                    .iter()
                    .filter(|x| x.workspace.id != id)
                    .cloned()
                    .collect();
                Ok((Change::sleepers(list), ()))
            })
            .await
        {
            // The next start sees a committed wake with its panes in the layout.
            eprintln!("orchd: {}", e.message);
        }
        let _ = self.ui_events.send(crate::ui::UiEvent::Panes);
        self.after_wake_all(&ok, &w.label, failed.len(), rest_label.as_deref());
        Ok(json!({
            "session": self.session(),
            "workspace": w.id,
            "panes": new_panes,
            "failed": failed.iter().map(|(o, _)| o.clone()).collect::<Vec<_>>(),
            "rest": rest_label,
        }))
    }

    /// Starts one slept pane exactly in its folder (a folder that is gone is a failure: the
    /// record is kept, not moved to the home folder).
    fn spawn_slept(&self, old: &str, sp: &SavedPane, ws_id: &str) -> Result<Arc<Pane>, String> {
        if crate::fault::value("spawn_fail").is_some_and(|n| sp.name.as_deref() == Some(n)) {
            return Err("injected spawn failure".into());
        }
        if !std::path::Path::new(&sp.cwd).is_dir() {
            return Err(format!("its folder {} is gone", sp.cwd));
        }
        let (argv, extra) = wake_argv(sp);
        let env = (!extra.is_empty()).then(|| {
            orch_proto::env::full_env()
                .into_iter()
                .chain(extra)
                .collect::<Vec<_>>()
        });
        // Its lineage goes on; it joins the wake's reservation before anything can find it.
        let origin = sp.origin.clone().unwrap_or_else(|| old.to_string());
        self.new_pane_as(
            NewPane {
                name: sp.name.clone(),
                argv,
                cwd: Some(sp.cwd.clone()),
                env,
                rows: Some(sp.rows.max(2)),
                cols: Some(sp.cols.max(2)),
            },
            Some(&origin),
            Some(ws_id),
        )
        .map_err(|e| e.message)
    }

    /// Takes the woken workspace (same id as the sleeper) out of the awake layout again.
    fn unplace_woken(&self, ws_id: &str) {
        let _ = self.edit(|s| {
            s.workspaces.retain(|w| w.id != ws_id);
            Ok(())
        });
    }

    /// Undoes a wake that did not commit: owners back, new panes ended (survivors kept
    /// visible and blocking), the map cleared, but only once every owner is back: otherwise
    /// it stays in the record and the next start tries again.
    async fn undo_wake(
        self: &Arc<Self>,
        ws_id: &str,
        sl: &Sleeping,
        map: &HashMap<String, String>,
        owners_moved: bool,
        panes: &[String],
    ) {
        let back = if owners_moved {
            self.move_owners(sl, map, true)
        } else {
            Ok(())
        };
        {
            let mut deferred = self.deferred.lock().unwrap();
            for p in panes {
                deferred.remove(p);
            }
        }
        let survivors = self.kill_panes(panes).await;
        if !survivors.is_empty() {
            self.keep_survivors(ws_id, &sl.workspace.label, &survivors)
                .await;
        }
        match back {
            Ok(()) => {
                let id = ws_id.to_string();
                if let Err(e) = self
                    .commit_sleeper(&id, |x| x.wake = None)
                    .await
                {
                    // The next start undoes an uncommitted wake by itself.
                    eprintln!("orchd: {}", e.message);
                }
            }
            Err(e) => eprintln!(
                "orchd: moving the tasks / names of {} back failed: {e:#}; its map is kept, the next start tries again",
                sl.workspace.label
            ),
        }
    }

    fn after_wake_all(
        self: &Arc<Self>,
        ok: &[(String, SleptPane, Arc<Pane>)],
        label: &str,
        failed: usize,
        rest: Option<&str>,
    ) {
        let mut resumed = 0;
        for (_, slept, p) in ok {
            let Some(kind) = slept.saved.agent.clone() else {
                continue;
            };
            resumed += 1;
            p.set_kind(&kind);
            if !slept.auto_continue {
                crate::limits::auto_continue(&p.id, Some(false));
            }
            let name = slept.saved.bus_name.clone();
            if let Some(n) = &name {
                let _ = self.bus.set_status(n, "unknown", None);
            }
            let unread = name
                .as_deref()
                .and_then(|n| self.bus.inbox(n).ok())
                .map_or(0, |m| m.len());
            let d = Arc::clone(self);
            let (p, slept) = (Arc::clone(p), slept.clone());
            let name = name.unwrap_or_else(|| format!("{kind} in {}", p.id));
            tokio::spawn(async move { after_wake(d, p, name, slept, unread).await });
        }
        let text = format!(
            "{label} is awake: {} pane(s), resumed {resumed} agent(s){}",
            ok.len(),
            match rest {
                Some(r) => format!(", {failed} could not be started and stay asleep as {r}"),
                None => String::new(),
            }
        );
        if let Err(e) = self
            .bus
            .send("orchd", HUMAN, "msg", &text, Some("sleep"), None)
        {
            eprintln!("orchd: wake summary: {e}");
        }
    }

    /// `orch ws close` on a sleeping workspace: it is forgotten (nothing runs to kill). Its
    /// running tasks fail; its queued ones keep waiting for the label, as for a closed one.
    pub async fn forget_sleeping(self: &Arc<Self>, key: &str) -> Result<bool, ApiError> {
        let Some(id) = self.session().sleeping(key).map(|x| x.workspace.id.clone()) else {
            return Ok(false);
        };
        let _reserved = self.reserve(&id, false)?;
        let Some(sl) = self.session().sleeping(&id).cloned() else {
            return Ok(false);
        };
        let id2 = id.clone();
        self.commit_off(move |s, _| {
            let list = s
                .sleeping
                .iter()
                .filter(|x| x.workspace.id != id2)
                .cloned()
                .collect();
            Ok((Change::sleepers(list), ()))
        })
        .await?;
        let old: HashSet<String> = sl.panes.keys().cloned().collect();
        if let Ok(tasks) = self.bus.tasks(false) {
            for t in tasks.iter().filter(|t| {
                t.state == "running" && t.pane.as_ref().is_some_and(|p| old.contains(p))
            }) {
                let _ = self
                    .bus
                    .task_finish(t.id, "failed", Some("workspace closed while asleep"));
            }
        }
        let _ = self.expire_questions();
        Ok(true)
    }
}

/// A usage-limit continue for `pane` at `reset` + grace. It is recorded (and goes into the
/// layout snapshot, so a restart keeps it), checked again before typing: still this pause,
/// auto-continue still on.
pub(crate) fn schedule_continue(
    d: &Arc<Daemon>,
    pane: Arc<Pane>,
    name: String,
    reset: i64,
    mail: String,
) {
    d.deferred.lock().unwrap().insert(pane.id.clone(), reset);
    let d = Arc::clone(d);
    tokio::spawn(async move {
        let due = reset + LIMIT_GRACE_SECS - now_ms() / 1000;
        if due > 0 {
            tokio::time::sleep(Duration::from_secs(due as u64)).await;
        }
        if d.deferred.lock().unwrap().get(&pane.id) != Some(&reset) {
            return;
        }
        if !crate::limits::auto_continue(&pane.id, None) {
            d.deferred.lock().unwrap().remove(&pane.id);
            return;
        }
        let text = format!("{}{mail}", crate::limits::CONTINUE_TEXT);
        let typed = type_when_idle(&d, &pane, &text).await;
        d.deferred.lock().unwrap().remove(&pane.id);
        if typed {
            let _ = d.bus.send(
                "orchd",
                HUMAN,
                "msg",
                &format!("{name}: continued after its usage limit reset"),
                Some("limits"),
                None,
            );
        }
    });
}

/// What an agent gets once it is back at its prompt after a wake: the interrupted-turn note,
/// a reminder of unread mail, or (paused on its limit) "continue" after the reset.
async fn after_wake(
    d: Arc<Daemon>,
    pane: Arc<Pane>,
    name: String,
    slept: SleptPane,
    unread: usize,
) {
    let mail = if unread > 0 {
        format!(" You have {unread} unread orch message(s): call the orch read_inbox tool.")
    } else {
        String::new()
    };
    let due = slept
        .saved
        .continue_at
        .or(slept.limited.then_some(slept.limit_reset).flatten());
    if (slept.limited || slept.saved.continue_at.is_some()) && slept.auto_continue {
        match due {
            Some(reset) => schedule_continue(&d, pane, name, reset, mail),
            None => {
                let _ = d.bus.send(
                    "orchd",
                    HUMAN,
                    "msg",
                    &format!("{name} was on its usage limit with no reset time when it was put to sleep; it is awake now: type continue once the limit has reset"),
                    Some("limits"),
                    None,
                );
            }
        }
        return;
    }
    let text = if slept.interrupted {
        format!("{SLEEP_NOTE}{mail}")
    } else if unread > 0 {
        format!("[orch] you were asleep (orch ws sleep) and are awake again.{mail}")
    } else {
        return;
    };
    if !type_when_idle(&d, &pane, &text).await {
        eprintln!(
            "orchd: wake: {name} ({}) was not told: not back at its prompt",
            pane.id
        );
    }
}

/// Types `text` + Enter once the agent is back at its prompt (≤ 90 s), never into a dialog,
/// a limit notice or a working agent. Under the task gate, so a queued task dispatched to the
/// same pane can't interleave with it.
async fn type_when_idle(d: &Daemon, pane: &Pane, text: &str) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        match pane.detect() {
            // Its workspace is still waking (or going to sleep): wait, don't type yet.
            Detected::Idle if d.in_transition(&pane.id) => {}
            Detected::Idle => {
                let _gate = d.task_gate.lock().await;
                if d.in_transition(&pane.id)
                    || pane.detect() != Detected::Idle
                    || !pane.write(text.as_bytes().to_vec())
                {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
                return pane.detect() == Detected::Idle && pane.write(b"\r".to_vec());
            }
            Detected::Blocked | Detected::Limited | Detected::Done => return false,
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn pane(argv: &[&str], agent: &str) -> SavedPane {
        serde_json::from_value(json!({
            "argv": argv, "cwd": "/tmp", "rows": 20, "cols": 80, "agent": agent,
        }))
        .unwrap()
    }

    #[test]
    fn wake_resumes_only_a_verified_conversation() {
        // No verified id: the pickers, never --continue / --last / a saved positional target.
        assert_eq!(
            wake_argv(&pane(&["claude", "--model", "x"], "claude")).0,
            v(&["claude", "--resume", "--model", "x"])
        );
        assert_eq!(
            wake_argv(&pane(&["codex", "resume", "abc", "--no-daemon"], "codex")).0,
            v(&["codex", "resume", "--no-daemon"])
        );
        let mut sole = pane(&["codex"], "codex");
        sole.sole_codex_in_cwd = true;
        assert_eq!(wake_argv(&sole).0, v(&["codex", "resume"]));
        // Verified: exactly that one.
        let mut known = pane(&["claude", "-c"], "claude");
        known.session_id = Some("11111111-1111-4111-8111-111111111111".into());
        assert_eq!(
            wake_argv(&known).0,
            v(&["claude", "--resume", "11111111-1111-4111-8111-111111111111"])
        );
        // A shell wrapper is rebuilt, never its saved script replayed.
        let mut wrapped = pane(&["bash", "-lc", "codex resume --last; exec bash"], "codex");
        wrapped.bus_name = Some("x".into());
        let (argv, env) = wake_argv(&wrapped);
        assert_eq!(&argv[..2], &v(&["bash", "-lc"])[..]);
        assert_eq!(argv[2], "ORCH_AGENT=x codex resume; exec bash");
        assert!(env.is_empty());
        // Not an agent: as saved.
        assert_eq!(wake_argv(&pane(&["bash"], "shell")).0, v(&["bash"]));
    }
}
