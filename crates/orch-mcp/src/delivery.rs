//! Receipts cross the stdout/daemon boundary: retain them after flush until ACK succeeds.
//! The next inbox request carries them too, so it cannot overtake a post-flush ACK.
use orch_proto::api::Request;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

#[derive(Default)]
pub struct Receipts(Mutex<HashMap<String, BTreeSet<i64>>>);

impl Receipts {
    pub async fn for_agent(&self, name: &str) -> Vec<i64> {
        self.0
            .lock()
            .await
            .get(name)
            .map(|ids| ids.iter().copied().collect())
            .unwrap_or_default()
    }

    pub async fn write<W: AsyncWrite + Unpin>(
        &self,
        out: &mut W,
        value: &Value,
        after: Option<&Request>,
    ) -> std::io::Result<()> {
        // Lock BEFORE output: a client can issue its next call as soon as it reads the
        // newline, before flush() returns. It must see this receipt or wait for the flush.
        let mut receipts = self.0.lock().await;
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        out.write_all(&line).await?;
        out.flush().await?;
        if let Some(Request::Ack { name, ids }) = after {
            receipts.entry(name.clone()).or_default().extend(ids);
        }
        Ok(())
    }

    pub async fn confirm(&self, request: &Request) {
        if let Request::Ack { name, ids } = request {
            let mut receipts = self.0.lock().await;
            if let Some(pending) = receipts.get_mut(name) {
                for id in ids {
                    pending.remove(id);
                }
                if pending.is_empty() {
                    receipts.remove(name);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;

    fn ack(id: i64) -> Request {
        Request::Ack {
            name: "agent".into(),
            ids: vec![id],
        }
    }

    #[tokio::test]
    async fn read_waits_for_output_and_retains_receipt_until_ack_completes() {
        let receipts = Arc::new(Receipts::default());
        let (mut out, mut input) = tokio::io::duplex(1);
        let writer_receipts = Arc::clone(&receipts);
        let writer = tokio::spawn(async move {
            writer_receipts
                .write(
                    &mut out,
                    &serde_json::json!({"text":"delivered"}),
                    Some(&ack(7)),
                )
                .await
                .unwrap();
        });
        let mut byte = [0];
        input.read_exact(&mut byte).await.unwrap();
        assert!(
            receipts.0.try_lock().is_err(),
            "read cannot overtake unfinished output"
        );
        input.read_to_end(&mut Vec::new()).await.unwrap();
        writer.await.unwrap();
        assert_eq!(
            receipts.for_agent("agent").await,
            vec![7],
            "ACK deliberately delayed"
        );
        assert!(receipts.for_agent("other").await.is_empty());
        receipts
            .write(&mut tokio::io::sink(), &Value::Null, Some(&ack(8)))
            .await
            .unwrap();
        receipts.confirm(&ack(7)).await;
        assert_eq!(receipts.for_agent("agent").await, vec![8]);
        receipts.confirm(&ack(8)).await;
        assert!(receipts.for_agent("agent").await.is_empty());
    }

    #[tokio::test]
    async fn failed_or_cancelled_output_creates_no_receipt() {
        let receipts = Arc::new(Receipts::default());
        let (mut out, input) = tokio::io::duplex(1);
        drop(input);
        assert!(receipts
            .write(&mut out, &Value::Null, Some(&ack(7)))
            .await
            .is_err());
        assert!(receipts.for_agent("agent").await.is_empty());
        let (mut out, mut input) = tokio::io::duplex(1);
        let writer_receipts = Arc::clone(&receipts);
        let writer = tokio::spawn(async move {
            writer_receipts
                .write(&mut out, &Value::Null, Some(&ack(8)))
                .await
        });
        input.read_exact(&mut [0]).await.unwrap();
        writer.abort();
        assert!(writer.await.unwrap_err().is_cancelled());
        assert!(receipts.for_agent("agent").await.is_empty());
    }
}
