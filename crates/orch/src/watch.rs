//! `orch watch --json`: one JSON line per change, for Kabuk (the phone) over an SSH exec channel.
//!
//! Events are only hints: after each burst the watcher reads the authoritative state (agents,
//! panes, layout, open questions, timeline tail) and prints it only when it differs from the last
//! line. A line is also forced every 60 s so the phone knows the stream is alive. When orchd is
//! not reachable it prints `{"error": "..."}` and retries every 5 s. Stdin EOF (the phone hung up)
//! ends it at once.
//!
//! The line is a versioned contract (notes/kabuk-contract-v2.md): `"v"` bumps only when a field
//! is removed, renamed or retyped, or an `orch` exit code changes — never on an addition; the
//! phone ignores fields it does not know. `"orch"` is this binary's version, for diagnostics.
//! `tests/fixtures/watch-v1.json` is a real line of the shape Kabuk's tests load too; the test
//! below keeps every key of it in what `compose` builds.

use anyhow::Result;
use orch_proto::api::{AgentRow, MessageRow, PaneInfo, PaneState, QuestionRow, Request, HUMAN};
use orch_proto::client::Conn;
use orch_proto::layout::Session;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

const COALESCE: Duration = Duration::from_millis(150);
const REFRESH: Duration = Duration::from_secs(15);
const HEARTBEAT: Duration = Duration::from_secs(60);
const RETRY: Duration = Duration::from_secs(5);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
/// Agents without a live pane disappear from the phone after this long without a sign of life.
const STALE_MS: i64 = 6 * 3600 * 1000;
const TIMELINE: usize = 40;
const SCREEN_LINES: usize = 60;

