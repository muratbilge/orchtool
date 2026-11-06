//! Restart continuation, including the agent which invokes stop-daemon itself.
use orch_proto::api::{NewPane, RegisterAgent, Request};
use orch_proto::client::Conn;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

struct Private {
    child: Child,
    dir: PathBuf,
    socket: PathBuf,
}
impl Drop for Private {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
fn start(dir: &Path, socket: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_orchd"))
        .arg("--socket")
        .arg(socket)
        .arg("--db")
        .arg(dir.join("bus.db"))
        .env("HOME", dir.join("home"))
        .env_remove("ORCH_HOST")
        .spawn()
        .unwrap()
}
async fn connect(socket: &Path) -> Conn {
    for _ in 0..200 {
        if let Ok(mut c) = Conn::unix(socket).await {
            if c.request(&Request::Ping {}).await.is_ok() {
                return c;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("private daemon did not start");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn working_agents_including_the_restart_caller_continue_once() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!(
        "orch-restart-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let socket = dir.join("s.sock");
    let mut d = Private {
        child: start(&dir, &socket),
        dir,
        socket,
    };
    let mut c = connect(&d.socket).await;
    let script = r#"#!/bin/sh
stty -echo
# Settings come from the script's own path, not arguments: a restart resumes the agent without
# its positional arguments (they are prompts for a real agent).
here=$(dirname "$0"); top=$(dirname "$here"); role=$(basename "$here")
marker=$top/$role.started; receipts=$top/$role.received; trigger=$top/stop-now
socket=$(cat "$top/socket-path"); orch=$(cat "$top/orch-path")
initial=no
if [ ! -f "$marker" ]; then initial=yes; : > "$marker"; fi
case "$role" in
 blocked) printf 'Do you want to proceed?\n⏵⏵ auto mode on\n';;
 limited) printf 'You have hit your usage limit\n⏵⏵ auto mode on\n';;
 shell) printf '$ ready\n';;
 *) if [ "$initial" = yes ] && [ "$role" != idle ]; then
        printf 'Thinking (esc to interrupt)\n⏵⏵ auto mode on\n'
    else
        printf 'Done\n⏵⏵ auto mode on (shift+tab to cycle)\n'
    fi;;
esac
if [ "$role" = self ] && [ "$initial" = yes ]; then
    while [ ! -f "$trigger" ]; do sleep 0.05; done
    unset ORCH_HOST
    "$orch" --socket "$socket" stop-daemon --restart
fi
while IFS= read -r line; do printf '%s\n' "$line" >> "$receipts"; done
"#;
    let trigger = d.dir.join("stop-now");
    let orch = Path::new(env!("CARGO_BIN_EXE_orchd")).with_file_name("orch");
    std::fs::write(
        d.dir.join("socket-path"),
        d.socket.to_string_lossy().as_bytes(),
    )
    .unwrap();
    std::fs::write(d.dir.join("orch-path"), orch.to_string_lossy().as_bytes()).unwrap();
    let mut ids = Vec::new();
    for role in [
        "working", "self", "idle", "blocked", "limited", "asking", "shell",
    ] {
        let program = d.dir.join(role).join(if role == "shell" {
            "fake-program"
        } else {
            "fake-claude"
        });
        std::fs::create_dir_all(program.parent().unwrap()).unwrap();
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let argv = vec![program.to_string_lossy().into_owned()];
        let v = c
            .request(&Request::NewPane(NewPane {
                argv,
                name: Some(role.into()),
                rows: Some(20),
                cols: Some(120),
                ..Default::default()
            }))
            .await
            .unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        if role != "shell" {
            c.request(&Request::RegisterAgent(RegisterAgent {
                kind: "claude".into(),
                pane: Some(format!("orch:{id}")),
                name: Some(role.into()),
                cwd: None,
                instance: None,
            }))
            .await
            .unwrap();
        }
        ids.push((role, id));
    }
    c.request(&Request::Ask {
        from: "asking".into(),
        text: "Wait for Murat?".into(),
        options: vec![],
    })
    .await
    .unwrap();
    let mut ready = false;
    for _ in 0..200 {
        let v = c.request(&Request::ListPanes {}).await.unwrap();
        ready = ids.iter().all(|(role, id)| {
            v["panes"].as_array().unwrap().iter().any(|p| {
                p["id"] == *id
                    && match *role {
                        "working" | "self" | "asking" => p["agent_status"] == "working",
                        "idle" => p["agent_status"] == "idle",
                        "blocked" => p["agent_status"] == "blocked",
                        "limited" => p["agent_status"] == "limited",
                        _ => p["agent"].is_null(),
                    }
            })
        });
        if ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(ready, "fake agents never reached their initial screens");
    // This pane invokes the real CLI while its screen still says Working.
    std::fs::write(&trigger, "go").unwrap();
    for _ in 0..200 {
        if d.child.try_wait().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        d.child.try_wait().unwrap().is_some(),
        "restart caller failed to stop private orchd"
    );
    let saved: orchd::restore::Saved =
        serde_json::from_slice(&std::fs::read(d.dir.join("restore.json")).unwrap()).unwrap();
    for (role, id) in &ids {
        assert_eq!(
            saved.panes[id].was_working,
            matches!(*role, "working" | "self"),
            "{role}"
        );
    }
    d.child = start(&d.dir, &d.socket);
    let mut c = connect(&d.socket).await;
    let mut summary = None;
    for _ in 0..200 {
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
    let summary = summary.expect("restart summary missing");
    assert_eq!(summary["to"], "murat");
    assert_eq!(
        summary["text"],
        "restart: resumed 6 agents, nudged 2 that were working (self, working)"
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    for (role, _) in &ids {
        let received =
            std::fs::read_to_string(d.dir.join(format!("{role}.received"))).unwrap_or_default();
        if matches!(*role, "working" | "self") {
            assert_eq!(
                received,
                format!("{}\n", orchd::restore::RESTART_NOTE),
                "{role}"
            );
        } else {
            assert!(received.is_empty(), "{role} was nudged: {received}");
        }
    }
    assert!(!d.dir.join("restore.json").exists());
}
