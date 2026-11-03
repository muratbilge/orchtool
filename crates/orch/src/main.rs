//! orch — CLI for orchd: create, list, attach, drive and read panes; `orch proxy` for SSH.

mod attach;
mod bus;
mod exit;
mod tui;
mod watch;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use orch_proto::api::{
    AttachMode, NewPane, PaneInfo, PaneState, ReadPane, ReadSource, Request, SendPane,
};
use orch_proto::client::{default_socket_path, Conn};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Parser)]
#[command(
    name = "orch",
    version,
    about = "orchtool: agent workspace manager",
    after_help = exit::HELP
)]
struct Cli {
    /// orchd socket (default $ORCHD_SOCKET or ~/.local/state/orchtool/orchd.sock)
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Talk to orchd on another machine through `ssh HOST orch proxy` (or $ORCH_HOST)
    #[arg(long, global = true, env = "ORCH_HOST")]
    host: Option<String>,
    /// Without a command: open the workspace TUI.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the workspace TUI (same as plain `orch`)
    Ui,
    /// Start a program in a new pane (default: your login shell)
    New {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        /// Extra environment variable KEY=VALUE (repeatable)
        #[arg(long = "env", value_name = "KEY=VALUE")]
        envs: Vec<String>,
        /// Use orchd's environment instead of this shell's
        #[arg(long)]
        daemon_env: bool,
        /// Attach right after creating it
        #[arg(short, long)]
        attach: bool,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    /// List panes
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Attach to a pane full-screen (detach: prefix d, take control: prefix t; prefix = Ctrl-b or $ORCH_PREFIX)
    Attach {
        pane: String,
        /// Watch only (never resize, no input)
        #[arg(long, conflicts_with = "control")]
        observe: bool,
        /// Take control even if another client has it
        #[arg(long)]
        control: bool,
    },
    /// Type text into a pane (and/or press keys)
    Send {
        pane: String,
        text: Option<String>,
        /// Press Enter after the text
        #[arg(long)]
        enter: bool,
        /// Key to press after the text: Enter Escape Tab BSpace Up Down Left Right C-c C-d … (repeatable)
        #[arg(long = "key")]
        keys: Vec<String>,
        /// Only if the screen still has this hash (from `orch read --hash`, same --lines); a
        /// screen state is answered once (first answer wins)
        #[arg(long)]
        expect: Option<String>,
        #[arg(long, default_value_t = 60)]
        lines: usize,
    },
    /// Print a pane's text
    Read {
        pane: String,
        #[arg(long, default_value_t = 60)]
        lines: usize,
        /// Only the visible screen
        #[arg(long)]
        visible: bool,
        /// First line `hash:<h>` for a later `orch send --expect <h>`
        #[arg(long)]
        hash: bool,
    },
    /// Kill a pane's program (or remove an exited pane)
    Kill { pane: String },
    /// Focus a pane and show its workspace in every open `orch` TUI
    Focus { pane: String },
    /// Stream agents, questions, dialogs and the timeline as JSON lines (for Kabuk)
    Watch {
        /// JSON lines (the only format; accepted for clarity)
        #[arg(long)]
        json: bool,
        /// Print one snapshot and exit
        #[arg(long)]
        once: bool,
    },
    /// stdio ⇄ orchd bridge, used by `orch --host H …` via ssh
    Proxy {
        /// Multiplex many orch connections over this one stream (the TUI uses this)
        #[arg(long)]
        mux: bool,
    },
    /// Round-trip time to orchd
    Ping {
        #[arg(long, default_value_t = 1)]
        count: u32,
    },
    /// Measure attach time and keystroke → screen latency on a pane that echoes input (e.g. `cat`)
    Latency {
        pane: String,
        #[arg(long, default_value_t = 20)]
        count: u32,
    },
    /// Stop orchd (kills all panes)
    StopDaemon {
        /// Save the layout: the next orchd recreates it (same folders; agents continue)
        #[arg(long)]
        restart: bool,
    },

    // ---- agent bus ----
    /// List agents on the bus
    Agents {
        #[arg(long)]
        json: bool,
    },
    /// Show the communication timeline (-f: follow live)
    Tail {
        #[arg(short, long)]
        follow: bool,
        #[arg(short = 'n', long, default_value_t = 30)]
        lines: usize,
    },
    /// Send a message to an agent ("all" = everyone)
    Msg {
        to: String,
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        #[arg(long, default_value = "murat")]
        from: String,
    },
    /// Unread messages for you (or --name AGENT)
    Inbox {
        #[arg(long, default_value = "murat")]
        name: String,
        #[arg(long, default_value_t = 0)]
        wait: u64,
        #[arg(long)]
        peek: bool,
    },
    /// Open questions from agents (--all: also answered ones)
    Questions {
        #[arg(long)]
        all: bool,
    },
    /// Answer a question: orch answer <id> <text>  (or the option number)
    Answer {
        id: i64,
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        /// Who answers (shown in the timeline)
        #[arg(long, default_value = "cli")]
        by: String,
        /// The text is the answer as is (a number is not an option index)
        #[arg(long)]
        literal: bool,
    },
    /// Ask Murat a question from a script/shell agent and wait for the answer
    Ask {
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        /// Asking agent's name (default: register this shell as kind "shell")
        #[arg(long)]
        from: Option<String>,
        #[arg(long = "option")]
        options: Vec<String>,
        /// Seconds to wait (0 = just create it)
        #[arg(long, default_value_t = 3600)]
        wait: u64,
    },
    /// Set an agent's status: working | idle | blocked | done
    Status {
        name: String,
        status: String,
        text: Vec<String>,
    },
    /// Claude Code hook entry point (reads the hook JSON on stdin; never fails the hook)
    Hook {
        /// Agent kind to register as
        #[arg(long, default_value = "claude")]
        kind: String,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("orch: {e:#}");
        std::process::exit(exit::code(&e));
    }
}

async fn run(cli: Cli) -> Result<()> {
    let socket = cli.socket.clone().unwrap_or_else(default_socket_path);
    let cli_host = cli.host.clone();
    let connector = Connector::new(socket.clone(), cli.host.clone());
    let cmd = match cli.cmd {
        None | Some(Cmd::Ui) => return tui::run(connector).await,
        Some(cmd) => cmd,
    };
    let cli = CliRest { cmd };
    if let Cmd::Proxy { mux } = cli.cmd {
        return proxy(&socket, mux).await;
    }
    let herdr_pane = match &cli.cmd {
        Cmd::Read { pane, .. } | Cmd::Send { pane, .. } => pane.starts_with("herdr:"),
        _ => false,
    };
    if cli_host.is_some() && (herdr_pane || matches!(cli.cmd, Cmd::Watch { .. })) {
        anyhow::bail!(
            "--host is not supported for `orch watch` or herdr panes: run it there (ssh HOST orch …)"
        );
    }
    if let Cmd::Watch { once, .. } = cli.cmd {
        return watch::run(&socket, once).await;
    }
    // herdr panes (as agents on the bus name them) are read and typed into through herdr.
    match &cli.cmd {
        Cmd::Read {
            pane, lines, hash, ..
        } if pane.starts_with("herdr:") => {
            let id = &pane["herdr:".len()..];
            let text = watch::herdr_read(id, *lines)
                .await
                .context("herdr pane read failed")?;
            if *hash {
                println!("hash:{}", orch_proto::api::text_hash(&text));
            }
            print!("{text}");
            return Ok(());
        }
        Cmd::Send {
            pane,
            text,
            enter,
            keys,
            expect,
            lines,
        } if pane.starts_with("herdr:") => {
            let id = &pane["herdr:".len()..];
            // Best effort for herdr: re-read and compare right before typing (not atomic).
            if let Some(expect) = expect {
                let now = watch::herdr_read(id, *lines)
                    .await
                    .context("herdr pane read failed")?;
                if &orch_proto::api::text_hash(&now) != expect {
                    return Err(exit::CliError::Changed(
                        "the pane's screen changed: not sent".into(),
                    )
                    .into());
                }
            }
            if let Some(t) = text {
                watch::herdr(&["pane", "send-text", id, t])
                    .await
                    .context("herdr pane send-text failed")?;
            }
            let mut keys: Vec<&str> = keys.iter().map(String::as_str).collect();
            if *enter {
                keys.push("Enter");
            }
            for k in keys {
                watch::herdr(&["pane", "send-keys", id, k])
                    .await
                    .with_context(|| format!("herdr pane send-keys {k} failed"))?;
            }
            return Ok(());
        }
        _ => {}
    }
    if let Cmd::Hook { kind } = &cli.cmd {
        bus::hook(&socket, kind).await;
        return Ok(());
    }
    let mut conn = connector.connect().await?;
    match cli.cmd {
        Cmd::New {
            name,
            cwd,
            envs,
            daemon_env,
            attach,
            argv,
        } => {
            let env = if daemon_env {
                None
            } else {
                Some(client_env(&envs)?)
            };
            let cwd = cwd.or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            });
            // Size from this terminal only when there is one (scripts get orchd's default).
            let (rows, cols) = if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                crossterm::terminal::size()
                    .map(|(c, r)| (Some(r), Some(c)))
                    .unwrap_or((None, None))
            } else {
                (None, None)
            };
            let info: PaneInfo = serde_json::from_value(
                conn.request(&Request::NewPane(NewPane {
                    name,
                    argv,
                    cwd,
                    env,
                    rows,
                    cols,
                }))
                .await?,
            )?;
            if attach {
                return attach::run(conn, &info.id, AttachMode::Auto).await;
            }
            println!("{}", info.id);
        }
        Cmd::Ls { json } => {
            let v = conn.request(&Request::ListPanes {}).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            let panes: Vec<PaneInfo> = serde_json::from_value(v["panes"].clone())?;
            println!("ID    NAME             STATE      CLI     SIZE  COMMAND");
            for p in panes {
                let state = match p.state {
                    PaneState::Running => "running".to_string(),
                    PaneState::Exited => {
                        format!("exit {}", p.exit_code.map_or("?".into(), |c| c.to_string()))
                    }
                };
                println!(
                    "{:<5} {:<16} {:<10} {:>3} {:>8}  {}",
                    p.id,
                    p.name.unwrap_or_default(),
                    state,
                    p.clients,
                    format!("{}x{}", p.cols, p.rows),
                    p.argv.join(" ")
                );
            }
        }
        Cmd::Attach {
            pane,
            observe,
            control,
        } => {
            let mode = if observe {
                AttachMode::Observe
            } else if control {
                AttachMode::Control
            } else {
                AttachMode::Auto
            };
            return attach::run(conn, &pane, mode).await;
        }
        Cmd::Send {
            pane,
            text,
            enter,
            keys,
            expect,
            lines,
        } => {
            let mut text = text.unwrap_or_default();
            for k in &keys {
                text.push_str(key_bytes(k).with_context(|| format!("unknown key {k:?}"))?);
            }
            conn.request(&Request::Send(SendPane {
                pane: strip_orch(pane),
                text,
                enter,
                expect_lines: expect.is_some().then_some(lines),
                expect,
            }))
            .await?;
        }
        Cmd::Read {
            pane,
            lines,
            visible,
            hash,
        } => {
            let v = conn
                .request(&Request::Read(ReadPane {
                    pane: strip_orch(pane),
                    lines: Some(lines),
                    source: if visible {
                        ReadSource::Visible
                    } else {
                        ReadSource::Recent
                    },
                }))
                .await?;
            if hash {
                println!("hash:{}", v["hash"].as_str().unwrap_or_default());
            }
            println!("{}", v["text"].as_str().unwrap_or_default());
        }
        Cmd::Kill { pane } => {
            conn.request(&Request::Kill {
                pane: strip_orch(pane),
            })
            .await?;
        }
        Cmd::Focus { pane } => {
            conn.request(&Request::Ui(orch_proto::api::UiOp::Raise {
                pane: strip_orch(pane),
            }))
            .await?;
        }
        Cmd::Ping { count } => {
            for _ in 0..count {
                let t = Instant::now();
                let v = conn.request(&Request::Ping {}).await?;
                println!(
                    "pong from orchd pid {} in {:.2} ms",
                    v["pid"],
                    t.elapsed().as_secs_f64() * 1000.0
                );
            }
        }
        Cmd::Latency { pane, count } => attach::latency(conn, &pane, count).await?,
        Cmd::StopDaemon { restart } => {
            conn.request(&Request::StopDaemon { restart }).await?;
        }
        Cmd::Proxy { .. } | Cmd::Hook { .. } | Cmd::Ui | Cmd::Watch { .. } => unreachable!(),
        Cmd::Agents { json } => bus::agents(&mut conn, json).await?,
        Cmd::Tail { follow, lines } => bus::tail(conn, follow, lines).await?,
        Cmd::Msg { to, text, from } => bus::msg(&mut conn, &from, &to, &text.join(" ")).await?,
        Cmd::Inbox { name, wait, peek } => bus::inbox(&mut conn, &name, wait, peek).await?,
        Cmd::Questions { all } => bus::questions(&mut conn, all).await?,
        Cmd::Answer {
            id,
            text,
            by,
            literal,
        } => bus::answer(&mut conn, id, &text.join(" "), &by, literal).await?,
        Cmd::Ask {
            text,
            from,
            options,
            wait,
        } => bus::ask(&mut conn, from, &text.join(" "), options, wait).await?,
        Cmd::Status { name, status, text } => {
            let text = (!text.is_empty()).then(|| text.join(" "));
            conn.request(&Request::SetStatus { name, status, text })
                .await?;
        }
    }
    Ok(())
}

