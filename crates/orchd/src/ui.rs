//! Workspaces / tabs / splits (the layout), owned by orchd so every client sees the same
//! thing and a detach never loses it; plus the event stream that keeps TUIs current.

use crate::bus::BusEvent;
use crate::server::{err, ApiError, Daemon};
use orch_proto::api::{Event, NewPane, PaneInfo, Snapshot, Spawn, Stream, UiOp};
use orch_proto::client::{FrameReader, FrameWriter};
use orch_proto::layout::{Node, Session, Tab, Workspace};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// Hints for subscribers: re-read panes / session.
#[derive(Debug, Clone, Copy)]
pub enum UiEvent {
    Panes,
    Session,
}

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// The shell's current directory (so a split opens where you are), else its start dir.
fn pane_cwd(info: &PaneInfo) -> String {
    info.pid
        .and_then(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| info.cwd.clone())
}

impl Daemon {
    pub fn session(&self) -> Session {
        self.session.lock().unwrap().clone()
    }

    /// Apply a change to the session and announce it.
    fn edit<R>(&self, f: impl FnOnce(&mut Session) -> Result<R, ApiError>) -> Result<R, ApiError> {
        let r = {
            let mut s = self.session.lock().unwrap();
            let r = f(&mut s)?;
            s.version += 1;
            r
        };
        let _ = self.ui_events.send(UiEvent::Session);
        Ok(r)
    }

    fn spawn(&self, spawn: Spawn, cwd_default: Option<String>) -> Result<PaneInfo, ApiError> {
        let pane = self.new_pane(NewPane {
            name: spawn.name,
            argv: spawn.argv,
            cwd: spawn.cwd.or(cwd_default),
            env: spawn.env,
            rows: spawn.rows,
            cols: spawn.cols,
        })?;
        let _ = self.ui_events.send(UiEvent::Panes);
        Ok(pane.info())
    }

    fn unique_label(s: &Session, base: &str) -> String {
        let mut label = base.to_string();
        let mut n = 1;
        while s.workspaces.iter().any(|w| w.label == label) {
            n += 1;
            label = format!("{base}-{n}");
        }
        label
    }

    fn add_workspace(&self, label: Option<String>, info: &PaneInfo) -> Result<String, ApiError> {
        self.edit(|s| {
            let label = Self::unique_label(s, &label.unwrap_or_else(|| basename(&info.cwd)));
            let id = s.new_id("w");
            let tab = s.new_id("t");
            s.workspaces.push(Workspace {
                id: id.clone(),
                label,
                cwd: info.cwd.clone(),
                tabs: vec![Tab {
                    id: tab,
                    label: "1".into(),
                    root: Node::leaf(&info.id),
                    focus: info.id.clone(),
                    zoomed: false,
                }],
                active_tab: 0,
            });
            Ok(id)
        })
    }

    fn add_tab(&self, ws: &str, info: &PaneInfo) -> Result<(), ApiError> {
        self.edit(|s| {
            let tab = s.new_id("t");
            let w = s
                .workspace_mut(ws)
                .ok_or_else(|| err("no_workspace", format!("no workspace {ws:?}")))?;
            w.tabs.push(Tab {
                id: tab,
                label: (w.tabs.len() + 1).to_string(),
                root: Node::leaf(&info.id),
                focus: info.id.clone(),
                zoomed: false,
            });
            w.active_tab = w.tabs.len() - 1;
            Ok(())
        })
    }

    /// A pane started outside the TUI (`orch new`): a new tab in the workspace for its cwd,
    /// or a new workspace.
    pub fn place_new_pane(&self, info: &PaneInfo) -> Result<(), ApiError> {
        let ws = self
            .session()
            .workspaces
            .iter()
            .find(|w| w.cwd == info.cwd)
            .map(|w| w.id.clone());
        match ws {
            Some(ws) => self.add_tab(&ws, info),
            None => self.add_workspace(None, info).map(|_| ()),
        }
    }

    /// Forget a pane in the layout (after it was killed).
    pub fn unplace(&self, pane: &str) {
        let _ = self.edit(|s| {
            s.remove_pane(pane);
            Ok(())
        });
        let _ = self.ui_events.send(UiEvent::Panes);
    }

    /// Kills panes (already gone is fine) and takes them out of the layout; empty tabs and
    /// workspaces collapse with them.
    async fn close_panes(&self, panes: Vec<String>) -> Result<(), ApiError> {
        for id in panes {
            match self.kill(&id).await {
                Ok(_) => {}
                Err(e) if e.code == "no_pane" => {}
                Err(e) => return Err(e),
            }
            self.unplace(&id);
        }
        Ok(())
    }

