//! The layout survives an unplanned stop (kill -9, SIGTERM) through `autosave.json`.

use orch_proto::api::{NewPane, RegisterAgent, Request, Spawn, UiOp};
use orch_proto::client::Conn;
use orch_proto::layout::{Dir, Node, Session};
use serde_json::Value;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

struct D {
    child: Child,
    socket: PathBuf,
    dir: PathBuf,
    env: Vec<(&'static str, String)>,
}

impl Drop for D {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn spawn_orchd(dir: &Path, socket: &Path, env: &[(&'static str, String)]) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_orchd"));
    cmd.arg("--socket")
        .arg(socket)
        .arg("--db")
        .arg(dir.join("db"))
        .env("HOME", dir.join("home"))
        .env_remove("ORCH_HOST")
        .env_remove("ORCHD_AUTOSAVE_DEBOUNCE_MS")
        .env_remove("ORCHD_AUTOSAVE_INTERVAL_MS");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.spawn().unwrap()
}

/// `debounce` / `interval` in ms for the autosave timers.
async fn daemon(tag: &str, debounce: u64, interval: u64) -> D {
    let dir = std::env::temp_dir().join(format!(
        "orch-autosave-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let socket = dir.join("s.sock");
    let env = vec![
        ("ORCHD_AUTOSAVE_DEBOUNCE_MS", debounce.to_string()),
        ("ORCHD_AUTOSAVE_INTERVAL_MS", interval.to_string()),
    ];
    let child = spawn_orchd(&dir, &socket, &env);
    let d = D {
        child,
        socket,
        dir,
        env,
    };
    conn(&d).await;
    d
}

async fn conn(d: &D) -> Conn {
    for _ in 0..200 {
        if let Ok(mut c) = Conn::unix(&d.socket).await {
            if c.request(&Request::Ping {}).await.is_ok() {
                return c;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("private daemon did not start");
}

/// The next orchd on the same folder (the old one must be gone).
async fn restart(d: &mut D) -> Conn {
    d.child = spawn_orchd(&d.dir, &d.socket, &d.env);
    conn(d).await
}

fn kill(d: &mut D, signal: &str) {
    let ok = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(d.child.id().to_string())
        .status()
        .unwrap()
        .success();
    assert!(ok);
    d.child.wait().unwrap();
}

async fn op(d: &D, op: UiOp) -> Value {
    conn(d).await.request(&Request::Ui(op)).await.unwrap()
}

async fn session(d: &D) -> Session {
    serde_json::from_value(conn(d).await.request(&Request::UiGet {}).await.unwrap()).unwrap()
}

fn spawn(argv: &[&str]) -> Spawn {
    Spawn {
        argv: argv.iter().map(|s| s.to_string()).collect(),
        cwd: Some("/tmp".into()),
        rows: Some(20),
        cols: Some(80),
        ..Default::default()
    }
}

fn autosave(d: &D) -> PathBuf {
    d.dir.join("autosave.json")
}

/// Waits until the autosave on disk satisfies `ok`.
async fn wait_saved(d: &D, what: &str, ok: impl Fn(&orchd::restore::Saved) -> bool) {
    for _ in 0..400 {
        if let Ok(data) = std::fs::read(autosave(d)) {
            if let Ok(saved) = serde_json::from_slice(&data) {
                if ok(&saved) {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("autosave never showed: {what}");
}

fn labels(s: &Session) -> Vec<String> {
    s.workspaces.iter().map(|w| w.label.clone()).collect()
}

async fn pane_infos(c: &mut Conn) -> Vec<orch_proto::api::PaneInfo> {
    let v = c.request(&Request::ListPanes {}).await.unwrap();
    serde_json::from_value(v["panes"].clone()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_9_brings_the_layout_back_and_a_closed_workspace_stays_gone() {
    let mut d = daemon("kill9", 100, 200).await;
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("one".into()),
            spawn: spawn(&["bash", "--norc"]),
        },
    )
    .await;
    let one = v["session"]["workspaces"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let shell = v["pane"]["id"].as_str().unwrap().to_string();
    op(
        &d,
        UiOp::Split {
            pane: shell.clone(),
            dir: Dir::Down,
            spawn: spawn(&["cat"]),
        },
    )
    .await;
    op(
        &d,
        UiOp::NewTab {
            workspace: one.clone(),
            spawn: spawn(&["sleep", "1000"]),
        },
    )
    .await;
    op(
        &d,
        UiOp::NewWorkspace {
            label: Some("two".into()),
            spawn: spawn(&["sleep", "1001"]),
        },
    )
    .await;
    // A workspace whose last pane is closed: it must not come back.
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("gone".into()),
            spawn: spawn(&["sleep", "1002"]),
        },
    )
    .await;
    let gone = v["pane"]["id"].as_str().unwrap().to_string();
    wait_saved(&d, "workspace gone", |s| s.panes.contains_key(&gone)).await;
    op(&d, UiOp::ClosePane { pane: gone.clone() }).await;
    // The shell moves to another folder (no layout op: the timer picks it up).
    let folder = d.dir.join("home");
    let mut c = conn(&d).await;
    c.request(&Request::Send(orch_proto::api::SendPane {
        pane: shell.clone(),
        text: format!("cd {}", folder.display()),
        enter: true,
        ..Default::default()
    }))
    .await
    .unwrap();
    let want = folder.display().to_string();
    wait_saved(&d, "the shell's folder and no 'gone'", |s| {
        s.panes.get(&shell).is_some_and(|p| p.cwd == want)
            && !s.session.workspaces.iter().any(|w| w.label == "gone")
            && !s.panes.contains_key(&gone)
    })
    .await;
    let before = session(&d).await;

    kill(&mut d, "KILL");
    let mut c = restart(&mut d).await;
    let after = session(&d).await;
    assert_eq!(labels(&after), vec!["one", "two"]);
    let (b, a) = (&before.workspaces[0], &after.workspaces[0]);
    assert_eq!(a.id, b.id);
    assert_eq!(a.tabs.len(), 2);
    assert!(matches!(a.tabs[0].root, Node::Split { dir: Dir::Down, .. }));
    let panes = pane_infos(&mut c).await;
    let info = |id: &str| panes.iter().find(|p| p.id == id).unwrap().clone();
    let new = a.tabs[0].root.panes();
    assert!(!new.contains(&shell));
    assert_eq!(info(&new[0]).argv, vec!["bash", "--norc"]);
    assert_eq!(info(&new[0]).cwd, want, "the shell's folder");
    assert_eq!(info(&new[1]).argv, vec!["cat"]);
    assert_eq!(info(&a.tabs[1].root.panes()[0]).argv, vec!["sleep", "1000"]);
    assert_eq!(
        info(&after.workspaces[1].tabs[0].root.panes()[0]).argv,
        vec!["sleep", "1001"]
    );
    assert_eq!(panes.len(), 4, "nothing else came back");
    // Restored once, marker cleared, and autosaving runs again for the new layout.
    assert!(!d.dir.join("autosave.json.restoring").exists());
    assert!(!d.dir.join("autosave.json.failed").exists());
    let new_shell = new[0].clone();
    wait_saved(&d, "the new panes", |s| s.panes.contains_key(&new_shell)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_saves_a_change_made_just_before_it() {
    // A long debounce: only the final save on SIGTERM can have the late workspace.
    let mut d = daemon("sigterm", 30_000, 60_000).await;
    op(
        &d,
        UiOp::NewWorkspace {
            label: Some("early".into()),
            spawn: spawn(&["sleep", "1000"]),
        },
    )
    .await;
    op(
        &d,
        UiOp::NewWorkspace {
            label: Some("late".into()),
            spawn: spawn(&["sleep", "1001"]),
        },
    )
    .await;
    let saved = std::fs::read_to_string(autosave(&d)).unwrap_or_default();
    assert!(!saved.contains("\"late\""), "not saved before the signal");
    kill(&mut d, "TERM");
    let saved: orchd::restore::Saved =
        serde_json::from_slice(&std::fs::read(autosave(&d)).unwrap()).unwrap();
    assert_eq!(saved.panes.len(), 2, "saved while the panes still ran");
    let mode = std::fs::metadata(autosave(&d)).unwrap().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let mut c = restart(&mut d).await;
    assert_eq!(labels(&session(&d).await), vec!["early", "late"]);
    assert_eq!(pane_infos(&mut c).await.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_stop_daemon_forgets_the_layout() {
    let mut d = daemon("stop", 100, 200).await;
    op(
        &d,
        UiOp::NewWorkspace {
            label: Some("w".into()),
            spawn: spawn(&["sleep", "1000"]),
        },
    )
    .await;
    wait_saved(&d, "the workspace", |s| s.panes.len() == 1).await;
    conn(&d)
        .await
        .request(&Request::StopDaemon { restart: false })
        .await
        .unwrap();
    d.child.wait().unwrap();
    assert!(!autosave(&d).exists(), "no autosave left");
    let mut c = restart(&mut d).await;
    assert!(session(&d).await.workspaces.is_empty());
    assert!(pane_infos(&mut c).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_planned_restart_wins_over_an_older_autosave() {
    // Autosave written once (at start-up and after the first workspace), then held back.
    let mut d = daemon("planned", 100, 60_000).await;
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("old".into()),
            spawn: spawn(&["sleep", "1000"]),
        },
    )
    .await;
    let ws = v["session"]["workspaces"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_saved(&d, "label old", |s| {
        s.session.workspaces.iter().any(|w| w.label == "old")
    })
    .await;
    // Make the autosave stale: the next debounced write can't happen before the restart.
    let stale = std::fs::read(autosave(&d)).unwrap();
    op(
        &d,
        UiOp::RenameWorkspace {
            workspace: ws,
            label: "new".into(),
        },
    )
    .await;
    conn(&d)
        .await
        .request(&Request::StopDaemon { restart: true })
        .await
        .unwrap();
    d.child.wait().unwrap();
    // Whatever the autosaver managed meanwhile, put the old layout back.
    std::fs::write(autosave(&d), &stale).unwrap();
    assert!(d.dir.join("restore.json").exists());
    let mut c = restart(&mut d).await;
    assert_eq!(labels(&session(&d).await), vec!["new"]);
    assert_eq!(pane_infos(&mut c).await.len(), 1);
    assert!(!d.dir.join("restore.json").exists(), "read once");
    wait_saved(&d, "the restored layout", |s| {
        s.session.workspaces.iter().any(|w| w.label == "new")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restore_that_never_finished_is_not_tried_again() {
    let mut d = daemon("loop", 100, 200).await;
    op(
        &d,
        UiOp::NewWorkspace {
            label: Some("crashy".into()),
            spawn: spawn(&["sleep", "1000"]),
        },
    )
    .await;
    wait_saved(&d, "the workspace", |s| s.panes.len() == 1).await;
    kill(&mut d, "KILL");
    // As if the next orchd died while restoring it.
    let content = std::fs::read(autosave(&d)).unwrap();
    std::fs::rename(autosave(&d), d.dir.join("autosave.json.restoring")).unwrap();
    let mut c = restart(&mut d).await;
    assert!(session(&d).await.workspaces.is_empty(), "not restored");
    assert!(pane_infos(&mut c).await.is_empty());
    assert!(!d.dir.join("autosave.json.restoring").exists());
    assert_eq!(
        std::fs::read(d.dir.join("autosave.json.failed")).unwrap(),
        content,
        "kept for inspection"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchanged_layout_is_not_rewritten() {
    let d = daemon("unchanged", 100, 150).await;
    op(
        &d,
        UiOp::NewWorkspace {
            label: Some("still".into()),
            spawn: spawn(&["sleep", "1000"]),
        },
    )
    .await;
    wait_saved(&d, "the workspace", |s| s.panes.len() == 1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let stamp = || {
        let m = std::fs::metadata(autosave(&d)).unwrap();
        (m.ino(), m.mtime(), m.mtime_nsec())
    };
    let first = stamp();
    // About ten timer ticks.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(stamp(), first, "rewritten without a change");
    // A real change is still written.
    op(
        &d,
        UiOp::NewWorkspace {
            label: Some("more".into()),
            spawn: spawn(&["sleep", "1001"]),
        },
    )
    .await;
    wait_saved(&d, "the second workspace", |s| s.panes.len() == 2).await;
    assert_ne!(stamp(), first);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_working_agent_is_recovered_with_the_unplanned_stop_note_once() {
    use std::os::unix::fs::PermissionsExt;
    let mut d = daemon("agents", 100, 200).await;
    // Working at first; after the restart (marker exists) idle, and it records its input.
    let script = r#"#!/bin/sh
stty -echo
here=$(dirname "$0"); top=$(dirname "$here"); role=$(basename "$here")
marker=$top/$role.started; receipts=$top/$role.received
initial=no
if [ ! -f "$marker" ]; then initial=yes; : > "$marker"; fi
if [ "$initial" = yes ] && [ "$role" != idle ]; then
    printf 'Thinking (esc to interrupt)\n⏵⏵ auto mode on\n'
else
    printf 'Done\n⏵⏵ auto mode on (shift+tab to cycle)\n'
fi
while IFS= read -r line; do printf '%s\n' "$line" >> "$receipts"; done
"#;
    let mut c = conn(&d).await;
    let mut ids = Vec::new();
    for role in ["working", "idle", "asking"] {
        let program = d.dir.join(role).join("fake-claude");
        std::fs::create_dir_all(program.parent().unwrap()).unwrap();
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let v = c
            .request(&Request::NewPane(NewPane {
                argv: vec![program.to_string_lossy().into_owned()],
                name: Some(role.into()),
                rows: Some(20),
                cols: Some(120),
                ..Default::default()
            }))
            .await
            .unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        c.request(&Request::RegisterAgent(RegisterAgent {
            kind: "claude".into(),
            pane: Some(format!("orch:{id}")),
            name: Some(role.into()),
            cwd: None,
            instance: None,
        }))
        .await
        .unwrap();
        ids.push((role, id));
    }
    c.request(&Request::Ask {
        from: "asking".into(),
        text: "Wait for Murat?".into(),
        options: vec![],
    })
    .await
    .unwrap();
    wait_saved(&d, "the working agent", |s| {
        ids.iter().all(|(role, id)| {
            s.panes.get(id).is_some_and(|p| {
                p.agent.as_deref() == Some("claude") && p.was_working == (*role == "working")
            })
        })
    })
    .await;
    // The asking agent's screen says Working too: only the open question keeps it out.
    let panes = pane_infos(&mut c).await;
    let asking = &ids[2].1;
    assert_eq!(
        panes
            .iter()
            .find(|p| &p.id == asking)
            .unwrap()
            .agent_status
            .as_deref(),
        Some("working")
    );

    kill(&mut d, "KILL");
    let mut c = restart(&mut d).await;
    let mut summary = None;
    for _ in 0..400 {
        let log = c
            .request(&Request::Log {
                after: None,
                limit: Some(100),
                follow: false,
            })
            .await
            .unwrap();
        let rows: Vec<_> = log["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["thread"] == "restart")
            .cloned()
            .collect();
        if !rows.is_empty() {
            assert_eq!(rows.len(), 1);
            summary = Some(rows[0].clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let summary = summary.expect("recovery summary missing");
    assert_eq!(summary["to"], "murat");
    assert_eq!(
        summary["text"],
        "recovered after an unplanned stop: resumed 3 agents, nudged 1 that were working (working)"
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    for (role, _) in &ids {
        let received =
            std::fs::read_to_string(d.dir.join(format!("{role}.received"))).unwrap_or_default();
        if *role == "working" {
            assert_eq!(
                received,
                format!("{}\n", orchd::restore::RECOVERY_NOTE),
                "exactly once, and not the restart note"
            );
        } else {
            assert!(received.is_empty(), "{role} got input: {received}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_keeps_an_agent_that_died_before_its_shell() {
    // A long timer: after the agent shows up, only a layout op writes the autosave.
    let mut d = daemon("shellagent", 100, 60_000).await;
    let agent = d.dir.join("fake-agent");
    let pidfile = d.dir.join("agent.pid");
    // Looks like Claude Code; dies on SIGTERM (clearing its screen, as a TUI leaves it).
    std::fs::write(
        &agent,
        format!(
            "trap 'printf \"\\033[2J\\033[H\"; exit 143' TERM\n\
             printf 'Done\\n⏵⏵ auto mode on (shift+tab to cycle)\\n'\n\
             echo $$ > {}\n\
             while :; do sleep 0.05; done\n",
            pidfile.display()
        ),
    )
    .unwrap();
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("agent".into()),
            spawn: spawn(&["bash", "--norc"]),
        },
    )
    .await;
    let ws = v["session"]["workspaces"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let pane = v["pane"]["id"].as_str().unwrap().to_string();
    let mut c = conn(&d).await;
    c.request(&Request::Send(orch_proto::api::SendPane {
        pane: pane.clone(),
        text: format!("sh {}", agent.display()),
        enter: true,
        ..Default::default()
    }))
    .await
    .unwrap();
    let agent_seen = |want: bool| {
        let pane = pane.clone();
        let socket = d.socket.clone();
        async move {
            for _ in 0..400 {
                let mut c = Conn::unix(&socket).await.unwrap();
                let panes = pane_infos(&mut c).await;
                let p = panes.iter().find(|p| p.id == pane).unwrap();
                if p.agent.is_some() == want {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("agent seen never became {want}");
        }
    };
    agent_seen(true).await;
    // A layout op: the debounced autosave records the agent.
    op(
        &d,
        UiOp::RenameWorkspace {
            workspace: ws,
            label: "agent2".into(),
        },
    )
    .await;
    wait_saved(&d, "the agent in the shell", |s| {
        s.panes
            .get(&pane)
            .is_some_and(|p| p.agent.as_deref() == Some("claude"))
            && s.session.workspaces.iter().any(|w| w.label == "agent2")
    })
    .await;
    // The shutdown: the agent dies, the interactive shell ignores SIGTERM.
    let mut pid = String::new();
    for _ in 0..200 {
        pid = std::fs::read_to_string(&pidfile).unwrap_or_default();
        if !pid.trim().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let bash = pane_infos(&mut c)
        .await
        .into_iter()
        .find(|p| p.id == pane)
        .unwrap()
        .pid
        .unwrap();
    for target in [pid.trim().to_string(), bash.to_string()] {
        assert!(Command::new("kill")
            .args(["-TERM", &target])
            .status()
            .unwrap()
            .success());
    }
    agent_seen(false).await;
    kill(&mut d, "TERM");
    let saved: orchd::restore::Saved =
        serde_json::from_slice(&std::fs::read(autosave(&d)).unwrap()).unwrap();
    assert_eq!(saved.panes[&pane].agent.as_deref(), Some("claude"));
    let mut c = restart(&mut d).await;
    let after = session(&d).await;
    assert_eq!(labels(&after), vec!["agent2"]);
    let new = after.workspaces[0].tabs[0].root.panes()[0].clone();
    let info = pane_infos(&mut c)
        .await
        .into_iter()
        .find(|p| p.id == new)
        .unwrap();
    assert_eq!(info.argv[..2], ["bash", "-lc"]);
    assert!(
        info.argv[2].contains("claude --continue"),
        "the agent is resumed in its shell: {:?}",
        info.argv
    );
}