/// Bus agents name orch panes `orch:pN`; the pane API wants `pN`.
fn strip_orch(pane: String) -> String {
    match pane.strip_prefix("orch:") {
        Some(id) => id.to_string(),
        None => pane,
    }
}

/// Variables that describe the *calling* session, not the new pane: another multiplexer, or
/// a Claude Code session (its child-session marker would switch off transcripts in the pane).
const SESSION_VARS: &[&str] = &[
    "ORCH_PANE",
    "STY",
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
];

/// This shell's environment minus variables that belong to the calling session.
pub(crate) fn client_env(extra: &[String]) -> Result<Vec<(String, String)>> {
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| {
            !(k.starts_with("HERDR_")
                || k.starts_with("TMUX")
                || SESSION_VARS.contains(&k.as_str()))
        })
        .collect();
    for kv in extra {
        let (k, v) = kv.split_once('=').context("--env needs KEY=VALUE")?;
        env.retain(|(ek, _)| ek != k);
        env.push((k.to_string(), v.to_string()));
    }
    Ok(env)
}

fn key_bytes(name: &str) -> Option<&'static str> {
    Some(match name {
        "Enter" | "enter" | "CR" => "\r",
        "Escape" | "Esc" | "esc" => "\x1b",
        "Tab" | "tab" => "\t",
        "BSpace" | "Backspace" => "\x7f",
        "Space" | "space" => " ",
        "Up" | "up" => "\x1b[A",
        "Down" | "down" => "\x1b[B",
        "Right" | "right" => "\x1b[C",
        "Left" | "left" => "\x1b[D",
        "C-c" => "\x03",
        "C-d" => "\x04",
        "C-z" => "\x1a",
        "C-l" => "\x0c",
        "C-u" => "\x15",
        "C-r" => "\x12",
        "C-o" => "\x0f",
        "S-Tab" => "\x1b[Z",
        _ => return None,
    })
}