pub async fn run(socket: &Path, once: bool) -> Result<()> {
    if !once {
        // The phone closing the channel closes our stdin: stop right away.
        std::thread::spawn(|| {
            let mut buf = [0u8; 256];
            loop {
                match std::io::Read::read(&mut std::io::stdin(), &mut buf) {
                    Ok(0) | Err(_) => std::process::exit(0),
                    Ok(_) => {}
                }
            }
        });
    }
    let mut last = String::new();
    loop {
        match watch(socket, once, &mut last).await {
            Ok(()) if once => return Ok(()),
            Ok(()) => {}
            Err(e) => {
                if once {
                    return Err(e);
                }
                line(&json!({ "error": format!("{e:#}") }).to_string());
                last.clear();
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}

fn line(s: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{s}");
    let _ = out.flush();
}

/// One connection's worth of watching; returns (or errs) when the event stream ends.
async fn watch(socket: &Path, once: bool, last: &mut String) -> Result<()> {
    let unreachable = |e: std::io::Error| {
        crate::exit::CliError::Unreachable(format!("orchd is not running: {e}"))
    };
    let mut req = Conn::unix(socket).await.map_err(unreachable)?;
    if once {
        let s = tokio::time::timeout(SNAPSHOT_TIMEOUT, snapshot(&mut req))
            .await
            .map_err(|_| {
                crate::exit::CliError::Unreachable(format!(
                    "orchd did not answer within {SNAPSHOT_TIMEOUT:?}"
                ))
            })??;
        line(&s);
        return Ok(());
    }
    let (mut events, mut w) = Conn::unix(socket).await.map_err(unreachable)?.into_parts();
    w.send_body("w", serde_json::to_value(Request::Subscribe {})?)
        .await?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
    let reader = tokio::spawn(async move {
        while let Ok(Some(_)) = events.next().await {
            let _ = tx.try_send(());
        }
    });
    let mut beat = Instant::now();
    // Every pass: let a burst settle, read the state, print it if it changed, wait for a hint.
    let result = loop {
        tokio::time::sleep(COALESCE).await;
        while rx.try_recv().is_ok() {}
        // A daemon that stops answering must not freeze the phone's view: error out and retry.
        let s = match tokio::time::timeout(SNAPSHOT_TIMEOUT, snapshot(&mut req)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => break Err(e),
            Err(_) => {
                break Err(anyhow::anyhow!(
                    "orchd did not answer within {SNAPSHOT_TIMEOUT:?}"
                ))
            }
        };
        if s != *last || beat.elapsed() >= HEARTBEAT {
            line(&s);
            *last = s;
            beat = Instant::now();
        }
        tokio::select! {
            ev = rx.recv() => if ev.is_none() {
                break Err(anyhow::anyhow!("orchd closed the event stream"));
            },
            _ = tokio::time::sleep(REFRESH) => {}
        }
    };
    reader.abort();
    result
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The authoritative state as one JSON line.
async fn snapshot(c: &mut Conn) -> Result<String> {
    let agents: Vec<AgentRow> =
        serde_json::from_value(c.request(&Request::ListAgents {}).await?["agents"].clone())?;
    let panes: Vec<PaneInfo> =
        serde_json::from_value(c.request(&Request::ListPanes {}).await?["panes"].clone())?;
    let session: Session = serde_json::from_value(c.request(&Request::UiGet {}).await?)?;
    let questions: Vec<QuestionRow> = serde_json::from_value(
        c.request(&Request::ListQuestions { all: false }).await?["questions"].clone(),
    )?;
    let timeline: Vec<MessageRow> = serde_json::from_value(
        c.request(&Request::Log {
            after: None,
            limit: Some(TIMELINE),
            follow: false,
        })
        .await?["messages"]
            .clone(),
    )?;
    let herdr_alive = if agents.iter().any(|a| herdr_pane(a).is_some()) {
        herdr_panes().await
    } else {
        None
    };
    let list = merge(&agents, &panes, &session, herdr_alive.as_ref(), now_ms());
    // The text of every waiting agent's pane, so the phone can show and answer its dialog.
    let mut screens = serde_json::Map::new();
    for a in &list {
        if a["status"] != "blocked" {
            continue;
        }
        let Some(pane) = a["pane"].as_str() else {
            continue;
        };
        if let Some(text) = read_pane(c, pane, SCREEN_LINES).await {
            screens.insert(pane.to_string(), Value::String(text));
        }
    }
    let workspaces = workspaces(&session, &panes);
    Ok(compose(list, &questions, screens, &timeline, workspaces).to_string())
}

/// The watch line from its parts: agents (from `merge`) sorted blocked-first, the open questions,
/// the blocked agents' screens and the timeline tail. Pure, so the fixture test can build one.
fn compose(
    mut agents: Vec<Value>,
    questions: &[QuestionRow],
    screens: serde_json::Map<String, Value>,
    timeline: &[MessageRow],
    workspaces: Vec<Value>,
) -> Value {
    let hashes: serde_json::Map<String, Value> = screens
        .iter()
        .filter_map(|(pane, text)| {
            Some((
                pane.clone(),
                Value::String(orch_proto::api::text_hash(text.as_str()?)),
            ))
        })
        .collect();
    agents.sort_by_key(|a| {
        (
            order(a["status"].as_str().unwrap_or("")),
            a["workspace_number"].as_u64().unwrap_or(u64::MAX),
        )
    });
    json!({
        "v": 1,
        "orch": env!("CARGO_PKG_VERSION"),
        "human": HUMAN,
        "agents": agents,
        "questions": questions,
        // Per screen the hash `orch read --hash --lines 60` would print for that same text, so
        // the phone can answer with one `orch send --expect` (orchd refuses a stale screen).
        "hashes": hashes,
        "screens": screens,
        "timeline": timeline,
        "workspaces": workspaces,
    })
}

/// Most urgent status among panes: blocked > working > idle; None without agents.
fn worst<'a>(statuses: impl Iterator<Item = Option<&'a str>>) -> Option<&'a str> {
    let mut best: Option<&str> = None;
    for s in statuses.flatten() {
        match s {
            "blocked" => return Some("blocked"),
            "working" => best = Some("working"),
            "idle" if best.is_none() => best = Some("idle"),
            _ => {}
        }
    }
    best
}

/// The layout for the phone's workspace bar: workspaces in orchd's order with their tabs (by
/// id, so a tab closed meanwhile can't shift a tap onto another), each with the agents' most
/// urgent status, and which workspace the TUI shows (the last raise).
fn workspaces(session: &Session, panes: &[PaneInfo]) -> Vec<Value> {
    let status: HashMap<&str, Option<&str>> = panes
        .iter()
        .map(|p| (p.id.as_str(), p.agent_status.as_deref()))
        .collect();
    let shown = session
        .raised
        .as_ref()
        .map(|r| r.workspace.as_str())
        .or_else(|| session.workspaces.first().map(|w| w.id.as_str()));
    session
        .workspaces
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let tabs: Vec<Value> = w
                .tabs
                .iter()
                .enumerate()
                .map(|(ti, t)| {
                    let ps = t.root.panes();
                    json!({
                        "index": ti,
                        "id": t.id,
                        "label": t.label,
                        "panes": ps.iter().map(|p| format!("orch:{p}")).collect::<Vec<_>>(),
                        "focus": if t.focus.is_empty() { Value::Null } else { json!(format!("orch:{}", t.focus)) },
                        "zoomed": t.zoomed,
                        "status": worst(ps.iter().map(|p| status.get(p.as_str()).copied().flatten())),
                    })
                })
                .collect();
            let all = w.tabs.iter().flat_map(|t| t.root.panes()).collect::<Vec<_>>();
            json!({
                "id": w.id,
                "number": i + 1,
                "label": w.label,
                "cwd": w.cwd,
                "status": worst(all.iter().map(|p| status.get(p.as_str()).copied().flatten())),
                "shown": shown == Some(w.id.as_str()),
                "active_tab": w.active_tab.min(w.tabs.len().saturating_sub(1)),
                "tabs": tabs,
            })
        })
        .collect()
}

