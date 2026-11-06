//! Private daemon, real CLI, fake terminal agent; never uses the user's daemon or HOME.
use orch_proto::api::{RegisterAgent, Request, Spawn, TaskRow, UiOp};
use orch_proto::client::Conn;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

struct Private {
    dir: PathBuf,
    child: Child,
}
impl Drop for Private {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
fn start(dir: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_orchd"))
        .args([
            "--socket",
            dir.join("s.sock").to_str().unwrap(),
            "--db",
            dir.join("db").to_str().unwrap(),
        ])
        .env("HOME", dir.join("home"))
        .env_remove("ORCH_HOST")
        .env("ORCHD_TASK_IDLE_MS", "300")
        .spawn()
        .unwrap()
}
async fn connect(dir: &Path) -> Conn {
    for _ in 0..150 {
        if let Ok(mut c) = Conn::unix(&dir.join("s.sock")).await {
            if c.request(&Request::Ping {}).await.is_ok() {
                return c;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("private task daemon did not start");
}
async fn list(c: &mut Conn) -> Vec<TaskRow> {
    serde_json::from_value(
        c.request(&Request::TaskList { all: true }).await.unwrap()["tasks"].clone(),
    )
    .unwrap()
}
async fn add(c: &mut Conn, text: &str, after: Option<i64>) -> i64 {
    c.request(&Request::TaskAdd {
        target: "queue".into(),
        text: text.into(),
        created_by: "test".into(),
        after,
    })
    .await
    .unwrap()["id"]
        .as_i64()
        .unwrap()
}
async fn wait(c: &mut Conn, id: i64, state: &str) -> TaskRow {
    for _ in 0..200 {
        let t = list(c).await.into_iter().find(|t| t.id == id).unwrap();
        if t.state == state {
            return t;
        }
        assert_ne!(t.state, "failed", "unexpected failure: {t:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("task {id} never became {state}: {:?}", list(c).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_orders_dependencies_cancel_and_restart_resolution() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!(
        "orch-tasks-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let script = dir.join("fake-agent");
    // Fixed filenames relative to the pane cwd survive restore without environment leakage.
    std::fs::write(
        &script,
        r#"#!/bin/sh
stty -echo
prompt() { printf '\033[2J\033[H› Ask Codex to do anything\n? for shortcuts\n'; }
printf 'Working (esc to interrupt)\n› Ask Codex\n'
while [ ! -f ready ]; do sleep 0.05; done
prompt
while IFS= read -r line; do
    printf 'start:%s\n' "$line" >> received
    printf '\033[2J\033[HWorking (esc to interrupt)\n› Ask Codex\n'
    sleep 0.6
    printf 'end:%s\n' "$line" >> received
    prompt
done
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut d = Private {
        child: start(&dir),
        dir,
    };
    let mut c = connect(&d.dir).await;
    let v = c
        .request(&Request::Ui(UiOp::NewWorkspace {
            label: Some("queue".into()),
            spawn: Spawn {
                argv: vec![script.to_string_lossy().into_owned()],
                cwd: Some(d.dir.to_string_lossy().into_owned()),
                ..Default::default()
            },
        }))
        .await
        .unwrap();
    let pane = v["pane"]["id"].as_str().unwrap().to_string();
    let first = add(&mut c, "first", None).await;
    let second = add(&mut c, "second", Some(first)).await;
    let cancelled = add(&mut c, "never", None).await;
    c.request(&Request::TaskCancel { id: cancelled })
        .await
        .unwrap();
    let blocked = add(&mut c, "blocked", Some(cancelled)).await;
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert!(
        !d.dir.join("received").exists(),
        "nothing sent while working"
    );
    std::fs::write(d.dir.join("ready"), "").unwrap();
    wait(&mut c, second, "done").await;
    assert_eq!(
        std::fs::read_to_string(d.dir.join("received")).unwrap(),
        "start:first\nend:first\nstart:second\nend:second\n"
    );
    assert_eq!(
        list(&mut c)
            .await
            .into_iter()
            .find(|t| t.id == blocked)
            .unwrap()
            .state,
        "queued"
    );

    // MCP registration disables heuristic completion; explicit done is needed, including
    // after restart. Its running reservation and the queued dependent survive with new pane IDs.
    c.request(&Request::RegisterAgent(RegisterAgent {
        kind: "codex".into(),
        pane: Some(format!("orch:{pane}")),
        instance: Some("mcp:test".into()),
        ..Default::default()
    }))
    .await
    .unwrap();
    let explicit = add(&mut c, "explicit", None).await;
    wait(&mut c, explicit, "running").await;
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert_eq!(
        list(&mut c)
            .await
            .into_iter()
            .find(|t| t.id == explicit)
            .unwrap()
            .state,
        "running"
    );
    let following = add(&mut c, "following", Some(explicit)).await;
    c.request(&Request::StopDaemon { restart: true })
        .await
        .unwrap();
    d.child.wait().unwrap();
    d.child = start(&d.dir);
    c = connect(&d.dir).await;
    let old = wait(&mut c, explicit, "running").await;
    assert_ne!(old.pane.as_deref(), Some(pane.as_str()));
    c.request(&Request::TaskDone {
        id: None,
        pane: old.pane,
    })
    .await
    .unwrap();
    let new = wait(&mut c, following, "running").await;
    assert_ne!(new.pane.as_deref(), Some(pane.as_str()));
    c.request(&Request::TaskDone {
        id: Some(following),
        pane: None,
    })
    .await
    .unwrap();
    let cli = Path::new(env!("CARGO_BIN_EXE_orchd")).with_file_name("orch");
    let out = Command::new(cli)
        .arg("--socket")
        .arg(d.dir.join("s.sock"))
        .args(["task", "done", "99999"])
        .env("HOME", d.dir.join("home"))
        .env_remove("ORCH_HOST")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(14));
    for _ in 0..100 {
        if std::fs::read_to_string(d.dir.join("received"))
            .unwrap_or_default()
            .matches("start:[orch task #")
            .count()
            == 2
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let log = std::fs::read_to_string(d.dir.join("received")).unwrap();
    assert_eq!(
        log.matches("start:[orch task #").count(),
        2,
        "running work was not replayed: {log}"
    );
}
