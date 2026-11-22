//! `orch ws …` / `orch tab …`: workspace and tab control from a script or the phone (Kabuk's
//! native workspace bar), over the same layout operations the TUI uses. A workspace is named by
//! its id or label, a tab by its id (`orch watch` lists both), so a tab closed meanwhile can't
//! shift a tap onto another. A workspace or tab that is gone exits 14.

use crate::exit::CliError;
use anyhow::Result;
use clap::Subcommand;
use orch_proto::api::{Request, Spawn, UiOp};
use orch_proto::client::Conn;
use orch_proto::layout::{Session, Tab, Workspace};

#[derive(Subcommand, Debug)]
pub enum WsCmd {
    /// Show a workspace in the TUI (its focused pane)
    Focus { workspace: String },
    /// New workspace with a shell (or ARGV) in DIR; prints the new pane id
    New {
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    /// Rename a workspace
    Rename {
        workspace: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        label: Vec<String>,
    },
    /// Close a workspace: kills every pane in it (a sleeping one is forgotten)
    Close { workspace: String },
    /// Put a workspace to sleep: its layout and how to resume each agent are kept, then its
    /// programs end (memory freed). Refused while an agent works, waits on a dialog or a
    /// question, runs a task, or a program runs that can't be resumed (exit 15).
    /// WORKSPACE "." = the one this command runs in
    Sleep {
        workspace: String,
        /// Sleep anyway: busy agents are interrupted (they get a note on wake), running
        /// programs end and come back as fresh shells
        #[arg(long)]
        force: bool,
        /// Only show what would happen
        #[arg(long)]
        dry_run: bool,
    },
    /// Wake a sleeping workspace: the layout comes back, agents resume their conversation
    Wake { workspace: String },
    /// Exempt a workspace from auto-sleep (`orch auto-sleep`): on | off (without: show)
    KeepAwake {
        workspace: String,
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum TabCmd {
    /// New tab with a shell (or ARGV) in a workspace; prints the new pane id.
    /// WORKSPACE "." = the workspace this command runs in ($ORCH_PANE): how an agent starts a
    /// helper agent next to itself instead of in a workspace of its own
    New {
        /// Name the tab (e.g. the agent in it)
        #[arg(long)]
        label: Option<String>,
        /// Start in DIR instead of the workspace's folder
        #[arg(long)]
        cwd: Option<String>,
        workspace: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    /// Show a tab (and its workspace) in the TUI
    Focus { workspace: String, tab: String },
    /// Rename a tab
    Rename {
        workspace: String,
        tab: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        label: Vec<String>,
    },
    /// Close a tab: kills its panes
    Close { workspace: String, tab: String },
}

async fn session(c: &mut Conn) -> Result<Session> {
    Ok(serde_json::from_value(
        c.request(&Request::UiGet {}).await?,
    )?)
}

fn find<'a>(s: &'a Session, key: &str) -> Result<&'a Workspace> {
    s.workspace(key)
        .ok_or_else(|| CliError::NotFound(format!("no workspace {key:?}")).into())
}

/// A pane id as the CLI prints it ("p7") or as the layout may hold it ("orch:p7").
fn holds(t: &Tab, pane: &str) -> bool {
    let bare = pane.strip_prefix("orch:").unwrap_or(pane);
    t.root.contains(bare) || t.root.contains(&format!("orch:{bare}"))
}

/// The workspace the caller's own pane ($ORCH_PANE) is in.
fn own(s: &Session) -> Result<&Workspace> {
    let pane = std::env::var("ORCH_PANE").map_err(|_| {
        CliError::NotFound("\".\" needs $ORCH_PANE: run it inside an orch pane".into())
    })?;
    s.workspaces
        .iter()
        .find(|w| w.tabs.iter().any(|t| holds(t, &pane)))
        .ok_or_else(|| CliError::NotFound(format!("pane {pane} is not in a workspace")).into())
}

/// (index, focused pane) of a tab given by id.
fn tab(w: &Workspace, id: &str) -> Result<(usize, String)> {
    w.tabs
        .iter()
        .enumerate()
        .find(|(_, t)| t.id == id)
        .map(|(i, t)| (i, t.focus.clone()))
        .ok_or_else(|| CliError::NotFound(format!("no tab {id:?} in {}", w.label)).into())
}

fn spawn(argv: Vec<String>, cwd: Option<String>) -> Result<Spawn> {
    Ok(Spawn {
        argv,
        cwd,
        env: Some(crate::client_env(&[])?),
        ..Default::default()
    })
}

fn print_pane(v: &serde_json::Value) {
    if let Some(id) = v["pane"]["id"].as_str() {
        println!("{id}");
    }
}

/// The sleep plan: what blocks it (without --force) and how each pane comes back.
fn print_plan(plan: &serde_json::Value, dry_run: bool) {
    let list = |k: &str| -> Vec<String> {
        plan[k]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let blockers = list("blockers");
    if dry_run {
        let label = plan["label"].as_str().unwrap_or_default();
        if blockers.is_empty() {
            println!("{label} can sleep now:");
        } else {
            println!("{label} is busy: sleep needs --force, which interrupts this:");
            for b in &blockers {
                println!("  ! {b}");
            }
        }
    } else {
        for b in &blockers {
            println!("  ! {b} (forced)");
        }
    }
    for n in list("notes") {
        println!("  - {n}");
    }
}

pub async fn ws(c: &mut Conn, cmd: WsCmd) -> Result<()> {
    match cmd {
        WsCmd::Focus { workspace } => {
            let s = session(c).await?;
            let w = find(&s, &workspace)?;
            let pane = w
                .tabs
                .get(w.active_tab.min(w.tabs.len().saturating_sub(1)))
                .map(|t| t.focus.clone())
                .unwrap_or_default();
            c.request(&Request::Ui(UiOp::Raise { pane })).await?;
        }
        WsCmd::New { label, cwd, argv } => {
            let cwd = cwd.or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            });
            let v = c
                .request(&Request::Ui(UiOp::NewWorkspace {
                    label,
                    spawn: spawn(argv, cwd)?,
                }))
                .await?;
            print_pane(&v);
        }
        WsCmd::Rename { workspace, label } => {
            let s = session(c).await?;
            let id = find(&s, &workspace)?.id.clone();
            c.request(&Request::Ui(UiOp::RenameWorkspace {
                workspace: id,
                label: label.join(" "),
            }))
            .await?;
        }
        WsCmd::Sleep {
            workspace,
            force,
            dry_run,
        } => {
            let s = session(c).await?;
            let w = if workspace == "." {
                own(&s)?
            } else if let Some(sl) = s.sleeping(&workspace) {
                println!("already asleep: {}", sl.workspace.label);
                return Ok(());
            } else {
                find(&s, &workspace)?
            };
            let (id, label) = (w.id.clone(), w.label.clone());
            let mine = std::env::var("ORCH_PANE")
                .ok()
                .is_some_and(|p| w.tabs.iter().any(|t| holds(t, &p)));
            if mine && !dry_run {
                eprintln!("orch: this pane is in {label}: it ends with the workspace");
            }
            let v = c
                .request(&Request::Ui(UiOp::SleepWorkspace {
                    workspace: id,
                    force,
                    dry_run,
                }))
                .await?;
            print_plan(&v["plan"], dry_run);
            if v["slept"] == true {
                println!("asleep: {label} (wake: orch ws wake {label})");
            }
        }
        WsCmd::Wake { workspace } => {
            let v = c
                .request(&Request::Ui(UiOp::WakeWorkspace {
                    workspace: workspace.clone(),
                }))
                .await?;
            let s: Session = serde_json::from_value(v["session"].clone())?;
            let id = v["workspace"].as_str().unwrap_or_default();
            let label = s
                .workspace(id)
                .map_or(workspace.as_str(), |w| w.label.as_str());
            if v["already"] == true {
                println!("already awake: {label}");
                return Ok(());
            }
            let panes: Vec<&str> = v["panes"]
                .as_array()
                .map(|a| a.iter().filter_map(|p| p.as_str()).collect())
                .unwrap_or_default();
            println!("awake: {label} ({})", panes.join(" "));
            if let Some(failed) = v["failed"].as_array().filter(|f| !f.is_empty()) {
                eprintln!("orch: {} pane(s) could not be started", failed.len());
            }
        }
        WsCmd::KeepAwake { workspace, state } => {
            let s = session(c).await?;
            let w = if workspace == "." {
                own(&s)?
            } else {
                s.workspace(&workspace)
                    .or_else(|| s.sleeping(&workspace).map(|x| &x.workspace))
                    .ok_or_else(|| CliError::NotFound(format!("no workspace {workspace:?}")))?
            };
            let label = w.label.clone();
            let v = c
                .request(&Request::KeepAwake {
                    workspace: w.id.clone(),
                    on: state.map(|s| s == "on"),
                })
                .await?;
            println!(
                "{label}: keep-awake {}",
                if v["keep_awake"] == true { "on" } else { "off" }
            );
        }
        WsCmd::Close { workspace } => {
            let s = session(c).await?;
            if let Some(sl) = s.sleeping(&workspace) {
                let id = sl.workspace.id.clone();
                c.request(&Request::Ui(UiOp::CloseWorkspace { workspace: id }))
                    .await?;
                return Ok(());
            }
            let id = find(&s, &workspace)?.id.clone();
            c.request(&Request::Ui(UiOp::CloseWorkspace { workspace: id }))
                .await?;
        }
    }
    Ok(())
}

pub async fn tab_cmd(c: &mut Conn, cmd: TabCmd) -> Result<()> {
    match cmd {
        TabCmd::New {
            label,
            cwd,
            workspace,
            argv,
        } => {
            let s = session(c).await?;
            let w = if workspace == "." {
                own(&s)?
            } else {
                find(&s, &workspace)?
            };
            let ws_id = w.id.clone();
            let v = c
                .request(&Request::Ui(UiOp::NewTab {
                    workspace: ws_id.clone(),
                    spawn: spawn(argv, cwd.or_else(|| Some(w.cwd.clone())))?,
                }))
                .await?;
            if let (Some(label), Some(pane)) = (label, v["pane"]["id"].as_str()) {
                let s = session(c).await?;
                if let Some((index, t)) = find(&s, &ws_id)?
                    .tabs
                    .iter()
                    .enumerate()
                    .find(|(_, t)| holds(t, pane))
                {
                    c.request(&Request::Ui(UiOp::RenameTab {
                        workspace: ws_id,
                        index,
                        tab: Some(t.id.clone()),
                        label,
                    }))
                    .await?;
                }
            }
            print_pane(&v);
        }
        TabCmd::Focus { workspace, tab: t } => {
            let s = session(c).await?;
            let (_, pane) = tab(find(&s, &workspace)?, &t)?;
            c.request(&Request::Ui(UiOp::Raise { pane })).await?;
        }
        TabCmd::Rename {
            workspace,
            tab: t,
            label,
        } => {
            let s = session(c).await?;
            let w = find(&s, &workspace)?;
            let (index, _) = tab(w, &t)?;
            c.request(&Request::Ui(UiOp::RenameTab {
                workspace: w.id.clone(),
                index,
                tab: Some(t),
                label: label.join(" "),
            }))
            .await?;
        }
        TabCmd::Close { workspace, tab: t } => {
            let s = session(c).await?;
            let w = find(&s, &workspace)?;
            let (index, _) = tab(w, &t)?;
            c.request(&Request::Ui(UiOp::CloseTab {
                workspace: w.id.clone(),
                index,
                tab: Some(t),
            }))
            .await?;
        }
    }
    Ok(())
}