fn order(s: &str) -> u8 {
    match s {
        "blocked" => 0,
        "working" => 1,
        "done" => 2,
        "idle" => 3,
        _ => 4,
    }
}

fn herdr_pane(a: &AgentRow) -> Option<&str> {
    a.pane.as_deref()?.strip_prefix("herdr:")
}

/// Bus agents joined with orch panes and the layout, plus agent panes nobody registered.
fn merge(
    agents: &[AgentRow],
    panes: &[PaneInfo],
    session: &Session,
    herdr_alive: Option<&HashSet<String>>,
    now: i64,
) -> Vec<Value> {
    let by_id: HashMap<&str, &PaneInfo> = panes.iter().map(|p| (p.id.as_str(), p)).collect();
    let place = |pane: &str| -> (String, u64, bool) {
        match session.locate(pane) {
            Some((wi, ti)) => {
                let w = &session.workspaces[wi];
                let focused = session.raised.as_ref().is_some_and(|r| r.workspace == w.id)
                    && w.active_tab == ti
                    && w.tabs[ti].focus == pane;
                (w.label.clone(), wi as u64 + 1, focused)
            }
            None => (String::new(), 0, false),
        }
    };
    let mut claimed = HashSet::new();
    let mut out = Vec::new();
    for a in agents {
        let mut status = a.status.clone();
        let (mut workspace, mut number, mut focused) = (String::new(), 0, false);
        let mut cwd = a.cwd.clone().unwrap_or_default();
        let live = match a.pane.as_deref() {
            Some(p) if p.starts_with("orch:") => {
                let id = &p["orch:".len()..];
                match by_id.get(id) {
                    Some(info) if info.state == PaneState::Running => {
                        claimed.insert(id.to_string());
                        (workspace, number, focused) = place(id);
                        if cwd.is_empty() {
                            cwd = info.cwd.clone();
                        }
                        // The screen knows best about dialogs; the bus about everything else.
                        match info.agent_status.as_deref() {
                            Some("blocked") => status = "blocked".into(),
                            Some(s) if status == "unknown" => status = s.into(),
                            _ => {}
                        }
                        true
                    }
                    _ => continue, // its pane is gone
                }
            }
            Some(p) if p.starts_with("herdr:") => match herdr_alive {
                Some(alive) => {
                    if !alive.contains(&p["herdr:".len()..]) {
                        continue;
                    }
                    true
                }
                None => false,
            },
            _ => false,
        };
        if !live && now - a.last_seen_ms > STALE_MS {
            continue;
        }
        out.push(json!({
            "name": a.name,
            "kind": a.kind,
            "status": status,
            "status_text": a.status_text,
            "pane": a.pane,
            "cwd": cwd,
            "workspace": workspace,
            "workspace_number": number,
            "focused": focused,
            // Whole minutes: every hook call bumps last_seen, and the exact value would make
            // the line differ (and go to the phone) about once a second for nothing.
            "seen_ms": a.last_seen_ms - a.last_seen_ms.rem_euclid(60_000),
            "registered": true,
        }));
    }
    for p in panes {
        let Some(kind) = p.agent.as_deref() else {
            continue;
        };
        if p.state != PaneState::Running || claimed.contains(&p.id) {
            continue;
        }
        let (workspace, number, focused) = place(&p.id);
        out.push(json!({
            "name": p.name.clone().unwrap_or_else(|| format!("{kind}-{}", p.id)),
            "kind": kind,
            "status": p.agent_status.as_deref().unwrap_or("unknown"),
            "status_text": Value::Null,
            "pane": format!("orch:{}", p.id),
            "cwd": p.cwd,
            "workspace": workspace,
            "workspace_number": number,
            "focused": focused,
            "seen_ms": Value::Null,
            "registered": false,
        }));
    }
    out
}

