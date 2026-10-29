//! Program clipboard writes from authoritative Rex events, never replica VT.
//!
//! One connection-local FIFO reads omitted text through `clipboard_read`. Only
//! complete owned UTF-8 writes leave this module; it never reads an OS clipboard.
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    task::{Context as TaskContext, Poll, Waker},
};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use super::transport::{ControlHandle, PendingCall};

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_WRITES: usize = 8;
const CHUNK_BYTES: usize = 48 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClipboardWrite {
    pub session_id: String,
    pub block_id: String,
    pub id: u64,
    pub text: String,
}

struct Transfer {
    write: ClipboardWrite,
    total: usize,
    complete: bool,
}

impl Transfer {
    fn accept_chunk(&mut self, value: &Value) -> Result<()> {
        ensure!(
            value["id"].as_u64() == Some(self.write.id),
            "clipboard copy identity changed"
        );
        ensure!(
            value["total"].as_u64() == Some(u64::try_from(self.total)?),
            "clipboard copy length changed"
        );
        let data = value["data"]
            .as_str()
            .context("clipboard reply lacks text")?;
        let eof = value["eof"]
            .as_bool()
            .context("clipboard reply lacks eof")?;
        let length = self
            .write
            .text
            .len()
            .checked_add(data.len())
            .context("clipboard copy length overflow")?;
        ensure!(
            length <= self.total && length <= MAX_BYTES,
            "clipboard copy exceeds declared length"
        );
        ensure!(
            eof == (length == self.total),
            "clipboard copy has inconsistent eof"
        );
        ensure!(eof || !data.is_empty(), "clipboard copy made no progress");
        self.write.text.push_str(data);
        self.complete = eof;
        Ok(())
    }
}

/// Drop on a new server instance. Preserve its high-water sequence on ordinary
/// resume to reject replayed events; do not reconstruct writes from bootstrap.
pub(crate) struct ClipboardTransfers<F = PendingCall> {
    queue: VecDeque<Transfer>,
    pending: Option<F>,
    reserved_bytes: usize,
    last_sequence: Option<u64>,
}

impl<F> Default for ClipboardTransfers<F> {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            pending: None,
            reserved_bytes: 0,
            last_sequence: None,
        }
    }
}

impl<F> ClipboardTransfers<F> {
    /// Accept the original transport event envelope, including its sequence.
    /// Duplicate events and unrelated events return false. Admission errors are
    /// explicit; rejected clipboard events are not replayed automatically.
    pub(crate) fn accept_event(&mut self, value: &Value) -> Result<bool> {
        let Some((sequence, transfer)) = parse_event(value)? else {
            return Ok(false);
        };
        if self.last_sequence.is_some_and(|last| sequence <= last) {
            return Ok(false);
        }
        self.last_sequence = Some(sequence);
        ensure!(
            self.queue.len() < MAX_WRITES,
            "program clipboard queue is full"
        );
        ensure!(
            self.reserved_bytes + transfer.total <= MAX_BYTES,
            "program clipboard queue exceeds 4 MiB"
        );
        self.reserved_bytes += transfer.total;
        self.queue.push_back(transfer);
        Ok(true)
    }

    /// Discard admitted work on disconnection without replaying it on resume.
    /// The sequence high-water mark remains until this instance is replaced.
    pub(crate) fn disconnect(&mut self) {
        self.pending = None;
        self.queue.clear();
        self.reserved_bytes = 0;
    }

    fn pop(&mut self) -> Option<ClipboardWrite> {
        self.queue.pop_front().map(|transfer| {
            self.reserved_bytes -= transfer.total;
            transfer.write
        })
    }
}

impl ClipboardTransfers {
    /// Poll without waiting. Admit at most one read-only RPC per worker turn.
    pub(crate) fn poll(&mut self, control: &ControlHandle) -> Result<Option<ClipboardWrite>> {
        self.advance(|method, args| control.request(method, args))
    }
}

