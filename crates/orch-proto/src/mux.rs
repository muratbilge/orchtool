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

/// One writer task owns the stream; everything else sends encoded frames to it.
fn spawn_writer(mut w: impl AsyncWrite + Unpin + Send + 'static) -> mpsc::UnboundedSender<Vec<u8>> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
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

/// Bridge one channel to a local byte stream: its reads become DATA frames, incoming DATA
/// is written to it, EOF on either side becomes CLOSE.
fn bridge(
    chan: u32,
    stream: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    out: mpsc::UnboundedSender<Vec<u8>>,
    mut incoming: mpsc::UnboundedReceiver<Vec<u8>>,
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
                    if out2.send(encode(DATA, chan, &buf[..n])).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = out2.send(encode(CLOSE, chan, &[]));
    });
    tokio::spawn(async move {
        while let Some(data) = incoming.recv().await {
            if w.write_all(&data).await.is_err() || w.flush().await.is_err() {
                break;
            }
        }
        let _ = w.shutdown().await;
        // Remote side closed: stop reading too.
        reader.abort();
        let _ = out.send(encode(CLOSE, chan, &[]));
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
    let chans: Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<Vec<u8>>>>> = Arc::default();
    let connect = Arc::new(connect);
    while let Some((ty, chan, payload)) = read_frame(&mut input).await? {
        match ty {
            OPEN => {
                let (tx, rx) = mpsc::unbounded_channel();
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
                            let _ = out.send(encode(CLOSE, chan, &[]));
                        }
                    }
                });
            }
            DATA => {
                if let Some(tx) = chans.lock().unwrap().get(&chan) {
                    let _ = tx.send(payload);
                }
            }
            CLOSE => {
                chans.lock().unwrap().remove(&chan);
            }
            _ => {}
        }
    }
    Ok(())
}

/// Client side: open channels over one stream (e.g. an ssh child's stdio).
#[derive(Clone)]
pub struct MuxClient {
    out: mpsc::UnboundedSender<Vec<u8>>,
    chans: Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<Vec<u8>>>>>,
    next: Arc<AtomicU32>,
}

impl MuxClient {
    pub fn new(
        input: impl AsyncRead + Unpin + Send + 'static,
        output: impl AsyncWrite + Unpin + Send + 'static,
    ) -> Self {
        let out = spawn_writer(output);
        let chans: Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<Vec<u8>>>>> = Arc::default();
        let chans2 = Arc::clone(&chans);
        tokio::spawn(async move {
            let mut input = input;
            while let Ok(Some((ty, chan, payload))) = read_frame(&mut input).await {
                match ty {
                    DATA => {
                        if let Some(tx) = chans2.lock().unwrap().get(&chan) {
                            let _ = tx.send(payload);
                        }
                    }
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
        let (tx, rx) = mpsc::unbounded_channel();
        self.chans.lock().unwrap().insert(chan, tx);
        self.out
            .send(encode(OPEN, chan, &[]))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "connection to host lost"))?;
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
}