/// herdr's executable: $ORCH_HERDR, else on PATH, else ~/.local/bin/herdr (SSH exec has a short PATH).
pub(crate) fn herdr_bin() -> String {
    if let Ok(h) = std::env::var("ORCH_HERDR") {
        return h;
    }
    let on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join("herdr").is_file()));
    if on_path {
        return "herdr".into();
    }
    std::env::var("HOME").map_or("herdr".into(), |h| format!("{h}/.local/bin/herdr"))
}

pub(crate) async fn herdr(args: &[&str]) -> Option<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(herdr_bin())
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Recent text of a herdr pane (what `orch read herdr:ID` prints and hashes).
pub(crate) async fn herdr_read(id: &str, lines: usize) -> Option<String> {
    let n = lines.to_string();
    herdr(&["pane", "read", id, "--source", "recent", "--lines", &n]).await
}

/// How long herdr's pane list is reused: asking herdr costs 40-80 ms and a process, and a
/// snapshot runs on every hint (often several per second).
const HERDR_CACHE: Duration = Duration::from_secs(15);

/// The ids of herdr's live panes (cached for [`HERDR_CACHE`]); None when herdr can't be asked.
async fn herdr_panes() -> Option<HashSet<String>> {
    type Cache = std::sync::Mutex<Option<(Instant, Option<HashSet<String>>)>>;
    static CACHE: Cache = std::sync::Mutex::new(None);
    if let Some((at, v)) = CACHE.lock().unwrap().as_ref() {
        if at.elapsed() < HERDR_CACHE {
            return v.clone();
        }
    }
    let v = herdr_panes_now().await;
    *CACHE.lock().unwrap() = Some((Instant::now(), v.clone()));
    v
}

async fn herdr_panes_now() -> Option<HashSet<String>> {
    let v: Value = serde_json::from_str(herdr(&["pane", "list"]).await?.trim()).ok()?;
    Some(
        v["result"]["panes"]
            .as_array()?
            .iter()
            .filter_map(|p| p["pane_id"].as_str().map(String::from))
            .collect(),
    )
}