struct CliRest {
    cmd: Cmd,
}

/// Opens connections to orchd: the local socket (starting orchd if needed) or `ssh host orch
/// proxy`. The TUI opens several (events, one per visible pane, requests).
/// Remote: ONE `ssh host orch proxy --mux`, shared by all connections of this process.
#[derive(Clone)]
pub(crate) struct Connector {
    socket: PathBuf,
    host: Option<String>,
    mux: std::sync::Arc<tokio::sync::OnceCell<orch_proto::mux::MuxClient>>,
}

impl Connector {
    fn new(socket: PathBuf, host: Option<String>) -> Self {
        Self {
            socket,
            host,
            mux: Default::default(),
        }
    }

    /// The local orchd socket (None when talking to another machine).
    pub(crate) fn local_socket(&self) -> Option<&std::path::Path> {
        self.host.is_none().then_some(self.socket.as_path())
    }

    pub(crate) async fn connect(&self) -> Result<Conn> {
        let Some(host) = self.host.as_deref() else {
            return connect(&self.socket, None).await;
        };
        let mux = self
            .mux
            .get_or_try_init(|| async {
                let (r, w) = ssh_stdio(host, &["proxy", "--mux"])?;
                Ok::<_, anyhow::Error>(orch_proto::mux::MuxClient::new(r, w))
            })
            .await?;
        Ok(mux.open()?)
    }
}

