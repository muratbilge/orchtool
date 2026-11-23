//! Workspaces that sleep and wake (`orch ws sleep|wake`). A sleeping workspace keeps its
//! layout (tabs, splits, sizes, labels, ids) and, per pane, the record a restart uses to start it
//! again; its programs end and their memory is free.
//!
//! Sleepers have their own record (`sleepstore.rs`); the restart path (restore.json, the
//! autosave) is the same as for an orchd without sleep. Start-up loads the record before the
//! restore and reconciles it with what the restore brought back (`install_sleepers`): if the
//! restored layout holds a sleeper's panes, that awake copy is what runs and the sleeper goes.
//!
//! - **Sleep:** reserve the workspace (no layout change, input or kill reaches it; ones already
//!   under way finish first), check it, write the record durably (rename and folder sync), and
//!   only then take it out of the layout and end its programs. Any failed write ends nothing.
//! - **Wake:** start every pane (all or nothing), write the old→new pane map into the record,
//!   move running tasks and bus names to the new panes, put the workspace back, and drop the
//!   sleeper once the autosave on disk holds the new panes.
//!
//! What waits for a sleeping agent is kept: unread mail (it is told on wake), queued tasks (no
//! agent there to dispatch to), running tasks (they follow the pane), open questions (they don't
//! expire), its bus name (reserved for it). Nothing wakes a workspace by itself; `--wake` on a
//! message or task asks for it.

use crate::detect::{self, Detected};
use crate::pane::Pane;
use crate::server::{err, ApiError, Daemon};
use orch_proto::api::{NewPane, RegisterAgent, SleepPlan, HUMAN};
use orch_proto::layout::{Node, SavedPane, Session, Sleeping, SleptPane, WakeState, Workspace};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
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
/// How long a wake waits for the autosave to hold its new panes before it returns (the
/// sleeper is dropped once it does, also later).
const CONFIRM_FOR: Duration = Duration::from_secs(30);
const SHELLS: [&str; 5] = ["bash", "zsh", "sh", "fish", "dash"];

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

fn base(p: &str) -> &str {
    Path::new(p)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(p)
}

fn shell_pane(sp: &SavedPane) -> bool {
    sp.argv.is_empty() || SHELLS.contains(&base(&sp.argv[0]))
}

/// What a woken pane runs. Agents resume only a **verified** conversation (the session id the
/// agent's hook or process reported); without one Codex and Claude come back at their resume
/// picker, never at "the latest conversation in this folder" (another agent may have made a
/// newer one while this one slept) and never by replaying a saved command line. Only a
/// command that parses completely is started; else the pane is a shell showing a note (see
/// `agent_start`).
pub fn wake_start(sp: &SavedPane) -> crate::agent_start::Start {
    crate::agent_start::start(sp, crate::agent_start::Target::Verified)
}

/// [`wake_start`]'s argv and environment.
pub fn wake_argv(sp: &SavedPane) -> (Vec<String>, Vec<(String, String)>) {
    let s = wake_start(sp);
    (s.argv, s.env)
}

