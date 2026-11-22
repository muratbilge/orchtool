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

/// The pane's process has a child (an agent started inside a shell wrapper is running).
fn has_child(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .is_ok_and(|s| !s.trim().is_empty())
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

/// The sleepers read at start-up, until they are installed in the layout.
#[derive(Default)]
pub struct Sleepers {
    pub list: Vec<Sleeping>,
    next_id: u64,
    reconciled: bool,
    dirty: bool,
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

impl Daemon {
    // ---------- reservations ----------

    pub(crate) fn reserve(self: &Arc<Self>, ws_id: &str) -> Result<Reservation, ApiError> {
        if self.stopping_soon.load(std::sync::atomic::Ordering::SeqCst)
            || self.stopping.load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(err("ws_transition", "orchd is stopping"));
        }
        if !self.transitions.lock().unwrap().insert(ws_id.to_string()) {
            return Err(err(
                "ws_transition",
                "that workspace is already going to sleep or waking",
            ));
        }
        Ok(Reservation {
            d: Arc::clone(self),
            id: ws_id.to_string(),
        })
    }

    pub(crate) fn ws_in_transition(&self, ws_id: &str) -> bool {
        self.transitions.lock().unwrap().contains(ws_id)
    }

    /// The pane belongs to a workspace going to sleep or waking.
    pub(crate) fn in_transition(&self, pane: &str) -> bool {
        let busy = self.transitions.lock().unwrap().clone();
        if busy.is_empty() {
            return false;
        }
        let s = self.session();
        s.locate(pane)
            .is_some_and(|(wi, _)| busy.contains(&s.workspaces[wi].id))
    }

    pub(crate) fn not_in_transition(&self, pane: &str) -> Result<(), ApiError> {
        if self.in_transition(pane) {
            return Err(err(
                "ws_transition",
                "its workspace is going to sleep or waking; try again",
            ));
        }
        Ok(())
    }

    /// An explicit bus name of an agent in a sleeping workspace is reserved for it: only its
    /// own new pane (during its wake) may register it.
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
            if !x
                .panes
                .values()
                .any(|p| p.saved.bus_name.as_deref() == Some(name.as_str()))
            {
                continue;
            }
            let own = x.wake.as_ref().is_some_and(|w| {
                w.map
                    .values()
                    .any(|n| r.pane.as_deref() == Some(format!("orch:{n}").as_str()))
            });
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

    /// Writes the sleepers as they are now (durably, a new generation). Serialized: the
    /// state is read under the store's lock.
    pub(crate) fn persist_sleepers(&self) -> Result<(), ApiError> {
        let mut st = self.sleep_store.lock();
        let (list, next_id) = {
            let s = self.session.lock().unwrap();
            (s.sleeping.clone(), s.next_id)
        };
        self.sleep_store
            .write(&mut st, &list, next_id)
            .map_err(|e| {
                err(
                    "save_failed",
                    format!("saving the sleeping workspaces failed: {e}"),
                )
            })
    }

    pub fn load_sleepers(&self) -> Sleepers {
        match self.sleep_store.load() {
            Some(r) => Sleepers {
                list: r.sleeping,
                next_id: r.next_id,
                ..Default::default()
            },
            None => Sleepers::default(),
        }
    }

    /// Moves the owners of `x`'s panes (running tasks, bus names) along `map` (old → new), or
    /// back (`back`).
    fn move_owners(
        &self,
        x: &Sleeping,
        map: &HashMap<String, String>,
        back: bool,
    ) -> anyhow::Result<()> {
        for (old, new) in map {
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

    /// Start-up, before the awake layout is restored: finishes or undoes a wake that a crash
    /// interrupted, takes over sleepers from an older layout file, and drops from `saved` every
    /// workspace that is asleep (the sleeper record is newer than a stale awake layout).
    pub fn reconcile_sleepers(
        &self,
        sl: &mut Sleepers,
        mut saved: Option<&mut crate::restore::Saved>,
    ) {
        if let Some(saved) = saved.as_deref_mut() {
            for x in saved.session.sleeping.drain(..) {
                if !sl.list.iter().any(|y| y.workspace.id == x.workspace.id) {
                    sl.list.push(x);
                    sl.dirty = true;
                }
            }
            sl.next_id = sl.next_id.max(saved.session.next_id);
        }
        let in_layout = |id: &str| {
            saved
                .as_deref()
                .is_some_and(|s| s.session.workspaces.iter().any(|w| w.id == id))
        };
        let mut keep = Vec::new();
        for mut x in std::mem::take(&mut sl.list) {
            match x.wake.take() {
                Some(w) if w.committed && in_layout(&x.workspace.id) => {
                    eprintln!(
                        "orchd: the wake of {} had committed; it is awake",
                        x.workspace.label
                    );
                    sl.dirty = true;
                    continue;
                }
                Some(w) => {
                    if let Err(e) = self.move_owners(&x, &w.map, true) {
                        eprintln!(
                            "orchd: undoing the interrupted wake of {}: {e:#}",
                            x.workspace.label
                        );
                    }
                    eprintln!(
                        "orchd: the wake of {} was interrupted; it stays asleep",
                        x.workspace.label
                    );
                    sl.dirty = true;
                }
                None => {}
            }
            keep.push(x);
        }
        sl.list = keep;
        if let Some(saved) = saved {
            saved
                .session
                .workspaces
                .retain(|w| !sl.list.iter().any(|x| x.workspace.id == w.id));
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
    pub fn install_sleepers(&self, mut sl: Sleepers) {
        if !sl.reconciled {
            self.reconcile_sleepers(&mut sl, None);
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
            if let Err(e) = self.persist_sleepers() {
                eprintln!("orchd: {}", e.message);
            }
        }
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
        let _reserved = self.reserve(&ws_id)?;
        // A task or nudge already being typed into it finishes first; later ones see the
        // reservation.
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
        // The commit: the layout must still hold exactly the panes inspected.
        let position = self.edit(|s| {
            let at = s
                .workspaces
                .iter()
                .position(|w| w.id == ws_id)
                .ok_or_else(|| err("no_workspace", "the workspace is gone"))?;
            let now: Vec<String> = s.workspaces[at]
                .tabs
                .iter()
                .flat_map(|t| t.root.panes())
                .collect();
            if now != ids {
                return Err(err(
                    "ws_changed",
                    "its panes changed while it was checked; nothing was done, try again",
                ));
            }
            let workspace = s.workspaces.remove(at);
            s.sleeping.push(Sleeping {
                workspace,
                position: at,
                slept_ms: now_ms(),
                panes,
                wake: None,
                stuck: Vec::new(),
            });
            Ok(at)
        })?;
        // Durable before anything ends; otherwise put it back and end nothing.
        if let Err(e) = self.persist_sleepers() {
            let _ = self.edit(|s| {
                if let Some(i) = s.sleeping.iter().position(|x| x.workspace.id == ws_id) {
                    let x = s.sleeping.remove(i);
                    let at = position.min(s.workspaces.len());
                    s.workspaces.insert(at, x.workspace);
                }
                Ok(())
            });
            return Err(err(
                "save_failed",
                format!("{}; nothing was put to sleep", e.message),
            ));
        }
        crate::fault::crash_at("sleep_after_commit");
        let d = Arc::clone(self);
        let _ = tokio::task::spawn_blocking(move || d.autosave_now(false)).await;
        let failures = self.kill_panes(&ids).await;
        let mut warnings: Vec<String> = failures.iter().map(|(_, e)| e.clone()).collect();
        if !failures.is_empty() {
            warnings.push(self.keep_survivors(&ws_id, &plan.label, &failures));
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

    /// Programs that outlived SIGKILL stay visible in an awake workspace "<label>-stuck", and
    /// the sleeper can't wake (no second copy) until they are gone.
    fn keep_survivors(&self, ws_id: &str, label: &str, failed: &[(String, String)]) -> String {
        let ids: Vec<String> = failed
            .iter()
            .map(|(id, _)| id.clone())
            .filter(|id| !id.is_empty())
            .collect();
        let mut stuck_label = String::new();
        let _ = self.edit(|s| {
            let cwd = s
                .sleeping(ws_id)
                .map(|x| x.workspace.cwd.clone())
                .unwrap_or_default();
            stuck_label = unique_label(s, &format!("{label}-stuck"));
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
                cwd,
                tabs,
                active_tab: 0,
            });
            if let Some(x) = s.sleeping_mut(ws_id) {
                x.stuck = ids.clone();
            }
            Ok(())
        });
        if let Err(e) = self.persist_sleepers() {
            eprintln!("orchd: {}", e.message);
        }
        format!(
            "{} pane(s) survived SIGKILL: they are in workspace {stuck_label}; {label} wakes once they are gone",
            ids.len()
        )
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
        let _reserved = self.reserve(&ws_id)?;
        let sl = self
            .session()
            .sleeping(&ws_id)
            .cloned()
            .ok_or_else(|| err("no_workspace", "it was closed meanwhile"))?;
        let label = sl.workspace.label.clone();
        let stuck: Vec<&String> = sl
            .stuck
            .iter()
            .filter(|p| self.find(p).is_ok_and(|p| p.exit_code().is_none()))
            .collect();
        if !stuck.is_empty() {
            return Err(err(
                "ws_stuck",
                format!(
                    "{label} can't wake while its old program(s) still run: {} (close them first)",
                    stuck
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
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
        // Start every pane (off the runtime), in tab order.
        let order: Vec<(String, SleptPane)> = sl
            .workspace
            .tabs
            .iter()
            .flat_map(|t| t.root.panes())
            .filter_map(|old| sl.panes.get(&old).map(|p| (old, p.clone())))
            .collect();
        let d = Arc::clone(self);
        let started = tokio::task::spawn_blocking(move || {
            order
                .into_iter()
                .map(|(old, sp)| {
                    let r = d.spawn_slept(&sp.saved);
                    (old, sp, r)
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|e| err("spawn_failed", e.to_string()))?;
        // An agent counts as started once it is still up after a moment (inside a shell
        // wrapper: the shell still has it as a child).
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
        for (old, sp, r) in started {
            match r {
                Ok(p) => {
                    let up = p.exit_code().is_none()
                        && (sp.saved.agent.is_none()
                            || !shell_pane(&sp.saved)
                            || p.pid.is_some_and(has_child));
                    if up {
                        ok.push((old, sp, p));
                    } else {
                        failed.push((old, format!("{} did not start", p.id)));
                        let _ = self.kill(&p.id).await;
                    }
                }
                Err(e) => failed.push((old, e)),
            }
        }
        let new_panes: Vec<String> = ok.iter().map(|(_, _, p)| p.id.clone()).collect();
        if ok.is_empty() || (!failed.is_empty() && !partial) {
            self.kill_panes(&new_panes).await;
            return Err(err(
                "wake_failed",
                format!(
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
                ),
            ));
        }
        let map: HashMap<String, String> = ok
            .iter()
            .map(|(o, _, p)| (o.clone(), p.id.clone()))
            .collect();
        // 1. The map, durably, before any owner moves.
        let _ = self.edit(|s| {
            if let Some(x) = s.sleeping_mut(&ws_id) {
                x.wake = Some(WakeState {
                    map: map.clone(),
                    committed: false,
                });
            }
            Ok(())
        });
        if let Err(e) = self.persist_sleepers() {
            self.undo_wake(&ws_id, &sl, &map, false, &new_panes).await;
            return Err(e);
        }
        // 2. Owners: running tasks and bus names follow the new panes.
        if let Err(e) = self.move_owners(&sl, &map, false) {
            self.undo_wake(&ws_id, &sl, &map, true, &new_panes).await;
            return Err(err("bus", format!("moving tasks / names failed: {e:#}")));
        }
        crate::fault::crash_at("wake_after_owners");
        // 3. The awake layout, durably.
        let Some(mut w) = mapped(&sl.workspace, &map) else {
            self.undo_wake(&ws_id, &sl, &map, true, &new_panes).await;
            return Err(err("wake_failed", "nothing of it came back"));
        };
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
        // 4. Committed: the sleeper (and the panes that did not come back, kept asleep).
        let mut rest_label = None;
        let _ = self.edit(|s| {
            if let Some(x) = s.sleeping_mut(&ws_id) {
                if let Some(wk) = &mut x.wake {
                    wk.committed = true;
                }
            }
            if let Some(Some(mut r)) = rest.clone() {
                r.workspace.id = s.new_id("w");
                for t in &mut r.workspace.tabs {
                    t.id = s.new_id("t");
                }
                r.workspace.label = unique_label(s, &format!("{label}-rest"));
                rest_label = Some(r.workspace.label.clone());
                s.sleeping.push(r);
            }
            Ok(())
        });
        if let Err(e) = self.persist_sleepers() {
            let _ = self.edit(|s| {
                if let Some(l) = &rest_label {
                    s.sleeping.retain(|x| &x.workspace.label != l);
                }
                Ok(())
            });
            self.unplace_woken(&ws_id);
            self.undo_wake(&ws_id, &sl, &map, true, &new_panes).await;
            let d = Arc::clone(self);
            let _ = tokio::task::spawn_blocking(move || d.autosave_sync()).await;
            return Err(e);
        }
        crate::fault::crash_at("wake_after_commit");
        // 5. The sleeper goes.
        let _ = self.edit(|s| {
            s.sleeping.retain(|x| x.workspace.id != ws_id);
            Ok(())
        });
        if let Err(e) = self.persist_sleepers() {
            // The next start sees a committed wake with the workspace in the layout.
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
    fn spawn_slept(&self, sp: &SavedPane) -> Result<Arc<Pane>, String> {
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
        self.new_pane(NewPane {
            name: sp.name.clone(),
            argv,
            cwd: Some(sp.cwd.clone()),
            env,
            rows: Some(sp.rows.max(2)),
            cols: Some(sp.cols.max(2)),
        })
        .map_err(|e| e.message)
    }

    /// Takes the woken workspace (same id as the sleeper) out of the awake layout again.
    fn unplace_woken(&self, ws_id: &str) {
        let _ = self.edit(|s| {
            s.workspaces.retain(|w| w.id != ws_id);
            Ok(())
        });
    }

    /// Undoes a wake that did not commit: owners back, new panes ended, the record cleared.
    async fn undo_wake(
        self: &Arc<Self>,
        ws_id: &str,
        sl: &Sleeping,
        map: &HashMap<String, String>,
        owners_moved: bool,
        panes: &[String],
    ) {
        if owners_moved {
            if let Err(e) = self.move_owners(sl, map, true) {
                eprintln!("orchd: undoing the wake of {}: {e:#}", sl.workspace.label);
            }
        }
        self.kill_panes(panes).await;
        let _ = self.edit(|s| {
            if let Some(x) = s.sleeping_mut(ws_id) {
                x.wake = None;
            }
            Ok(())
        });
        if let Err(e) = self.persist_sleepers() {
            // The next start undoes the owners of an uncommitted wake by itself.
            eprintln!("orchd: {}", e.message);
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
    pub fn forget_sleeping(self: &Arc<Self>, key: &str) -> Result<bool, ApiError> {
        let Some(id) = self.session().sleeping(key).map(|x| x.workspace.id.clone()) else {
            return Ok(false);
        };
        let _reserved = self.reserve(&id)?;
        let Some(sl) = self.session().sleeping(&id).cloned() else {
            return Ok(false);
        };
        self.edit(|s| {
            s.sleeping.retain(|x| x.workspace.id != id);
            Ok(())
        })?;
        if let Err(e) = self.persist_sleepers() {
            let _ = self.edit(|s| {
                s.sleeping.push(sl.clone());
                Ok(())
            });
            return Err(e);
        }
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