/// `$ORCH_SSH host $ORCH_REMOTE_BIN <args>` with piped stdio.
fn ssh_stdio(
    host: &str,
    args: &[&str],
) -> Result<(tokio::process::ChildStdout, tokio::process::ChildStdin)> {
    let remote_bin = std::env::var("ORCH_REMOTE_BIN").unwrap_or_else(|_| "orch".into());
    // $ORCH_SSH = ssh command with options, e.g. "ssh -p 2222" (default "ssh").
    let ssh = std::env::var("ORCH_SSH").unwrap_or_else(|_| "ssh".into());
    let mut ssh = ssh.split_whitespace();
    let mut child = tokio::process::Command::new(ssh.next().unwrap_or("ssh"))
        .args(ssh)
        .args(["-T", "-o", "BatchMode=yes", host, &remote_bin])
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting ssh")?;
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    // The child is reaped when this process exits.
    std::mem::forget(child);
    Ok((stdout, stdin))
}

async fn connect(socket: &Path, host: Option<&str>) -> Result<Conn> {
    if let Some(host) = host {
        let remote_bin = std::env::var("ORCH_REMOTE_BIN").unwrap_or_else(|_| "orch".into());
        // $ORCH_SSH = ssh command with options, e.g. "ssh -p 2222" (default "ssh").
        let ssh = std::env::var("ORCH_SSH").unwrap_or_else(|_| "ssh".into());
        let mut ssh = ssh.split_whitespace();
        let mut child = tokio::process::Command::new(ssh.next().unwrap_or("ssh"))
            .args(ssh)
            .args(["-T", "-o", "BatchMode=yes", host, &remote_bin, "proxy"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("starting ssh")?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        // The child is reaped when this process exits.
        std::mem::forget(child);
        return Ok(Conn::from_parts(Box::new(stdout), Box::new(stdin)));
    }
    orch_proto::client::connect_or_start(socket)
        .await
        .map_err(|e| {
            exit::CliError::Unreachable(format!("connecting to orchd at {}: {e}", socket.display()))
        })
        .map_err(Into::into)
}

async fn proxy(socket: &Path, mux: bool) -> Result<()> {
    let conn = connect(socket, None).await;
    drop(conn); // make sure orchd runs; then use a raw byte pipe
    if mux {
        let socket = socket.to_path_buf();
        orch_proto::mux::serve(tokio::io::stdin(), tokio::io::stdout(), move || {
            let s = socket.clone();
            async move { tokio::net::UnixStream::connect(s).await }
        })
        .await?;
        return Ok(());
    }
    let stream = tokio::net::UnixStream::connect(socket).await?;
    let (mut sr, mut sw) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let up = async {
        let _ = tokio::io::copy(&mut stdin, &mut sw).await;
        let _ = tokio::io::AsyncWriteExt::shutdown(&mut sw).await;
    };
    let down = async {
        let _ = tokio::io::copy(&mut sr, &mut stdout).await;
    };
    tokio::select! {
        _ = down => {}
        _ = async { up.await; std::future::pending::<()>().await } => {}
    }
    Ok(())
}