/// How an agent will come back, in words (for the plan).
fn resume_words(sp: &SavedPane) -> String {
    let kind = sp.agent.as_deref().unwrap_or("agent");
    if let Some(why) = wake_start(sp).refused {
        return format!(
            "{kind} will not be started: its start command can't be rebuilt safely: {why} (comes back as a shell with a note)"
        );
    }
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
    base(&cmd).to_string()
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
fn mapped(ws: &Workspace, map: &HashMap<String, String>) -> Option<Workspace> {
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

/// The sleepers read at start-up.
#[derive(Default)]
pub struct Sleepers {
    list: Vec<Sleeping>,
    next_id: u64,
}

/// What a commit changes: the whole new sleeper list, and awake workspaces taken out (by id).
pub(crate) struct Change {
    pub sleeping: Vec<Sleeping>,
    pub remove: Vec<String>,
}

impl Change {
    fn sleepers(sleeping: Vec<Sleeping>) -> Self {
        Change {
            sleeping,
            remove: Vec::new(),
        }
    }
}

/// A workspace going to sleep or waking: its panes (old ones, and the new ones of a wake).
#[derive(Default)]
pub(crate) struct Transit {
    panes: HashSet<String>,
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

/// A request writing into a pane or changing a workspace's layout (see `admit_op`).
pub(crate) struct OpGuard {
    d: Arc<Daemon>,
    key: String,
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        let mut m = self.d.inflight.lock().unwrap();
        if let Some(n) = m.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&self.key);
            }
        }
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

    /// Reserves `ws_id` with its current panes. Admission is decided under the lock a stop
    /// uses to wait for transitions, so none starts after a stop saw none running.
    pub(crate) fn reserve(self: &Arc<Self>, ws_id: &str) -> Result<Reservation, ApiError> {
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
        t.insert(ws_id.to_string(), Transit { panes });
        Ok(Reservation {
            d: Arc::clone(self),
            id: ws_id.to_string(),
        })
    }

    /// New panes of a wake join its reservation (called as each is created).
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

    /// Refused while the pane's workspace goes to sleep or wakes (attach).
    pub(crate) fn admit(&self, pane: &str) -> Result<(), ApiError> {
        if self.in_transition(pane) {
            return Err(busy_err());
        }
        Ok(())
    }

    /// `admit` for a request that writes into the pane (send, kill): counted as in flight
    /// until the guard is dropped, under the transitions lock, so a reservation sees every one
    /// that got in before it and waits for it (`drain`).
    pub(crate) fn admit_op(self: &Arc<Self>, pane: &str) -> Result<OpGuard, ApiError> {
        let map = self.transitions.lock().unwrap();
        if map.values().any(|t| t.panes.contains(pane)) {
            return Err(busy_err());
        }
        self.count_in(pane.to_string(), map)
    }

    /// The same for a change of a workspace's layout (a split, a new tab, a close, …): counted
    /// through placement, so a sleep never captures a pane list that is about to change.
    pub(crate) fn admit_ws_op(self: &Arc<Self>, ws_id: &str) -> Result<OpGuard, ApiError> {
        let map = self.transitions.lock().unwrap();
        if map.contains_key(ws_id) {
            return Err(busy_err());
        }
        self.count_in(format!("ws:{ws_id}"), map)
    }

    fn count_in(
        self: &Arc<Self>,
        key: String,
        held: std::sync::MutexGuard<'_, HashMap<String, Transit>>,
    ) -> Result<OpGuard, ApiError> {
        *self
            .inflight
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_default() += 1;
        drop(held);
        Ok(OpGuard {
            d: Arc::clone(self),
            key,
        })
    }

    /// After reserving: waits (≤ 10 s) until no admitted request still writes into its panes or
    /// changes its layout.
    async fn drain(&self, ws_id: &str) -> Result<(), ApiError> {
        let mut keys: HashSet<String> = self
            .transitions
            .lock()
            .unwrap()
            .get(ws_id)
            .map(|t| t.panes.clone())
            .unwrap_or_default();
        keys.insert(format!("ws:{ws_id}"));
        for _ in 0..500 {
            let busy = self
                .inflight
                .lock()
                .unwrap()
                .iter()
                .any(|(k, n)| *n > 0 && keys.contains(k));
            if !busy {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Err(err(
            "ws_transition",
            "a change to it did not finish within 10 s; try again",
        ))
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

    /// Pane ids of every sleeping workspace (old ones, and the new ones of a wake in progress),
    /// including the ones loaded at start-up before they are installed.
    pub fn asleep_panes(&self) -> HashSet<String> {
        let loading = self.loading_sleepers.lock().unwrap().clone();
        let s = self.session.lock().unwrap();
        s.sleeping
            .iter()
            .chain(loading.iter())
            .flat_map(|x| {
                x.workspace
                    .tabs
                    .iter()
                    .flat_map(|t| t.root.panes())
                    .chain(x.panes.keys().cloned())
                    .chain(x.wake.iter().flat_map(|w| w.map.values().cloned()))
            })
            .collect()
    }

    // ---------- the record ----------

    /// Changes the sleepers durably, in one pass: `f` looks at the layout and returns the
    /// change (the whole new sleeper list, awake workspaces to take out; it allocates no ids).
    /// That change is written (rename and folder sync) and exactly it is published to the live
    /// layout afterwards. Any failed write publishes
    /// nothing and is returned. Commits are serialized by the store's lock. Blocking (disk I/O):
    /// from async code use [`Self::commit_off`].
    pub(crate) fn commit<R>(
        &self,
        f: impl FnOnce(&Session) -> Result<(Change, R), ApiError>,
    ) -> Result<R, ApiError> {
        let mut st = self.sleep_store.lock();
        let snapshot = self.session();
        let (change, r) = f(&snapshot)?;
        let next_id = self.session.lock().unwrap().next_id;
        self.sleep_store
            .write(&mut st, &change.sleeping, next_id)
            .map_err(|e| {
                err(
                    "save_failed",
                    format!("saving the sleeping workspaces failed: {e}"),
                )
            })?;
        let _ = self.edit(|s| {
            s.sleeping = change.sleeping;
            s.workspaces.retain(|w| !change.remove.contains(&w.id));
            Ok(())
        });
        Ok(r)
    }

    /// [`Self::commit`] on a blocking thread.
    pub(crate) async fn commit_off<R: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&Session) -> Result<(Change, R), ApiError> + Send + 'static,
    ) -> Result<R, ApiError> {
        let d = Arc::clone(self);
        tokio::task::spawn_blocking(move || d.commit(f))
            .await
            .map_err(|e| err("save_failed", e.to_string()))?
    }

    /// A commit that changes one sleeper (by id) in place, or drops it (`f` returns false).
    pub(crate) async fn commit_sleeper(
        self: &Arc<Self>,
        ws_id: &str,
        f: impl FnOnce(&mut Sleeping) -> bool + Send + 'static,
    ) -> Result<(), ApiError> {
        let id = ws_id.to_string();
        self.commit_off(move |s| {
            let mut list = s.sleeping.clone();
            let i = list
                .iter()
                .position(|x| x.workspace.id == id)
                .ok_or_else(|| err("no_workspace", "it was closed meanwhile"))?;
            if !f(&mut list[i]) {
                list.remove(i);
            }
            Ok((Change::sleepers(list), ()))
        })
        .await
    }

    /// Start-up, before the restore: the sleeper record. The sleepers count as asleep from now
    /// on, and the restore leaves their panes out (`skip_asleep`).
    pub fn load_sleepers(&self) -> Sleepers {
        let mut sl = Sleepers::default();
        if let crate::sleepstore::Loaded::Found { record, .. } = self.sleep_store.load() {
            sl.list = record.sleeping;
            sl.next_id = record.next_id;
        }
        *self.loading_sleepers.lock().unwrap() = sl.list.clone();
        sl
    }

    /// Whether the restore must not give this awake pane a latest-conversation selector
    /// (Claude `--continue`, Codex `resume --last`, on any restart branch): a loaded sleeper
    /// holds a pane of the same agent kind, in any folder, so "the latest" could be the
    /// sleeper's conversation. False with no such sleeper (main's behaviour).
    /// What the restore starts for a saved pane: main's [`crate::restore::restart_spec`],
    /// unless an agent of this kind sleeps (`no_latest`): then a fresh command from
    /// `agent_start` (verified id, else a target on the parsed command line, else the picker),
    /// or, when it can't be rebuilt completely, a shell with a note (logged). The flag says
    /// whether the agent is started.
    pub fn restart_for(
        &self,
        old: &str,
        sp: &SavedPane,
    ) -> (Vec<String>, Vec<(String, String)>, bool) {
        if !self.no_latest(sp) {
            let (argv, env) = crate::restore::restart_spec(sp);
            return (argv, env, true);
        }
        let s = crate::agent_start::start(sp, crate::agent_start::Target::Explicit);
        if let Some(why) = s.refused {
            let kind = sp.agent.as_deref().unwrap_or("agent");
            eprintln!(
                "orchd: {old}: {kind} not restarted ({why}); the pane is a shell with a note"
            );
        }
        (s.argv, s.env, s.agent)
    }

    pub fn no_latest(&self, sp: &crate::restore::SavedPane) -> bool {
        let Some(kind) = sp
            .agent
            .as_deref()
            .filter(|k| matches!(*k, "claude" | "codex"))
        else {
            return false;
        };
        self.loading_sleepers
            .lock()
            .unwrap()
            .iter()
            .flat_map(|x| x.panes.values())
            .any(|p| p.saved.agent.as_deref() == Some(kind))
    }

    /// Called by the restore on the layout it parsed, whichever file that was, before it starts
    /// anything (after it marked the lone Codex panes): every pane of the sleepers loaded at
    /// start-up (all pane ids in their tab trees, also ones whose program had already ended,
    /// their restartable records, and the new ids of an interrupted wake) is taken out, so a
    /// stale awake copy of a sleeper is never started; the sleeper record is the truth for them.
    /// (Awake agents of a sleeping kind get no latest-conversation selector: `no_latest`.)
    /// Other panes and workspaces stay. With no sleepers this returns
    /// at once and the layout is untouched.
    pub fn skip_asleep(&self, saved: &mut crate::restore::Saved) {
        let loading = self.loading_sleepers.lock().unwrap().clone();
        let mine: HashSet<String> = loading
            .iter()
            .flat_map(|x| {
                x.workspace
                    .tabs
                    .iter()
                    .flat_map(|t| t.root.panes())
                    .chain(x.panes.keys().cloned())
                    .chain(x.wake.iter().flat_map(|w| w.map.values().cloned()))
            })
            .collect();
        if mine.is_empty() {
            return;
        }
        saved.panes.retain(|id, _| !mine.contains(id));
        let keep: HashMap<String, String> = saved
            .session
            .workspaces
            .iter()
            .flat_map(|w| w.tabs.iter().flat_map(|t| t.root.panes()))
            .filter(|p| !mine.contains(p))
            .map(|p| (p.clone(), p))
            .collect();
        saved.session.workspaces = saved
            .session
            .workspaces
            .iter()
            .filter_map(|w| mapped(w, &keep))
            .collect();
    }

    /// Start-up, after the restore (which, see `skip_asleep`, started none of a sleeper's
    /// panes). No sleeper is retired here. Two sleepers of one workspace id: the first one
    /// stays. An interrupted wake moves its tasks and names back to the old panes (its map stays
    /// if that fails, for the next start). Ids an older orchd handed out meanwhile are
    /// renumbered. The record is rewritten if anything changed.
    pub fn install_sleepers(&self, sl: Sleepers) {
        let mut dirty = false;
        let mut seen: HashSet<String> = HashSet::new();
        let mut list = Vec::new();
        for mut x in sl.list {
            if !seen.insert(x.workspace.id.clone()) {
                eprintln!(
                    "orchd: a second sleeper record of {} is dropped",
                    x.workspace.label
                );
                dirty = true;
                continue;
            }
            if let Some(w) = x.wake.clone() {
                match self.move_owners(&x, &w.map, true) {
                    Ok(()) => {
                        x.wake = None;
                        dirty = true;
                    }
                    Err(e) => eprintln!(
                        "orchd: undoing the interrupted wake of {} failed: {e:#}; tried again at the next start",
                        x.workspace.label
                    ),
                }
            }
            list.push(x);
        }
        let num = |id: &str| id.get(1..).and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
        let _ = self.edit(|s| {
            s.next_id = s.next_id.max(sl.next_id);
            for x in &list {
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
            for x in &mut list {
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
            s.sleeping = list;
            Ok(())
        });
        self.loading_sleepers.lock().unwrap().clear();
        if dirty {
            if let Err(e) = self.commit(|s| Ok((Change::sleepers(s.sleeping.clone()), ()))) {
                eprintln!("orchd: {}", e.message);
            }
        }
        crate::fault::crash_at("after_install");
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

    // ---------- sleep ----------

    /// What sleeping `ws_id` would do, each pane's record and the pane ids it held. Reads
    /// the screens, the bus and /proc; changes nothing.
    fn sleep_inspect(&self, ws_id: &str) -> Result<Inspected, ApiError> {
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
        // The same per-pane record a restart saves.
        let saved = self.snapshot(false);
        let mut plan = SleepPlan {
            workspace: w.id.clone(),
            label: w.label.clone(),
            ..Default::default()
        };
        let mut panes = HashMap::new();
        let ids: Vec<String> = w.tabs.iter().flat_map(|t| t.root.panes()).collect();
        for id in &ids {
            let live = self.find(id).ok().filter(|p| p.exit_code().is_none());
            let (Some(p), Some(sp)) = (live, saved.panes.get(id)) else {
                plan.notes
                    .push(format!("{id} has ended already: it is left out"));
                continue;
            };
            let mut sp = sp.clone();
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
                    sp.argv.first().map_or("a program", |a| base(a)),
                    sp.cwd
                ));
                sp.argv = Vec::new();
            }
            let interrupted = !busy.is_empty();
            plan.blockers
                .extend(busy.into_iter().map(|b| format!("{who} {b}")));
            panes.insert(
                id.clone(),
                SleptPane {
                    saved: sp,
                    limited,
                    limit_reset,
                    auto_continue,
                    interrupted,
                },
            );
        }
        Ok((plan, panes, ids))
    }

    /// `orch ws sleep`.
    pub async fn sleep_workspace(
        self: &Arc<Self>,
        key: &str,
        force: bool,
        dry_run: bool,
    ) -> Result<Value, ApiError> {
        let session = self.session();
        let ws_id = match (session.workspace(key), session.sleeping(key)) {
            (Some(w), _) if session.sleeping.iter().any(|x| x.workspace.id == w.id) => {
                return Err(err(
                    "ws_transition",
                    format!(
                        "{} was just woken and is not in the autosave yet; try again shortly",
                        w.label
                    ),
                ))
            }
            (Some(w), _) => w.id.clone(),
            (None, Some(s)) => {
                return Ok(json!({
                    "session": session, "slept": false, "already": true,
                    "plan": SleepPlan { workspace: s.workspace.id.clone(), label: s.workspace.label.clone(), ..Default::default() },
                }))
            }
            _ => return Err(err("no_workspace", format!("no workspace {key:?}"))),
        };
        drop(session);
        let _reserved = self.reserve(&ws_id)?;
        // Requests already writing into it or changing its layout finish first, and a task or
        // nudge being typed; later ones see the reservation.
        self.drain(&ws_id).await?;
        drop(self.task_gate.lock().await);
        crate::fault::pause_at("sleep_pause_ms").await;
        let d = Arc::clone(self);
        let id = ws_id.clone();
        let (plan, panes, ids) = tokio::task::spawn_blocking(move || d.sleep_inspect(&id))
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
        // The record is written durably (rename and folder sync) before the workspace leaves
        // the layout and before anything ends. A failed write ends nothing.
        let (id, ids2) = (ws_id.clone(), ids.clone());
        let committed = self
            .commit_off(move |s| {
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
                let mut list = s.sleeping.clone();
                list.push(Sleeping {
                    workspace: s.workspaces[at].clone(),
                    position: at,
                    slept_ms: now_ms(),
                    panes,
                    wake: None,
                });
                Ok((
                    Change {
                        sleeping: list,
                        remove: vec![id.clone()],
                    },
                    (),
                ))
            })
            .await;
        if let Err(e) = committed {
            return Err(match e.code {
                "save_failed" => err(
                    "save_failed",
                    format!("{}; nothing was put to sleep", e.message),
                ),
                _ => e,
            });
        }
        crate::fault::crash_at("sleep_after_commit");
        crate::fault::pause_at("sleep_before_kill_ms").await;
        let failures = self.kill_panes(&ids).await;
        let warnings: Vec<String> = failures
            .iter()
            .map(|(id, e)| {
                format!("{id} survived ({e}): it keeps running outside any workspace (orch ls; orch kill {id})")
            })
            .collect();
        if !failures.is_empty() {
            self.strays.lock().unwrap().insert(
                ws_id.clone(),
                failures.iter().map(|(i, _)| i.clone()).collect(),
            );
        }
        let _ = self.ui_events.send(crate::ui::UiEvent::Panes);
        for name in &agents {
            let _ = self.bus.set_status(
                name,
                "idle",
                Some(&format!("asleep · orch ws wake {}", plan.label)),
            );
        }
        let text = format!(
            "{} is asleep: {} pane(s), {n_agents} agent(s){}; wake it with: orch ws wake {}",
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

    // ---------- wake ----------

    /// `orch ws wake`: every pane comes back, or none does and the sleeper stays as it was.
    pub async fn wake_workspace(self: &Arc<Self>, key: &str) -> Result<Value, ApiError> {
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
        let reserved = self.reserve(&ws_id)?;
        let sl = self
            .session()
            .sleeping(&ws_id)
            .cloned()
            .ok_or_else(|| err("no_workspace", "it was closed meanwhile"))?;
        let label = sl.workspace.label.clone();
        if sl.wake.is_some() {
            return Err(err(
                "ws_transition",
                format!("{label} is still waking (or an earlier wake is not undone yet); try again shortly"),
            ));
        }
        // A program of it that survived being ended still runs: no second copy.
        let strays: Vec<String> = self
            .strays
            .lock()
            .unwrap()
            .get(&ws_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|p| self.find(p).is_ok_and(|p| p.exit_code().is_none()))
            .collect();
        if !strays.is_empty() {
            return Err(err(
                "ws_stuck",
                format!(
                    "{label} can't wake while its old program(s) still run: {} (orch kill them first)",
                    strays.join(", ")
                ),
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
        // Nothing starts unless its record can be written.
        let d = Arc::clone(self);
        if let Err(e) = tokio::task::spawn_blocking(move || d.sleep_store.writable())
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r)
        {
            return Err(err("save_failed", format!("{label} stays asleep: {e}")));
        }
        // Start every pane (off the runtime), in tab order; each joins the reservation as it is
        // created.
        let order: Vec<(String, SleptPane)> = sl
            .workspace
            .tabs
            .iter()
            .flat_map(|t| t.root.panes())
            .filter_map(|old| sl.panes.get(&old).map(|p| (old, p.clone())))
            .collect();
        let d = Arc::clone(self);
        let wsid = ws_id.clone();
        let started = tokio::task::spawn_blocking(move || {
            order
                .into_iter()
                .map(|(old, sp)| {
                    let r = d.spawn_slept(&sp.saved, &wsid);
                    (old, sp, r)
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|e| err("spawn_failed", e.to_string()))?;
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
        let mut spawned: Vec<String> = Vec::new();
        for (old, sp, r) in started {
            match r {
                Ok(p) => {
                    spawned.push(p.id.clone());
                    let up = p.exit_code().is_none()
                        && match sp.saved.agent.as_deref() {
                            None => true,
                            // Not started on purpose: a shell showing the note.
                            Some(_) if !wake_start(&sp.saved).agent => true,
                            Some(_) if !shell_pane(&sp.saved) => true,
                            Some(_) => p
                                .pid
                                .is_some_and(|pid| has_agent_child(pid, &agent_exe(&sp.saved))),
                        };
                    if up {
                        ok.push((old, sp, p));
                    } else {
                        failed.push((old, format!("{} did not start", p.id)));
                    }
                }
                Err(e) => failed.push((old, e)),
            }
        }
        if !failed.is_empty() || ok.is_empty() {
            let msg = format!(
                "{label} stays asleep: {}",
                failed
                    .iter()
                    .map(|(o, e)| format!("{o}: {e}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            );
            return Err(err(
                "wake_failed",
                self.end_started(&ws_id, &spawned, msg).await,
            ));
        }
        let new_panes: Vec<String> = ok.iter().map(|(_, _, p)| p.id.clone()).collect();
        let map: HashMap<String, String> = ok
            .iter()
            .map(|(o, _, p)| (o.clone(), p.id.clone()))
            .collect();
        // 1. The map, durably, before any owner moves.
        let m = map.clone();
        let written = self
            .commit_sleeper(&ws_id, move |x| {
                x.wake = Some(WakeState { map: m });
                true
            })
            .await;
        if let Err(e) = written {
            let msg = format!("{label} stays asleep: {}", e.message);
            return Err(err(e.code, self.end_started(&ws_id, &new_panes, msg).await));
        }
        crate::fault::pause_at("wake_pause_ms").await;
        // 2. Owners: running tasks and bus names follow the new panes.
        if let Err(e) = self.move_owners(&sl, &map, false) {
            self.undo_wake(&ws_id, &sl, &map, &new_panes).await;
            return Err(err("bus", format!("moving tasks / names failed: {e:#}")));
        }
        crate::fault::crash_at("wake_after_owners");
        // 3. Back in the layout; the autosave picks it up.
        let Some(mut w) = mapped(&sl.workspace, &map) else {
            self.undo_wake(&ws_id, &sl, &map, &new_panes).await;
            return Err(err("wake_failed", "nothing of it came back"));
        };
        let _ = self.edit(|s| {
            if s.workspaces.iter().any(|x| x.label == w.label) {
                w.label = unique_label(s, &w.label);
            }
            let at = sl.position.min(s.workspaces.len());
            s.workspaces.insert(at, w.clone());
            Ok(())
        });
        let _ = self.ui_events.send(crate::ui::UiEvent::Panes);
        let not_started: Vec<Value> = ok
            .iter()
            .filter_map(|(old, sp, p)| {
                let why = wake_start(&sp.saved).refused?;
                let kind = sp.saved.agent.as_deref().unwrap_or("agent");
                eprintln!(
                    "orchd: wake {label}: {} (was {old}): {kind} not started ({why}); the pane is a shell with a note",
                    p.id
                );
                Some(json!({
                    "pane": p.id, "old": old, "kind": kind,
                    "name": sp.saved.bus_name.clone().or_else(|| sp.saved.name.clone()),
                    "reason": why,
                }))
            })
            .collect();
        self.after_wake_all(&ok, &w.label);
        // 4. The sleeper goes once the autosave on disk holds the new panes (until then a crash
        //    leaves it asleep, see `install_sleepers`). The reservation lasts until then.
        let confirmed = self.confirm_wake(&ws_id, &map, reserved).await;
        Ok(json!({
            "session": self.session(),
            "workspace": w.id,
            "panes": new_panes,
            "failed": [],
            "not_started": not_started,
            "confirmed": confirmed,
        }))
    }

    /// Waits until the autosave file holds the woken workspace with its new panes placed, then
    /// drops the sleeper. It belongs to this one wake: the sleeper is removed only if it still
    /// carries this wake's pane map (compared in the same commit), and it stops as soon as that
    /// sleeper is gone or carries another map (closed, or slept and woken again). A new pane
    /// that has ended is no longer waited for. The reservation is released when the sleeper is
    /// dropped or by a timer at the deadline, whatever the polling is doing then; the sleeper is
    /// still dropped later if it takes longer. Returns whether it was dropped in time.
    async fn confirm_wake(
        self: &Arc<Self>,
        ws_id: &str,
        map: &HashMap<String, String>,
        reserved: Reservation,
    ) -> bool {
        let wait = std::env::var("ORCHD_WAKE_CONFIRM_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .map_or(CONFIRM_FOR, Duration::from_millis);
        let reserved = Arc::new(std::sync::Mutex::new(Some(reserved)));
        let timer = Arc::clone(&reserved);
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            timer.lock().unwrap().take();
        });
        let d = Arc::clone(self);
        let (id, mine) = (ws_id.to_string(), map.clone());
        let want: Vec<String> = map.values().cloned().collect();
        let job = tokio::spawn(async move {
            loop {
                // Still this wake's sleeper?
                let ours = d.session().sleeping.iter().any(|x| {
                    x.workspace.id == id && x.wake.as_ref().map(|w| &w.map) == Some(&mine)
                });
                if !ours {
                    break;
                }
                let live: Vec<&String> = want
                    .iter()
                    .filter(|p| d.find(p).is_ok_and(|p| p.exit_code().is_none()))
                    .collect();
                let saved = d
                    .autosave
                    .path()
                    .and_then(|p| std::fs::read(p).ok())
                    .and_then(|b| serde_json::from_slice::<crate::restore::Saved>(&b).ok());
                // Placed in its workspace there (the snapshot's pane records also list panes
                // not yet placed).
                let placed = live.is_empty()
                    || saved.is_some_and(|s| {
                        s.session.workspaces.iter().any(|w| {
                            w.id == id
                                && live
                                    .iter()
                                    .all(|p| w.tabs.iter().any(|t| t.root.contains(p)))
                        }) && live.iter().all(|p| s.panes.contains_key(*p))
                    });
                if placed {
                    crate::fault::crash_at("wake_before_drop");
                    let (id2, mine2) = (id.clone(), mine.clone());
                    let dropped = if crate::fault::on("drop_fail") {
                        Err(err("save_failed", "injected"))
                    } else {
                        d.commit_off(move |s| {
                            let list: Vec<Sleeping> = s
                                .sleeping
                                .iter()
                                .filter(|x| {
                                    !(x.workspace.id == id2
                                        && x.wake.as_ref().map(|w| &w.map) == Some(&mine2))
                                })
                                .cloned()
                                .collect();
                            if list.len() == s.sleeping.len() {
                                return Err(err("no_workspace", "not this wake's sleeper"));
                            }
                            Ok((Change::sleepers(list), ()))
                        })
                        .await
                    };
                    match dropped {
                        Ok(()) => return true,
                        Err(e) if e.code == "no_workspace" => break,
                        Err(e) => {
                            eprintln!("orchd: dropping the sleeper of a wake: {}", e.message)
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            false
        });
        let done = Arc::clone(&reserved);
        let confirmed = tokio::spawn(async move {
            let ok = job.await.unwrap_or(false);
            done.lock().unwrap().take();
            ok
        });
        matches!(tokio::time::timeout(wait, confirmed).await, Ok(Ok(true)))
    }

    /// Ends the panes a failed wake started. One that can't be ended keeps running outside any
    /// workspace (`orch ls`, `orch kill`), and the sleeper can't wake while it does.
    async fn end_started(self: &Arc<Self>, ws_id: &str, panes: &[String], msg: String) -> String {
        let survivors = self.kill_panes(panes).await;
        if survivors.is_empty() {
            return msg;
        }
        let ids: Vec<String> = survivors.into_iter().map(|(i, _)| i).collect();
        self.strays
            .lock()
            .unwrap()
            .insert(ws_id.to_string(), ids.clone());
        format!(
            "{msg}; still running outside any workspace (could not be ended): {} - orch kill them before waking it again",
            ids.join(", ")
        )
    }

    /// Starts one slept pane exactly in its folder (a folder that is gone is a failure: the
    /// record is kept, not moved to the home folder). It joins the wake's reservation before
    /// anything can find it.
    fn spawn_slept(&self, sp: &SavedPane, ws_id: &str) -> Result<Arc<Pane>, String> {
        if crate::fault::value("spawn_fail").is_some_and(|n| sp.name.as_deref() == Some(n)) {
            return Err("injected spawn failure".into());
        }
        if !Path::new(&sp.cwd).is_dir() {
            return Err(format!("its folder {} is gone", sp.cwd));
        }
        let (argv, extra) = wake_argv(sp);
        let env = (!extra.is_empty()).then(|| {
            orch_proto::env::full_env()
                .into_iter()
                .chain(extra)
                .collect::<Vec<_>>()
        });
        self.new_pane_in(
            NewPane {
                name: sp.name.clone(),
                argv,
                cwd: Some(sp.cwd.clone()),
                env,
                rows: Some(sp.rows.max(2)),
                cols: Some(sp.cols.max(2)),
            },
            Some(ws_id),
        )
        .map_err(|e| e.message)
    }

    /// Undoes a wake whose owners moved: owners back, new panes ended, the map cleared, but only
    /// once every owner is back: otherwise it stays in the record and the next start tries
    /// again.
    async fn undo_wake(
        self: &Arc<Self>,
        ws_id: &str,
        sl: &Sleeping,
        map: &HashMap<String, String>,
        panes: &[String],
    ) {
        let back = self.move_owners(sl, map, true);
        let msg = self.end_started(ws_id, panes, String::new()).await;
        if !msg.is_empty() {
            eprintln!("orchd: {msg}");
        }
        match back {
            Ok(()) => {
                if let Err(e) = self
                    .commit_sleeper(ws_id, |x| {
                        x.wake = None;
                        true
                    })
                    .await
                {
                    // The next start undoes an interrupted wake by itself.
                    eprintln!("orchd: {}", e.message);
                }
            }
            Err(e) => eprintln!(
                "orchd: moving the tasks / names of {} back failed: {e:#}; its map is kept, the next start tries again",
                sl.workspace.label
            ),
        }
    }

    fn after_wake_all(self: &Arc<Self>, ok: &[(String, SleptPane, Arc<Pane>)], label: &str) {
        let mut resumed = 0;
        for (_, slept, p) in ok {
            let Some(kind) = slept.saved.agent.clone() else {
                continue;
            };
            if !wake_start(&slept.saved).agent {
                continue;
            }
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
            "{label} is awake: {} pane(s), resumed {resumed} agent(s)",
            ok.len()
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
        let _reserved = self.reserve(&id)?;
        let Some(sl) = self.session().sleeping(&id).cloned() else {
            return Ok(false);
        };
        self.commit_sleeper(&id, |_| false).await?;
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

/// A usage-limit continue for `pane` after `reset` + grace, if auto-continue is still on then.
/// In memory only: an orchd restart before it is due drops it (the agent's screen no longer
/// shows its limit notice after the wake).
fn schedule_continue(d: &Arc<Daemon>, pane: Arc<Pane>, name: String, reset: i64, mail: String) {
    let d = Arc::clone(d);
    tokio::spawn(async move {
        let due = reset + LIMIT_GRACE_SECS - now_ms() / 1000;
        if due > 0 {
            tokio::time::sleep(Duration::from_secs(due as u64)).await;
        }
        if !crate::limits::auto_continue(&pane.id, None) {
            return;
        }
        let text = format!("{}{mail}", crate::limits::CONTINUE_TEXT);
        if type_when_idle(&d, &pane, &text).await {
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
    if slept.limited && slept.auto_continue {
        match slept.limit_reset {
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
