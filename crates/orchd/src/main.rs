//! orchd — the orchtool daemon. Usually started by `orch` on first use (or by systemd --user).
//!
//! Usage: orchd [--socket PATH] [--db PATH] [--scrollback LINES]

use orchd::bus::Bus;
use orchd::server::{serve, start_nudger, Daemon};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};

const DEFAULT_SCROLLBACK: usize = 3000;

fn usage() -> ! {
    eprintln!("usage: orchd [--socket PATH] [--db PATH] [--scrollback LINES]");
    std::process::exit(2);
}

#[tokio::main(worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let mut socket = orch_proto::client::default_socket_path();
    let mut scrollback = std::env::var("ORCHD_SCROLLBACK")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SCROLLBACK);
    let mut db = orch_proto::client::state_dir().join("orchtool.db");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = PathBuf::from(args.next().unwrap_or_else(|| usage())),
            "--scrollback" => {
                scrollback = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| usage())
            }
            "--db" => db = PathBuf::from(args.next().unwrap_or_else(|| usage())),
            "--version" => {
                println!("orchd {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => usage(),
        }
    }

    let dir = socket
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    if socket.exists() {
        if tokio::net::UnixStream::connect(&socket).await.is_ok() {
            eprintln!("orchd: already running on {}", socket.display());
            std::process::exit(1);
        }
        std::fs::remove_file(&socket)?;
    }
    let listener = tokio::net::UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    eprintln!(
        "orchd {} pid {} listening on {} (scrollback {scrollback})",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        socket.display()
    );

    let bus = Arc::new(Bus::open(&db)?);
    let daemon = Daemon::new(scrollback, Arc::clone(&bus));
    start_nudger(Arc::clone(&daemon));
    orchd::ui::start_detector(Arc::clone(&daemon));
    orchd::limits::start(Arc::clone(&daemon));
    // After a restart: the layout the previous orchd saved, with fresh panes.
    let restore = dir.join("restore.json");
    let _ = daemon.restore_path.set(restore.clone());
    if let Ok(data) = std::fs::read(&restore) {
        // Read once: a layout that makes orchd fail must not come back on every start.
        let _ = std::fs::remove_file(&restore);
        match serde_json::from_slice::<orchd::restore::Saved>(&data) {
            Ok(saved) => {
                let n = daemon.restore_layout(saved);
                eprintln!("orchd: restored the layout with {n} pane(s)");
            }
            Err(e) => eprintln!("orchd: ignoring {}: {e}", restore.display()),
        }
    }
    let server = tokio::spawn(serve(listener, Arc::clone(&daemon)));
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;
    loop {
        tokio::select! {
            _ = daemon.shutdown.notified() => break,
            _ = term.recv() => break,
            _ = int.recv() => break,
            // Survive the controlling terminal going away (SSH logout).
            _ = hup.recv() => continue,
        }
    }
    eprintln!("orchd: stopping, killing panes");
    // The socket goes first: from now on a client finds no orchd and starts a new one, and
    // this one never touches the path again (it would unlink the new orchd's socket).
    let _ = std::fs::remove_file(&socket);
    server.abort();
    daemon.kill_all();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    Ok(())
}
