//! orch-mcp — MCP server (stdio) that connects an agent (Claude Code, Codex, …) to the orch bus.
//!
//! Tools: send_message, read_inbox, ask_human, wait_for_answer, post_status, list_agents.
//! A small hand-written JSON-RPC loop (newline-delimited, MCP stdio transport) — no SDK churn.
//! Every tool call runs as its own task, so a long `ask_human` never blocks other calls, and
//! `notifications/cancelled` aborts it. The agent's identity is (re-)registered on every call:
//! cheap, and it survives an orchd restart.
//!
//! Stdout carries only MCP frames; diagnostics go to stderr.

use orch_proto::api::{MessageRow, QuestionRow, Request, SendMessage};
use orch_proto::client::{connect_or_start, default_socket_path, register_request, ClientError};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

mod delivery;

const PROTOCOL_VERSION: &str = "2025-06-18";

struct State {
    /// "claude", "codex", … from the client's `clientInfo.name`.
    kind: Mutex<String>,
    delivered: delivery::Receipts,
    inflight: Mutex<HashMap<String, tokio::task::AbortHandle>>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let state = Arc::new(State {
        kind: Mutex::new("agent".into()),
        delivered: delivery::Receipts::default(),
        inflight: Mutex::new(HashMap::new()),
    });
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Out>();
    let writer_state = Arc::clone(&state);
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some((v, after)) = out_rx.recv().await {
            if writer_state
                .delivered
                .write(&mut stdout, &v, after.as_ref())
                .await
                .is_err()
            {
                break; // not delivered: its messages are NOT acknowledged
            }
            // Receipt is already visible to the next inbox read, even while this ACK waits.
            if let Some(req) = after {
                if request(req.clone()).await.is_ok() {
                    writer_state.delivered.confirm(&req).await;
                }
            }
        }
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            let _ = out_tx.send((rpc_error(Value::Null, -32700, "parse error"), None));
            continue;
        };
        handle(&state, &out_tx, msg);
    }
    // stdin closed: the client is gone. Abort waits; flush what is queued.
    for (_, h) in state.inflight.lock().unwrap().drain() {
        h.abort();
    }
    drop(out_tx);
    let _ = writer.await;
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A JSON-RPC message for stdout, and a request to run once it has been written.
type Out = (Value, Option<Request>);

fn handle(state: &Arc<State>, out: &mpsc::UnboundedSender<Out>, msg: Value) {
    let method = msg["method"].as_str().unwrap_or("").to_string();
    let id = msg.get("id").cloned();
    match (method.as_str(), id) {
        ("initialize", Some(id)) => {
            let client = msg["params"]["clientInfo"]["name"]
                .as_str()
                .unwrap_or("agent")
                .to_lowercase();
            let kind = if client.contains("claude") {
                "claude"
            } else if client.contains("codex") {
                "codex"
            } else {
                "agent"
            };
            *state.kind.lock().unwrap() = kind.to_string();
            // Join the bus right away, so other agents can reach this one before it makes
            // its first tool call (best effort; every call registers again anyway).
            let kind2 = kind.to_string();
            tokio::spawn(async move {
                let _ = whoami(&kind2).await;
            });
            let version = msg["params"]["protocolVersion"]
                .as_str()
                .unwrap_or(PROTOCOL_VERSION)
                .to_string();
            let _ = out.send((
                rpc_result(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "orch", "version": env!("CARGO_PKG_VERSION") },
                        "instructions": INSTRUCTIONS,
                    }),
                ),
                None,
            ));
        }
        ("ping", Some(id)) => {
            let _ = out.send((rpc_result(id, json!({})), None));
        }
        ("tools/list", Some(id)) => {
            let _ = out.send((rpc_result(id, json!({ "tools": tools() })), None));
        }
        ("tools/call", Some(id)) => {
            let key = id.to_string();
            let (state2, out2, key2) = (Arc::clone(state), out.clone(), key.clone());
            let params = msg["params"].clone();
            let task = tokio::spawn(async move {
                let name = params["name"].as_str().unwrap_or("").to_string();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let kind = state2.kind.lock().unwrap().clone();
                let (result, after) = match call_tool(&kind, &name, &args, &state2.delivered).await
                {
                    Ok((text, after)) => (
                        json!({ "content": [{ "type": "text", "text": text }] }),
                        after,
                    ),
                    Err(e) => (
                        json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
                        None,
                    ),
                };
                let _ = out2.send((rpc_result(id, result), after));
                state2.inflight.lock().unwrap().remove(&key2);
            });
            state
                .inflight
                .lock()
                .unwrap()
                .insert(key, task.abort_handle());
        }
        ("notifications/cancelled", None) => {
            let key = msg["params"]["requestId"].to_string();
            if let Some(h) = state.inflight.lock().unwrap().remove(&key) {
                h.abort();
            }
        }
        (_, Some(id)) => {
            let _ = out.send((
                rpc_error(id, -32601, &format!("unknown method {method}")),
                None,
            ));
        }
        (_, None) => {} // other notifications
    }
}

