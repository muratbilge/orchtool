//! Layout operations and the event stream, against a real orchd.

use orch_proto::api::{NewPane, Request, Spawn, Stream, UiOp};
use orch_proto::client::Conn;
use orch_proto::layout::{Dir, Session};
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;
use tokio::time::timeout;

struct D {
    child: Child,
    socket: PathBuf,
    dir: PathBuf,
}

impl Drop for D {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn daemon(tag: &str) -> D {
    let dir = std::env::temp_dir().join(format!(
        "orch-ui-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("s.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_orchd"))
        .arg("--socket")
        .arg(&socket)
        .arg("--db")
        .arg(dir.join("db"))
        .env("HOME", &dir)
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if Conn::unix(&socket).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    D { child, socket, dir }
}

async fn op(d: &D, op: UiOp) -> Value {
    Conn::unix(&d.socket)
        .await
        .unwrap()
        .request(&Request::Ui(op))
        .await
        .unwrap()
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

fn session(v: &Value) -> Session {
    serde_json::from_value(v["session"].clone()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspaces_tabs_splits_and_close() {
    let d = daemon("layout").await;
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("main".into()),
            spawn: spawn(&["cat"]),
        },
    )
    .await;
    let s = session(&v);
    assert_eq!(s.workspaces.len(), 1);
    let ws = s.workspaces[0].id.clone();
    let p1 = v["pane"]["id"].as_str().unwrap().to_string();
    assert_eq!(s.workspaces[0].tabs[0].focus, p1);

    let v = op(
        &d,
        UiOp::Split {
            pane: p1.clone(),
            dir: Dir::Right,
            spawn: spawn(&["cat"]),
        },
    )
    .await;
    let p2 = v["pane"]["id"].as_str().unwrap().to_string();
    let s = session(&v);
    assert_eq!(
        s.workspaces[0].tabs[0].root.panes(),
        vec![p1.clone(), p2.clone()]
    );
    assert_eq!(
        s.workspaces[0].tabs[0].focus, p2,
        "focus follows the new pane"
    );

    let v = op(
        &d,
        UiOp::NewTab {
            workspace: ws.clone(),
            spawn: spawn(&["cat"]),
        },
    )
    .await;
    let p3 = v["pane"]["id"].as_str().unwrap().to_string();
    let s = session(&v);
    assert_eq!(s.workspaces[0].tabs.len(), 2);
    assert_eq!(s.workspaces[0].active_tab, 1);

    let s = session(&op(&d, UiOp::Focus { pane: p1.clone() }).await);
    assert_eq!(
        s.workspaces[0].active_tab, 0,
        "focusing a pane selects its tab"
    );
    // Raise (phone / `orch focus`): selects the tab like Focus and records the workspace.
    let s = session(&op(&d, UiOp::Raise { pane: p3.clone() }).await);
    assert_eq!(s.workspaces[0].active_tab, 1);
    let r1 = s.raised.clone().expect("raised");
    assert_eq!(r1.workspace, ws);
    let s = session(&op(&d, UiOp::Raise { pane: p1.clone() }).await);
    assert_eq!(s.workspaces[0].active_tab, 0);
    assert!(s.raised.unwrap().seq > r1.seq, "every raise counts");
    let s = session(&op(&d, UiOp::ToggleZoom { pane: p1.clone() }).await);
    assert!(s.workspaces[0].tabs[0].zoomed);
    let s = session(
        &op(
            &d,
            UiOp::RenameWorkspace {
                workspace: ws.clone(),
                label: "work".into(),
            },
        )
        .await,
    );
    assert_eq!(s.workspaces[0].label, "work");

    // Closing kills the program and collapses the layout.
    let s = session(&op(&d, UiOp::ClosePane { pane: p2.clone() }).await);
    assert_eq!(s.workspaces[0].tabs[0].root.panes(), vec![p1.clone()]);
    assert!(!s.workspaces[0].tabs[0].zoomed);
    let mut c = Conn::unix(&d.socket).await.unwrap();
    let panes = c.request(&Request::ListPanes {}).await.unwrap();
    let ids: Vec<&str> = panes["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&p2.as_str()));
    // The last pane of a tab removes the tab; the last tab removes the workspace.
    let s = session(&op(&d, UiOp::ClosePane { pane: p3 }).await);
    assert_eq!(s.workspaces[0].tabs.len(), 1);
    let s = session(&op(&d, UiOp::ClosePane { pane: p1 }).await);
    assert!(s.workspaces.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_panes_are_placed_and_kill_unplaces() {
    let d = daemon("place").await;
    let mut c = Conn::unix(&d.socket).await.unwrap();
    let a = c
        .request(&Request::NewPane(NewPane {
            argv: vec!["cat".into()],
            cwd: Some("/tmp".into()),
            ..Default::default()
        }))
        .await
        .unwrap();
    let b = c
        .request(&Request::NewPane(NewPane {
            argv: vec!["cat".into()],
            cwd: Some("/tmp".into()),
            ..Default::default()
        }))
        .await
        .unwrap();
    let s: Session = serde_json::from_value(c.request(&Request::UiGet {}).await.unwrap()).unwrap();
    assert_eq!(s.workspaces.len(), 1, "same cwd → same workspace");
    assert_eq!(s.workspaces[0].label, "tmp");
    assert_eq!(s.workspaces[0].tabs.len(), 2, "each as a tab");
    c.request(&Request::Kill {
        pane: a["id"].as_str().unwrap().into(),
    })
    .await
    .unwrap();
    let s: Session = serde_json::from_value(c.request(&Request::UiGet {}).await.unwrap()).unwrap();
    assert_eq!(s.all_panes(), vec![b["id"].as_str().unwrap().to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_stream_reports_layout_panes_and_status() {
    let d = daemon("events").await;
    let conn = Conn::unix(&d.socket).await.unwrap();
    let (mut r, mut w) = conn.into_parts();
    w.send_body("e", serde_json::to_value(Request::Subscribe {}).unwrap())
        .await
        .unwrap();
    r.next().await.unwrap().unwrap(); // reply
    let snap = r.next().await.unwrap().unwrap();
    assert!(matches!(
        serde_json::from_value::<Stream>(snap.body).unwrap(),
        Stream::Snapshot { .. }
    ));

    // A pane running a known agent kind that shows an approval dialog → status "blocked".
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: None,
            spawn: Spawn {
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    "printf 'Allow the tool?\\n  enter to submit | esc to cancel\\n'; exec cat"
                        .into(),
                ],
                cwd: Some("/tmp".into()),
                ..Default::default()
            },
        },
    )
    .await;
    let pane = v["pane"]["id"].as_str().unwrap().to_string();
    // Teach orchd that this pane runs codex (as orch-mcp would when it registers).
    Conn::unix(&d.socket)
        .await
        .unwrap()
        .request(&Request::RegisterAgent(orch_proto::api::RegisterAgent {
            kind: "codex".into(),
            pane: Some(format!("orch:{pane}")),
            cwd: Some("/tmp".into()),
            name: Some("dialog-agent".into()),
            instance: None,
        }))
        .await
        .unwrap();
    let (mut saw_session, mut saw_blocked) = (false, false);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    while !(saw_session && saw_blocked) {
        let f = timeout(deadline - tokio::time::Instant::now(), r.next())
            .await
            .expect("events")
            .unwrap()
            .unwrap();
        let v = f.body;
        match v["event"]["event"].as_str() {
            Some("session") => {
                saw_session |= v["event"]["session"]["workspaces"]
                    .as_array()
                    .is_some_and(|w| !w.is_empty())
            }
            Some("panes") => {
                saw_blocked |= v["event"]["panes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p["id"] == pane.as_str() && p["agent_status"] == "blocked")
            }
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_tab_and_workspace_kill_their_panes() {
    let d = daemon("close").await;
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("a".into()),
            spawn: spawn(&["cat"]),
        },
    )
    .await;
    let ws = session(&v).workspaces[0].id.clone();
    let p1 = v["pane"]["id"].as_str().unwrap().to_string();
    let v = op(
        &d,
        UiOp::NewTab {
            workspace: ws.clone(),
            spawn: spawn(&["cat"]),
        },
    )
    .await;
    let p2 = v["pane"]["id"].as_str().unwrap().to_string();
    op(
        &d,
        UiOp::Split {
            pane: p2.clone(),
            dir: Dir::Right,
            spawn: spawn(&["cat"]),
        },
    )
    .await;
    op(
        &d,
        UiOp::NewWorkspace {
            label: Some("b".into()),
            spawn: spawn(&["cat"]),
        },
    )
    .await;
    let list = || async {
        let v = Conn::unix(&d.socket)
            .await
            .unwrap()
            .request(&Request::ListPanes {})
            .await
            .unwrap();
        v["panes"].as_array().unwrap().len()
    };
    assert_eq!(list().await, 4);

    // Closing tab 2 of "a" kills both panes of its split; tab 1 stays.
    let v = op(
        &d,
        UiOp::CloseTab {
            workspace: ws.clone(),
            index: 1,
        },
    )
    .await;
    let s = session(&v);
    let a = s.workspace(&ws).unwrap();
    assert_eq!(a.tabs.len(), 1);
    assert_eq!(a.tabs[0].root.panes(), vec![p1]);
    assert_eq!(list().await, 2);

    // Closing workspace "a" (by label) removes it and its pane; "b" is untouched.
    let v = op(
        &d,
        UiOp::CloseWorkspace {
            workspace: "a".into(),
        },
    )
    .await;
    let s = session(&v);
    assert_eq!(s.workspaces.len(), 1);
    assert_eq!(s.workspaces[0].label, "b");
    assert_eq!(list().await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_brings_the_layout_back_with_fresh_panes() {
    let mut d = daemon("restart").await;
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("keep".into()),
            spawn: spawn(&["bash", "--norc"]),
        },
    )
    .await;
    let ws = session(&v).workspaces[0].id.clone();
    let shell = v["pane"]["id"].as_str().unwrap().to_string();
    let v = op(
        &d,
        UiOp::Split {
            pane: shell.clone(),
            dir: Dir::Right,
            spawn: spawn(&["cat"]),
        },
    )
    .await;
    let cat = v["pane"]["id"].as_str().unwrap().to_string();
    op(
        &d,
        UiOp::NewTab {
            workspace: ws.clone(),
            spawn: spawn(&["sleep", "1000"]),
        },
    )
    .await;
    // The shell moves to another folder: it comes back there, not where it started.
    let mut c = Conn::unix(&d.socket).await.unwrap();
    c.request(&Request::Send(orch_proto::api::SendPane {
        pane: shell.clone(),
        text: format!("cd {}", d.dir.display()),
        enter: true,
        ..Default::default()
    }))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let before: Session =
        serde_json::from_value(c.request(&Request::UiGet {}).await.unwrap()).unwrap();

    c.request(&Request::StopDaemon { restart: true })
        .await
        .unwrap();
    let _ = d.child.wait();
    assert!(d.dir.join("restore.json").exists(), "layout saved");
    d.child = Command::new(env!("CARGO_BIN_EXE_orchd"))
        .arg("--socket")
        .arg(&d.socket)
        .arg("--db")
        .arg(d.dir.join("db"))
        .env("HOME", &d.dir)
        .spawn()
        .unwrap();
    let mut c = None;
    for _ in 0..100 {
        if let Ok(x) = Conn::unix(&d.socket).await {
            c = Some(x);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut c = c.unwrap();
    let after: Session =
        serde_json::from_value(c.request(&Request::UiGet {}).await.unwrap()).unwrap();
    assert!(!d.dir.join("restore.json").exists(), "read once");

    // Same workspaces, tabs and split shape, new pane ids.
    assert_eq!(after.workspaces.len(), 1);
    let (b, a) = (&before.workspaces[0], &after.workspaces[0]);
    assert_eq!((a.id.as_str(), a.label.as_str()), (b.id.as_str(), "keep"));
    assert_eq!(a.tabs.len(), 2);
    assert_eq!(a.active_tab, b.active_tab);
    let new = a.tabs[0].root.panes();
    assert_eq!(new.len(), 2);
    assert!(!new.contains(&shell) && !new.contains(&cat));
    assert!(matches!(
        a.tabs[0].root,
        orch_proto::layout::Node::Split {
            dir: Dir::Right,
            ..
        }
    ));
    let panes = c.request(&Request::ListPanes {}).await.unwrap();
    let panes: Vec<orch_proto::api::PaneInfo> =
        serde_json::from_value(panes["panes"].clone()).unwrap();
    let info = |id: &str| panes.iter().find(|p| p.id == id).unwrap().clone();
    assert_eq!(info(&new[0]).argv, vec!["bash", "--norc"]);
    assert_eq!(
        info(&new[0]).cwd,
        d.dir.display().to_string(),
        "the shell's folder"
    );
    assert_eq!(info(&new[1]).argv, vec!["cat"]);
    assert_eq!(info(&a.tabs[1].root.panes()[0]).argv, vec!["sleep", "1000"]);

    // A plain stop forgets the layout.
    c.request(&Request::StopDaemon { restart: false })
        .await
        .unwrap();
    let _ = d.child.wait();
    assert!(!d.dir.join("restore.json").exists());
}
