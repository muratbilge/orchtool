//! Many orch connections over ONE byte stream (one `ssh host orch proxy --mux`).
//!
//! The TUI needs several connections at once (events, one per visible pane, requests).
//! Opening an ssh session for each would be slow; instead the client multiplexes channels:
//!
//! frame = type:u8 | channel:u32 (BE) | length:u32 (BE) | payload
//! types: OPEN (client → proxy: connect channel to orchd), DATA, CLOSE (either side)
//!
//! Each channel carries the normal orch protocol unchanged, so orchd knows nothing of this.

use crate::client::{BoxRead, BoxWrite, Conn};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

const OPEN: u8 = 1;
const DATA: u8 = 2;
const CLOSE: u8 = 3;
const MAX_PAYLOAD: usize = 1 << 20;

type Frame = (u8, u32, Vec<u8>);

async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<Frame>> {
    let mut head = [0u8; 9];
    match r.read_exact(&mut head).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let ty = head[0];
    let chan = u32::from_be_bytes(head[1..5].try_into().unwrap());
    let len = u32::from_be_bytes(head[5..9].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mux frame too large",
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    Ok(Some((ty, chan, payload)))
}

fn encode(ty: u8, chan: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(9 + payload.len());
    out.push(ty);
    out.extend_from_slice(&chan.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Frames queued for the transport (each ≤ 32 KiB of data): bounded, so a stalled link
/// pushes back on the channel readers, and from there on orchd — whose screen sync then skips
/// to the latest state instead of piling up frames.
const OUT_QUEUE: usize = 128;
/// Chunks queued per channel towards its local side.
const CHAN_QUEUE: usize = 64;
/// A channel whose local side does not drain for this long is closed (it will reconnect and
/// resnapshot) instead of stalling every other channel.
const STALL: std::time::Duration = std::time::Duration::from_secs(5);

type Chans = Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>;

/// One writer task owns the stream; everything else sends encoded frames to it.
fn spawn_writer(mut w: impl AsyncWrite + Unpin + Send + 'static) -> mpsc::Sender<Vec<u8>> {
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(OUT_QUEUE);
    tokio::spawn(async move {
        while let Some(buf) = rx.recv().await {
            if w.write_all(&buf).await.is_err() {
                return;
            }
            // Batch whatever is queued before flushing.
            while let Ok(more) = rx.try_recv() {
                if w.write_all(&more).await.is_err() {
                    return;
                }
            }
            if w.flush().await.is_err() {
                return;
            }
        }
    });
    tx
}

/// Hand incoming DATA to its channel; a channel that stays full for STALL is closed.
async fn deliver(chans: &Chans, out: &mpsc::Sender<Vec<u8>>, chan: u32, payload: Vec<u8>) {
    let tx = chans.lock().unwrap().get(&chan).cloned();
    let Some(tx) = tx else { return };
    if tx.send_timeout(payload, STALL).await.is_err() {
        chans.lock().unwrap().remove(&chan);
        let _ = out.send(encode(CLOSE, chan, &[])).await;
    }
}

/// Bridge one channel to a local byte stream: its reads become DATA frames, incoming DATA
/// is written to it, EOF on either side becomes CLOSE.
fn bridge(
    chan: u32,
    stream: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    out: mpsc::Sender<Vec<u8>>,
    mut incoming: mpsc::Receiver<Vec<u8>>,
    on_done: impl FnOnce() + Send + 'static,
) {
    let (mut r, mut w) = tokio::io::split(stream);
    let out2 = out.clone();
    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // Awaiting here is the backpressure: we stop reading orchd.
                    if out2.send(encode(DATA, chan, &buf[..n])).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = out2.send(encode(CLOSE, chan, &[])).await;
    });
    tokio::spawn(async move {
        // Ends when the remote side closes the channel or the whole transport goes away
        // (every sender is dropped on teardown).
        while let Some(data) = incoming.recv().await {
            if w.write_all(&data).await.is_err() || w.flush().await.is_err() {
                break;
            }
        }
        let _ = w.shutdown().await;
        reader.abort();
        let _ = out.try_send(encode(CLOSE, chan, &[]));
        on_done();
    });
}

/// Proxy side (`orch proxy --mux`): every OPEN connects a new channel to orchd.
pub async fn serve<F, Fut>(
    input: impl AsyncRead + Unpin + Send + 'static,
    output: impl AsyncWrite + Unpin + Send + 'static,
    connect: F,
) -> io::Result<()>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = io::Result<tokio::net::UnixStream>> + Send + 'static,
{
    let mut input = input;
    let out = spawn_writer(output);
    let chans: Chans = Arc::default();
    let connect = Arc::new(connect);
    let result = loop {
        let frame = match read_frame(&mut input).await {
            Ok(Some(f)) => f,
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        };
        let (ty, chan, payload) = frame;
        match ty {
            OPEN => {
                let (tx, rx) = mpsc::channel(CHAN_QUEUE);
                chans.lock().unwrap().insert(chan, tx);
                let (out, chans, connect) = (out.clone(), Arc::clone(&chans), Arc::clone(&connect));
                tokio::spawn(async move {
                    match connect().await {
                        Ok(stream) => {
                            let chans2 = Arc::clone(&chans);
                            bridge(chan, stream, out, rx, move || {
                                chans2.lock().unwrap().remove(&chan);
                            })
                        }
                        Err(_) => {
                            chans.lock().unwrap().remove(&chan);
                            let _ = out.send(encode(CLOSE, chan, &[])).await;
                        }
                    }
                });
            }
            DATA => deliver(&chans, &out, chan, payload).await,
            CLOSE => {
                chans.lock().unwrap().remove(&chan);
            }
            _ => {}
        }
    };
    // Transport gone (EOF or error): close every channel, so every orchd connection ends
    // and releases what it holds (e.g. pane control).
    chans.lock().unwrap().clear();
    result
}

/// Client side: open channels over one stream (e.g. an ssh child's stdio).
#[derive(Clone)]
pub struct MuxClient {
    out: mpsc::Sender<Vec<u8>>,
    chans: Chans,
    next: Arc<AtomicU32>,
}

impl MuxClient {
    pub fn new(
        input: impl AsyncRead + Unpin + Send + 'static,
        output: impl AsyncWrite + Unpin + Send + 'static,
    ) -> Self {
        let out = spawn_writer(output);
        let chans: Chans = Arc::default();
        let (chans2, out2) = (Arc::clone(&chans), out.clone());
        tokio::spawn(async move {
            let mut input = input;
            while let Ok(Some((ty, chan, payload))) = read_frame(&mut input).await {
                match ty {
                    DATA => deliver(&chans2, &out2, chan, payload).await,
                    CLOSE => {
                        chans2.lock().unwrap().remove(&chan);
                    }
                    _ => {}
                }
            }
            // The transport is gone: close every channel.
            chans2.lock().unwrap().clear();
        });
        Self {
            out,
            chans,
            next: Arc::new(AtomicU32::new(1)),
        }
    }

    /// A new orch connection over the shared stream.
    pub fn open(&self) -> io::Result<Conn> {
        let chan = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel(CHAN_QUEUE);
        self.chans.lock().unwrap().insert(chan, tx);
        self.out.try_send(encode(OPEN, chan, &[])).map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "connection to host lost or busy")
        })?;
        let (app, mux_side) = tokio::io::duplex(256 * 1024);
        let chans = Arc::clone(&self.chans);
        bridge(chan, mux_side, self.out.clone(), rx, move || {
            chans.lock().unwrap().remove(&chan);
        });
        let (r, w) = tokio::io::split(app);
        let r: BoxRead = Box::new(r);
        let w: BoxWrite = Box::new(w);
        Ok(Conn::from_parts(r, w))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Request;

    /// A fake orchd that answers every request with {"result": {"pong": <id>}}.
    async fn fake_orchd(path: std::path::PathBuf) {
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        loop {
            let (s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let (r, w) = s.into_split();
                let mut reader = crate::client::FrameReader::new(Box::new(r));
                let mut writer = crate::client::FrameWriter::new(Box::new(w));
                while let Ok(Some(f)) = reader.next().await {
                    let body = serde_json::json!({ "result": { "pong": f.id } });
                    if writer.send_body(&f.id, body).await.is_err() {
                        return;
                    }
                }
            });
        }
    }

    #[tokio::test]
    async fn channels_are_independent_over_one_stream() {
        let dir = std::env::temp_dir().join(format!("orch-mux-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s.sock");
        tokio::spawn(fake_orchd(sock.clone()));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // client ⇄ (two pipes) ⇄ proxy
        let (c_out, p_in) = tokio::io::duplex(64 * 1024);
        let (p_out, c_in) = tokio::io::duplex(64 * 1024);
        let sock2 = sock.clone();
        tokio::spawn(async move {
            serve(p_in, p_out, move || {
                let s = sock2.clone();
                async move { tokio::net::UnixStream::connect(s).await }
            })
            .await
        });
        let mux = MuxClient::new(c_in, c_out);
        let mut conns: Vec<Conn> = (0..5).map(|_| mux.open().unwrap()).collect();
        for round in 0..3 {
            for c in conns.iter_mut().rev() {
                let v = c.request(&Request::Ping {}).await.unwrap();
                assert!(
                    v["pong"].as_str().unwrap().starts_with('c'),
                    "round {round}"
                );
            }
        }
        // Dropping one channel does not disturb the others.
        drop(conns.remove(0));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(conns[0].request(&Request::Ping {}).await.is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn transport_eof_closes_every_channel_on_both_sides() {
        let dir = std::env::temp_dir().join(format!("orch-mux-eof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s.sock");
        // An orchd stand-in that reports when its connections close.
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let (closed_tx, mut closed_rx) = mpsc::unbounded_channel::<()>();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let closed_tx = closed_tx.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                    }
                    let _ = closed_tx.send(());
                });
            }
        });
        // Raw transport: we play the client by hand, so we can cut the link while two
        // channels are still open.
        let (mut raw_w, p_in) = tokio::io::duplex(64 * 1024);
        let (p_out, mut sink) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            while let Ok(n) = sink.read(&mut buf).await {
                if n == 0 {
                    break;
                }
            }
        });
        let sock2 = sock.clone();
        let proxy = tokio::spawn(async move {
            serve(p_in, p_out, move || {
                let s = sock2.clone();
                async move { tokio::net::UnixStream::connect(s).await }
            })
            .await
        });
        raw_w.write_all(&encode(OPEN, 1, &[])).await.unwrap();
        raw_w.write_all(&encode(OPEN, 2, &[])).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        drop(raw_w); // the ssh link dies
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), proxy).await;
        for _ in 0..2 {
            tokio::time::timeout(std::time::Duration::from_secs(2), closed_rx.recv())
                .await
                .expect("orchd-side connection closed");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
