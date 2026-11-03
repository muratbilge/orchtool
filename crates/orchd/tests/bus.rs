//! End-to-end bus tests: real orchd, real orch-mcp / orch binaries.

use orch_proto::api::{QuestionRow, Request};
use orch_proto::client::Conn;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn bin(name: &str) -> PathBuf {
    let p = Path::new(env!("CARGO_BIN_EXE_orchd")).with_file_name(name);
    assert!(p.exists(), "{name} not built; run `cargo test --workspace`");
    p
}

struct Env {
    dir: PathBuf,
    socket: PathBuf,
    daemon: Option<Child>,
}

impl Env {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "orch-bus-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s.sock");
        Self {
            dir,
            socket,
            daemon: None,
        }
    }

    /// Start orchd with its own db; HOME points into the temp dir so no real config is read.
    async fn start(&mut self) {
        let mut cmd = Command::new(bin("orchd"));
        cmd.arg("--socket")
            .arg(&self.socket)
            .arg("--db")
            .arg(self.dir.join("bus.db"))
            .env("HOME", &self.dir);
        self.daemon = Some(cmd.spawn().unwrap());
        for _ in 0..100 {
            if Conn::unix(&self.socket).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("orchd did not start");
    }

    fn stop(&mut self) {
        if let Some(mut d) = self.daemon.take() {
            let _ = d.kill();
            let _ = d.wait();
        }
    }

    async fn conn(&self) -> Conn {
        Conn::unix(&self.socket).await.unwrap()
    }

    fn mcp(&self, agent: &str, client: &str) -> Mcp {
        let mut child = Command::new(bin("orch-mcp"))
            .env("ORCHD_SOCKET", &self.socket)
            .env("ORCH_AGENT", agent)
            .env("HOME", &self.dir)
            .env_remove("ORCH_PANE")
            .env_remove("HERDR_PANE_ID")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut m = Mcp {
            child,
            stdin,
            lines: Arc::new(Mutex::new(stdout)),
            next: 1,
        };
        let init = m.call(
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": { "name": client, "version": "1" } }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "orch");
        m.notify("notifications/initialized", json!({}));
        m
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    lines: Arc<Mutex<BufReader<std::process::ChildStdout>>>,
    next: i64,
}

impl Mcp {
    fn notify(&mut self, method: &str, params: Value) {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        writeln!(self.stdin, "{msg}").unwrap();
    }

    fn send(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next;
        self.next += 1;
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(self.stdin, "{msg}").unwrap();
        id
    }

    fn recv(&self, id: i64) -> Value {
        let mut lines = self.lines.lock().unwrap();
        loop {
            let mut line = String::new();
            assert!(
                lines.read_line(&mut line).unwrap() > 0,
                "orch-mcp closed stdout"
            );
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == id {
                return v;
            }
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        self.recv(id)
    }

    fn tool(&mut self, name: &str, args: Value) -> String {
        let v = self.call("tools/call", json!({ "name": name, "arguments": args }));
        assert!(v["result"]["isError"] != true, "tool {name} failed: {v}");
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn open_questions(env: &Env) -> Vec<QuestionRow> {
    let v = env
        .conn()
        .await
        .request(&Request::ListQuestions { all: false })
        .await
        .unwrap();
    serde_json::from_value(v["questions"].clone()).unwrap()
}

async fn answer(env: &Env, id: i64, text: &str, by: &str) -> Value {
    env.conn()
        .await
        .request(&Request::Answer {
            id,
            answer: text.into(),
            by: by.into(),
        })
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_agents_talk_and_one_asks_murat() {
    let mut env = Env::new("talk");
    env.start().await;
    let mut alice = env.mcp("alice", "claude-code");
    let mut bob = env.mcp("bob", "codex-mcp-client");

    // Both joined the bus at initialize, before any tool call.
    let mut names = Vec::new();
    for _ in 0..50 {
        let v = env
            .conn()
            .await
            .request(&Request::ListAgents {})
            .await
            .unwrap();
        names = v["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        if names.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert_eq!(names, vec!["alice", "bob"]);

    let tools = alice.call("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for t in [
        "send_message",
        "read_inbox",
        "ask_human",
        "wait_for_answer",
        "post_status",
        "list_agents",
    ] {
        assert!(names.contains(&t), "missing tool {t}");
    }

    // Both register on their first call.
    assert!(bob.tool("list_agents", json!({})).contains("You are bob"));
    assert!(alice
        .tool(
            "send_message",
            json!({ "to": "bob", "text": "please review PR 7" })
        )
        .starts_with("Sent"));
    let inbox = bob.tool("read_inbox", json!({}));
    assert!(inbox.contains("from alice: please review PR 7"), "{inbox}");
    assert_eq!(bob.tool("read_inbox", json!({})), "No new messages.");

    // bob waits for a message that arrives while it waits.
    let wait_id = bob.send(
        "tools/call",
        json!({ "name": "read_inbox", "arguments": { "wait_seconds": 10 } }),
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    alice.tool(
        "send_message",
        json!({ "to": "bob", "text": "ping while waiting" }),
    );
    let lines = bob.lines.clone();
    let got = tokio::task::spawn_blocking(move || mcp_recv(&lines, wait_id))
        .await
        .unwrap();
    assert!(got.contains("ping while waiting"), "{got}");

    // alice asks Murat; the answer arrives while the tool call blocks.
    let ask_id = alice.send(
        "tools/call",
        json!({ "name": "ask_human", "arguments": { "question": "Merge now?", "options": ["yes", "no"], "wait_seconds": 10 } }),
    );
    let mut qs = Vec::new();
    for _ in 0..50 {
        qs = open_questions(&env).await;
        if !qs.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(qs.len(), 1);
    assert_eq!(qs[0].from, "alice");
    assert_eq!(qs[0].options, vec!["yes", "no"]);
    let first = answer(&env, qs[0].id, "yes", "cli").await;
    assert_eq!(first["won"], true);
    let second = answer(&env, qs[0].id, "no", "telegram").await;
    assert_eq!(second["won"], false, "second answer must not win");
    let lines = alice.lines.clone();
    let got = tokio::task::spawn_blocking(move || mcp_recv(&lines, ask_id))
        .await
        .unwrap();
    assert!(
        got.contains("Murat answered question") && got.contains(": yes"),
        "{got}"
    );

    // The timeline has everything, in order.
    let v = env
        .conn()
        .await
        .request(&Request::Log {
            after: None,
            limit: Some(50),
            follow: false,
        })
        .await
        .unwrap();
    let kinds: Vec<&str> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["msg", "msg", "ask", "answer"]);
}

fn mcp_recv(lines: &Arc<Mutex<BufReader<std::process::ChildStdout>>>, id: i64) -> String {
    let mut lines = lines.lock().unwrap();
    loop {
        let mut line = String::new();
        assert!(lines.read_line(&mut line).unwrap() > 0);
        let v: Value = serde_json::from_str(&line).unwrap();
        if v["id"] == id {
            return v["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .to_string();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn questions_survive_mcp_and_daemon_restarts() {
    let mut env = Env::new("resume");
    env.start().await;
    let mut a = env.mcp("worker", "codex-mcp-client");
    let r = a.tool(
        "ask_human",
        json!({ "question": "Which DB?", "wait_seconds": 0 }),
    );
    assert!(r.contains("Not answered yet"), "{r}");
    let id: i64 = r
        .split("question #")
        .nth(1)
        .unwrap()
        .split(')')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    drop(a); // the agent's MCP process dies
    env.stop(); // and orchd restarts
    env.start().await;
    assert_eq!(open_questions(&env).await.len(), 1, "question persisted");
    answer(&env, id, "sqlite", "cli").await;
    let mut a = env.mcp("worker", "codex-mcp-client");
    let r = a.tool(
        "wait_for_answer",
        json!({ "question_id": id, "wait_seconds": 1 }),
    );
    assert!(r.contains(": sqlite"), "{r}");
    // Delivered exactly once: wait_for_answer consumed the answer message, so it does not
    // turn up again in the inbox.
    assert_eq!(a.tool("read_inbox", json!({})), "No new messages.");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_call_cancellation_and_unknown_method() {
    let mut env = Env::new("cancel");
    env.start().await;
    let mut a = env.mcp("canceller", "claude-code");
    let id = a.send(
        "tools/call",
        json!({ "name": "read_inbox", "arguments": { "wait_seconds": 30 } }),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    a.notify("notifications/cancelled", json!({ "requestId": id }));
    // After cancelling, other calls still work promptly.
    let v = a.call("bogus/method", json!({}));
    assert_eq!(v["error"]["code"], -32601);
    assert!(a.tool("list_agents", json!({})).contains("canceller"));
}

fn run_hook(env: &Env, agent: &str, event: &str) -> String {
    let mut child = Command::new(bin("orch"))
        .args(["--socket"])
        .arg(&env.socket)
        .arg("hook")
        .env("ORCH_AGENT", agent)
        .env("HOME", &env.dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let ev = json!({ "hook_event_name": event, "session_id": "s1", "stop_hook_active": false });
    writeln!(child.stdin.take().unwrap(), "{ev}").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_stop_hook_hands_over_unread_messages() {
    let mut env = Env::new("hook");
    env.start().await;
    assert_eq!(run_hook(&env, "carol", "SessionStart"), "");
    let mut c = env.conn().await;
    let agents = c.request(&Request::ListAgents {}).await.unwrap();
    assert_eq!(agents["agents"][0]["status"], "idle");
    run_hook(&env, "carol", "UserPromptSubmit");
    c.request(&Request::SendMessage(orch_proto::api::SendMessage {
        from: "murat".into(),
        to: "carol".into(),
        text: "also update the changelog".into(),
        thread: None,
    }))
    .await
    .unwrap();
    let out = run_hook(&env, "carol", "Stop");
    let v: Value = serde_json::from_str(out.trim()).expect("block decision JSON");
    assert_eq!(v["decision"], "block");
    assert!(v["reason"]
        .as_str()
        .unwrap()
        .contains("also update the changelog"));
    // Delivered once: the next Stop lets Claude stop and marks it idle.
    assert_eq!(run_hook(&env, "carol", "Stop"), "");
    let agents = c.request(&Request::ListAgents {}).await.unwrap();
    assert_eq!(agents["agents"][0]["status"], "idle");
    // No daemon → hook stays silent and succeeds.
    env.stop();
    assert_eq!(run_hook(&env, "carol", "Stop"), "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_reads_and_retried_asks_lose_and_duplicate_nothing() {
    let mut env = Env::new("cancel2");
    env.start().await;
    let mut a = env.mcp("reader", "claude-code");
    let mut b = env.mcp("writer", "claude-code");
    b.tool("list_agents", json!({}));
    // A read_inbox wait is cancelled; a message arriving afterwards is not consumed by it.
    let id = a.send(
        "tools/call",
        json!({ "name": "read_inbox", "arguments": { "wait_seconds": 30 } }),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    a.notify("notifications/cancelled", json!({ "requestId": id }));
    tokio::time::sleep(Duration::from_millis(200)).await;
    b.tool(
        "send_message",
        json!({ "to": "reader", "text": "arrived after cancel" }),
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let inbox = a.tool("read_inbox", json!({}));
    assert!(inbox.contains("arrived after cancel"), "{inbox}");

    // ask_human cancelled before it returned; asking again does not duplicate the question.
    let id = a.send("tools/call", json!({ "name": "ask_human", "arguments": { "question": "Retry me?", "wait_seconds": 30 } }));
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.notify("notifications/cancelled", json!({ "requestId": id }));
    let r = a.tool(
        "ask_human",
        json!({ "question": "Retry me?", "wait_seconds": 0 }),
    );
    assert!(r.contains("Not answered yet"), "{r}");
    assert_eq!(open_questions(&env).await.len(), 1, "no duplicate question");
}

async fn register_in_pane(env: &Env, name: &str, kind: &str, pane: &str) {
    env.conn()
        .await
        .request(&Request::RegisterAgent(orch_proto::api::RegisterAgent {
            kind: kind.into(),
            pane: Some(format!("orch:{pane}")),
            cwd: Some("/tmp".into()),
            name: Some(name.into()),
            instance: None,
        }))
        .await
        .unwrap();
}

async fn new_pane(env: &Env, argv: &[&str]) -> String {
    let v = env
        .conn()
        .await
        .request(&Request::NewPane(orch_proto::api::NewPane {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: Some("/tmp".into()),
            rows: Some(20),
            cols: Some(100),
            ..Default::default()
        }))
        .await
        .unwrap();
    v["id"].as_str().unwrap().to_string()
}

async fn pane_text(env: &Env, pane: &str) -> String {
    let v = env
        .conn()
        .await
        .request(&Request::Read(orch_proto::api::ReadPane {
            pane: pane.into(),
            lines: Some(100),
            source: orch_proto::api::ReadSource::Recent,
        }))
        .await
        .unwrap();
    v["text"].as_str().unwrap().to_string()
}

async fn msg(env: &Env, to: &str, text: &str) {
    env.conn()
        .await
        .request(&Request::SendMessage(orch_proto::api::SendMessage {
            from: "murat".into(),
            to: to.into(),
            text: text.into(),
            thread: None,
        }))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nudges_only_reach_idle_panes() {
    let mut env = Env::new("nudge");
    env.start().await;
    // An "agent" pane showing an agent's idle prompt footer, then echoing input (cat).
    let idle = new_pane(
        &env,
        &["sh", "-c", "printf '› \\n  ? for shortcuts\\n'; exec cat"],
    )
    .await;
    register_in_pane(&env, "idler", "codex", &idle).await;
    msg(&env, "idler", "wake up").await;
    let mut ok = false;
    for _ in 0..40 {
        if pane_text(&env, &idle)
            .await
            .contains("[orch] new message from murat")
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ok, "idle agent got the notice");

    // A pane showing an approval dialog is never typed into.
    let blocked = new_pane(
        &env,
        &["sh", "-c", "printf 'Allow the orch MCP server to run tool?\\n  enter to submit | esc to cancel\\n'; exec cat"],
    )
    .await;
    register_in_pane(&env, "asker", "codex", &blocked).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    msg(&env, "asker", "do not type into my dialog").await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        !pane_text(&env, &blocked).await.contains("[orch]"),
        "nothing typed into a dialog"
    );

    // An agent whose bus status says working is not nudged either.
    let busy = new_pane(
        &env,
        &["sh", "-c", "printf '› \\n  ? for shortcuts\\n'; exec cat"],
    )
    .await;
    register_in_pane(&env, "busy", "codex", &busy).await;
    env.conn()
        .await
        .request(&Request::SetStatus {
            name: "busy".into(),
            status: "working".into(),
            text: None,
        })
        .await
        .unwrap();
    msg(&env, "busy", "later").await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        !pane_text(&env, &busy).await.contains("[orch]"),
        "working status respected"
    );
}
