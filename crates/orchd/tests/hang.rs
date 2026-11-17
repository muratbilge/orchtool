//! orchd must not hang: ListPanes racing the limit check deadlocked both runtime workers
//! (opposite lock order of `panes` and the limits state), and a hang must show in the log.

use orch_proto::api::{NewPane, Request};
use orch_proto::client::Conn;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct D {
    child: Child,
    socket: PathBuf,
    dir: PathBuf,
}

impl Drop for D {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl D {
    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("orchd.log")).unwrap_or_default()
    }

    fn signal(&self, sig: i32) {
        // SAFETY: plain kill(2) to our own child.
        assert_eq!(unsafe { libc::kill(self.child.id() as i32, sig) }, 0);
    }

    /// Waits for the exit; `None` if it is still running after `limit`.
    fn wait_exit(&mut self, limit: Duration) -> Option<std::process::ExitStatus> {
        let start = Instant::now();
        while start.elapsed() < limit {
            if let Some(s) = self.child.try_wait().unwrap() {
                return Some(s);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    async fn wait_log(&self, what: &str, limit: Duration) {
        let start = Instant::now();
        while start.elapsed() < limit {
            if self.log().contains(what) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("no {what:?} in the log:\n{}", self.log());
    }
}

async fn daemon(tag: &str, env: &[(&str, &str)]) -> D {
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "orch-hang-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let socket = dir.join("s.sock");
    let log = std::fs::File::create(dir.join("orchd.log")).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_orchd"));
    cmd.arg("--socket")
        .arg(&socket)
        .arg("--db")
        .arg(dir.join("db"))
        .env("HOME", dir.join("home"))
        .env_remove("ORCH_HOST")
        .env_remove("ORCHD_LIMIT_CHECK_MS")
        .env_remove("ORCHD_WATCHDOG_STALL_MS")
        .env_remove("ORCHD_TEST_STALL_RUNTIME_MS")
        .stdout(Stdio::null())
        .stderr(log);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let d = D {
        child: cmd.spawn().unwrap(),
        socket,
        dir,
    };
    conn(&d.socket).await;
    d
}

async fn conn(socket: &Path) -> Conn {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_panes_racing_the_limit_check_never_hangs() {
    // The limit check runs every millisecond: it takes the limits lock, then the pane list.
    let mut d = daemon("lock-order", &[("ORCHD_LIMIT_CHECK_MS", "1")]).await;
    let mut c = conn(&d.socket).await;
    for _ in 0..6 {
        c.request(&Request::NewPane(NewPane {
            argv: vec!["sleep".into(), "600".into()],
            cwd: Some("/tmp".into()),
            ..Default::default()
        }))
        .await
        .unwrap();
    }
    const PER_CONN: usize = 700;
    let mut jobs = Vec::new();
    for _ in 0..3 {
        let socket = d.socket.clone();
        jobs.push(tokio::spawn(async move {
            let mut c = conn(&socket).await;
            for i in 0..PER_CONN {
                let r =
                    tokio::time::timeout(Duration::from_secs(5), c.request(&Request::ListPanes {}))
                        .await;
                match r {
                    Ok(Ok(v)) => assert_eq!(v["panes"].as_array().map(Vec::len), Some(6)),
                    Ok(Err(e)) => panic!("request {i}: {e:?}"),
                    Err(_) => return Err(i),
                }
            }
            Ok(())
        }));
    }
    for job in jobs {
        if let Err(i) = job.await.unwrap() {
            // Let the timeout show plainly instead of hanging the test on Drop.
            d.signal(libc::SIGKILL);
            panic!("ListPanes request {i} got no answer in 5 s: orchd hangs");
        }
    }
    let ping = tokio::time::timeout(Duration::from_secs(5), c.request(&Request::Ping {})).await;
    assert!(matches!(ping, Ok(Ok(_))), "ping after the load: {ping:?}");
    d.signal(libc::SIGTERM);
    let status = d.wait_exit(Duration::from_secs(10));
    assert!(status.is_some(), "SIGTERM did not stop orchd:\n{}", d.log());
    assert!(d.log().contains("orchd: stopping"), "{}", d.log());
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stuck_runtime_is_logged_and_its_recovery_too() {
    // Both workers blocked for 4 s, one second after start; stuck counts after 1.5 s.
    let d = daemon(
        "watchdog",
        &[
            ("ORCHD_WATCHDOG_STALL_MS", "1500"),
            ("ORCHD_TEST_STALL_RUNTIME_MS", "4000"),
        ],
    )
    .await;
    d.wait_log("orchd: runtime not responding for", Duration::from_secs(10))
        .await;
    d.wait_log("orchd: runtime responding again", Duration::from_secs(15))
        .await;
    let log = d.log();
    assert_eq!(
        log.matches("not responding").count(),
        1,
        "one line per minute at most:\n{log}"
    );
    assert!(log.contains("(possible deadlock)"), "{log}");
    conn(&d.socket).await;
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_ends_orchd_even_with_a_stuck_runtime() {
    let mut d = daemon(
        "watchdog-term",
        &[
            ("ORCHD_WATCHDOG_STALL_MS", "1500"),
            ("ORCHD_TEST_STALL_RUNTIME_MS", "60000"),
        ],
    )
    .await;
    d.wait_log("orchd: runtime not responding for", Duration::from_secs(10))
        .await;
    d.signal(libc::SIGTERM);
    let status = d.wait_exit(Duration::from_secs(15));
    let log = d.log();
    let status = status.unwrap_or_else(|| panic!("orchd ignored SIGTERM:\n{log}"));
    assert_eq!(status.code(), Some(1), "{log}");
    assert!(log.contains("stop signal not handled"), "{log}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_healthy_sigterm_stops_normally() {
    // The watchdog's own signal flag must not get in the way of the graceful stop.
    let mut d = daemon("watchdog-ok", &[("ORCHD_WATCHDOG_STALL_MS", "3000")]).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    d.signal(libc::SIGTERM);
    let status = d.wait_exit(Duration::from_secs(10)).expect("stopped");
    let log = d.log();
    assert!(status.success(), "{log}");
    assert!(log.contains("orchd: stopping"), "{log}");
    assert!(!log.contains("not responding"), "{log}");
}
