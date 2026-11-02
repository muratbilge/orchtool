//! Async client over any byte stream (local Unix socket, or an `ssh host orch proxy` child).

use crate::api::{ErrorBody, Request};
use crate::{Frame, MAX_FRAME_BYTES};
use futures::StreamExt;
use serde_json::Value;
use std::fmt;
use std::io;
use std::path::PathBuf;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio_util::codec::{FramedRead, LinesCodec, LinesCodecError};

pub type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// `$ORCHD_SOCKET`, else `$XDG_STATE_HOME/orchtool/orchd.sock` (default `~/.local/state/…`).
/// Not under `$XDG_RUNTIME_DIR`: without linger that directory vanishes at SSH logout.
pub fn default_socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("ORCHD_SOCKET") {
        return PathBuf::from(p);
    }
    state_dir().join("orchd.sock")
}

pub fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("orchtool")
}

#[derive(Debug)]
pub enum ClientError {
    Io(io::Error),
    Remote(ErrorBody),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Io(e) => write!(f, "{e}"),
            ClientError::Remote(e) => write!(f, "{}: {}", e.code, e.message),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        ClientError::Io(e)
    }
}

pub struct FrameReader {
    inner: FramedRead<BoxRead, LinesCodec>,
}

impl FrameReader {
    pub fn new(read: BoxRead) -> Self {
        Self {
            inner: FramedRead::new(read, LinesCodec::new_with_max_length(MAX_FRAME_BYTES - 1)),
        }
    }

    /// `Ok(None)` on clean EOF.
    pub async fn next(&mut self) -> io::Result<Option<Frame>> {
        match self.inner.next().await {
            None => Ok(None),
            Some(Ok(line)) => Frame::from_line(&line).map(Some),
            Some(Err(LinesCodecError::Io(e))) => Err(e),
            Some(Err(LinesCodecError::MaxLineLengthExceeded)) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame exceeds byte limit",
            )),
        }
    }
}

pub struct FrameWriter {
    inner: BoxWrite,
}

impl FrameWriter {
    pub fn new(write: BoxWrite) -> Self {
        Self { inner: write }
    }

    pub async fn send(&mut self, frame: &Frame) -> io::Result<()> {
        let bytes = frame.encode()?;
        self.inner.write_all(&bytes).await?;
        self.inner.flush().await
    }

    pub async fn send_body(&mut self, id: &str, body: Value) -> io::Result<()> {
        self.send(&Frame::new(id, body)).await
    }
}

pub struct Conn {
    pub reader: FrameReader,
    pub writer: FrameWriter,
    next_id: u64,
}

impl Conn {
    pub fn from_parts(read: BoxRead, write: BoxWrite) -> Self {
        Self {
            reader: FrameReader::new(read),
            writer: FrameWriter::new(write),
            next_id: 1,
        }
    }

    pub async fn unix(path: &std::path::Path) -> io::Result<Self> {
        let stream = tokio::net::UnixStream::connect(path).await?;
        let (r, w) = stream.into_split();
        Ok(Self::from_parts(Box::new(r), Box::new(w)))
    }

    pub fn next_id(&mut self) -> String {
        let id = format!("c{}", self.next_id);
        self.next_id += 1;
        id
    }

    /// Send a request and wait for the reply with the same id.
    pub async fn request(&mut self, req: &Request) -> Result<Value, ClientError> {
        let id = self.next_id();
        let body = serde_json::to_value(req).map_err(|e| io::Error::other(e.to_string()))?;
        self.writer.send_body(&id, body).await?;
        loop {
            let frame = self.reader.next().await?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "orchd closed the connection")
            })?;
            if frame.id != id {
                continue;
            }
            return reply_result(frame.body);
        }
    }

    pub fn into_parts(self) -> (FrameReader, FrameWriter) {
        (self.reader, self.writer)
    }
}

pub fn reply_result(mut body: Value) -> Result<Value, ClientError> {
    if let Some(err) = body.get_mut("error") {
        let err: ErrorBody = serde_json::from_value(err.take()).unwrap_or(ErrorBody {
            code: "bad_reply".into(),
            message: "unparseable error".into(),
        });
        return Err(ClientError::Remote(err));
    }
    Ok(body
        .get_mut("result")
        .map(Value::take)
        .unwrap_or(Value::Null))
}