    pub async fn ui_op(&self, op: UiOp) -> Result<Value, ApiError> {
        let mut created: Option<PaneInfo> = None;
        match op {
            UiOp::NewWorkspace { label, spawn } => {
                let info = self.spawn(spawn, None)?;
                if let Err(e) = self.add_workspace(label, &info) {
                    let _ = self.kill(&info.id).await;
                    return Err(e);
                }
                created = Some(info);
            }
            UiOp::NewTab { workspace, spawn } => {
                let cwd = self.session().workspace(&workspace).map(|w| w.cwd.clone());
                if cwd.is_none() {
                    return Err(err("no_workspace", format!("no workspace {workspace:?}")));
                }
                let info = self.spawn(spawn, cwd)?;
                if let Err(e) = self.add_tab(&workspace, &info) {
                    // The workspace vanished meanwhile: don't leave an unplaced pane behind.
                    let _ = self.kill(&info.id).await;
                    return Err(e);
                }
                created = Some(info);
            }
            UiOp::Split { pane, dir, spawn } => {
                let at = self.find(&pane)?;
                let id = at.id.clone();
                if self.session().locate(&id).is_none() {
                    return Err(err("not_placed", format!("{id} is not in a workspace")));
                }
                let info = self.spawn(spawn, Some(pane_cwd(&at.info())))?;
                let new_id = info.id.clone();
                let placed = self.edit(|s| {
                    let (wi, ti) = s
                        .locate(&id)
                        .ok_or_else(|| err("not_placed", "pane vanished"))?;
                    let tab = &mut s.workspaces[wi].tabs[ti];
                    tab.root.split(&id, &new_id, dir);
                    tab.focus = new_id.clone();
                    tab.zoomed = false;
                    Ok(())
                });
                if let Err(e) = placed {
                    let _ = self.kill(&new_id).await;
                    return Err(e);
                }
                created = Some(info);
            }
            UiOp::ClosePane { pane } => {
                let id = self.find(&pane).map(|p| p.id.clone()).unwrap_or(pane);
                self.kill(&id).await.or_else(|e| {
                    // Already gone from orchd: still remove it from the layout.
                    if e.code == "no_pane" {
                        Ok(())
                    } else {
                        Err(e)
                    }
                })?;
                self.unplace(&id);
            }
            UiOp::CloseTab {
                workspace,
                index,
                tab,
            } => {
                let panes = {
                    let s = self.session.lock().unwrap();
                    let w = s
                        .workspace(&workspace)
                        .ok_or_else(|| err("no_workspace", format!("no workspace {workspace}")))?;
                    let t = match &tab {
                        Some(id) => w.tabs.iter().find(|t| &t.id == id),
                        None => w.tabs.get(index),
                    };
                    t.ok_or_else(|| err("no_tab", format!("that tab is gone from {}", w.label)))?
                        .root
                        .panes()
                };
                self.close_panes(panes).await?;
            }
            UiOp::CloseWorkspace { workspace } => {
                let panes: Vec<String> = {
                    let s = self.session.lock().unwrap();
                    let w = s
                        .workspace(&workspace)
                        .ok_or_else(|| err("no_workspace", format!("no workspace {workspace}")))?;
                    w.tabs.iter().flat_map(|t| t.root.panes()).collect()
                };
                self.close_panes(panes).await?;
            }
            UiOp::Focus { pane } => self.edit(|s| {
                let (wi, ti) = s
                    .locate(&pane)
                    .ok_or_else(|| err("not_placed", format!("{pane} is not in a workspace")))?;
                let w = &mut s.workspaces[wi];
                w.active_tab = ti;
                w.tabs[ti].focus = pane.clone();
                Ok(())
            })?,
            UiOp::Raise { pane } => self.edit(|s| {
                let (wi, ti) = s
                    .locate(&pane)
                    .ok_or_else(|| err("not_placed", format!("{pane} is not in a workspace")))?;
                let w = &mut s.workspaces[wi];
                w.active_tab = ti;
                w.tabs[ti].focus = pane.clone();
                let workspace = w.id.clone();
                let seq = s.raised.as_ref().map_or(0, |r| r.seq) + 1;
                s.raised = Some(orch_proto::layout::Raised { seq, workspace });
                Ok(())
            })?,
            UiOp::SelectTab { workspace, index } => self.edit(|s| {
                let w = s
                    .workspace_mut(&workspace)
                    .ok_or_else(|| err("no_workspace", workspace.clone()))?;
                if index >= w.tabs.len() {
                    return Err(err("no_tab", format!("no tab {}", index + 1)));
                }
                w.active_tab = index;
                Ok(())
            })?,
            UiOp::RenameWorkspace { workspace, label } => self.edit(|s| {
                if s.workspaces
                    .iter()
                    .any(|w| w.label == label && w.id != workspace)
                {
                    return Err(err("label_taken", label.clone()));
                }
                s.workspace_mut(&workspace)
                    .ok_or_else(|| err("no_workspace", workspace.clone()))?
                    .label = label.clone();
                Ok(())
            })?,
            UiOp::RenameTab {
                workspace,
                index,
                tab,
                label,
            } => self.edit(|s| {
                let w = s
                    .workspace_mut(&workspace)
                    .ok_or_else(|| err("no_workspace", workspace.clone()))?;
                match &tab {
                    Some(id) => w.tabs.iter_mut().find(|t| &t.id == id),
                    None => w.tabs.get_mut(index),
                }
                .ok_or_else(|| err("no_tab", format!("no tab {}", index + 1)))?
                .label = label.clone();
                Ok(())
            })?,
            UiOp::Resize { pane, dir, delta } => self.edit(|s| {
                let (wi, ti) = s
                    .locate(&pane)
                    .ok_or_else(|| err("not_placed", pane.clone()))?;
                s.workspaces[wi].tabs[ti]
                    .root
                    .resize(&pane, dir, delta.clamp(-0.5, 0.5));
                Ok(())
            })?,
            UiOp::ToggleZoom { pane } => self.edit(|s| {
                let (wi, ti) = s
                    .locate(&pane)
                    .ok_or_else(|| err("not_placed", pane.clone()))?;
                let tab = &mut s.workspaces[wi].tabs[ti];
                tab.focus = pane.clone();
                tab.zoomed = !tab.zoomed && tab.root.panes().len() > 1;
                Ok(())
            })?,
        }
        Ok(json!({ "session": self.session(), "pane": created }))
    }

