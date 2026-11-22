//! Workspaces that sleep and wake (`orch ws sleep|wake`). A sleeping workspace keeps its
//! layout (tabs, splits, sizes, labels, ids) and, per pane, the record `restore.json` uses to
//! start it again; then its programs end and their memory is free. It lives in the layout
//! (`Session::sleeping`), so it travels through `restore.json` and `autosave.json` and stays
//! asleep across an orchd restart or crash. Waking starts its panes again with the same
//! machinery as a restart (`restore::spawn_saved`): agents resume their conversation and take
//! their bus name back.
//!
//! What waits for a sleeping agent is kept, not delivered and not dropped: its unread messages
//! stay in its inbox (it gets a note to read them on wake), queued tasks for the workspace stay
//! queued (the dispatcher finds no agent there), a running task stays running (it follows the
//! pane on wake), open questions stay open, and an agent paused on its usage limit gets its
//! "continue" after the reset once it is awake. Nothing wakes a workspace by itself.

use crate::detect::{self, Detected};
use crate::server::{err, ApiError, Daemon};
use orch_proto::api::{RegisterAgent, SleepPlan, HUMAN};
use orch_proto::layout::{SavedPane, Sleeping, SleptPane};
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

/// How an agent will come back, in words (for the plan).
fn resume_words(sp: &SavedPane) -> String {
    let kind = sp.agent.as_deref().unwrap_or("agent");
    if let Some(id) = &sp.session_id {
        return format!("{kind} resumes conversation {id}");
    }
    // The command it will run (inside its shell's script when it ran in a shell).
    let joined = crate::restore::restart_spec(sp).0.join(" ");
    let words: Vec<&str> = joined.split_whitespace().collect();
    let after = |w: &str| {
        words
            .iter()
            .position(|x| *x == w)
            .and_then(|i| words.get(i + 1))
            .is_some_and(|next| !next.starts_with('-') && !next.ends_with(';'))
    };
    match kind {
        "codex" if words.contains(&"--last") => {
            "codex resumes its last conversation in this folder (resume --last)".into()
        }
        "codex" if after("resume") => "codex resumes the conversation on its command line".into(),
        "codex" => {
            "codex comes back at its resume picker (no verified session id: someone has to pick)"
                .into()
        }
        _ if after("--resume") || after("-r") => {
            format!("{kind} resumes the conversation on its command line")
        }
        _ => format!(
            "{kind} continues its last conversation in {} (--continue)",
            sp.cwd
        ),
    }
}

impl Daemon {
    /// Old pane ids of every sleeping workspace.
    pub fn asleep_panes(&self) -> HashSet<String> {
        self.session
            .lock()
            .unwrap()
            .sleeping
            .iter()
            .flat_map(|s| s.workspace.tabs.iter().flat_map(|t| t.root.panes()))
            .collect()
    }

    /// What sleeping `ws_id` would do, and each pane's record. Reads the screens, the bus and
    /// /proc; changes nothing.
    fn sleep_inspect(
        &self,
        ws_id: &str,
    ) -> Result<(SleepPlan, HashMap<String, SleptPane>), ApiError> {
        let session = self.session();
        let w = session
            .workspace(ws_id)
            .ok_or_else(|| err("no_workspace", format!("no workspace {ws_id:?}")))?;
        let bus = |e: anyhow::Error| err("bus", format!("{e:#}"));
        // Every pane's record (Codex `--last` is decided against all panes, as on a restart).
        let saved = self.snapshot(false);
        let questions = self.bus.questions(false).map_err(bus)?;
        let running: Vec<_> = self
            .bus
            .tasks(false)
            .map_err(bus)?
            .into_iter()
            .filter(|t| t.state == "running")
            .collect();
        let mut plan = SleepPlan {
            workspace: w.id.clone(),
            label: w.label.clone(),
            ..Default::default()
        };
        let mut panes = HashMap::new();
        for id in w.tabs.iter().flat_map(|t| t.root.panes()) {
            let live = self.find(&id).ok().filter(|p| p.exit_code().is_none());
            let (Some(p), Some(sp)) = (live, saved.panes.get(&id)) else {
                plan.notes
                    .push(format!("{id} has ended already: it is left out"));
                continue;
            };
            let mut sp = sp.clone();
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
            let limit_reset = crate::limits::info(&id).and_then(|(reset, _)| reset);
            let auto_continue = crate::limits::auto_continue(&id, None);
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
            } else if sp.argv.is_empty() || crate::restore::is_shell(&sp.argv[0]) {
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
            let interrupted = !busy.is_empty();
            plan.blockers
                .extend(busy.into_iter().map(|b| format!("{who} {b}")));
            panes.insert(
                id,
                SleptPane {
                    saved: sp,
                    limited,
                    limit_reset,
                    auto_continue,
                    interrupted,
                },
            );
        }
        Ok((plan, panes))
    }

