//! Workspaces sleep and wake (`orch ws sleep|wake`), with fake agents: no real Claude/Codex.

use orch_proto::api::{NewPane, RegisterAgent, Request, Spawn, UiOp};
use orch_proto::client::{ClientError, Conn};
use orch_proto::layout::{Dir, Node, Session};
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

struct D {
    child: Child,
    socket: PathBuf,
    dir: PathBuf,
    /// ORCHD_FAULT for the next start (failure injection; empty = none).
    fault: String,
}

impl Drop for D {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn spawn_orchd(dir: &Path, socket: &Path) -> Child {
    spawn_faulty(dir, socket, "")
}

fn spawn_faulty(dir: &Path, socket: &Path, fault: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_orchd"))
        .arg("--socket")
        .arg(socket)
        .arg("--db")
        .arg(dir.join("db"))
        .env("HOME", dir.join("home"))
        .env_remove("ORCH_HOST")
        .env("ORCHD_AUTOSAVE_DEBOUNCE_MS", "100")
        .env("ORCHD_AUTOSAVE_INTERVAL_MS", "200")
        .env("ORCHD_TASK_IDLE_MS", "300")
        .env("ORCHD_AUTO_SLEEP_TICK_MS", "100")
        .env("ORCHD_WAKE_GRACE_MS", "300")
        .env("ORCHD_LIMIT_CHECK_MS", "100")
        .env("ORCHD_FAULT", fault)
        .spawn()
        .unwrap()
}

async fn daemon(tag: &str) -> D {
    let dir = std::env::temp_dir().join(format!(
        "orch-sleep-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let socket = dir.join("s.sock");
    let child = spawn_orchd(&dir, &socket);
    let d = D {
        child,
        socket,
        dir,
        fault: String::new(),
    };
    conn(&d).await;
    d
}

/// The private daemon again, now with `fault` (the old one must be gone or is killed).
async fn restart_with(d: &mut D, fault: &str) {
    // Let the debounced autosave catch up with the layout first.
    if d.child.try_wait().unwrap().is_none() {
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    let _ = d.child.kill();
    let _ = d.child.wait();
    d.fault = fault.to_string();
    d.child = spawn_faulty(&d.dir, &d.socket, fault);
    conn(d).await;
}

/// The daemon again with a fault that kills it during start-up (it never serves): waits for that.
async fn start_crashing(d: &mut D, fault: &str) {
    if d.child.try_wait().unwrap().is_none() {
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    let _ = d.child.kill();
    let _ = d.child.wait();
    d.fault = fault.to_string();
    d.child = spawn_faulty(&d.dir, &d.socket, fault);
    wait_dead(d);
}

/// Waits for the daemon to die by itself (an injected crash).
fn wait_dead(d: &mut D) {
    for _ in 0..400 {
        if d.child.try_wait().unwrap().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("the daemon did not crash at the injected point");
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

async fn req(d: &D, r: Request) -> Result<Value, ClientError> {
    conn(d).await.request(&r).await
}

async fn op(d: &D, op: UiOp) -> Value {
    req(d, Request::Ui(op)).await.unwrap()
}

async fn session(d: &D) -> Session {
    serde_json::from_value(req(d, Request::UiGet {}).await.unwrap()).unwrap()
}

fn spawn(argv: &[String]) -> Spawn {
    Spawn {
        argv: argv.to_vec(),
        cwd: Some("/tmp".into()),
        rows: Some(20),
        cols: Some(100),
        ..Default::default()
    }
}

/// A fake Claude: records its arguments, shows a working screen on its first start when its
/// folder is called "working" (idle otherwise), and records every line typed into it.
const FAKE: &str = r#"#!/bin/sh
stty -echo
here=$(dirname "$0"); role=$(basename "$here")
printf '%s\n' "$*" >> "$here/args"
starts=$(wc -l < "$here/args")
if [ "$role" = working ] && [ "$starts" -eq 1 ]; then
    printf 'Thinking (esc to interrupt)\n⏵⏵ auto mode on\n'
else
    printf 'Done\n⏵⏵ auto mode on (shift+tab to cycle)\n'
fi
while IFS= read -r line; do printf '%s\n' "$line" >> "$here/received"; done
"#;

fn fake(d: &D, role: &str) -> String {
    let program = d.dir.join(role).join("fake-claude");
    std::fs::create_dir_all(program.parent().unwrap()).unwrap();
    std::fs::write(&program, FAKE).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    program.to_string_lossy().into_owned()
}

fn read(d: &D, role: &str, file: &str) -> String {
    std::fs::read_to_string(d.dir.join(role).join(file)).unwrap_or_default()
}

async fn register(d: &D, pane: &str, name: &str) {
    req(
        d,
        Request::RegisterAgent(RegisterAgent {
            kind: "claude".into(),
            pane: Some(format!("orch:{pane}")),
            name: Some(name.into()),
            cwd: None,
            instance: None,
        }),
    )
    .await
    .unwrap();
}

/// Waits for the pane's screen detection to say `status`.
async fn wait_status(d: &D, pane: &str, status: &str) {
    for _ in 0..400 {
        let v = req(d, Request::ListPanes {}).await.unwrap();
        if v["panes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == pane && p["agent_status"] == status)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{pane} never became {status}");
}

async fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..400 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never happened: {what}");
}

async fn running_panes(d: &D) -> Vec<String> {
    let v = req(d, Request::ListPanes {}).await.unwrap();
    v["panes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["state"] == "running")
        .map(|p| p["id"].as_str().unwrap().to_string())
        .collect()
}

fn orch() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_orchd")).with_file_name("orch")
}

fn orch_cli(d: &D, args: &[&str]) -> std::process::Output {
    Command::new(orch())
        .arg("--socket")
        .arg(&d.socket)
        .args(args)
        .env("HOME", d.dir.join("home"))
        .env_remove("ORCH_HOST")
        .env_remove("ORCH_PANE")
        .output()
        .unwrap()
}

/// A workspace "proj": tab 1 = an idle fake Claude (bus name "proj-claude") split with a
/// bash; tab 2 = a second shell. Returns (workspace id, agent pane, shell pane).
async fn proj(d: &D) -> (String, String, String) {
    let agent = fake(d, "idle");
    let v = op(
        d,
        UiOp::NewWorkspace {
            label: Some("proj".into()),
            spawn: spawn(&[agent]),
        },
    )
    .await;
    let ws = v["session"]["workspaces"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let a = v["pane"]["id"].as_str().unwrap().to_string();
    register(d, &a, "proj-claude").await;
    let v = op(
        d,
        UiOp::Split {
            pane: a.clone(),
            dir: Dir::Right,
            spawn: spawn(&["bash".into(), "--norc".into()]),
        },
    )
    .await;
    let sh = v["pane"]["id"].as_str().unwrap().to_string();
    op(
        d,
        UiOp::NewTab {
            workspace: ws.clone(),
            spawn: spawn(&["bash".into(), "--norc".into()]),
        },
    )
    .await;
    op(
        d,
        UiOp::RenameTab {
            workspace: ws.clone(),
            index: 0,
            tab: None,
            label: "agent".into(),
        },
    )
    .await;
    op(
        d,
        UiOp::Resize {
            pane: a.clone(),
            dir: Dir::Right,
            delta: 0.2,
        },
    )
    .await;
    wait_status(d, &a, "idle").await;
    (ws, a, sh)
}

fn shape(n: &Node) -> String {
    match n {
        Node::Pane { .. } => "pane".into(),
        Node::Split { dir, ratio, a, b } => {
            format!("split({dir:?},{ratio:.2},{},{})", shape(a), shape(b))
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sleep_and_wake_keep_the_layout_and_resume_the_agent() {
    let d = daemon("roundtrip").await;
    let (ws, agent, _shell) = proj(&d).await;
    // A second workspace stays awake; "proj" comes back in front of it.
    op(
        &d,
        UiOp::NewWorkspace {
            label: Some("other".into()),
            spawn: spawn(&["bash".into(), "--norc".into()]),
        },
    )
    .await;
    let before = session(&d).await.workspace(&ws).unwrap().clone();
    let all_before: Vec<String> = before.tabs.iter().flat_map(|t| t.root.panes()).collect();

    // Dry run: nothing busy, nothing changes.
    let v = op(
        &d,
        UiOp::SleepWorkspace {
            workspace: "proj".into(),
            force: false,
            dry_run: true,
        },
    )
    .await;
    assert_eq!(v["slept"], false);
    assert!(v["plan"]["blockers"].as_array().unwrap().is_empty(), "{v}");
    assert!(session(&d).await.workspace(&ws).is_some());

    let out = orch_cli(&d, &["ws", "sleep", "proj"]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("asleep: proj"));
    let s = session(&d).await;
    assert!(s.workspace(&ws).is_none(), "not among the awake workspaces");
    let sl = s.sleeping("proj").expect("listed as sleeping");
    assert_eq!(sl.position, 0);
    assert_eq!(sl.workspace, before, "layout kept exactly");
    let running = running_panes(&d).await;
    for p in &all_before {
        assert!(!running.contains(p), "{p} still runs");
    }
    assert_eq!(
        sl.panes[&agent].saved.bus_name.as_deref(),
        Some("proj-claude")
    );
    // Listed for clients: `orch ls` and the watch line.
    let ls = orch_cli(&d, &["ls"]);
    assert!(String::from_utf8_lossy(&ls.stdout).contains("SLEEPING WORKSPACES"));
    let watch = orch_cli(&d, &["watch", "--once"]);
    let line: Value = serde_json::from_slice(&watch.stdout).unwrap();
    assert_eq!(line["sleeping"][0]["label"], "proj");
    assert_eq!(line["sleeping"][0]["agents"][0]["name"], "proj-claude");
    assert_eq!(line["workspaces"].as_array().unwrap().len(), 1);
    // Sleeping twice is fine; the label stays taken.
    assert!(orch_cli(&d, &["ws", "sleep", "proj"]).status.success());
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("proj".into()),
            spawn: spawn(&["bash".into(), "--norc".into()]),
        },
    )
    .await;
    let dup = v["pane"]["id"].as_str().unwrap().to_string();
    let s = session(&d).await;
    let (wi, _) = s.locate(&dup).unwrap();
    assert_eq!(s.workspaces[wi].label, "proj-2");
    op(&d, UiOp::ClosePane { pane: dup }).await;

    let out = orch_cli(&d, &["ws", "wake", "proj"]);
    assert!(out.status.success(), "{out:?}");
    let s = session(&d).await;
    assert!(s.sleeping.is_empty());
    assert_eq!(s.workspaces[0].id, ws, "back in its place");
    let w = &s.workspaces[0];
    assert_eq!(w.label, "proj");
    assert_eq!(w.tabs.len(), 2);
    for (a, b) in w.tabs.iter().zip(&before.tabs) {
        assert_eq!((&a.id, &a.label), (&b.id, &b.label));
        assert_eq!(shape(&a.root), shape(&b.root), "splits and sizes");
    }
    let new_panes: Vec<String> = w.tabs.iter().flat_map(|t| t.root.panes()).collect();
    assert!(
        new_panes.iter().all(|p| !all_before.contains(p)),
        "fresh panes"
    );
    // The agent resumed (claude --continue) under its bus name, which follows the new pane.
    wait_for("agent restarted", || {
        read(&d, "idle", "args").lines().count() == 2
    })
    .await;
    // No verified session id: Claude's resume picker, never "the latest one in this folder".
    assert_eq!(read(&d, "idle", "args").lines().nth(1), Some("--resume"));
    let v = req(&d, Request::ListAgents {}).await.unwrap();
    let row = v["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "proj-claude")
        .unwrap()
        .clone();
    assert_eq!(row["pane"], format!("orch:{}", w.tabs[0].root.panes()[0]));
    // Nothing to tell it: an idle agent without mail gets no input.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(read(&d, "idle", "received"), "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_workspaces_are_refused_unless_forced() {
    let d = daemon("busy").await;
    let program = fake(&d, "working");
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("busy".into()),
            spawn: spawn(&[program]),
        },
    )
    .await;
    let ws = v["session"]["workspaces"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let agent = v["pane"]["id"].as_str().unwrap().to_string();
    register(&d, &agent, "busy-claude").await;
    // A program that cannot be resumed, and a shell running a foreground job.
    op(
        &d,
        UiOp::NewTab {
            workspace: ws.clone(),
            spawn: spawn(&["sleep".into(), "1000".into()]),
        },
    )
    .await;
    let v = op(
        &d,
        UiOp::NewTab {
            workspace: ws.clone(),
            spawn: spawn(&["bash".into(), "--norc".into()]),
        },
    )
    .await;
    let sh = v["pane"]["id"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(300)).await;
    req(
        &d,
        Request::Send(orch_proto::api::SendPane {
            pane: sh.clone(),
            text: "sleep 1001".into(),
            enter: true,
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    wait_status(&d, &agent, "working").await;

    let v = op(
        &d,
        UiOp::SleepWorkspace {
            workspace: ws.clone(),
            force: false,
            dry_run: true,
        },
    )
    .await;
    let blockers: Vec<String> = serde_json::from_value(v["plan"]["blockers"].clone()).unwrap();
    let all = blockers.join("\n");
    assert!(
        all.contains("busy-claude") && all.contains("is working"),
        "{all}"
    );
    assert!(all.contains("sleep, which cannot be resumed"), "{all}");
    let mut seen_job = false;
    for _ in 0..80 {
        let v = op(
            &d,
            UiOp::SleepWorkspace {
                workspace: ws.clone(),
                force: false,
                dry_run: true,
            },
        )
        .await;
        if v["plan"]["blockers"]
            .to_string()
            .contains("runs sleep in its shell")
        {
            seen_job = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(seen_job, "a shell's foreground job blocks");

    // Refused: through the API, and exit 15 from the CLI. Nothing changed.
    let e = req(
        &d,
        Request::Ui(UiOp::SleepWorkspace {
            workspace: ws.clone(),
            force: false,
            dry_run: false,
        }),
    )
    .await
    .unwrap_err();
    match e {
        ClientError::Remote(b) => {
            assert_eq!(b.code, "ws_busy");
            assert!(b.message.contains("--force"));
        }
        other => panic!("{other:?}"),
    }
    let out = orch_cli(&d, &["ws", "sleep", "busy"]);
    assert_eq!(out.status.code(), Some(15), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("is working"));
    assert!(session(&d).await.workspace(&ws).is_some());
    assert!(running_panes(&d).await.contains(&agent));

    // Forced: asleep; on wake the agent is told its turn was interrupted, the program that
    // could not be resumed comes back as a shell.
    let out = orch_cli(&d, &["ws", "sleep", "busy", "--force"]);
    assert!(out.status.success(), "{out:?}");
    let s = session(&d).await;
    let sl = s.sleeping("busy").unwrap();
    assert!(sl.panes[&agent].interrupted);
    assert!(sl
        .panes
        .values()
        .any(|p| p.saved.argv.is_empty() && p.saved.agent.is_none()));
    let out = orch_cli(&d, &["ws", "wake", "busy"]);
    assert!(out.status.success(), "{out:?}");
    wait_for("the interrupted agent gets its note", || {
        read(&d, "working", "received").contains("put to sleep while you were busy")
    })
    .await;
    assert_eq!(read(&d, "working", "args").lines().count(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sleeping_workspace_stays_asleep_across_restarts_and_crashes() {
    let mut d = daemon("restart").await;
    let (_ws, _agent, _sh) = proj(&d).await;
    op(
        &d,
        UiOp::NewWorkspace {
            label: Some("awake".into()),
            spawn: spawn(&["bash".into(), "--norc".into()]),
        },
    )
    .await;
    op(
        &d,
        UiOp::SleepWorkspace {
            workspace: "proj".into(),
            force: false,
            dry_run: false,
        },
    )
    .await;
    // An unplanned stop right away (the sleep wrote the autosave itself).
    let pid = d.child.id().to_string();
    assert!(Command::new("kill")
        .args(["-9", &pid])
        .status()
        .unwrap()
        .success());
    d.child.wait().unwrap();
    d.child = spawn_orchd(&d.dir, &d.socket);
    let s = session(&d).await;
    assert_eq!(s.sleeping.len(), 1, "still asleep after a crash");
    assert_eq!(s.workspaces.len(), 1);
    assert_eq!(s.workspaces[0].label, "awake");
    assert_eq!(
        read(&d, "idle", "args").lines().count(),
        1,
        "not woken by itself"
    );

    // A planned restart.
    let out = orch_cli(&d, &["stop-daemon", "--restart"]);
    assert!(out.status.success(), "{out:?}");
    d.child.wait().unwrap();
    d.child = spawn_orchd(&d.dir, &d.socket);
    let s = session(&d).await;
    assert_eq!(s.sleeping.len(), 1, "still asleep after a restart");
    assert_eq!(s.workspaces.len(), 1);

    // A plain stop-daemon ends the awake workspace but keeps the sleeping one.
    let out = orch_cli(&d, &["stop-daemon"]);
    assert!(out.status.success(), "{out:?}");
    d.child.wait().unwrap();
    d.child = spawn_orchd(&d.dir, &d.socket);
    let s = session(&d).await;
    assert_eq!(s.sleeping.len(), 1, "still asleep after a plain stop");
    assert!(s.workspaces.is_empty());
    assert_eq!(read(&d, "idle", "args").lines().count(), 1);

    let out = orch_cli(&d, &["ws", "wake", "proj"]);
    assert!(out.status.success(), "{out:?}");
    wait_for("agent resumed after the restarts", || {
        read(&d, "idle", "args").lines().count() == 2
    })
    .await;
    let s = session(&d).await;
    assert_eq!(s.workspaces[0].label, "proj");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mail_tasks_and_questions_wait_for_a_sleeping_agent() {
    let d = daemon("mail").await;
    let (ws, agent, _sh) = proj(&d).await;
    op(
        &d,
        UiOp::SleepWorkspace {
            workspace: ws.clone(),
            force: false,
            dry_run: false,
        },
    )
    .await;
    // Mail for it, a task for its workspace (by label, accepted while asleep), a question it
    // asked before (via the bus directly: it stays open).
    req(
        &d,
        Request::SendMessage(orch_proto::api::SendMessage {
            from: "murat".into(),
            to: "proj-claude".into(),
            text: "please check the logs".into(),
            thread: None,
            wake: false,
        }),
    )
    .await
    .unwrap();
    let t = req(
        &d,
        Request::TaskAdd {
            target: "proj".into(),
            text: "run the tests".into(),
            created_by: "murat".into(),
            after: None,
            wake: false,
        },
    )
    .await
    .unwrap();
    let task = t["id"].as_i64().unwrap();
    let q = req(
        &d,
        Request::Ask {
            from: "proj-claude".into(),
            text: "Ship it?".into(),
            options: vec![],
        },
    )
    .await
    .unwrap();
    let qid = q["id"].as_i64().unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let v = req(&d, Request::TaskList { all: false }).await.unwrap();
    let row = v["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == task)
        .unwrap()
        .clone();
    assert_eq!(row["state"], "queued", "waits while asleep");
    let v = req(&d, Request::ListQuestions { all: false })
        .await
        .unwrap();
    assert!(
        v["questions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == qid),
        "open while asleep"
    );
    let v = req(
        &d,
        Request::Inbox {
            name: "proj-claude".into(),
            wait_ms: None,
            delivered: vec![],
        },
    )
    .await
    .unwrap();
    assert_eq!(v["messages"].as_array().unwrap().len(), 1, "mail kept");
    assert_eq!(read(&d, "idle", "received"), "");
    // Answer it so the agent counts as free once awake (questions block task dispatch).
    req(
        &d,
        Request::Answer {
            id: qid,
            answer: "yes".into(),
            by: "murat".into(),
        },
    )
    .await
    .unwrap();

    let v = op(
        &d,
        UiOp::WakeWorkspace {
            workspace: ws.clone(),
            partial: false,
        },
    )
    .await;
    let new_agent = session(&d).await.workspaces[0].tabs[0].root.panes()[0].clone();
    assert_ne!(new_agent, agent);
    assert!(v["panes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p == &new_agent));
    wait_for("told about its unread mail", || {
        read(&d, "idle", "received").contains("read_inbox")
    })
    .await;
    wait_for("the queued task is dispatched once awake", || {
        read(&d, "idle", "received").contains("run the tests")
    })
    .await;
    let v = req(&d, Request::TaskList { all: true }).await.unwrap();
    let row = v["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == task)
        .unwrap()
        .clone();
    assert_eq!(row["pane"], new_agent.as_str());
    // Closing a sleeping workspace forgets it.
    op(
        &d,
        UiOp::SleepWorkspace {
            workspace: ws.clone(),
            force: true,
            dry_run: false,
        },
    )
    .await;
    assert_eq!(session(&d).await.sleeping.len(), 1);
    op(
        &d,
        UiOp::CloseWorkspace {
            workspace: "proj".into(),
        },
    )
    .await;
    assert!(session(&d).await.sleeping.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_can_put_its_own_workspace_to_sleep() {
    let d = daemon("self").await;
    // A shell pane in the workspace runs `orch ws sleep . --force` itself: its connection drops
    // with it, the sleep still finishes.
    let script = format!(
        "sleep 0.5; unset ORCH_HOST; {} --socket {} ws sleep . --force; sleep 1000",
        orch().display(),
        d.socket.display()
    );
    let v = req(
        &d,
        Request::NewPane(NewPane {
            argv: vec!["sh".into(), "-c".into(), script],
            cwd: Some("/tmp".into()),
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    let pane = v["id"].as_str().unwrap().to_string();
    for _ in 0..400 {
        if !session(&d).await.sleeping.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let s = session(&d).await;
    assert_eq!(s.sleeping.len(), 1, "the workspace went to sleep");
    for _ in 0..200 {
        if !running_panes(&d).await.contains(&pane) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!running_panes(&d).await.contains(&pane));
    // It comes back as a plain shell (the -c program is not run again).
    let sl = &s.sleeping[0];
    assert_eq!(sl.panes[&pane].saved.argv, vec!["sh".to_string()]);
}

fn fake_codex(d: &D, role: &str) -> String {
    let program = d.dir.join(role).join("fake-codex");
    std::fs::create_dir_all(program.parent().unwrap()).unwrap();
    std::fs::write(&program, FAKE).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    program.to_string_lossy().into_owned()
}

/// Murat (2026-11-22): a Codex without a verified session id comes back at its picker after a
/// wake, never `resume --last`, even when it is the only Codex in its folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_without_a_session_id_wakes_at_its_picker() {
    let d = daemon("codex").await;
    let program = fake_codex(&d, "codex");
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("cx".into()),
            spawn: spawn(&[program]),
        },
    )
    .await;
    let pane = v["pane"]["id"].as_str().unwrap().to_string();
    req(
        &d,
        Request::RegisterAgent(RegisterAgent {
            kind: "codex".into(),
            pane: Some(format!("orch:{pane}")),
            name: Some("cx-codex".into()),
            cwd: None,
            instance: None,
        }),
    )
    .await
    .unwrap();
    wait_for("codex started", || {
        read(&d, "codex", "args").lines().count() == 1
    })
    .await;
    let v = op(
        &d,
        UiOp::SleepWorkspace {
            workspace: "cx".into(),
            force: false,
            dry_run: false,
        },
    )
    .await;
    assert!(
        v["plan"]["notes"].to_string().contains("resume picker"),
        "{v}"
    );
    let s = session(&d).await;
    assert!(
        !s.sleeping("cx").unwrap().panes[&pane]
            .saved
            .sole_codex_in_cwd
    );
    op(
        &d,
        UiOp::WakeWorkspace {
            workspace: "cx".into(),
            partial: false,
        },
    )
    .await;
    wait_for("codex resumed", || {
        read(&d, "codex", "args").lines().count() == 2
    })
    .await;
    assert_eq!(read(&d, "codex", "args").lines().nth(1), Some("resume"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mail_and_tasks_wake_a_workspace_only_on_request() {
    let d = daemon("wakeflag").await;
    let (ws, _agent, _sh) = proj(&d).await;
    let asleep = |s: &Session| s.sleeping("proj").is_some();
    op(
        &d,
        UiOp::SleepWorkspace {
            workspace: ws.clone(),
            force: false,
            dry_run: false,
        },
    )
    .await;
    // Without --wake: kept, still asleep.
    assert!(orch_cli(&d, &["msg", "proj-claude", "later"])
        .status
        .success());
    assert!(orch_cli(&d, &["task", "add", "proj", "later too"])
        .status
        .success());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(asleep(&session(&d).await));
    // A broadcast never wakes anything: refused, nothing sent.
    let out = orch_cli(&d, &["msg", "--wake", "all", "everyone up"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("all"));
    let log = req(
        &d,
        Request::Log {
            after: None,
            limit: Some(100),
            follow: false,
        },
    )
    .await
    .unwrap();
    assert!(!log.to_string().contains("everyone up"));
    assert!(asleep(&session(&d).await));
    // A message with --wake wakes the addressed agent's workspace; it is told to read it.
    let out = orch_cli(&d, &["msg", "--wake", "proj-claude", "now please"]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("woke proj"));
    assert!(!asleep(&session(&d).await));
    wait_for("told about its mail", || {
        read(&d, "idle", "received").contains("read_inbox")
    })
    .await;
    // The queued task runs once it is awake.
    wait_for("the earlier task runs", || {
        read(&d, "idle", "received").contains("later too")
    })
    .await;
    // A task with --wake wakes its workspace too.
    op(
        &d,
        UiOp::SleepWorkspace {
            workspace: ws.clone(),
            force: true,
            dry_run: false,
        },
    )
    .await;
    assert!(asleep(&session(&d).await));
    let out = orch_cli(&d, &["task", "add", "proj", "--wake", "run the tests"]);
    assert!(out.status.success(), "{out:?}");
    assert!(!asleep(&session(&d).await));
}

async fn attach(
    d: &D,
    pane: &str,
) -> (
    orch_proto::client::FrameReader,
    orch_proto::client::FrameWriter,
) {
    let (mut reader, mut writer) = Conn::unix(&d.socket).await.unwrap().into_parts();
    let r = Request::Attach(orch_proto::api::AttachPane {
        pane: pane.into(),
        mode: orch_proto::api::AttachMode::Observe,
        rows: 20,
        cols: 100,
    });
    writer
        .send_body("a", serde_json::to_value(&r).unwrap())
        .await
        .unwrap();
    reader.next().await.unwrap().unwrap();
    (reader, writer)
}

async fn shell_ws(d: &D, label: &str) -> (String, String) {
    let v = op(
        d,
        UiOp::NewWorkspace {
            label: Some(label.into()),
            spawn: spawn(&["bash".into(), "--norc".into()]),
        },
    )
    .await;
    let s: Session = serde_json::from_value(v["session"].clone()).unwrap();
    let pane = v["pane"]["id"].as_str().unwrap().to_string();
    let (wi, _) = s.locate(&pane).unwrap();
    (s.workspaces[wi].id.clone(), pane)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_workspaces_fall_asleep_but_not_busy_viewed_or_exempt_ones() {
    let d = daemon("auto").await;
    let out = orch_cli(&d, &["auto-sleep"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("auto-sleep: off"));
    shell_ws(&d, "quiet").await;
    let (_, viewed) = shell_ws(&d, "viewed").await;
    shell_ws(&d, "kept").await;
    let program = fake(&d, "working");
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("busy".into()),
            spawn: spawn(&[program]),
        },
    )
    .await;
    let busy = v["pane"]["id"].as_str().unwrap().to_string();
    register(&d, &busy, "busy-claude").await;
    wait_status(&d, &busy, "working").await;
    let viewer = attach(&d, &viewed).await;
    assert!(orch_cli(&d, &["ws", "keep-awake", "kept", "on"])
        .status
        .success());
    tokio::time::sleep(Duration::from_millis(400)).await;
    let out = orch_cli(&d, &["auto-sleep", "1500ms"]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("after 1500ms idle"));
    // Not before a full period.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(session(&d).await.sleeping.is_empty(), "too early");
    for _ in 0..200 {
        if session(&d).await.sleeping("quiet").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let s = session(&d).await;
    assert!(s.sleeping("quiet").is_some(), "idle: asleep");
    for awake in ["viewed", "kept", "busy"] {
        assert!(s.workspace(awake).is_some(), "{awake} must stay awake");
    }
    let log = req(
        &d,
        Request::Log {
            after: None,
            limit: Some(100),
            follow: false,
        },
    )
    .await
    .unwrap();
    assert!(
        log.to_string()
            .contains("quiet is asleep (auto-sleep: idle for 1500ms)"),
        "{log}"
    );
    let watch = orch_cli(&d, &["watch", "--once"]);
    let line: Value = serde_json::from_slice(&watch.stdout).unwrap();
    assert_eq!(line["auto_sleep_ms"], 1500);
    assert!(line["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w["label"] == "kept" && w["keep_awake"] == true));
    // The viewer leaves: a full period later it sleeps too.
    drop(viewer);
    for _ in 0..200 {
        if session(&d).await.sleeping("viewed").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(session(&d).await.sleeping("viewed").is_some());
    // Woken by hand: a fresh period, not straight back to sleep.
    assert!(orch_cli(&d, &["ws", "wake", "quiet"]).status.success());
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(session(&d).await.workspace("quiet").is_some());
    assert!(orch_cli(&d, &["auto-sleep", "off"]).status.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_gives_every_workspace_a_full_period() {
    let mut d = daemon("autorestart").await;
    shell_ws(&d, "a").await;
    shell_ws(&d, "b").await;
    assert!(orch_cli(&d, &["auto-sleep", "3s"]).status.success());
    // Almost a full period idle, then a planned restart.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(session(&d).await.sleeping.is_empty());
    assert!(orch_cli(&d, &["stop-daemon", "--restart"]).status.success());
    d.child.wait().unwrap();
    d.child = spawn_orchd(&d.dir, &d.socket);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        session(&d).await.sleeping.is_empty(),
        "no sleep storm right after a start"
    );
    let out = orch_cli(&d, &["auto-sleep"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("after 3s idle"),
        "kept"
    );
    for _ in 0..400 {
        if session(&d).await.sleeping.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(session(&d).await.sleeping.len(), 2, "then they do sleep");
}

// ---------- review findings (Codex, 2026-11-22) ----------

async fn sleep_op(d: &D, key: &str, force: bool) -> Result<Value, ClientError> {
    req(
        d,
        Request::Ui(UiOp::SleepWorkspace {
            workspace: key.into(),
            force,
            dry_run: false,
        }),
    )
    .await
}

async fn wake_op(d: &D, key: &str, partial: bool) -> Result<Value, ClientError> {
    req(
        d,
        Request::Ui(UiOp::WakeWorkspace {
            workspace: key.into(),
            partial,
        }),
    )
    .await
}

fn code(e: ClientError) -> String {
    match e {
        ClientError::Remote(b) => b.code,
        other => format!("{other:?}"),
    }
}

/// #1: the sleeper record is written and confirmed before any pane ends; a failed write puts
/// the workspace back and ends nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f1_a_failed_save_puts_nothing_to_sleep() {
    let mut d = daemon("f1").await;
    let (ws, agent, sh) = proj(&d).await;
    restart_with(&mut d, "store_write").await;
    let s = session(&d).await;
    let w = s.workspace("proj").unwrap().clone();
    let panes: Vec<String> = w.tabs.iter().flat_map(|t| t.root.panes()).collect();
    assert_ne!(w.id, "", "{ws} {agent} {sh}");
    let e = sleep_op(&d, "proj", false).await.unwrap_err();
    assert_eq!(code(e), "save_failed");
    let s = session(&d).await;
    assert!(s.sleeping.is_empty());
    assert!(s.workspace("proj").is_some(), "back in the layout");
    let running = running_panes(&d).await;
    for p in &panes {
        assert!(running.contains(p), "{p} must still run");
    }
}

/// #1/#3: killed right after the record is committed (before the panes end and before the
/// awake layout is rewritten): the next orchd keeps it asleep, the stale layout loses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f1_a_crash_after_the_commit_keeps_it_asleep() {
    let mut d = daemon("f1crash").await;
    proj(&d).await;
    tokio::time::sleep(Duration::from_millis(400)).await; // an autosave with proj awake
    restart_with(&mut d, "sleep_after_commit").await;
    let _ = sleep_op(&d, "proj", false).await;
    wait_dead(&mut d);
    restart_with(&mut d, "").await;
    let s = session(&d).await;
    assert!(s.sleeping("proj").is_some(), "asleep");
    assert!(s.workspace("proj").is_none(), "not also awake");
    wake_op(&d, "proj", false).await.unwrap();
    assert!(session(&d).await.workspace("proj").is_some());
}

/// #2: a plain stop never touches the sleeper record, and waits for a sleep in progress.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f2_a_plain_stop_waits_for_a_sleep_and_keeps_the_record() {
    let mut d = daemon("f2").await;
    proj(&d).await;
    restart_with(&mut d, "sleep_pause_ms=800").await;
    let socket = d.socket.clone();
    let sleeping = tokio::spawn(async move {
        let mut c = Conn::unix(&socket).await.unwrap();
        c.request(&Request::Ui(UiOp::SleepWorkspace {
            workspace: "proj".into(),
            force: false,
            dry_run: false,
        }))
        .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let out = orch_cli(&d, &["stop-daemon"]);
    assert!(out.status.success(), "{out:?}");
    // Its reply may race the shutdown; what counts is the record below.
    let _ = sleeping.await;
    d.child.wait().unwrap();
    assert!(!d.dir.join("autosave.json").exists());
    restart_with(&mut d, "").await;
    assert!(session(&d).await.sleeping("proj").is_some());
}

/// #3: the record lives apart from the layout files: a layout file rewritten without sleepers
/// (as an older orchd does), a garbage restore.json, or a corrupt newest slot lose nothing
/// older than the newest generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f3_layout_files_and_a_downgrade_never_hold_the_only_record() {
    let mut d = daemon("f3").await;
    proj(&d).await;
    sleep_op(&d, "proj", false).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let auto = std::fs::read_to_string(d.dir.join("autosave.json")).unwrap();
    assert!(
        !auto.contains("\"sleeping\""),
        "layout files carry no sleepers"
    );
    // What main's orchd would leave: its own layout file, no sleepers; plus a garbage
    // restore.json.
    let _ = d.child.kill();
    let _ = d.child.wait();
    std::fs::remove_file(d.dir.join("autosave.json")).unwrap();
    std::fs::write(d.dir.join("restore.json"), b"{ not json").unwrap();
    restart_with(&mut d, "").await;
    assert!(
        session(&d).await.sleeping("proj").is_some(),
        "after a downgrade"
    );
    // A second generation, then the newest slot corrupted: the predecessor is used.
    shell_ws(&d, "second").await;
    sleep_op(&d, "second", false).await.unwrap();
    let _ = d.child.kill();
    let _ = d.child.wait();
    let newest = ["sleeping.a.json", "sleeping.b.json"]
        .iter()
        .map(|n| d.dir.join(n))
        .max_by_key(|p| {
            serde_json::from_slice::<Value>(&std::fs::read(p).unwrap_or_default())
                .map(|v| v["generation"].as_u64().unwrap_or(0))
                .unwrap_or(0)
        })
        .unwrap();
    std::fs::write(&newest, b"garbage").unwrap();
    restart_with(&mut d, "").await;
    let s = session(&d).await;
    assert!(s.sleeping("proj").is_some(), "the predecessor generation");
    assert_eq!(std::fs::read(&newest).unwrap(), b"garbage", "left alone");
}

/// #4: a wake is all or nothing; a pane that can't start (its name is taken, its agent
/// exits at once) keeps every record, and the wake can be retried. --partial wakes the rest
/// and keeps the failed one asleep.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f4_a_pane_that_cannot_start_keeps_its_record() {
    let mut d = daemon("f4").await;
    let agent = fake(&d, "idle");
    let mut named = spawn(&["bash".into(), "--norc".into()]);
    named.name = Some("keep".into());
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("pair".into()),
            spawn: spawn(&[agent]),
        },
    )
    .await;
    let a = v["pane"]["id"].as_str().unwrap().to_string();
    register(&d, &a, "pair-claude").await;
    op(
        &d,
        UiOp::Split {
            pane: a.clone(),
            dir: Dir::Right,
            spawn: named.clone(),
        },
    )
    .await;
    wait_status(&d, &a, "idle").await;
    sleep_op(&d, "pair", false).await.unwrap();
    // The name is taken elsewhere now.
    let v = req(
        &d,
        Request::NewPane(NewPane {
            argv: vec!["bash".into(), "--norc".into()],
            name: Some("keep".into()),
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    let other = v["id"].as_str().unwrap().to_string();
    let e = wake_op(&d, "pair", false).await.unwrap_err();
    assert_eq!(code(e), "wake_failed");
    let s = session(&d).await;
    assert_eq!(
        s.sleeping("pair").unwrap().panes.len(),
        2,
        "both records kept"
    );
    assert!(s.workspace("pair").is_none());
    req(&d, Request::Kill { pane: other }).await.unwrap();
    wake_op(&d, "pair", false).await.unwrap();
    assert!(session(&d).await.workspace("pair").is_some(), "retry works");
    // An agent that exits at once did not start.
    sleep_op(&d, "pair", false).await.unwrap();
    let program = d.dir.join("idle").join("fake-claude");
    let good = std::fs::read(&program).unwrap();
    std::fs::write(&program, b"#!/bin/sh\nexit 1\n").unwrap();
    let e = wake_op(&d, "pair", false).await.unwrap_err();
    assert_eq!(code(e), "wake_failed");
    assert_eq!(session(&d).await.sleeping("pair").unwrap().panes.len(), 2);
    std::fs::write(&program, good).unwrap();
    // --partial: the failed one stays asleep on its own.
    restart_with(&mut d, "spawn_fail=keep").await;
    let v = wake_op(&d, "pair", true).await.unwrap();
    assert_eq!(v["failed"].as_array().unwrap().len(), 1);
    let rest = v["rest"].as_str().unwrap().to_string();
    let s = session(&d).await;
    assert!(s.workspace("pair").is_some());
    let r = s.sleeping(&rest).unwrap();
    assert_eq!(r.panes.len(), 1);
    assert_eq!(
        r.panes.values().next().unwrap().saved.name.as_deref(),
        Some("keep")
    );
}

/// #5: killed in the middle of a wake (owners moved, layout not committed): the next orchd
/// moves the running task and the bus name back, the question stays open, it is asleep and
/// wakes again. Killed after the commit: awake, no sleeper left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f5_a_crash_mid_wake_keeps_tasks_names_and_questions_consistent() {
    let mut d = daemon("f5").await;
    let (_ws, agent, _sh) = proj(&d).await;
    let t = req(
        &d,
        Request::TaskAdd {
            target: "proj".into(),
            text: "long job".into(),
            created_by: "murat".into(),
            after: None,
            wake: false,
        },
    )
    .await
    .unwrap();
    let task = t["id"].as_i64().unwrap();
    wait_for("the task runs", || {
        read(&d, "idle", "received").contains("long job")
    })
    .await;
    let q = req(
        &d,
        Request::Ask {
            from: "proj-claude".into(),
            text: "ok?".into(),
            options: vec![],
        },
    )
    .await
    .unwrap();
    let qid = q["id"].as_i64().unwrap();
    sleep_op(&d, "proj", true).await.unwrap();
    restart_with(&mut d, "wake_after_owners").await;
    let _ = wake_op(&d, "proj", false).await;
    wait_dead(&mut d);
    restart_with(&mut d, "").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let s = session(&d).await;
    assert!(s.sleeping("proj").is_some() && s.workspace("proj").is_none());
    assert!(s.sleeping("proj").unwrap().wake.is_none());
    let v = req(&d, Request::TaskList { all: true }).await.unwrap();
    let row = v["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == task)
        .unwrap()
        .clone();
    assert_eq!(row["state"], "running", "{row}");
    assert_eq!(row["pane"], agent.as_str(), "back on its old pane");
    let v = req(&d, Request::ListAgents {}).await.unwrap();
    let a = v["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "proj-claude")
        .unwrap()
        .clone();
    assert_eq!(a["pane"], format!("orch:{agent}"));
    let v = req(&d, Request::ListQuestions { all: false })
        .await
        .unwrap();
    assert!(v["questions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["id"] == qid));
    // After the commit.
    restart_with(&mut d, "wake_after_commit").await;
    let _ = wake_op(&d, "proj", false).await;
    wait_dead(&mut d);
    restart_with(&mut d, "").await;
    let s = session(&d).await;
    assert!(s.workspace("proj").is_some(), "awake");
    assert!(s.sleeping.is_empty(), "no sleeper left");
}

/// #6: while it goes to sleep nothing changes its layout or reaches its panes; a pane that
/// survives SIGKILL stays visible and blocks a second copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f6_a_sleep_reserves_its_workspace_and_keeps_survivors_visible() {
    let mut d = daemon("f6").await;
    let (_ws, agent, sh) = proj(&d).await;
    restart_with(&mut d, "sleep_pause_ms=1000").await;
    let s = session(&d).await;
    let panes: Vec<String> = s
        .workspace("proj")
        .unwrap()
        .tabs
        .iter()
        .flat_map(|t| t.root.panes())
        .collect();
    let socket = d.socket.clone();
    let sleeping = tokio::spawn(async move {
        let mut c = Conn::unix(&socket).await.unwrap();
        c.request(&Request::Ui(UiOp::SleepWorkspace {
            workspace: "proj".into(),
            force: false,
            dry_run: false,
        }))
        .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let e = req(
        &d,
        Request::Ui(UiOp::Split {
            pane: panes[0].clone(),
            dir: Dir::Down,
            spawn: spawn(&["bash".into(), "--norc".into()]),
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(code(e), "ws_transition");
    let e = req(
        &d,
        Request::Send(orch_proto::api::SendPane {
            pane: panes[0].clone(),
            text: "x".into(),
            ..Default::default()
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(code(e), "ws_transition");
    sleeping.await.unwrap().unwrap();
    let running = running_panes(&d).await;
    assert!(
        panes.iter().all(|p| !running.contains(p)),
        "nothing left running"
    );
    let _ = (agent, sh);
    // A pane that survives SIGKILL.
    wake_op(&d, "proj", false).await.unwrap();
    let mut named = spawn(&["bash".into(), "--norc".into()]);
    named.name = Some("stubborn".into());
    let ws = session(&d).await.workspace("proj").unwrap().id.clone();
    op(
        &d,
        UiOp::NewTab {
            workspace: ws,
            spawn: named,
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(600)).await; // in the autosave
    restart_with(&mut d, "kill_fail=stubborn").await;
    // (A restart restores the layout; the sleep below is of the restored workspace.)
    let v = sleep_op(&d, "proj", false).await.unwrap();
    assert!(v["warnings"].to_string().contains("proj-stuck"), "{v}");
    let s = session(&d).await;
    let stuck = s.workspace("proj-stuck").expect("survivor visible");
    let survivor = stuck.tabs[0].root.panes()[0].clone();
    assert!(running_panes(&d).await.contains(&survivor));
    let e = wake_op(&d, "proj", false).await.unwrap_err();
    assert_eq!(code(e), "ws_stuck");
}

/// #7: auto-sleep re-checks at the commit: a viewer that attached after it decided stops
/// it. A manual sleep refuses an agent whose hooks say it is working.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f7_auto_sleep_revalidates_and_bus_working_blocks() {
    let mut d = daemon("f7").await;
    restart_with(&mut d, "auto_pause_ms=1500").await;
    let (_, pane) = shell_ws(&d, "quiet").await;
    assert!(orch_cli(&d, &["auto-sleep", "1s"]).status.success());
    // Decided after ~1 s, then held 1.5 s before the commit: attach in that window.
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let viewer = attach(&d, &pane).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(session(&d).await.workspace("quiet").is_some(), "aborted");
    drop(viewer);
    assert!(orch_cli(&d, &["auto-sleep", "off"]).status.success());
    restart_with(&mut d, "").await;
    let (_, a, _) = proj(&d).await;
    let _ = a;
    req(
        &d,
        Request::SetStatus {
            name: "proj-claude".into(),
            status: "working".into(),
            text: None,
        },
    )
    .await
    .unwrap();
    let e = sleep_op(&d, "proj", false).await.unwrap_err();
    assert_eq!(code(e), "ws_busy");
}

fn codex_clock(secs: i64) -> String {
    let t = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + secs) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    let h = tm.tm_hour % 12;
    format!(
        "{}:{:02} {}",
        if h == 0 { 12 } else { h },
        tm.tm_min,
        if tm.tm_hour < 12 { "AM" } else { "PM" }
    )
}

/// #8: an agent with a usage-limit continue still due is never auto-slept (before or after a
/// wake), and the due continue survives a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f8_a_due_limit_continue_is_not_stranded() {
    let mut d = daemon("f8").await;
    let program = d.dir.join("limited").join("fake-codex");
    std::fs::create_dir_all(program.parent().unwrap()).unwrap();
    let at = codex_clock(7200);
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nstty -echo\nhere=$(dirname \"$0\")\nprintf '%s\\n' \"$*\" >> \"$here/args\"\nif [ $(wc -l < \"$here/args\") -eq 1 ]; then printf '• Ran cargo test\\n\\n■ You have hit your usage limit. Upgrade or try again at {at}.\\n\\n› Ask Codex to do anything\\n\\n  GPT-6-Astra medium · /tmp\\n'; else printf '› Ask Codex to do anything\\n'; fi\ncat > /dev/null\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("lim".into()),
            spawn: spawn(&[program.to_string_lossy().into_owned()]),
        },
    )
    .await;
    let pane = v["pane"]["id"].as_str().unwrap().to_string();
    req(
        &d,
        Request::RegisterAgent(RegisterAgent {
            kind: "codex".into(),
            pane: Some(format!("orch:{pane}")),
            name: Some("lim-codex".into()),
            cwd: None,
            instance: None,
        }),
    )
    .await
    .unwrap();
    wait_status(&d, &pane, "limited").await;
    shell_ws(&d, "quiet").await;
    assert!(orch_cli(&d, &["auto-sleep", "1s"]).status.success());
    for _ in 0..200 {
        if session(&d).await.sleeping("quiet").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let s = session(&d).await;
    assert!(s.sleeping("quiet").is_some(), "the idle one sleeps");
    assert!(s.workspace("lim").is_some(), "the limited one stays awake");
    assert!(orch_cli(&d, &["auto-sleep", "off"]).status.success());
    // Put away by hand, woken: the continue is due later, so it stays awake again...
    sleep_op(&d, "lim", false).await.unwrap();
    wake_op(&d, "lim", false).await.unwrap();
    assert!(orch_cli(&d, &["auto-sleep", "1s"]).status.success());
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(session(&d).await.workspace("lim").is_some());
    assert!(orch_cli(&d, &["auto-sleep", "off"]).status.success());
    // ...and the due continue goes through a planned restart.
    let out = orch_cli(&d, &["stop-daemon", "--restart"]);
    assert!(out.status.success(), "{out:?}");
    d.child.wait().unwrap();
    let saved = std::fs::read_to_string(d.dir.join("restore.json")).unwrap();
    assert!(saved.contains("continue_at"), "{saved}");
}

/// #9: a woken pane whose folder is gone keeps its record (not moved to the home folder).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f9_a_missing_folder_keeps_the_record() {
    let d = daemon("f9").await;
    let folder = d.dir.join("project");
    std::fs::create_dir_all(&folder).unwrap();
    let mut sp = spawn(&["bash".into(), "--norc".into()]);
    sp.cwd = Some(folder.to_string_lossy().into_owned());
    op(
        &d,
        UiOp::NewWorkspace {
            label: Some("gone".into()),
            spawn: sp,
        },
    )
    .await;
    sleep_op(&d, "gone", false).await.unwrap();
    std::fs::remove_dir_all(&folder).unwrap();
    let e = wake_op(&d, "gone", false).await.unwrap_err();
    assert_eq!(code(e), "wake_failed");
    assert!(session(&d).await.sleeping("gone").is_some());
}

/// #10: a sleeping agent's bus name is reserved; nobody else can take it meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f10_a_sleeping_agents_name_is_reserved() {
    let d = daemon("f10").await;
    proj(&d).await;
    let (_, other) = shell_ws(&d, "other").await;
    sleep_op(&d, "proj", false).await.unwrap();
    let e = req(
        &d,
        Request::RegisterAgent(RegisterAgent {
            kind: "claude".into(),
            pane: Some(format!("orch:{other}")),
            name: Some("proj-claude".into()),
            cwd: None,
            instance: None,
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(code(e), "name_reserved");
    wake_op(&d, "proj", false).await.unwrap();
    let v = req(&d, Request::ListAgents {}).await.unwrap();
    let a = v["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "proj-claude")
        .unwrap()
        .clone();
    assert_ne!(a["pane"], format!("orch:{other}"));
}

/// #11: ids win over labels across awake and sleeping workspaces.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f11_ids_win_over_labels_for_close_sleep_and_wake() {
    let d = daemon("f11").await;
    let (a_id, _) = shell_ws(&d, "alpha").await;
    // A workspace labelled like alpha's id, put to sleep.
    let (b_id, _) = shell_ws(&d, &a_id).await;
    sleep_op(&d, &b_id, false).await.unwrap();
    // Closing alpha by its id closes alpha, not the sleeper labelled that way.
    op(
        &d,
        UiOp::CloseWorkspace {
            workspace: a_id.clone(),
        },
    )
    .await;
    let s = session(&d).await;
    assert!(s.workspaces.iter().all(|w| w.id != a_id), "alpha closed");
    assert_eq!(s.sleeping.len(), 1, "the sleeper is untouched");
    // Its id still wakes it.
    wake_op(&d, &b_id, false).await.unwrap();
    assert!(session(&d).await.workspaces.iter().any(|w| w.id == b_id));
}

/// #12: no pane without a durable unique id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f12_no_pane_without_a_durable_id() {
    let mut d = daemon("f12").await;
    restart_with(&mut d, "pane_counter").await;
    let e = req(
        &d,
        Request::NewPane(NewPane {
            argv: vec!["bash".into(), "--norc".into()],
            ..Default::default()
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(code(e), "spawn_failed");
}

/// #13: a slow sleep (a pane that only goes with SIGKILL) holds up no other workspace's task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f13_a_slow_sleep_does_not_stall_other_tasks() {
    let d = daemon("f13").await;
    let (_, _, _) = proj(&d).await;
    op(
        &d,
        UiOp::NewWorkspace {
            label: Some("slow".into()),
            spawn: spawn(&[
                "sh".into(),
                "-c".into(),
                "trap '' HUP TERM; while :; do sleep 1; done".into(),
            ]),
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let socket = d.socket.clone();
    let sleeping = tokio::spawn(async move {
        let mut c = Conn::unix(&socket).await.unwrap();
        c.request(&Request::Ui(UiOp::SleepWorkspace {
            workspace: "slow".into(),
            force: true,
            dry_run: false,
        }))
        .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    req(
        &d,
        Request::TaskAdd {
            target: "proj".into(),
            text: "quick one".into(),
            created_by: "murat".into(),
            after: None,
            wake: false,
        },
    )
    .await
    .unwrap();
    wait_for("the other task runs", || {
        read(&d, "idle", "received").contains("quick one")
    })
    .await;
    assert!(!sleeping.is_finished(), "the slow sleep is still going");
    assert!(started.elapsed() < Duration::from_millis(2500));
    sleeping.await.unwrap().unwrap();
}

// ---------- re-check of the fixes (Codex, 2026-11-22) ----------

fn files_starting(d: &D, prefix: &str) -> Vec<PathBuf> {
    std::fs::read_dir(&d.dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix))
        })
        .collect()
}

async fn ping_ms(d: &D) -> u128 {
    let t = std::time::Instant::now();
    req(d, Request::Ping {}).await.unwrap();
    t.elapsed().as_millis()
}

/// #14: a restart whose panes can't be started (here: no durable pane id) keeps them in a file
/// of their own and brings them back at the next start; the layout is never consumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r14_a_failed_restore_keeps_the_layout_for_the_next_start() {
    let mut d = daemon("r14").await;
    shell_ws(&d, "one").await;
    shell_ws(&d, "two").await;
    // Planned restart into a daemon that can't allocate pane ids.
    let out = orch_cli(&d, &["stop-daemon", "--restart"]);
    assert!(out.status.success(), "{out:?}");
    d.child.wait().unwrap();
    restart_with(&mut d, "pane_counter").await;
    assert!(session(&d).await.workspaces.is_empty());
    assert!(d.dir.join("unrestored.json").exists(), "kept");
    assert!(!d.dir.join("restore.json").exists());
    assert!(!d.dir.join("restore.json.restoring").exists());
    // The next start (ids work again) brings them back.
    restart_with(&mut d, "").await;
    let s = session(&d).await;
    for l in ["one", "two"] {
        assert!(s.workspace(l).is_some(), "{l} is back");
    }
    assert!(!d.dir.join("unrestored.json").exists());
    // The same through an unplanned stop.
    restart_with(&mut d, "pane_counter").await;
    assert!(session(&d).await.workspaces.is_empty());
    restart_with(&mut d, "").await;
    let s = session(&d).await;
    assert!(s.workspace("one").is_some() && s.workspace("two").is_some());
}

/// #1: while a sleeper write is in flight (slow, then failing) no autosave shows the workspace
/// gone: killed during the write, the next start has it awake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r1_an_unconfirmed_sleep_never_reaches_the_awake_layout() {
    let mut d = daemon("r1a").await;
    proj(&d).await;
    restart_with(&mut d, "store_slow_ms=1500,store_write").await;
    let socket = d.socket.clone();
    let _bg = tokio::spawn(async move {
        let mut c = Conn::unix(&socket).await.unwrap();
        c.request(&Request::Ui(UiOp::SleepWorkspace {
            workspace: "proj".into(),
            force: false,
            dry_run: false,
        }))
        .await
    });
    // Several autosave intervals into the slow write, then an unplanned stop.
    tokio::time::sleep(Duration::from_millis(900)).await;
    let _ = d.child.kill();
    let _ = d.child.wait();
    restart_with(&mut d, "").await;
    let s = session(&d).await;
    assert!(s.workspace("proj").is_some(), "still awake");
    assert!(s.sleeping.is_empty());
}

/// #1: a write whose slot was renamed into place but whose folder could not be synced counts as
/// written (no compensating write that could itself fail): it is asleep, now and after a
/// restart, and never also awake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r1_a_rename_that_could_not_be_synced_counts_as_written() {
    let mut d = daemon("r1b").await;
    proj(&d).await;
    restart_with(&mut d, "store_after_rename_once").await;
    sleep_op(&d, "proj", false).await.unwrap();
    let s = session(&d).await;
    assert!(s.sleeping("proj").is_some() && s.workspace("proj").is_none());
    restart_with(&mut d, "").await;
    let s = session(&d).await;
    assert!(s.sleeping("proj").is_some(), "asleep");
    assert!(s.workspace("proj").is_none(), "not also awake");
}

/// #2: a planned restart that could not save its layout leaves orchd running with sleep and
/// wake allowed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r2_a_failed_planned_restart_does_not_block_sleep() {
    let mut d = daemon("r2").await;
    proj(&d).await;
    restart_with(&mut d, "layout_save").await;
    let out = orch_cli(&d, &["stop-daemon", "--restart"]);
    assert!(!out.status.success(), "{out:?}");
    sleep_op(&d, "proj", false).await.unwrap();
    wake_op(&d, "proj", false).await.unwrap();
}

/// #3: an older orchd may hand a sleeper's id to an unrelated workspace; the upgrade keeps both
/// (told apart by their panes) and renumbers the sleeper.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r3_a_reused_id_does_not_drop_an_awake_workspace() {
    let mut d = daemon("r3a").await;
    proj(&d).await;
    let sleeper = session(&d).await.workspace("proj").unwrap().id.clone();
    sleep_op(&d, "proj", false).await.unwrap();
    shell_ws(&d, "other").await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let _ = d.child.kill();
    let _ = d.child.wait();
    // What an older orchd could have written: "other" with the sleeper's id.
    let path = d.dir.join("autosave.json");
    let mut v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for w in v["session"]["workspaces"].as_array_mut().unwrap() {
        if w["label"] == "other" {
            w["id"] = Value::String(sleeper.clone());
        }
    }
    std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    restart_with(&mut d, "").await;
    let s = session(&d).await;
    assert!(s.workspace("other").is_some(), "the awake workspace stays");
    assert!(s.sleeping("proj").is_some(), "and the sleeper");
}

/// #3: sleepers found in an older layout file are taken over; that file goes only once the
/// record holding them is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r3_legacy_sleepers_keep_their_source_until_the_record_is_written() {
    let mut d = daemon("r3b").await;
    proj(&d).await;
    sleep_op(&d, "proj", false).await.unwrap();
    shell_ws(&d, "awake").await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let _ = d.child.kill();
    let _ = d.child.wait();
    // The first version of this branch kept sleepers in the layout file.
    let newest = ["sleeping.a.json", "sleeping.b.json"]
        .iter()
        .map(|n| d.dir.join(n))
        .filter(|p| p.exists())
        .max_by_key(|p| {
            serde_json::from_slice::<Value>(&std::fs::read(p).unwrap()).unwrap()["generation"]
                .as_u64()
                .unwrap()
        })
        .unwrap();
    let record: Value = serde_json::from_slice(&std::fs::read(&newest).unwrap()).unwrap();
    for n in ["sleeping.a.json", "sleeping.b.json"] {
        let _ = std::fs::remove_file(d.dir.join(n));
    }
    let path = d.dir.join("autosave.json");
    let mut v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    v["session"]["sleeping"] = record["sleeping"].clone();
    std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    // The record can't be written: the source is kept.
    restart_with(&mut d, "store_write").await;
    let kept = d.dir.join("autosave.json.failed");
    assert!(
        std::fs::read_to_string(&kept)
            .unwrap_or_default()
            .contains("\"sleeping\""),
        "the layout file with the sleepers is kept"
    );
    // Put back and started with a working store: taken over.
    let _ = d.child.kill();
    let _ = d.child.wait();
    std::fs::rename(&kept, &path).unwrap();
    restart_with(&mut d, "").await;
    assert!(session(&d).await.sleeping("proj").is_some());
    assert!(files_starting(&d, "sleeping.").iter().any(|p| p.exists()));
}

/// #4: a replacement that survives being ended during a failed wake stays visible and
/// blocks the next wake (no second copy).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r4_wake_rollback_survivors_block_a_second_copy() {
    let mut d = daemon("r4a").await;
    let mut a = spawn(&["bash".into(), "--norc".into()]);
    a.name = Some("a".into());
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("pair2".into()),
            spawn: a,
        },
    )
    .await;
    let first = v["pane"]["id"].as_str().unwrap().to_string();
    let mut b = spawn(&["bash".into(), "--norc".into()]);
    b.name = Some("b".into());
    op(
        &d,
        UiOp::Split {
            pane: first,
            dir: Dir::Right,
            spawn: b,
        },
    )
    .await;
    sleep_op(&d, "pair2", false).await.unwrap();
    restart_with(&mut d, "spawn_fail=a,kill_fail=b").await;
    let e = wake_op(&d, "pair2", false).await.unwrap_err();
    assert_eq!(code(e), "wake_failed");
    let s = session(&d).await;
    let stuck = s.workspace("pair2-stuck").expect("the survivor is visible");
    let survivor = stuck.tabs[0].root.panes()[0].clone();
    assert!(running_panes(&d).await.contains(&survivor));
    let e = wake_op(&d, "pair2", false).await.unwrap_err();
    assert_eq!(code(e), "ws_stuck");
}

/// #4: inside a shell wrapper the agent itself must be running; a shell that started something
/// else is not a started agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r4_a_wrapper_without_its_agent_did_not_start() {
    let d = daemon("r4b").await;
    let bin = d.dir.join("agentbin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    std::fs::write(&claude, FAKE).unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o700)).unwrap();
    let script = format!("{}; exec bash", claude.display());
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("wrapped".into()),
            spawn: spawn(&["bash".into(), "-c".into(), script]),
        },
    )
    .await;
    let pane = v["pane"]["id"].as_str().unwrap().to_string();
    register(&d, &pane, "wrapped-claude").await;
    wait_status(&d, &pane, "idle").await;
    sleep_op(&d, "wrapped", false).await.unwrap();
    // Its program now starts something else instead.
    std::fs::write(&claude, b"#!/bin/sh\nexec sleep 30\n").unwrap();
    let e = wake_op(&d, "wrapped", false).await.unwrap_err();
    assert_eq!(code(e), "wake_failed");
    assert!(session(&d).await.sleeping("wrapped").is_some());
}

/// #5: when moving the owners back fails too, the map stays in the record and the next start
/// moves them back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r5_a_failed_undo_keeps_its_map_for_the_next_start() {
    let mut d = daemon("r5").await;
    let (_ws, agent, _sh) = proj(&d).await;
    let t = req(
        &d,
        Request::TaskAdd {
            target: "proj".into(),
            text: "long job".into(),
            created_by: "murat".into(),
            after: None,
            wake: false,
        },
    )
    .await
    .unwrap();
    let task = t["id"].as_i64().unwrap();
    wait_for("the task runs", || {
        read(&d, "idle", "received").contains("long job")
    })
    .await;
    sleep_op(&d, "proj", true).await.unwrap();
    restart_with(&mut d, "owners_fail,owners_back_fail").await;
    assert!(wake_op(&d, "proj", false).await.is_err());
    assert!(
        session(&d).await.sleeping("proj").unwrap().wake.is_some(),
        "the map is kept"
    );
    restart_with(&mut d, "").await;
    let s = session(&d).await;
    assert!(
        s.sleeping("proj").unwrap().wake.is_none(),
        "undone at start"
    );
    let v = req(&d, Request::TaskList { all: true }).await.unwrap();
    let row = v["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == task)
        .unwrap()
        .clone();
    assert_eq!(
        row["pane"],
        agent.as_str(),
        "the task is back on its old pane"
    );
    wake_op(&d, "proj", false).await.unwrap();
}

/// #6: the panes stay reserved after the workspace left the layout, until they are ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r6_panes_stay_reserved_until_they_are_ended() {
    let mut d = daemon("r6").await;
    let (_ws, agent, _sh) = proj(&d).await;
    restart_with(&mut d, "sleep_before_kill_ms=1500").await;
    let agent_now = session(&d).await.workspace("proj").unwrap().tabs[0]
        .root
        .panes()[0]
        .clone();
    let _ = agent;
    let socket = d.socket.clone();
    let bg = tokio::spawn(async move {
        let mut c = Conn::unix(&socket).await.unwrap();
        c.request(&Request::Ui(UiOp::SleepWorkspace {
            workspace: "proj".into(),
            force: false,
            dry_run: false,
        }))
        .await
    });
    for _ in 0..200 {
        if session(&d).await.sleeping("proj").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        running_panes(&d).await.contains(&agent_now),
        "not ended yet"
    );
    let e = req(
        &d,
        Request::Send(orch_proto::api::SendPane {
            pane: agent_now.clone(),
            text: "x".into(),
            ..Default::default()
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(code(e), "ws_transition");
    bg.await.unwrap().unwrap();
}

/// #7: a question an agent asks while an automatic sleep is being decided keeps it awake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r7_a_question_during_an_auto_sleep_keeps_it_awake() {
    let mut d = daemon("r7").await;
    restart_with(&mut d, "auto_pause_ms=1500").await;
    proj(&d).await;
    assert!(orch_cli(&d, &["auto-sleep", "1s"]).status.success());
    tokio::time::sleep(Duration::from_millis(1600)).await;
    req(
        &d,
        Request::Ask {
            from: "proj-claude".into(),
            text: "still there?".into(),
            options: vec![],
        },
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(1800)).await;
    assert!(session(&d).await.workspace("proj").is_some(), "kept awake");
    assert!(orch_cli(&d, &["auto-sleep", "off"]).status.success());
}

/// #8: a usage-limit continue still due goes into the awake layout before the wake commits:
/// a crash right after the commit keeps it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r8_a_due_continue_survives_a_crash_after_the_wake_commit() {
    let mut d = daemon("r8").await;
    let program = d.dir.join("limited").join("fake-codex");
    std::fs::create_dir_all(program.parent().unwrap()).unwrap();
    let at = codex_clock(7200);
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nstty -echo\nhere=$(dirname \"$0\")\nprintf '%s\\n' \"$*\" >> \"$here/args\"\nif [ $(wc -l < \"$here/args\") -eq 1 ]; then printf '• Ran cargo test\\n\\n■ You have hit your usage limit. Upgrade or try again at {at}.\\n\\n› Ask Codex to do anything\\n\\n  GPT-6-Astra medium · /tmp\\n'; else printf '› Ask Codex to do anything\\n'; fi\ncat > /dev/null\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("lim".into()),
            spawn: spawn(&[program.to_string_lossy().into_owned()]),
        },
    )
    .await;
    let pane = v["pane"]["id"].as_str().unwrap().to_string();
    req(
        &d,
        Request::RegisterAgent(RegisterAgent {
            kind: "codex".into(),
            pane: Some(format!("orch:{pane}")),
            name: Some("lim-codex".into()),
            cwd: None,
            instance: None,
        }),
    )
    .await
    .unwrap();
    wait_status(&d, &pane, "limited").await;
    tokio::time::sleep(Duration::from_millis(400)).await; // the limits check saw it
    sleep_op(&d, "lim", false).await.unwrap();
    restart_with(&mut d, "wake_after_commit").await;
    let _ = wake_op(&d, "lim", false).await;
    wait_dead(&mut d);
    restart_with(&mut d, "").await;
    assert!(session(&d).await.workspace("lim").is_some());
    let out = orch_cli(&d, &["stop-daemon", "--restart"]);
    assert!(out.status.success(), "{out:?}");
    d.child.wait().unwrap();
    let saved = std::fs::read_to_string(d.dir.join("restore.json")).unwrap();
    assert!(saved.contains("continue_at"), "{saved}");
}

/// #10: during a two-agent wake, one agent's new pane can't take the other agent's name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r10_a_sibling_cannot_take_the_other_agents_name() {
    let mut d = daemon("r10").await;
    let a = fake(&d, "ra");
    let b = fake(&d, "rb");
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("two".into()),
            spawn: spawn(&[a]),
        },
    )
    .await;
    let pa = v["pane"]["id"].as_str().unwrap().to_string();
    register(&d, &pa, "two-a").await;
    let v = op(
        &d,
        UiOp::Split {
            pane: pa.clone(),
            dir: Dir::Right,
            spawn: spawn(&[b]),
        },
    )
    .await;
    let pb = v["pane"]["id"].as_str().unwrap().to_string();
    register(&d, &pb, "two-b").await;
    wait_status(&d, &pa, "idle").await;
    wait_status(&d, &pb, "idle").await;
    sleep_op(&d, "two", false).await.unwrap();
    restart_with(&mut d, "wake_pause_ms=1500").await;
    let socket = d.socket.clone();
    let bg = tokio::spawn(async move {
        let mut c = Conn::unix(&socket).await.unwrap();
        c.request(&Request::Ui(UiOp::WakeWorkspace {
            workspace: "two".into(),
            partial: false,
        }))
        .await
    });
    let mut map = None;
    for _ in 0..200 {
        if let Some(w) = session(&d)
            .await
            .sleeping("two")
            .and_then(|x| x.wake.clone())
        {
            map = Some(w.map);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let map = map.expect("the wake wrote its map");
    let new_a = map[&pa].clone();
    let e = req(
        &d,
        Request::RegisterAgent(RegisterAgent {
            kind: "claude".into(),
            pane: Some(format!("orch:{new_a}")),
            name: Some("two-b".into()),
            cwd: None,
            instance: None,
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(code(e), "name_reserved");
    bg.await.unwrap().unwrap();
}

/// #11: `task add --wake` wakes the workspace the task targets (by label), never another one
/// whose id equals that label.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r11_a_task_wake_resolves_the_targeted_workspace() {
    let d = daemon("r11").await;
    let (sleeper, _) = shell_ws(&d, "asleep").await;
    sleep_op(&d, "asleep", false).await.unwrap();
    // An awake workspace labelled like the sleeper's id.
    shell_ws(&d, &sleeper).await;
    let out = orch_cli(&d, &["task", "add", &sleeper, "--wake", "hello"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        session(&d).await.sleeping("asleep").is_some(),
        "the unrelated sleeper stays asleep"
    );
}

/// #13: slow sleeper-record writes run off the runtime: two sleeps writing slowly at once do
/// not stall a ping.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r13_slow_record_writes_do_not_stall_the_daemon() {
    let mut d = daemon("r13").await;
    shell_ws(&d, "s1").await;
    shell_ws(&d, "s2").await;
    restart_with(&mut d, "store_slow_ms=1500").await;
    let mut bg = Vec::new();
    for l in ["s1", "s2"] {
        let socket = d.socket.clone();
        bg.push(tokio::spawn(async move {
            let mut c = Conn::unix(&socket).await.unwrap();
            c.request(&Request::Ui(UiOp::SleepWorkspace {
                workspace: l.into(),
                force: false,
                dry_run: false,
            }))
            .await
        }));
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut worst = 0;
    for _ in 0..3 {
        worst = worst.max(ping_ms(&d).await);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(worst < 800, "a ping took {worst} ms");
    for j in bg {
        j.await.unwrap().unwrap();
    }
}

/// Store faults: a damaged slot is left alone and blocks writes (it may be the newest), so
/// nothing can overwrite the only good copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rstore_a_damaged_slot_blocks_writes() {
    let mut d = daemon("rstore").await;
    proj(&d).await;
    sleep_op(&d, "proj", false).await.unwrap();
    let _ = d.child.kill();
    let _ = d.child.wait();
    let slots: Vec<PathBuf> = ["sleeping.a.json", "sleeping.b.json"]
        .iter()
        .map(|n| d.dir.join(n))
        .collect();
    // Only one slot was ever written: damage it.
    let damaged = slots.iter().find(|p| p.exists()).unwrap().clone();
    std::fs::write(&damaged, b"garbage").unwrap();
    restart_with(&mut d, "").await;
    shell_ws(&d, "fresh").await;
    let e = sleep_op(&d, "fresh", false).await.unwrap_err();
    assert_eq!(code(e), "save_failed");
    assert_eq!(std::fs::read(&damaged).unwrap(), b"garbage", "left alone");
    assert!(
        slots.iter().filter(|p| p.exists()).count() == 1,
        "no slot written"
    );
}

// ---------- second re-check (Codex, 2026-11-22): one crash-safe reconcile by pane lineage ----------

fn labels(s: &Session) -> Vec<String> {
    let mut v: Vec<String> = s.workspaces.iter().map(|w| w.label.clone()).collect();
    v.sort();
    v
}

fn all_ids(s: &Session) -> Vec<String> {
    s.workspaces
        .iter()
        .chain(s.sleeping.iter().map(|x| &x.workspace))
        .map(|w| w.id.clone())
        .collect()
}

/// Two shell workspaces whose restart failed (no pane ids): they wait in unrestored.json.
async fn pending_retry(d: &mut D) {
    shell_ws(d, "one").await;
    shell_ws(d, "two").await;
    let out = orch_cli(d, &["stop-daemon", "--restart"]);
    assert!(out.status.success(), "{out:?}");
    d.child.wait().unwrap();
    restart_with(d, "pane_counter").await;
    assert!(d.dir.join("unrestored.json").exists());
}

/// #14: killed after a successful retry saved the layout but before the retry file was
/// rewritten: the next start finds the panes in both and starts each once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c14_a_crash_after_a_retry_never_starts_a_pane_twice() {
    let mut d = daemon("c14a").await;
    pending_retry(&mut d).await;
    start_crashing(&mut d, "after_restore_autosave").await;
    assert!(
        d.dir.join("unrestored.json").exists(),
        "the retry file is still there"
    );
    restart_with(&mut d, "").await;
    let s = session(&d).await;
    assert_eq!(
        labels(&s),
        vec!["one".to_string(), "two".to_string()],
        "once each"
    );
    assert!(!d.dir.join("unrestored.json").exists());
}

/// #14: a plain stop-daemon ends the session, also for panes still waiting to come back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c14_a_plain_stop_drops_panes_waiting_to_come_back() {
    let mut d = daemon("c14b").await;
    pending_retry(&mut d).await;
    let out = orch_cli(&d, &["stop-daemon"]);
    assert!(out.status.success(), "{out:?}");
    d.child.wait().unwrap();
    assert!(!d.dir.join("unrestored.json").exists());
    restart_with(&mut d, "").await;
    assert!(session(&d).await.workspaces.is_empty());
}

/// #14 / ids: the panes of a retry file bring their ids along; the live counter goes past them,
/// so a new workspace never gets one of those ids.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c14_retried_ids_raise_the_counter() {
    let mut d = daemon("c14c").await;
    pending_retry(&mut d).await;
    // Only the retry file: no layout file, no sleepers.
    let _ = d.child.kill();
    let _ = d.child.wait();
    for f in ["autosave.json", "restore.json"] {
        let _ = std::fs::remove_file(d.dir.join(f));
    }
    restart_with(&mut d, "").await;
    shell_ws(&d, "fresh").await;
    let s = session(&d).await;
    let mut ids = all_ids(&s);
    let n = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), n, "no workspace id twice: {:?}", all_ids(&s));
}

/// #5: killed after the woken layout was saved but before the wake committed: the sleeper owns
/// its panes (by lineage), so it is asleep and there is no awake copy as well.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c5_a_crash_before_the_wake_commit_leaves_one_copy() {
    let mut d = daemon("c5").await;
    proj(&d).await;
    sleep_op(&d, "proj", false).await.unwrap();
    restart_with(&mut d, "wake_after_layout").await;
    let _ = wake_op(&d, "proj", false).await;
    wait_dead(&mut d);
    restart_with(&mut d, "").await;
    let s = session(&d).await;
    assert!(s.sleeping("proj").is_some(), "asleep");
    assert!(s.workspaces.is_empty(), "no awake copy: {:?}", labels(&s));
    assert_eq!(
        read(&d, "idle", "args").lines().count(),
        2,
        "not started again"
    );
    wake_op(&d, "proj", false).await.unwrap();
    let s = session(&d).await;
    assert_eq!(labels(&s), vec!["proj".to_string()]);
    assert!(s.sleeping.is_empty());
}

/// #1: killed right after the sleeper slot was renamed into place (before it was published or
/// anything ended): the record is the authority, the workspace is asleep, never also awake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c1_a_crash_right_after_the_rename_keeps_one_copy() {
    let mut d = daemon("c1").await;
    proj(&d).await;
    restart_with(&mut d, "store_crash_after_rename").await;
    let _ = sleep_op(&d, "proj", false).await;
    wait_dead(&mut d);
    restart_with(&mut d, "").await;
    let s = session(&d).await;
    assert!(s.sleeping("proj").is_some());
    assert!(s.workspace("proj").is_none());
}

/// #4: in a partial wake, a replacement that did not start and survives being ended blocks the
/// remainder's wake (no second copy), through its lineage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c4_a_survivor_blocks_the_remainder() {
    let mut d = daemon("c4").await;
    let bin = d.dir.join("agentbin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    std::fs::write(&claude, FAKE).unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut wrapped = spawn(&[
        "bash".into(),
        "-c".into(),
        format!("{}; exec bash", claude.display()),
    ]);
    wrapped.name = Some("wr".into());
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("mix".into()),
            spawn: wrapped,
        },
    )
    .await;
    let pw = v["pane"]["id"].as_str().unwrap().to_string();
    register(&d, &pw, "mix-claude").await;
    wait_status(&d, &pw, "idle").await;
    op(
        &d,
        UiOp::Split {
            pane: pw.clone(),
            dir: Dir::Right,
            spawn: spawn(&["bash".into(), "--norc".into()]),
        },
    )
    .await;
    sleep_op(&d, "mix", false).await.unwrap();
    std::fs::write(&claude, b"#!/bin/sh\nexec sleep 30\n").unwrap();
    restart_with(&mut d, "kill_fail=wr").await;
    let v = wake_op(&d, "mix", true).await.unwrap();
    let rest = v["rest"].as_str().unwrap().to_string();
    let e = wake_op(&d, &rest, false).await.unwrap_err();
    assert_eq!(code(e), "ws_stuck");
}

/// #6: a send admitted before a sleep is protected until its Enter: the sleep waits for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c6_a_send_in_flight_finishes_before_a_sleep() {
    let mut d = daemon("c6").await;
    restart_with(&mut d, "send_pause_ms=1000").await;
    let (_, agent, _) = proj(&d).await;
    let socket = d.socket.clone();
    let a2 = agent.clone();
    let sending = tokio::spawn(async move {
        let mut c = Conn::unix(&socket).await.unwrap();
        c.request(&Request::Send(orch_proto::api::SendPane {
            pane: a2,
            text: "the whole line".into(),
            enter: true,
            ..Default::default()
        }))
        .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    sleep_op(&d, "proj", true).await.unwrap();
    sending.await.unwrap().unwrap();
    assert!(
        read(&d, "idle", "received").contains("the whole line"),
        "the Enter arrived before the pane ended"
    );
}

/// #7: an exemption set while an automatic sleep is being decided keeps the workspace awake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c7_keep_awake_during_an_auto_sleep_wins() {
    let mut d = daemon("c7").await;
    restart_with(&mut d, "auto_pause_ms=1500").await;
    shell_ws(&d, "quiet").await;
    assert!(orch_cli(&d, &["auto-sleep", "1s"]).status.success());
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let out = orch_cli(&d, &["ws", "keep-awake", "quiet", "on"]);
    assert!(out.status.success(), "{out:?}");
    tokio::time::sleep(Duration::from_millis(1800)).await;
    assert!(session(&d).await.workspace("quiet").is_some(), "kept awake");
    assert!(orch_cli(&d, &["auto-sleep", "off"]).status.success());
}

/// #8: a due continue of a pane that only came back through the retry file is kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c8_a_retried_pane_keeps_its_due_continue() {
    let mut d = daemon("c8").await;
    let program = d.dir.join("limited").join("fake-codex");
    std::fs::create_dir_all(program.parent().unwrap()).unwrap();
    let at = codex_clock(7200);
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nstty -echo\nhere=$(dirname \"$0\")\nprintf '%s\\n' \"$*\" >> \"$here/args\"\nif [ $(wc -l < \"$here/args\") -eq 1 ]; then printf '• Ran cargo test\\n\\n■ You have hit your usage limit. Upgrade or try again at {at}.\\n\\n› Ask Codex to do anything\\n\\n  GPT-6-Astra medium · /tmp\\n'; else printf '› Ask Codex to do anything\\n'; fi\ncat > /dev/null\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("lim".into()),
            spawn: spawn(&[program.to_string_lossy().into_owned()]),
        },
    )
    .await;
    let pane = v["pane"]["id"].as_str().unwrap().to_string();
    req(
        &d,
        Request::RegisterAgent(RegisterAgent {
            kind: "codex".into(),
            pane: Some(format!("orch:{pane}")),
            name: Some("lim-codex".into()),
            cwd: None,
            instance: None,
        }),
    )
    .await
    .unwrap();
    wait_status(&d, &pane, "limited").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    sleep_op(&d, "lim", false).await.unwrap();
    wake_op(&d, "lim", false).await.unwrap();
    // A restart that can't start it: kept for a retry, with its deadline.
    let out = orch_cli(&d, &["stop-daemon", "--restart"]);
    assert!(out.status.success(), "{out:?}");
    d.child.wait().unwrap();
    restart_with(&mut d, "pane_counter").await;
    assert!(std::fs::read_to_string(d.dir.join("unrestored.json"))
        .unwrap()
        .contains("continue_at"));
    // Back through the retry; the next planned restart still carries the deadline.
    restart_with(&mut d, "").await;
    assert!(session(&d).await.workspace("lim").is_some());
    let out = orch_cli(&d, &["stop-daemon", "--restart"]);
    assert!(out.status.success(), "{out:?}");
    d.child.wait().unwrap();
    let saved = std::fs::read_to_string(d.dir.join("restore.json")).unwrap();
    assert!(saved.contains("continue_at"), "{saved}");
}

/// #3: a malformed restore.json leaves the autosave for the start; every failed source is kept
/// under its own name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c3_a_bad_restore_file_keeps_the_autosave_and_earlier_failures() {
    let mut d = daemon("c3").await;
    shell_ws(&d, "kept").await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let _ = d.child.kill();
    let _ = d.child.wait();
    std::fs::write(d.dir.join("restore.json.failed"), b"an earlier one").unwrap();
    std::fs::write(d.dir.join("restore.json"), b"{ broken").unwrap();
    restart_with(&mut d, "").await;
    assert!(
        session(&d).await.workspace("kept").is_some(),
        "from the autosave"
    );
    assert_eq!(
        std::fs::read(d.dir.join("restore.json.failed")).unwrap(),
        b"an earlier one",
        "not overwritten"
    );
    assert_eq!(files_starting(&d, "restore.json.failed-").len(), 1);
}

/// Store: once the bad slot is moved away, writes go on without a restart; while blocked, a
/// wake starts nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cstore_a_blocked_store_recovers_once_the_bad_slot_is_moved() {
    let mut d = daemon("cstore").await;
    proj(&d).await;
    sleep_op(&d, "proj", false).await.unwrap();
    let _ = d.child.kill();
    let _ = d.child.wait();
    let bad = d.dir.join("sleeping.a.json");
    std::fs::write(&bad, b"garbage").unwrap();
    restart_with(&mut d, "").await;
    let before = running_panes(&d).await.len();
    let e = wake_op(&d, "proj", false).await.unwrap_err();
    assert_eq!(code(e), "save_failed");
    assert_eq!(running_panes(&d).await.len(), before, "nothing started");
    std::fs::rename(&bad, d.dir.join("moved-away.json")).unwrap();
    wake_op(&d, "proj", false).await.unwrap();
}

/// Ids allocated during a commit: the remainder of a partial wake gets ids from the live
/// counter, so workspaces created while its record is written never share them, and what is
/// live is what was written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cid_commit_ids_are_published_as_written() {
    let mut d = daemon("cid").await;
    let mut a = spawn(&["bash".into(), "--norc".into()]);
    a.name = Some("a".into());
    let v = op(
        &d,
        UiOp::NewWorkspace {
            label: Some("pair3".into()),
            spawn: a,
        },
    )
    .await;
    let first = v["pane"]["id"].as_str().unwrap().to_string();
    let mut b = spawn(&["bash".into(), "--norc".into()]);
    b.name = Some("b".into());
    op(
        &d,
        UiOp::Split {
            pane: first,
            dir: Dir::Right,
            spawn: b,
        },
    )
    .await;
    sleep_op(&d, "pair3", false).await.unwrap();
    restart_with(&mut d, "spawn_fail=a,store_slow_ms=700").await;
    let socket = d.socket.clone();
    let waking = tokio::spawn(async move {
        let mut c = Conn::unix(&socket).await.unwrap();
        c.request(&Request::Ui(UiOp::WakeWorkspace {
            workspace: "pair3".into(),
            partial: true,
        }))
        .await
    });
    for i in 0..6 {
        shell_ws(&d, &format!("busy{i}")).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    waking.await.unwrap().unwrap();
    let s = session(&d).await;
    let mut ids = all_ids(&s);
    let n = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), n, "no id twice: {:?}", all_ids(&s));
    let rest = s
        .sleeping
        .iter()
        .find(|x| x.workspace.label.starts_with("pair3-rest"))
        .expect("the remainder")
        .workspace
        .id
        .clone();
    // What is on disk is what is live.
    restart_with(&mut d, "").await;
    assert!(
        session(&d).await.sleeping(&rest).is_some(),
        "the same id after a restart"
    );
}