/// Recent text of a pane given as `orch:pN`, `pN` or `herdr:ID`.
pub(crate) async fn read_pane(c: &mut Conn, pane: &str, lines: usize) -> Option<String> {
    if let Some(id) = pane.strip_prefix("herdr:") {
        return herdr_read(id, lines).await;
    }
    let id = pane.strip_prefix("orch:").unwrap_or(pane);
    let v = c
        .request(&Request::Read(orch_proto::api::ReadPane {
            pane: id.into(),
            lines: Some(lines),
            source: orch_proto::api::ReadSource::Recent,
        }))
        .await
        .ok()?;
    v["text"].as_str().map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orch_proto::layout::{Node, Raised, Tab, Workspace};

    fn pane(id: &str, agent: Option<&str>, status: Option<&str>) -> PaneInfo {
        PaneInfo {
            id: id.into(),
            name: None,
            argv: vec![],
            cwd: "/w".into(),
            pid: None,
            state: PaneState::Running,
            exit_code: None,
            rows: 24,
            cols: 80,
            clients: 0,
            title: String::new(),
            created_unix: 0,
            agent: agent.map(String::from),
            agent_status: status.map(String::from),
        }
    }

    fn agent(name: &str, pane: Option<&str>, status: &str, seen: i64) -> AgentRow {
        AgentRow {
            name: name.into(),
            kind: "claude".into(),
            pane: pane.map(String::from),
            cwd: None,
            status: status.into(),
            status_text: None,
            last_seen_ms: seen,
        }
    }

    /// One workspace "proj" with one tab holding pane p1, raised.
    fn session_p1() -> Session {
        Session {
            version: 1,
            workspaces: vec![Workspace {
                id: "w1".into(),
                label: "proj".into(),
                cwd: "/w".into(),
                tabs: vec![Tab {
                    id: "t1".into(),
                    label: "1".into(),
                    root: Node::leaf("p1"),
                    focus: "p1".into(),
                    zoomed: false,
                }],
                active_tab: 0,
            }],
            next_id: 2,
            raised: Some(Raised {
                seq: 1,
                workspace: "w1".into(),
            }),
        }
    }

    #[test]
    fn merge_joins_layout_screen_and_drops_dead() {
        let session = session_p1();
        let panes = vec![
            pane("p1", Some("claude"), Some("blocked")),
            pane("p2", Some("codex"), Some("idle")),
        ];
        let now = 10 * STALE_MS;
        let agents = vec![
            agent("a", Some("orch:p1"), "working", now),
            agent("gone", Some("orch:p9"), "idle", now),
            agent("old-mcp", None, "idle", now - STALE_MS - 1),
            agent("mcp", None, "idle", now - 1000),
            agent("h", Some("herdr:w1:p1"), "idle", 0),
            agent("h-dead", Some("herdr:w9:p1"), "idle", now),
        ];
        let alive: HashSet<String> = ["w1:p1".to_string()].into();
        let out = merge(&agents, &panes, &session, Some(&alive), now);
        let names: Vec<&str> = out.iter().map(|a| a["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["a", "mcp", "h", "codex-p2"]);
        assert_eq!(out[0]["status"], "blocked", "the screen's dialog wins");
        assert_eq!(out[0]["workspace"], "proj");
        assert_eq!(out[0]["workspace_number"], 1);
        assert_eq!(out[0]["focused"], true);
        assert_eq!(out[3]["registered"], false);
        assert_eq!(out[3]["pane"], "orch:p2");
    }

    /// The contract with Kabuk: a composed line has every key of the frozen v1 fixture (a real
    /// `--once` line; Kabuk's tests load the same file). Removing or renaming a key fails here.
    #[test]
    fn line_keeps_every_key_of_the_v1_fixture() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/watch-v1.json")).unwrap();
        let session = session_p1();
        let panes = vec![pane("p1", Some("claude"), Some("blocked"))];
        let agents = vec![agent("webshop-claude", Some("orch:p1"), "blocked", 1000)];
        let list = merge(&agents, &panes, &session, None, 2000);
        let question = QuestionRow {
            id: 1,
            ts_ms: 1000,
            from: "webshop-claude".into(),
            text: "Deploy the webshop now?".into(),
            options: vec!["yes, deploy".into(), "not yet".into()],
            state: "open".into(),
            answer: None,
            answered_by: None,
            answered_ts_ms: None,
        };
        let row = MessageRow {
            id: 1,
            ts_ms: 1000,
            from: "webshop-claude".into(),
            to: HUMAN.into(),
            kind: "ask".into(),
            text: "Deploy the webshop now?".into(),
            thread: None,
            question: Some(1),
        };
        let mut screens = serde_json::Map::new();
        screens.insert(
            "orch:p1".into(),
            Value::String(" ❯ 1. Yes\n   2. No".into()),
        );
        let ws = workspaces(
            &session_p1(),
            &[pane("p1", Some("claude"), Some("blocked"))],
        );
        let line = compose(list, &[question], screens, &[row], ws);
        let keys = |v: &Value| -> Vec<String> { v.as_object().unwrap().keys().cloned().collect() };
        for k in keys(&fixture) {
            assert!(line.get(&k).is_some(), "top-level key {k:?} missing");
        }
        for k in keys(&fixture["agents"][0]) {
            assert!(
                line["agents"][0].get(&k).is_some(),
                "agent key {k:?} missing"
            );
        }
        for k in keys(&fixture["questions"][0]) {
            assert!(
                line["questions"][0].get(&k).is_some(),
                "question key {k:?} missing"
            );
        }
        for k in keys(&fixture["timeline"][0]) {
            assert!(
                line["timeline"][0].get(&k).is_some(),
                "timeline key {k:?} missing"
            );
        }
        assert_eq!(line["v"], 1);
        assert_eq!(fixture["v"], 1, "fixture and code must agree on v");
        assert!(line["orch"].as_str().is_some_and(|s| !s.is_empty()));
        assert_eq!(line["agents"][0]["status"], "blocked");
        assert_eq!(
            line["screens"]["orch:p1"].as_str().unwrap().len(),
            " ❯ 1. Yes\n   2. No".len()
        );
        assert_eq!(
            line["hashes"]["orch:p1"],
            orch_proto::api::text_hash(" ❯ 1. Yes\n   2. No"),
            "hash of exactly the screen text"
        );
        for (pane, text) in fixture["screens"].as_object().unwrap() {
            assert_eq!(
                fixture["hashes"][pane],
                orch_proto::api::text_hash(text.as_str().unwrap()),
                "fixture hashes match its screens"
            );
        }
        for k in keys(&fixture["workspaces"][0]) {
            assert!(
                line["workspaces"][0].get(&k).is_some(),
                "workspace key {k:?} missing"
            );
        }
        for k in keys(&fixture["workspaces"][0]["tabs"][0]) {
            assert!(
                line["workspaces"][0]["tabs"][0].get(&k).is_some(),
                "tab key {k:?} missing"
            );
        }
        let w = &line["workspaces"][0];
        assert_eq!(w["status"], "blocked");
        assert_eq!(w["tabs"][0]["panes"][0], "orch:p1");
        assert_eq!(w["tabs"][0]["focus"], "orch:p1");
        // One line, no embedded newline: what the phone reads with readLine().
        assert!(!line.to_string().contains('\n'));
    }
}