    pub fn pane_infos(&self) -> Vec<PaneInfo> {
        self.all_panes().iter().map(|p| p.info()).collect()
    }
}

/// Watch panes for status / title / exit changes and announce them (≤ 2×/s).
pub fn start_detector(daemon: Arc<Daemon>) {
    tokio::spawn(async move {
        let mut last: Vec<(String, Option<String>, String, bool)> = Vec::new();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            tick.tick().await;
            let d = Arc::clone(&daemon);
            let now = tokio::task::spawn_blocking(move || {
                d.pane_infos()
                    .into_iter()
                    .map(|p| (p.id, p.agent_status, p.title, p.exit_code.is_some()))
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            if now != last {
                last = now;
                let _ = daemon.ui_events.send(UiEvent::Panes);
            }
        }
    });
}

fn stream_body(s: &Stream) -> Value {
    serde_json::to_value(s).unwrap()
}

/// `events.subscribe`: snapshot, then changes until the client hangs up. Events from the
/// store are hints; rows are always re-read, so nothing is skipped or reordered.
pub async fn subscribe(
    daemon: &Daemon,
    id: String,
    mut reader: FrameReader,
    mut writer: FrameWriter,
) -> std::io::Result<()> {
    let mut ui_rx = daemon.ui_events.subscribe();
    let mut bus_rx = daemon.bus.subscribe();
    let messages = daemon.bus.log(None, 200).unwrap_or_default();
    let mut last_msg = messages.last().map(|m| m.id).unwrap_or(0);
    let snapshot = Snapshot {
        session: daemon.session(),
        panes: daemon.pane_infos(),
        agents: daemon.bus.agents().unwrap_or_default(),
        questions: daemon.bus.questions(false).unwrap_or_default(),
        messages,
    };
    writer
        .send_body(&id, json!({ "result": { "subscribed": true } }))
        .await?;
    writer
        .send_body(
            &id,
            stream_body(&Stream::Snapshot {
                snapshot: Box::new(snapshot),
            }),
        )
        .await?;
    let send = |e: Event| stream_body(&Stream::Event { event: Box::new(e) });
    loop {
        tokio::select! {
            f = reader.next() => if f?.is_none() { return Ok(()) },
            ev = ui_rx.recv() => match ev {
                Ok(UiEvent::Panes) => writer.send_body(&id, send(Event::Panes { panes: daemon.pane_infos() })).await?,
                Ok(UiEvent::Session) => writer.send_body(&id, send(Event::Session { session: daemon.session() })).await?,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    writer.send_body(&id, send(Event::Panes { panes: daemon.pane_infos() })).await?;
                    writer.send_body(&id, send(Event::Session { session: daemon.session() })).await?;
                }
                Err(_) => return Ok(()),
            },
            ev = bus_rx.recv() => {
                match ev {
                    // Events are hints: always send the current lists / ordered rows read from
                    // the store, so a late or dropped event can never leave stale state.
                    Ok(BusEvent::Agent(_)) => {
                        let agents = daemon.bus.agents().unwrap_or_default();
                        writer.send_body(&id, send(Event::Agents { agents })).await?
                    }
                    Ok(BusEvent::Question(_)) => {
                        let questions = daemon.bus.questions(false).unwrap_or_default();
                        writer.send_body(&id, send(Event::Questions { questions })).await?
                    }
                    Ok(BusEvent::Message(_)) => {
                        for message in daemon.bus.log(Some(last_msg), 500).unwrap_or_default() {
                            last_msg = last_msg.max(message.id);
                            writer.send_body(&id, send(Event::Message { message })).await?;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let agents = daemon.bus.agents().unwrap_or_default();
                        writer.send_body(&id, send(Event::Agents { agents })).await?;
                        let questions = daemon.bus.questions(false).unwrap_or_default();
                        writer.send_body(&id, send(Event::Questions { questions })).await?;
                        for message in daemon.bus.log(Some(last_msg), 500).unwrap_or_default() {
                            last_msg = last_msg.max(message.id);
                            writer.send_body(&id, send(Event::Message { message })).await?;
                        }
                    }
                    Err(_) => return Ok(()),
                }
            }
        }
    }
}