impl<F: Future<Output = Result<Value>> + Unpin> ClipboardTransfers<F> {
    fn advance(
        &mut self,
        request: impl FnOnce(&str, Value) -> Result<F>,
    ) -> Result<Option<ClipboardWrite>> {
        if let Some(pending) = &mut self.pending {
            let mut context = TaskContext::from_waker(Waker::noop());
            let Poll::Ready(result) = Pin::new(pending).poll(&mut context) else {
                return Ok(None);
            };
            self.pending = None;
            let accepted = result.and_then(|value| {
                self.queue
                    .front_mut()
                    .context("clipboard reply has no admitted copy")?
                    .accept_chunk(&value)
            });
            if let Err(error) = accepted {
                self.pop();
                return Err(error);
            }
        }
        let Some(transfer) = self.queue.front() else {
            return Ok(None);
        };
        if transfer.complete {
            return Ok(self.pop());
        }
        let args = json!({
            "session_id": transfer.write.session_id,
            "block_id": transfer.write.block_id,
            "args": {"id": transfer.write.id, "offset": transfer.write.text.len(), "max_bytes": CHUNK_BYTES}
        });
        match request("com.superlogical.terminal.clipboard_read", args) {
            Ok(pending) => self.pending = Some(pending),
            Err(error) => {
                self.pop();
                return Err(error);
            }
        }
        Ok(None)
    }
}

