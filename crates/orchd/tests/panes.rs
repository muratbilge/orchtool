//! End-to-end: a real orchd process on a temp socket, driven through the protocol client.

use base64::Engine as _;
use orch_proto::api::{
    AttachMode, AttachPane, NewPane, ReadPane, ReadSource, Request, Role, SendPane, Stream,
};
use orch_proto::client::{reply_result, Conn, FrameReader, FrameWriter};
use serde_json::Value;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;
use tokio::time::timeout;

struct Daemon {
    child: Child,
    socket: PathBuf,
    _dir: TempDir,
}

struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn daemon() -> Daemon {
    let dir = std::env::temp_dir().join(format!(
        "orchd-test-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("s.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_orchd"))
        .arg("--socket")
        .arg(&socket)
        .arg("--scrollback")
        .arg("500")
        .arg("--db")
        .arg(dir.join("bus.db"))
        .arg("--no-telegram")
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if Conn::unix(&socket).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Daemon {
        child,
        socket,
        _dir: TempDir(dir),
    }
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst) * 1000
        + std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_micros() as u64
}

async fn new_pane(conn: &mut Conn, argv: &[&str], rows: u16, cols: u16) -> String {
    let v = conn
        .request(&Request::NewPane(NewPane {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: Some("/tmp".into()),
            rows: Some(rows),
            cols: Some(cols),
            ..Default::default()
        }))
        .await
        .unwrap();
    v["id"].as_str().unwrap().to_string()
}

async fn read(conn: &mut Conn, pane: &str, source: ReadSource) -> String {
    let v = conn
        .request(&Request::Read(ReadPane {
            pane: pane.into(),
            lines: Some(100),
            source,
        }))
        .await
        .unwrap();
    v["text"].as_str().unwrap().to_string()
}

async fn wait_for_text(conn: &mut Conn, pane: &str, needle: &str) -> String {
    for _ in 0..100 {
        let t = read(conn, pane, ReadSource::Recent).await;
        if t.contains(needle) {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("{needle:?} never appeared in {pane}");
}

/// A test client that mirrors the stream into its own vt100 screen.
struct Viewer {
    reader: FrameReader,
    writer: FrameWriter,
    screen: vt100::Parser,
    rev: u64,
    role: Role,
    exited: Option<Option<u32>>,
}

impl Viewer {
    async fn attach(
        socket: &std::path::Path,
        pane: &str,
        mode: AttachMode,
        rows: u16,
        cols: u16,
    ) -> Self {
        let conn = Conn::unix(socket).await.unwrap();
        let (mut reader, mut writer) = conn.into_parts();
        let req = Request::Attach(AttachPane {
            pane: pane.into(),
            mode,
            rows,
            cols,
        });
        writer
            .send_body("a", serde_json::to_value(&req).unwrap())
            .await
            .unwrap();
        let first = reader.next().await.unwrap().unwrap();
        let result = reply_result(first.body).unwrap();
        let role = serde_json::from_value(result["role"].clone()).unwrap();
        Self {
            reader,
            writer,
            screen: vt100::Parser::new(
                NonZeroU16::new(rows).unwrap(),
                NonZeroU16::new(cols).unwrap(),
                0,
            ),
            rev: 0,
            role,
            exited: None,
        }
    }

    /// Process frames until `pred(screen text)` holds.
    async fn until(&mut self, what: &str, pred: impl Fn(&str) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !pred(&self.screen.screen().contents()) {
            let f = timeout(deadline - tokio::time::Instant::now(), self.reader.next())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "timeout waiting for {what}; screen:\n{}",
                        self.screen.screen().contents()
                    )
                })
                .unwrap()
                .expect("stream closed");
            self.apply(f.body);
        }
    }

    fn apply(&mut self, body: Value) {
        match serde_json::from_value::<Stream>(body).unwrap() {
            Stream::Screen {
                base,
                rev,
                full,
                data,
                ..
            } => {
                assert!(
                    full || base == self.rev,
                    "diff base {base} but client has {}",
                    self.rev
                );
                self.screen.process(data.as_bytes());
                self.rev = rev;
            }
            Stream::Role { role } => self.role = role,
            Stream::Exit { code } => self.exited = Some(code),
            Stream::Timeline { .. } => {}
        }
    }

    async fn wait_role(&mut self, role: Role) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while self.role != role {
            let f = timeout(deadline - tokio::time::Instant::now(), self.reader.next())
                .await
                .expect("role frame")
                .unwrap()
                .unwrap();
            self.apply(f.body);
        }
    }

    async fn send(&mut self, req: Request) {
        self.writer
            .send_body("a", serde_json::to_value(req).unwrap())
            .await
            .unwrap();
    }

    async fn type_keys(&mut self, text: &str) {
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(text);
        self.send(Request::Input { data_b64 }).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn echo_hi_is_readable_and_exit_is_reported() {
    let d = daemon().await;
    let mut c = Conn::unix(&d.socket).await.unwrap();
    let id = new_pane(&mut c, &["sh", "-c", "echo hi; exit 3"], 10, 40).await;
    wait_for_text(&mut c, &id, "hi").await;
    for _ in 0..100 {
        let v = c.request(&Request::ListPanes {}).await.unwrap();
        if v["panes"][0]["state"] == "exited" {
            assert_eq!(v["panes"][0]["exit_code"], 3);
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("exit never reported");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_with_enter_reaches_the_program() {
    let d = daemon().await;
    let mut c = Conn::unix(&d.socket).await.unwrap();
    let id = new_pane(&mut c, &["cat"], 10, 40).await;
    c.request(&Request::Send(SendPane {
        pane: id.clone(),
        text: "hello cat".into(),
        enter: true,
    }))
    .await
    .unwrap();
    let t = wait_for_text(&mut c, &id, "hello cat\nhello cat").await;
    assert!(t.starts_with("hello cat"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_mirror_and_detach_keeps_state() {
    let d = daemon().await;
    let mut c = Conn::unix(&d.socket).await.unwrap();
    let id = new_pane(&mut c, &["cat"], 12, 50).await;
    let mut a = Viewer::attach(&d.socket, &id, AttachMode::Auto, 12, 50).await;
    let mut b = Viewer::attach(&d.socket, &id, AttachMode::Auto, 12, 50).await;
    assert_eq!(a.role, Role::Controller);
    assert_eq!(b.role, Role::Observer);
    a.type_keys("mirror me\r").await;
    a.until("echo on a", |s| s.contains("mirror me\nmirror me"))
        .await;
    b.until("echo on b", |s| s.contains("mirror me\nmirror me"))
        .await;
    assert_eq!(a.screen.screen().contents(), b.screen.screen().contents());

    // Observer input is ignored.
    b.type_keys("ignored\r").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!read(&mut c, &id, ReadSource::Visible)
        .await
        .contains("ignored"));

    // Detach both, the program keeps running and state survives.
    drop(a);
    drop(b);
    c.request(&Request::Send(SendPane {
        pane: id.clone(),
        text: "while detached".into(),
        enter: true,
    }))
    .await
    .unwrap();
    wait_for_text(&mut c, &id, "while detached\nwhile detached").await;
    let mut again = Viewer::attach(&d.socket, &id, AttachMode::Auto, 12, 50).await;
    assert_eq!(
        again.role,
        Role::Controller,
        "controller slot freed on detach"
    );
    again
        .until("snapshot after reattach", |s| {
            s.contains("mirror me") && s.contains("while detached")
        })
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn takeover_switches_roles_and_controller_sets_size() {
    let d = daemon().await;
    let mut c = Conn::unix(&d.socket).await.unwrap();
    let id = new_pane(&mut c, &["cat"], 10, 40).await;
    let mut a = Viewer::attach(&d.socket, &id, AttachMode::Auto, 20, 70).await;
    a.until("first frame", |_| true).await;
    let size = |v: &Value| (v["panes"][0]["rows"].clone(), v["panes"][0]["cols"].clone());
    let v = c.request(&Request::ListPanes {}).await.unwrap();
    assert_eq!(
        size(&v),
        (20.into(), 70.into()),
        "controller's size applied"
    );

    // Observer with a small terminal does not shrink the pane.
    let mut b = Viewer::attach(&d.socket, &id, AttachMode::Observe, 8, 30).await;
    b.until("observer frame", |_| true).await;
    let v = c.request(&Request::ListPanes {}).await.unwrap();
    assert_eq!(size(&v), (20.into(), 70.into()));

    b.send(Request::Takeover {}).await;
    b.wait_role(Role::Controller).await;
    let v = c.request(&Request::ListPanes {}).await.unwrap();
    assert_eq!(
        size(&v),
        (8.into(), 30.into()),
        "new controller's size applied"
    );
    // The old controller is told it is an observer now.
    a.wait_role(Role::Observer).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resize_storm_ends_consistent() {
    let d = daemon().await;
    let mut c = Conn::unix(&d.socket).await.unwrap();
    // Continuous output while the controller resizes repeatedly.
    let id = new_pane(
        &mut c,
        &[
            "sh",
            "-c",
            "i=0; while [ $i -lt 400 ]; do echo line $i; i=$((i+1)); done; sleep 5",
        ],
        10,
        40,
    )
    .await;
    let mut a = Viewer::attach(&d.socket, &id, AttachMode::Control, 10, 40).await;
    for (r, col) in [(12, 50), (8, 30), (15, 60), (10, 40)] {
        a.send(Request::Resize { rows: r, cols: col }).await;
        a.screen
            .set_size(NonZeroU16::new(r).unwrap(), NonZeroU16::new(col).unwrap());
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    a.until("last line", |s| s.contains("line 399")).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    // Drain whatever is pending, then compare with the server's own view.
    while let Ok(Some(f)) = timeout(Duration::from_millis(150), a.reader.next())
        .await
        .map(|r| r.unwrap())
    {
        a.apply(f.body);
    }
    let server = read(&mut c, &id, ReadSource::Visible).await;
    let client = a.screen.screen().contents();
    assert_eq!(client.trim_end(), server.trim_end());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_queries_are_answered_without_a_client() {
    let d = daemon().await;
    let mut c = Conn::unix(&d.socket).await.unwrap();
    // The program asks for the cursor position and prints the reply it got.
    let id = new_pane(
        &mut c,
        &[
            "sh",
            "-c",
            "stty -icanon -echo min 1; printf 'ab\\033[6n'; r=$(dd bs=1 count=6 2>/dev/null); printf '\\ngot:%s\\n' \"${r#?}\"; sleep 2",
        ],
        10,
        40,
    )
    .await;
    wait_for_text(&mut c, &id, "got:[1;3R").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_removes_pane_and_names_resolve() {
    let d = daemon().await;
    let mut c = Conn::unix(&d.socket).await.unwrap();
    c.request(&Request::NewPane(NewPane {
        name: Some("worker".into()),
        argv: vec!["sleep".into(), "100".into()],
        ..Default::default()
    }))
    .await
    .unwrap();
    let dup = c
        .request(&Request::NewPane(NewPane {
            name: Some("worker".into()),
            argv: vec!["sleep".into(), "100".into()],
            ..Default::default()
        }))
        .await;
    assert!(dup.is_err(), "duplicate running name rejected");
    c.request(&Request::Kill {
        pane: "worker".into(),
    })
    .await
    .unwrap();
    let v = c.request(&Request::ListPanes {}).await.unwrap();
    assert_eq!(v["panes"].as_array().unwrap().len(), 0);
    let missing = c
        .request(&Request::Read(ReadPane {
            pane: "worker".into(),
            lines: None,
            source: ReadSource::Visible,
        }))
        .await;
    assert!(missing.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_escalates_for_programs_ignoring_hup_and_term() {
    let d = daemon().await;
    let mut c = Conn::unix(&d.socket).await.unwrap();
    let id = new_pane(
        &mut c,
        &[
            "sh",
            "-c",
            "trap '' HUP TERM; echo ready; while :; do sleep 1; done",
        ],
        10,
        40,
    )
    .await;
    wait_for_text(&mut c, &id, "ready").await;
    c.request(&Request::Kill { pane: id.clone() })
        .await
        .unwrap();
    let v = c.request(&Request::ListPanes {}).await.unwrap();
    assert_eq!(v["panes"].as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_is_bounded_when_the_program_does_not_read() {
    let d = daemon().await;
    let mut c = Conn::unix(&d.socket).await.unwrap();
    // Raw mode: a full PTY input buffer then blocks the writer (in canonical mode Linux
    // discards input past its line buffer instead, so nothing would pile up).
    let id = new_pane(
        &mut c,
        &["sh", "-c", "stty raw -echo; echo ready; sleep 30"],
        10,
        40,
    )
    .await;
    wait_for_text(&mut c, &id, "ready").await;
    let chunk = "x".repeat(256 * 1024);
    let mut refused = false;
    for _ in 0..12 {
        let r = c
            .request(&Request::Send(SendPane {
                pane: id.clone(),
                text: chunk.clone(),
                enter: false,
            }))
            .await;
        if let Err(orch_proto::client::ClientError::Remote(e)) = r {
            assert_eq!(e.code, "input_full");
            refused = true;
            break;
        }
    }
    assert!(refused, "3 MB into a non-reading program must be refused");
    // orchd is still responsive.
    c.request(&Request::Ping {}).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attached_client_sees_last_output_before_exit() {
    let d = daemon().await;
    let mut c = Conn::unix(&d.socket).await.unwrap();
    let id = new_pane(
        &mut c,
        &["sh", "-c", "sleep 0.3; seq 1 3000; echo LAST-LINE"],
        10,
        40,
    )
    .await;
    let mut a = Viewer::attach(&d.socket, &id, AttachMode::Auto, 10, 40).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while a.exited.is_none() {
        let f = timeout(deadline - tokio::time::Instant::now(), a.reader.next())
            .await
            .expect("exit frame")
            .unwrap()
            .expect("stream open until exit");
        a.apply(f.body);
    }
    assert!(a.screen.screen().contents().contains("LAST-LINE"));
    assert_eq!(a.exited, Some(Some(0)));
}
