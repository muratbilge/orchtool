//! orch protocol: versioned JSON-lines frames over a Unix socket (or `ssh host orch proxy`).
//! This is not the herdr compatibility protocol (that comes in phase 3).

pub mod api;
pub mod client;
pub mod env;
pub mod layout;
pub mod mux;

use serde::{Deserialize, Serialize};
use std::io;

pub const VERSION: u32 = 1;
/// Includes the terminating newline. Readers must enforce this before allocating.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    pub version: u32,
    pub id: String,
    pub body: serde_json::Value,
}

impl Frame {
    pub fn new(id: impl Into<String>, body: serde_json::Value) -> Self {
        Self {
            version: VERSION,
            id: id.into(),
            body,
        }
    }

    pub fn encode(&self) -> io::Result<Vec<u8>> {
        self.validate()?;
        let mut bytes = serde_json::to_vec(self).map_err(invalid)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(invalid("frame exceeds byte limit"));
        }
        Ok(bytes)
    }

    /// Decode exactly one complete JSON line; never silently accept a new version.
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_FRAME_BYTES || !bytes.ends_with(b"\n") {
            return Err(invalid("oversized or unterminated frame"));
        }
        if bytes[..bytes.len() - 1].contains(&b'\n') {
            return Err(invalid("expected one JSON line"));
        }
        let frame: Self = serde_json::from_slice(bytes).map_err(invalid)?;
        frame.validate()?;
        Ok(frame)
    }

    /// Decode a line produced by a line codec (newline already stripped).
    pub fn from_line(line: &str) -> io::Result<Self> {
        if line.len() >= MAX_FRAME_BYTES || line.contains('\n') {
            return Err(invalid("oversized frame or embedded newline"));
        }
        let frame: Self = serde_json::from_str(line).map_err(invalid)?;
        frame.validate()?;
        Ok(frame)
    }

    fn validate(&self) -> io::Result<()> {
        if self.version != VERSION {
            return Err(invalid("unsupported protocol version"));
        }
        if self.id.is_empty() || self.id.len() > 128 {
            return Err(invalid("id must contain 1..=128 bytes"));
        }
        Ok(())
    }
}

fn invalid(error: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn embedded_newline_is_not_a_frame_separator() {
        let frame = Frame::new("request-1", json!({"text": "first\nsecond 界"}));
        let bytes = frame.encode().unwrap();
        assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 1);
        assert_eq!(Frame::decode(&bytes).unwrap(), frame);
    }

    #[test]
    fn rejects_invalid_framing_and_versions() {
        let frame = Frame::new("1", json!({"method": "ping"}));
        let bytes = frame.encode().unwrap();
        assert!(Frame::decode(&bytes[..bytes.len() - 1]).is_err());
        assert!(Frame::decode(&[bytes.clone(), bytes].concat()).is_err());
        assert!(Frame::decode(b"\n").is_err());
        assert!(Frame::decode(b"").is_err());
        let mut wrong = Frame::new("1", json!(null));
        wrong.version = VERSION + 1;
        let mut wire = serde_json::to_vec(&wrong).unwrap();
        wire.push(b'\n');
        assert!(Frame::decode(&wire).is_err());
        assert!(wrong.encode().is_err());
    }

    #[test]
    fn rejects_oversized_frames_and_invalid_ids() {
        let huge = Frame::new("1", json!("x".repeat(MAX_FRAME_BYTES)));
        assert!(huge.encode().is_err());
        assert!(Frame::decode(&vec![b'\n'; MAX_FRAME_BYTES + 1]).is_err());
        assert!(Frame::new("", json!(null)).encode().is_err());
        assert!(Frame::new("x".repeat(129), json!(null)).encode().is_err());
    }
}