    /// `orch ws sleep`. Serialized with task dispatch, nudges and other sleeps / wakes.
    pub async fn sleep_workspace(
        self: &Arc<Self>,
        key: &str,
        force: bool,
        dry_run: bool,
    ) -> Result<Value, ApiError> {
        let _gate = self.task_gate.lock().await;
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
        let all: Vec<String> = session
            .workspace(&ws_id)
            .map(|w| w.tabs.iter().flat_map(|t| t.root.panes()).collect())
            .unwrap_or_default();
        drop(session);
        let d = Arc::clone(self);
        let id = ws_id.clone();
        let (plan, panes) = tokio::task::spawn_blocking(move || d.sleep_inspect(&id))
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
        // First the layout (durably), then the programs: if orchd dies in between, the
        // workspace comes back asleep, not half-gone.
        self.edit(|s| {
            let at = s
                .workspaces
                .iter()
                .position(|w| w.id == ws_id)
                .ok_or_else(|| err("no_workspace", "the workspace is gone"))?;
            let workspace = s.workspaces.remove(at);
            s.sleeping.push(Sleeping {
                workspace,
                position: at,
                slept_ms: now_ms(),
                panes,
            });
            Ok(())
        })?;
        let d = Arc::clone(self);
        let _ = tokio::task::spawn_blocking(move || d.autosave_now(false)).await;
        let mut warnings = Vec::new();
        for id in &all {
            match self.kill(id).await {
                Ok(()) => {}
                Err(e) if e.code == "no_pane" => {}
                Err(e) => {
                    eprintln!("orchd: sleep {}: {}", plan.label, e.message);
                    warnings.push(e.message);
                }
            }
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
            all.len(),
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

    /// `orch ws wake`: the layout comes back with fresh panes, agents resume.
    pub async fn wake_workspace(self: &Arc<Self>, key: &str) -> Result<Value, ApiError> {
        let _gate = self.task_gate.lock().await;
        let session = self.session();
        let Some(sl) = session.sleeping(key).cloned() else {
            return match session.workspace(key) {
                Some(w) => Ok(json!({
                    "session": session, "workspace": w.id, "already": true,
                    "panes": [], "failed": [],
                })),
                None => Err(err("no_workspace", format!("no workspace {key:?}"))),
            };
        };
        drop(session);
        let mut panes: HashMap<String, SavedPane> = sl
            .panes
            .iter()
            .map(|(k, v)| (k.clone(), v.saved.clone()))
            .collect();
        // `codex resume --last` only while no awake Codex pane shares the folder now.
        let awake_codex: HashSet<String> = self
            .all_panes()
            .iter()
            .filter(|p| p.exit_code().is_none())
            .filter(|p| p.agent(&p.visible_text()).as_deref() == Some("codex"))
            .map(|p| {
                p.pid
                    .and_then(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).ok())
                    .map(|c| c.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.cwd.clone())
            })
            .collect();
        for sp in panes.values_mut() {
            if sp.sole_codex_in_cwd && awake_codex.contains(&sp.cwd) {
                sp.sole_codex_in_cwd = false;
            }
        }
        let mut spawned = self.spawn_saved(vec![sl.workspace.clone()], &panes);
        if spawned.workspaces.is_empty() {
            return Err(err(
                "wake_failed",
                format!(
                    "no pane of {} could be started; it stays asleep",
                    sl.workspace.label
                ),
            ));
        }
        let mut w = spawned.workspaces.remove(0);
        let placed = self.edit(|s| {
            let i = s
                .sleeping
                .iter()
                .position(|x| x.workspace.id == sl.workspace.id)
                .ok_or_else(|| err("no_workspace", "it was closed meanwhile"))?;
            s.sleeping.remove(i);
            if s.label_taken(&w.label, Some(&w.id)) {
                let mut n = 2;
                while s.label_taken(&format!("{}-{n}", w.label), None) {
                    n += 1;
                }
                w.label = format!("{}-{n}", w.label);
            }
            let at = sl.position.min(s.workspaces.len());
            s.workspaces.insert(at, w.clone());
            Ok(())
        });
        if let Err(e) = placed {
            for p in spawned.ids.values() {
                let _ = self.kill(&p.id).await;
            }
            return Err(e);
        }
        let _ = self.ui_events.send(crate::ui::UiEvent::Panes);
        let d = Arc::clone(self);
        let _ = tokio::task::spawn_blocking(move || d.autosave_now(false)).await;
        // Bus names follow the new panes now (not only once the agent's hook runs): mail,
        // questions and nudges reach it again.
        let mut jobs = Vec::new();
        for (old, p) in &spawned.ids {
            let slept = &sl.panes[old];
            let Some(kind) = slept.saved.agent.clone() else {
                continue;
            };
            p.set_kind(&kind);
            if !slept.auto_continue {
                crate::limits::auto_continue(&p.id, Some(false));
            }
            let name = slept.saved.bus_name.clone();
            if let Some(n) = &name {
                let reg = RegisterAgent {
                    kind: kind.clone(),
                    pane: Some(format!("orch:{}", p.id)),
                    cwd: Some(slept.saved.cwd.clone()),
                    name: Some(n.clone()),
                    instance: None,
                };
                if let Err(e) = self.bus.register(&reg) {
                    eprintln!("orchd: wake: register {n}: {e:#}");
                }
                let _ = self.bus.set_status(n, "unknown", None);
            }
            let unread = name
                .as_deref()
                .and_then(|n| self.bus.inbox(n).ok())
                .map_or(0, |m| m.len());
            jobs.push((
                Arc::clone(p),
                name.unwrap_or_else(|| format!("{kind} in {}", p.id)),
                slept.clone(),
                unread,
            ));
        }
        let failed = spawned.failed.clone();
        let text = format!(
            "{} is awake: {} pane(s), resumed {} agent(s){}",
            w.label,
            spawned.ids.len(),
            spawned.resumed,
            if failed.is_empty() {
                String::new()
            } else {
                format!(", {} could not be started", failed.len())
            }
        );
        if let Err(e) = self
            .bus
            .send("orchd", HUMAN, "msg", &text, Some("sleep"), None)
        {
            eprintln!("orchd: wake summary: {e}");
        }
        for (pane, name, slept, unread) in jobs {
            let d = Arc::clone(self);
            tokio::spawn(async move { after_wake(d, pane, name, slept, unread).await });
        }
        Ok(json!({
            "session": self.session(),
            "workspace": w.id,
            "panes": spawned.ids.values().map(|p| p.id.clone()).collect::<Vec<_>>(),
            "failed": failed,
        }))
    }

    /// `orch ws close` on a sleeping workspace: it is forgotten (nothing runs to kill). Its
    /// running tasks fail; its queued ones keep waiting for the label, as for a closed one.
    pub fn forget_sleeping(&self, key: &str) -> Result<bool, ApiError> {
        let Some(sl) = self.session().sleeping(key).cloned() else {
            return Ok(false);
        };
        self.edit(|s| {
            s.sleeping.retain(|x| x.workspace.id != sl.workspace.id);
            Ok(())
        })?;
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

/// What an agent gets once it is back at its prompt after a wake: the interrupted-turn note,
/// a reminder of unread mail, or (paused on its limit) "continue" after the reset.
async fn after_wake(
    d: Arc<Daemon>,
    pane: Arc<crate::pane::Pane>,
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
        let Some(reset) = slept.limit_reset else {
            let _ = d.bus.send(
                "orchd",
                HUMAN,
                "msg",
                &format!("{name} was on its usage limit with no reset time when it was put to sleep; it is awake now: type continue once the limit has reset"),
                Some("limits"),
                None,
            );
            return;
        };
        let due = reset + LIMIT_GRACE_SECS - now_ms() / 1000;
        if due > 0 {
            tokio::time::sleep(Duration::from_secs(due as u64)).await;
        }
        let text = format!("{}{mail}", crate::limits::CONTINUE_TEXT);
        if type_when_idle(&d, &pane, &text).await {
            let _ = d.bus.send(
                "orchd",
                HUMAN,
                "msg",
                &format!("{name}: continued after its usage limit reset (woken from sleep)"),
                Some("limits"),
                None,
            );
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
async fn type_when_idle(d: &Daemon, pane: &crate::pane::Pane, text: &str) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        match pane.detect() {
            Detected::Idle => {
                let _gate = d.task_gate.lock().await;
                if pane.detect() != Detected::Idle || !pane.write(text.as_bytes().to_vec()) {
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