fn parse_event(value: &Value) -> Result<Option<(u64, Transfer)>> {
    if value["type"] != "event" || value["payload"]["type"] != "session_event" {
        return Ok(None);
    }
    let session = &value["payload"]["payload"];
    let event = &session["event"];
    let block = &event["payload"];
    if event["type"] != "block_event"
        || block["creator_name"] != "com.superlogical.terminal"
        || block["name"] != "clipboard_written"
    {
        return Ok(None);
    }
    let sequence = value["seq"]
        .as_u64()
        .context("clipboard event lacks sequence")?;
    let id_string = |value: &Value, prefix: &str| -> Result<String> {
        let text = value
            .as_str()
            .context("clipboard event lacks target identity")?;
        ensure!(
            text.starts_with(prefix) && text.len() > prefix.len() && text.len() <= 128,
            "clipboard event target identity is invalid"
        );
        Ok(text.to_owned())
    };
    let session_id = id_string(&session["session_id"], "session:")?;
    let block_id = id_string(&block["block_id"], "block:")?;
    let payload = &block["payload"];
    let id = payload["id"]
        .as_u64()
        .filter(|id| *id > 0)
        .context("clipboard event lacks copy identity")?;
    let total = usize::try_from(
        payload["bytes"]
            .as_u64()
            .context("clipboard event lacks byte count")?,
    )?;
    ensure!(total <= MAX_BYTES, "program clipboard write exceeds 4 MiB");
    let inline = payload
        .get("text")
        .map(|text| {
            text.as_str()
                .context("clipboard event text is not a string")
        })
        .transpose()?;
    if let Some(text) = inline {
        ensure!(
            text.len() == total,
            "clipboard event text length differs from byte count"
        );
    }
    Ok(Some((
        sequence,
        Transfer {
            write: ClipboardWrite {
                session_id,
                block_id,
                id,
                text: inline.unwrap_or_default().to_owned(),
            },
            total,
            complete: inline.is_some(),
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::{Ready, ready};

    fn event(sequence: u64, id: u64, bytes: usize, text: Option<&str>) -> Value {
        let mut payload = json!({"id":id,"bytes":bytes});
        if let Some(text) = text {
            payload["text"] = json!(text);
        }
        json!({"type":"event","seq":sequence,"payload":{"type":"session_event","payload":{
            "session_id":"session:one", "event":{"type":"block_event","payload":{
                "block_id":"block:one", "creator_name":"com.superlogical.terminal", "name":"clipboard_written", "payload":payload
            }}
        }}})
    }

    #[test]
    fn inline_writes_are_owned_ordered_and_duplicate_events_do_not_replay() -> Result<()> {
        let mut transfers = ClipboardTransfers::<Ready<Result<Value>>>::default();
        let value = event(10, 1, 3, Some("界"));
        assert!(transfers.accept_event(&value)?);
        assert!(!transfers.accept_event(&value)?);
        assert!(transfers.accept_event(&event(11, 2, 0, Some("")))?);
        let no_rpc = |_: &str, _: Value| -> Result<Ready<Result<Value>>> {
            anyhow::bail!("inline copy must not request text")
        };
        assert_eq!(
            transfers.advance(no_rpc)?.context("inline write")?.text,
            "界"
        );
        assert_eq!(transfers.advance(no_rpc)?.context("clear write")?.text, "");
        assert!(transfers.advance(no_rpc)?.is_none());
        transfers.disconnect();
        assert!(!transfers.accept_event(&value)?);
        Ok(())
    }

    #[test]
    fn chunk_reads_pin_identity_and_advance_utf8_byte_offsets() -> Result<()> {
        let mut transfers = ClipboardTransfers::<Ready<Result<Value>>>::default();
        transfers.accept_event(&event(1, 7, 5, None))?;
        assert!(
            transfers
                .advance(|method, args| {
                    assert_eq!(method, "com.superlogical.terminal.clipboard_read");
                    assert_eq!(args["args"]["id"], 7);
                    assert_eq!(args["args"]["offset"], 0);
                    Ok(ready(Ok(json!({"id":7,"data":"界","total":5,"eof":false}))))
                })?
                .is_none()
        );
        assert!(
            transfers
                .advance(|_, args| {
                    assert_eq!(args["args"]["offset"], 3);
                    Ok(ready(Ok(json!({"id":7,"data":"é","total":5,"eof":true}))))
                })?
                .is_none()
        );
        let result = transfers
            .advance(|_, _| anyhow::bail!("finished copy must not request"))?
            .context("completed copy")?;
        assert_eq!(result.text, "界é");
        assert_eq!(result.id, 7);
        Ok(())
    }

    #[test]
    fn invalid_chunks_never_publish_partial_clipboard_contents() -> Result<()> {
        for reply in [
            json!({"id":2,"data":"x","total":2,"eof":false}),
            json!({"id":1,"data":"x","total":3,"eof":false}),
            json!({"id":1,"data":"","total":2,"eof":false}),
            json!({"id":1,"data":"x","total":2,"eof":true}),
            json!({"id":1,"data":"xxx","total":2,"eof":true}),
            json!({"id":1,"data":"xx","total":2,"eof":false}),
        ] {
            let mut transfers = ClipboardTransfers::<Ready<Result<Value>>>::default();
            transfers.accept_event(&event(1, 1, 2, None))?;
            transfers.advance(|_, _| Ok(ready(Ok(reply))))?;
            assert!(
                transfers
                    .advance(|_, _| anyhow::bail!("invalid copy must not continue"))
                    .is_err()
            );
            assert!(transfers.queue.is_empty());
            assert_eq!(transfers.reserved_bytes, 0);
        }
        Ok(())
    }

    #[test]
    fn admission_is_bounded_and_pending_reads_do_not_queue_more_rpc_work() -> Result<()> {
        let mut transfers = ClipboardTransfers::<std::future::Pending<Result<Value>>>::default();
        transfers.accept_event(&event(1, 1, MAX_BYTES, None))?;
        assert!(transfers.accept_event(&event(2, 2, 1, Some("x"))).is_err());
        transfers.advance(|_, _| Ok(std::future::pending()))?;
        for _ in 0..10 {
            assert!(
                transfers
                    .advance(|_, _| anyhow::bail!("second request while read pending"))
                    .is_ok()
            );
        }
        transfers.disconnect();
        assert!(transfers.pending.is_none());
        assert!(transfers.queue.is_empty());
        for n in 3..11 {
            transfers.accept_event(&event(n, n, 0, Some("")))?;
        }
        assert!(transfers.accept_event(&event(11, 11, 0, Some(""))).is_err());
        assert!(
            transfers
                .accept_event(&event(12, 12, MAX_BYTES + 1, None))
                .is_err()
        );
        assert!(
            transfers
                .accept_event(&event(13, 13, 2, Some("界")))
                .is_err()
        );
        assert!(
            !transfers
                .accept_event(&json!({"type":"event","seq":14,"payload":{"type":"other"}}))?
        );
        Ok(())
    }
}
