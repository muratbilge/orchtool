//! orchd — the orchtool daemon. Usually started by `orch` on first use (or by systemd --user).
//!
//! Usage: orchd [--socket PATH] [--scrollback LINES]

use orchd::server::{serve, Daemon};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};

const DEFAULT_SCROLLBACK: usize = 3000;

fn usage() -> ! {
    eprintln!("usage: orchd [--socket PATH] [--scrollback LINES]");
    std::process::exit(2);
}

#[tokio::main(worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let mut socket = orch_proto::client::default_socket_path();
    let mut scrollback = std::env::var("ORCHD_SCROLLBACK")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SCROLLBACK);
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

    let daemon = Daemon::new(scrollback);
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
    server.abort();
    daemon.kill_all();
    let _ = std::fs::remove_file(&socket);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    Ok(())
}
