//! One pane = one program in a PTY, its terminal state and its attached clients.
//!
//! Threads per pane: a reader (PTY output → vt100, query replies), a writer (all input goes
//! through one queue so keystrokes and replies never interleave mid-sequence) and a waiter
//! (exit status). The terminal lock is only held while parsing a chunk or rendering a frame.

use crate::term::Term;
use anyhow::{Context, Result};
use orch_proto::api::{NewPane, PaneInfo, PaneState, Role};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use tokio::sync::watch;

pub const DEFAULT_ROWS: u16 = 40;
pub const DEFAULT_COLS: u16 = 120;

pub struct Pane {
    pub id: String,
    pub name: Option<String>,
    pub argv: Vec<String>,
    pub cwd: String,
    pub created_unix: u64,
    pub pid: Option<u32>,
    pub term: Mutex<Term>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    input: mpsc::Sender<Vec<u8>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// Bumped on every screen change.
    rev: watch::Sender<u64>,
    /// Bumped on every resize (forces full redraws).
    gen: AtomicU64,
    /// `Some(code)` once the program exited.
    exit: watch::Sender<Option<Option<u32>>>,
    /// Which client controls input + size.
    controller: watch::Sender<Option<u64>>,
    clients: Mutex<BTreeSet<u64>>,
    next_client: AtomicU64,
}

impl Pane {
    pub fn spawn(id: String, req: NewPane, scrollback: usize) -> Result<Arc<Pane>> {
        let rows = req.rows.unwrap_or(DEFAULT_ROWS).max(2);
        let cols = req.cols.unwrap_or(DEFAULT_COLS).max(2);
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;
        let mut cmd = if req.argv.is_empty() {
            CommandBuilder::new_default_prog()
        } else {
            CommandBuilder::from_argv(req.argv.iter().map(Into::into).collect())
        };
        let cwd = req
            .cwd
            .clone()
            .or_else(|| std::env::var("HOME").ok())
            .unwrap_or_else(|| "/".into());
        if !std::path::Path::new(&cwd).is_dir() {
            anyhow::bail!("cwd is not a directory: {cwd}");
        }
        cmd.cwd(&cwd);
        if let Some(env) = &req.env {
            cmd.env_clear();
            for (k, v) in env {
                cmd.env(k, v);
            }
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("ORCH_PANE", &id);
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawn {:?}", req.argv))?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().context("pty reader")?;
        let mut writer = pair.master.take_writer().context("pty writer")?;
        let (input, input_rx) = mpsc::channel::<Vec<u8>>();
        let argv = if req.argv.is_empty() {
            vec![std::env::var("SHELL").unwrap_or_else(|_| "sh".into())]
        } else {
            req.argv.clone()
        };
        let pane = Arc::new(Pane {
            id,
            name: req.name.clone(),
            argv,
            cwd,
            created_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            pid: child.process_id(),
            term: Mutex::new(Term::new(rows, cols, scrollback)),
            master: Mutex::new(pair.master),
            input,
            killer: Mutex::new(child.clone_killer()),
            rev: watch::Sender::new(0),
            gen: AtomicU64::new(0),
            exit: watch::Sender::new(None),
            controller: watch::Sender::new(None),
            clients: Mutex::new(BTreeSet::new()),
            next_client: AtomicU64::new(1),
        });

        std::thread::Builder::new()
            .name(format!("{}-in", pane.id))
            .spawn(move || {
                for bytes in input_rx {
                    if writer
                        .write_all(&bytes)
                        .and_then(|_| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            })?;

        let p = Arc::clone(&pane);
        std::thread::Builder::new()
            .name(format!("{}-out", pane.id))
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let replies = p.term.lock().unwrap().process(&buf[..n]);
                            if !replies.is_empty() {
                                let _ = p.input.send(replies);
                            }
                            p.rev.send_modify(|r| *r += 1);
                        }
                    }
                }
            })?;

        let p = Arc::clone(&pane);
        std::thread::Builder::new()
            .name(format!("{}-wait", pane.id))
            .spawn(move || {
                let code = child.wait().ok().map(|s| s.exit_code());
                // Let the reader drain the last output before announcing the exit.
                std::thread::sleep(std::time::Duration::from_millis(50));
                p.exit.send_replace(Some(code));
                p.rev.send_modify(|r| *r += 1);
            })?;
        Ok(pane)
    }

    pub fn write(&self, bytes: Vec<u8>) {
        let _ = self.input.send(bytes);
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        let rows = rows.max(2);
        let cols = cols.max(2);
        if self.size() == (rows, cols) {
            return;
        }
        let _ = self.master.lock().unwrap().resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.term.lock().unwrap().set_size(rows, cols);
        self.gen.fetch_add(1, Ordering::SeqCst);
        self.rev.send_modify(|r| *r += 1);
    }

    pub fn size(&self) -> (u16, u16) {
        self.term.lock().unwrap().size()
    }

    pub fn gen(&self) -> u64 {
        self.gen.load(Ordering::SeqCst)
    }

    pub fn kill(&self) {
        let _ = self.killer.lock().unwrap().kill();
    }

    pub fn exit_code(&self) -> Option<Option<u32>> {
        *self.exit.borrow()
    }

    pub fn subscribe_rev(&self) -> watch::Receiver<u64> {
        self.rev.subscribe()
    }

    pub fn subscribe_exit(&self) -> watch::Receiver<Option<Option<u32>>> {
        self.exit.subscribe()
    }

    pub fn subscribe_controller(&self) -> watch::Receiver<Option<u64>> {
        self.controller.subscribe()
    }

    /// Register an attached client; returns its id and initial role.
    pub fn attach(&self, take_control: Option<bool>) -> (u64, Role) {
        let id = self.next_client.fetch_add(1, Ordering::SeqCst);
        self.clients.lock().unwrap().insert(id);
        let role = match take_control {
            Some(true) => {
                self.controller.send_replace(Some(id));
                Role::Controller
            }
            Some(false) => Role::Observer,
            None => {
                let mut became = false;
                self.controller.send_if_modified(|c| {
                    if c.is_none() {
                        *c = Some(id);
                        became = true;
                    }
                    became
                });
                if became {
                    Role::Controller
                } else {
                    Role::Observer
                }
            }
        };
        (id, role)
    }

    pub fn take_control(&self, client: u64) {
        self.controller.send_replace(Some(client));
    }

    pub fn is_controller(&self, client: u64) -> bool {
        *self.controller.borrow() == Some(client)
    }

    pub fn detach(&self, client: u64) {
        self.clients.lock().unwrap().remove(&client);
        self.controller.send_if_modified(|c| {
            if *c == Some(client) {
                *c = None;
                true
            } else {
                false
            }
        });
    }

    pub fn info(&self) -> PaneInfo {
        let (rows, cols, title) = {
            let t = self.term.lock().unwrap();
            let (r, c) = t.size();
            (r, c, t.title().to_string())
        };
        let exit = self.exit_code();
        PaneInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            argv: self.argv.clone(),
            cwd: self.cwd.clone(),
            pid: self.pid,
            state: if exit.is_some() {
                PaneState::Exited
            } else {
                PaneState::Running
            },
            exit_code: exit.flatten(),
            rows,
            cols,
            clients: self.clients.lock().unwrap().len(),
            title,
            created_unix: self.created_unix,
        }
    }
}