const INSTRUCTIONS: &str = "You are connected to orchtool, Murat's agent bus. Other agents (Claude, Codex, …) \
and Murat himself are reachable here. Use send_message to talk to another agent or to Murat (to: \"murat\"), \
read_inbox to get messages sent to you, and ask_human when you need Murat's decision — he can answer from his \
terminal or phone. Keep messages short and concrete. When a tool result says a question is not answered \
yet, call wait_for_answer with that question_id instead of asking again. After completing a queued orch task, call task_done; omit id to complete the running task in your pane.";

fn tools() -> Value {
    json!([
        {
            "name": "send_message",
            "description": "Send a short message to another agent (by name, see list_agents), to Murat (\"murat\"), or to everyone (\"all\"). Returns immediately.",
            "inputSchema": { "type": "object", "properties": {
                "to": { "type": "string", "description": "Agent name, \"murat\", or \"all\"" },
                "text": { "type": "string" },
                "thread": { "type": "string", "description": "Optional topic to group related messages" }
            }, "required": ["to", "text"] }
        },
        {
            "name": "task_done",
            "description": "Mark your running orch task done after completing its work. Omit id to use your current pane's running task.",
            "inputSchema": { "type": "object", "properties": { "id": { "type": "integer" } } }
        },
        {
            "name": "read_inbox",
            "description": "Get unread messages sent to you (also Murat's answers). Marks them read. With wait_seconds > 0 it waits for the first message to arrive.",
            "inputSchema": { "type": "object", "properties": {
                "wait_seconds": { "type": "integer", "minimum": 0, "description": "Wait up to this long for a message (default 0 = return now)" }
            } }
        },
        {
            "name": "ask_human",
            "description": "Ask Murat a question and wait for his answer (he sees it in his terminal and on his phone). Offer options when there are clear choices. If the answer does not arrive within the wait, you get a question_id: call wait_for_answer with it — do not ask again.",
            "inputSchema": { "type": "object", "properties": {
                "question": { "type": "string", "description": "Self-contained question with the context Murat needs" },
                "options": { "type": "array", "items": { "type": "string" }, "description": "Optional short answer choices (max 8)" },
                "wait_seconds": { "type": "integer", "minimum": 0 }
            }, "required": ["question"] }
        },
        {
            "name": "wait_for_answer",
            "description": "Keep waiting for Murat's answer to a question asked earlier with ask_human.",
            "inputSchema": { "type": "object", "properties": {
                "question_id": { "type": "integer" },
                "wait_seconds": { "type": "integer", "minimum": 0 }
            }, "required": ["question_id"] }
        },
        {
            "name": "post_status",
            "description": "Tell Murat and the other agents what you are doing: working, idle (waiting for work), blocked, or done — with a one-line text.",
            "inputSchema": { "type": "object", "properties": {
                "status": { "type": "string", "enum": ["working", "idle", "blocked", "done"] },
                "text": { "type": "string" }
            }, "required": ["status"] }
        },
        {
            "name": "list_agents",
            "description": "List the agents on the bus (name, kind, status) and your own name.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

/// Default and max wait per client: Codex's MCP tool timeout defaults to 60 s, Claude Code's
/// is long. Waits are resumable (wait_for_answer), so a short default never loses anything.
fn wait_ms(kind: &str, args: &Value, default_secs_claude: u64) -> u64 {
    let (default, max) = if kind == "claude" {
        (default_secs_claude, 1800)
    } else {
        (50.min(default_secs_claude), 55)
    };
    let max = std::env::var("ORCH_MCP_MAX_WAIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(max);
    args["wait_seconds"].as_u64().unwrap_or(default).min(max) * 1000
}

async fn request(req: Request) -> Result<Value, String> {
    let socket = default_socket_path();
    let mut conn = connect_or_start(&socket)
        .await
        .map_err(|e| format!("orchd is not reachable: {e}"))?;
    conn.request(&req).await.map_err(|e| match e {
        ClientError::Remote(e) => e.message,
        ClientError::Io(e) => format!("orchd: {e}"),
    })
}

/// This orch-mcp process's identity, used when the agent has no pane.
static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

async fn whoami(kind: &str) -> Result<String, String> {
    let mut reg = register_request(kind);
    reg.instance = Some(
        INSTANCE
            .get_or_init(|| format!("mcp:{}", orch_proto::client::random_id()))
            .clone(),
    );
    let row = request(Request::RegisterAgent(reg)).await?;
    Ok(row["name"].as_str().unwrap_or("agent").to_string())
}

fn fmt_messages(rows: &[MessageRow]) -> String {
    if rows.is_empty() {
        return "No new messages.".into();
    }
    let mut out = format!("{} new message(s):", rows.len());
    for m in rows {
        let what = match (m.kind.as_str(), m.question) {
            ("answer", Some(q)) => format!("Murat's answer to question #{q}"),
            ("ask", Some(q)) => format!("question #{q} from {}", m.from),
            _ => format!("from {}", m.from),
        };
        let thread = m
            .thread
            .as_deref()
            .map(|t| format!(" [{t}]"))
            .unwrap_or_default();
        out.push_str(&format!("\n- #{} {what}{thread}: {}", m.id, m.text));
    }
    out
}

fn fmt_question(q: &QuestionRow) -> String {
    if q.state == "expired" {
        format!("Question #{} expired or was dismissed; no human answer was given. Stop waiting for it.", q.id)
    } else if q.state == "answered" {
        format!(
            "Murat answered question #{}: {}",
            q.id,
            q.answer.as_deref().unwrap_or("")
        )
    } else {
        format!(
            "Not answered yet (question #{id}). Murat has been notified. Call wait_for_answer with \
             question_id {id} to keep waiting (do not ask again), or continue with other work and \
             check read_inbox later — the answer will arrive there too.",
            id = q.id
        )
    }
}

/// A question's state for the agent. Once answered, the answer counts as received only after
/// this text reached the client (then asking the same question again is a new question).
fn answered(me: &str, v: Value) -> Result<(String, Option<Request>), String> {
    let mid = v["answer_message"].as_i64();
    let q: QuestionRow = serde_json::from_value(v).map_err(|e| e.to_string())?;
    let ack = mid.filter(|_| q.state == "answered").map(|m| Request::Ack {
        name: me.to_string(),
        ids: vec![m],
    });
    Ok((fmt_question(&q), ack))
}

/// The tool's text, plus a request to run after that text reached the client.
async fn call_tool(
    kind: &str,
    name: &str,
    args: &Value,
    delivered: &delivery::Receipts,
) -> Result<(String, Option<Request>), String> {
    call_tool_inner(kind, name, args, delivered).await
}

async fn call_tool_inner(
    kind: &str,
    name: &str,
    args: &Value,
    delivered: &delivery::Receipts,
) -> Result<(String, Option<Request>), String> {
    let me = whoami(kind).await?;
    let str_arg = |k: &str| args[k].as_str().map(str::to_string);
    match name {
        "task_done" => {
            let id = args
                .get("id")
                .map(|v| v.as_i64().ok_or("`id` must be an integer"))
                .transpose()?;
            let v = request(Request::TaskDone {
                id,
                pane: std::env::var("ORCH_PANE").ok(),
            })
            .await?;
            Ok((format!("Task #{} done.", v["id"]), None))
        }
        "send_message" => {
            let to = str_arg("to").ok_or("`to` is required")?;
            let text = str_arg("text").ok_or("`text` is required")?;
            let row = request(Request::SendMessage(SendMessage {
                from: me,
                to: to.clone(),
                text,
                thread: str_arg("thread"),
            }))
            .await?;
            Ok((format!("Sent (#{}) to {to}.", row["id"]), None))
        }
        "read_inbox" => {
            let wait = wait_ms(kind, args, 0);
            let v = request(Request::Inbox {
                delivered: delivered.for_agent(&me).await,
                name: me.clone(),
                wait_ms: Some(wait),
            })
            .await?;
            let rows: Vec<MessageRow> =
                serde_json::from_value(v["messages"].clone()).map_err(|e| e.to_string())?;
            // Consumed only after the result was written to the client (see the writer).
            let ack = (!rows.is_empty()).then(|| Request::Ack {
                name: me,
                ids: rows.iter().map(|r| r.id).collect(),
            });
            Ok((fmt_messages(&rows), ack))
        }
        "ask_human" => {
            let question = str_arg("question").ok_or("`question` is required")?;
            let options: Vec<String> = args["options"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|o| o.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let q = request(Request::Ask {
                from: me.clone(),
                text: question,
                options,
            })
            .await?;
            let id = q["id"].as_i64().ok_or("bad reply")?;
            let v = request(Request::AskWait {
                id,
                wait_ms: Some(wait_ms(kind, args, 600)),
            })
            .await?;
            answered(&me, v)
        }
        "wait_for_answer" => {
            let id = args["question_id"]
                .as_i64()
                .ok_or("`question_id` is required")?;
            let v = request(Request::AskWait {
                id,
                wait_ms: Some(wait_ms(kind, args, 600)),
            })
            .await?;
            answered(&me, v)
        }
        "post_status" => {
            let status = str_arg("status").ok_or("`status` is required")?;
            request(Request::SetStatus {
                name: me.clone(),
                status: status.clone(),
                text: str_arg("text"),
            })
            .await?;
            Ok((format!("Status of {me}: {status}."), None))
        }
        "list_agents" => {
            let v = request(Request::ListAgents {}).await?;
            let mut out = format!("You are {me}. Agents:");
            for a in v["agents"].as_array().cloned().unwrap_or_default() {
                out.push_str(&format!(
                    "\n- {} ({}) {}{}",
                    a["name"].as_str().unwrap_or("?"),
                    a["kind"].as_str().unwrap_or("?"),
                    a["status"].as_str().unwrap_or("?"),
                    a["status_text"]
                        .as_str()
                        .map(|t| format!(" — {t}"))
                        .unwrap_or_default()
                ));
            }
            out.push_str("\nMurat is \"murat\"; \"all\" reaches every agent.");
            Ok((out, None))
        }
        other => Err(format!("unknown tool {other}")),
    }
}
