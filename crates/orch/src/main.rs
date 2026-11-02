//! orch — CLI for orchd: create, list, attach, drive and read panes; `orch proxy` for SSH.

mod attach;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use orch_proto::api::{
    AttachMode, NewPane, PaneInfo, PaneState, ReadPane, ReadSource, Request, SendPane,
};
use orch_proto::client::{default_socket_path, state_dir, Conn};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "orch", version, about = "orchtool: agent workspace manager")]
struct Cli {
    /// orchd socket (default $ORCHD_SOCKET or ~/.local/state/orchtool/orchd.sock)
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Talk to orchd on another machine through `ssh HOST orch proxy` (or $ORCH_HOST)
    #[arg(long, global = true, env = "ORCH_HOST")]
    host: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
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
    },
    /// Print a pane's text
    Read {
        pane: String,
        #[arg(long, default_value_t = 60)]
        lines: usize,
        /// Only the visible screen
        #[arg(long)]
        visible: bool,
    },
    /// Kill a pane's program (or remove an exited pane)
    Kill { pane: String },
    /// stdio ⇄ orchd bridge, used by `orch --host H …` via ssh
    Proxy,
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
    StopDaemon,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("orch: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let socket = cli.socket.clone().unwrap_or_else(default_socket_path);
    if let Cmd::Proxy = cli.cmd {
        return proxy(&socket).await;
    }
    let mut conn = connect(&socket, cli.host.as_deref()).await?;
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
        } => {
            let mut text = text.unwrap_or_default();
            for k in &keys {
                text.push_str(key_bytes(k).with_context(|| format!("unknown key {k:?}"))?);
            }
            conn.request(&Request::Send(SendPane { pane, text, enter }))
                .await?;
        }
        Cmd::Read {
            pane,
            lines,
            visible,
        } => {
            let v = conn
                .request(&Request::Read(ReadPane {
                    pane,
                    lines: Some(lines),
                    source: if visible {
                        ReadSource::Visible
                    } else {
                        ReadSource::Recent
                    },
                }))
                .await?;
            println!("{}", v["text"].as_str().unwrap_or_default());
        }
        Cmd::Kill { pane } => {
            conn.request(&Request::Kill { pane }).await?;
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
        Cmd::StopDaemon => {
            conn.request(&Request::StopDaemon {}).await?;
        }
        Cmd::Proxy => unreachable!(),
    }
    Ok(())
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
fn client_env(extra: &[String]) -> Result<Vec<(String, String)>> {
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
    match Conn::unix(socket).await {
        Ok(c) => Ok(c),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            start_daemon(socket)?;
            for _ in 0..50 {
                tokio::time::sleep(Duration::from_millis(60)).await;
                if let Ok(c) = Conn::unix(socket).await {
                    return Ok(c);
                }
            }
            bail!(
                "orchd did not start; see {}",
                state_dir().join("orchd.log").display()
            )
        }
        Err(e) => Err(e).with_context(|| format!("connecting to {}", socket.display())),
    }
}

/// Start orchd detached from this terminal (own session), logging to the state dir.
fn start_daemon(socket: &Path) -> Result<()> {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("orchd")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("orchd"));
    let dir = state_dir();
    std::fs::create_dir_all(&dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("orchd.log"))?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("--socket")
        .arg(socket)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .env_remove("ORCH_PANE");
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()
        .with_context(|| format!("starting {}", exe.display()))?;
    Ok(())
}

async fn proxy(socket: &Path) -> Result<()> {
    let conn = connect(socket, None).await;
    drop(conn); // make sure orchd runs; then use a raw byte pipe
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
