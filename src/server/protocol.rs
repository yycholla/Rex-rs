//! Bounded, versioned length-prefixed JSON for local owned state only.
use crate::{
    core::{Command, GridSize, PaneId, Workspace, frame::TerminalFrame, input::KeyInput},
    terminal::PaneStatus,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    sync::Arc,
};

pub const PROTOCOL_VERSION: u16 = 4;
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_GRID_CELLS: usize = 65_536;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub version: u16,
    /// Every mutation is scoped to a server incarnation to reject stale IDs.
    pub server_id: Option<String>,
    pub known_revision: Option<u64>,
    pub operation: Operation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Operation {
    Snapshot,
    /// Desktop opt-in: incremental compact frames on this connection.
    Poll,
    /// First attached controller wins unless an explicit takeover is requested.
    AcquireControl {
        force: bool,
    },
    ReleaseControl,
    Initialize {
        title: String,
    },
    Apply(Command),
    Input {
        pane: PaneId,
        bytes: Vec<u8>,
    },
    Key {
        pane: PaneId,
        input: KeyInput,
    },
    Paste {
        pane: PaneId,
        text: String,
    },
    Scroll {
        pane: PaneId,
        lines: i32,
    },
    Resize {
        pane: PaneId,
        size: GridSize,
        cell_width: u32,
        cell_height: u32,
    },
    /// Explicit server shutdown closes its terminals. Disconnect never sends this.
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub version: u16,
    pub server_id: String,
    pub revision: u64,
    /// Whether this connection owns terminal input/resize/scroll authority.
    pub has_control: bool,
    pub snapshot: Option<Snapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<super::updates::SnapshotUpdate>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub state: Workspace,
    pub panes: BTreeMap<PaneId, PaneSnapshot>,
    /// Layout was recovered from metadata; all shell processes are new.
    pub restored_metadata: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaneSnapshot {
    pub frame: Arc<TerminalFrame>,
    pub status: Option<PaneStatus>,
    pub process_id: Option<u32>,
}

impl Snapshot {
    /// Reject malformed cache data before presentation can index into it.
    ///
    /// # Errors
    /// Returns an error for invalid workspace invariants, mismatched pane sets,
    /// malformed rows/cursors, or excessive total visible cells.
    pub fn validate(&self) -> Result<()> {
        ensure!(self.state.is_valid(), "invalid workspace snapshot");
        let ids = self.state.pane_ids();
        ensure!(
            ids.len() == self.panes.len(),
            "snapshot pane membership mismatch"
        );
        let mut total = 0usize;
        for id in ids {
            let pane = self.panes.get(&id).context("snapshot is missing a pane")?;
            let frame = &pane.frame;
            let columns = usize::from(frame.size.columns());
            let rows = usize::from(frame.size.rows());
            total = total
                .checked_add(columns * rows)
                .context("snapshot size overflow")?;
            ensure!(total <= MAX_GRID_CELLS, "snapshot exceeds cell limit");
            ensure!(frame.rows.len() == rows, "invalid snapshot row count");
            ensure!(
                frame.rows.iter().all(|row| row.len() == columns),
                "invalid snapshot column count"
            );
            if let Some(cursor) = frame.cursor {
                ensure!(
                    cursor.column < frame.size.columns() && cursor.row < frame.size.rows(),
                    "invalid snapshot cursor"
                );
            }
        }
        Ok(())
    }
}

struct BoundedBuffer(Vec<u8>);

impl Write for BoundedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_MESSAGE_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IPC message exceeds byte limit",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Read one frame, rejecting its declared size before allocating a payload.
///
/// # Errors
/// Returns an error for truncated, oversized, invalid UTF-8/JSON or invalid typed data.
pub fn read_message<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T> {
    let mut header = [0; 4];
    reader.read_exact(&mut header).context("read IPC header")?;
    let length = u32::from_be_bytes(header) as usize;
    ensure!(
        length > 0 && length <= MAX_MESSAGE_BYTES,
        "invalid IPC message length {length}"
    );
    let mut payload = vec![0; length];
    reader
        .read_exact(&mut payload)
        .context("read IPC payload")?;
    serde_json::from_slice(&payload).context("decode IPC message")
}

/// Serialize within the byte budget before emitting any part of a frame.
///
/// # Errors
/// Returns an error for oversized data, serialization failure or a stopped peer.
pub fn write_message<T: Serialize>(writer: &mut impl Write, value: &T) -> Result<()> {
    let mut buffer = BoundedBuffer(Vec::new());
    serde_json::to_writer(&mut buffer, value).context("encode IPC message")?;
    let length = u32::try_from(buffer.0.len()).context("IPC message length overflow")?;
    writer
        .write_all(&length.to_be_bytes())
        .context("write IPC header")?;
    writer.write_all(&buffer.0).context("write IPC payload")?;
    writer.flush().context("flush IPC message")
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests fail immediately if fixture serialization or construction fails."
)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversized_header_without_reading_body() {
        let header = ((MAX_MESSAGE_BYTES + 1) as u32).to_be_bytes();
        assert!(read_message::<Request>(&mut header.as_slice()).is_err());
        assert!(read_message::<Request>(&mut [0u8; 4].as_slice()).is_err());
    }

    #[test]
    fn rejects_zero_grid_in_typed_request() {
        let value = serde_json::json!({"version":1,"server_id":null,"known_revision":null,"operation":{"Resize":{"pane":3,"size":{"columns":0,"rows":24},"cell_width":8,"cell_height":16}}});
        assert!(serde_json::from_value::<Request>(value).is_err());
    }

    #[test]
    fn command_round_trip_and_truncated_payload() {
        let request = Request {
            version: PROTOCOL_VERSION,
            server_id: Some("fixture".into()),
            known_revision: Some(3),
            operation: Operation::Apply(Command::CreateSession {
                title: "界".into()
            }),
        };
        let mut bytes = Vec::new();
        write_message(&mut bytes, &request).unwrap();
        let decoded: Request = read_message(&mut bytes.as_slice()).unwrap();
        assert_eq!(decoded.server_id, request.server_id);
        assert!(
            matches!(decoded.operation, Operation::Apply(Command::CreateSession { title }) if title == "界")
        );
        bytes.pop();
        assert!(read_message::<Request>(&mut bytes.as_slice()).is_err());
    }

    #[test]
    fn oversized_encode_emits_no_partial_frame() {
        let mut output = Vec::new();
        assert!(write_message(&mut output, &"x".repeat(MAX_MESSAGE_BYTES)).is_err());
        assert_eq!(output, [] as [u8; 0]);
    }

    #[test]
    fn snapshot_requires_matching_panes_dimensions_and_safe_focus() {
        use crate::core::frame::{Cell, Rgb};
        let mut state = Workspace::default();
        state
            .apply(Command::CreateSession {
                title: "fixture".into(),
            })
            .unwrap();
        let pane = state.pane_ids()[0];
        let frame = TerminalFrame {
            images: Arc::default(),
            graphics_warning: None,
            size: GridSize::new(1, 1).unwrap(),
            rows: vec![Arc::new(vec![Cell::default()])],
            cursor: None,
            foreground: Rgb::default(),
            background: Rgb::default(),
        };
        let mut snapshot = Snapshot {
            restored_metadata: false,
            state,
            panes: BTreeMap::from([(
                pane,
                PaneSnapshot {
                    frame: Arc::new(frame),
                    status: Some(PaneStatus::Running),
                    process_id: Some(123),
                },
            )]),
        };
        snapshot.validate().unwrap();
        let mut value = serde_json::to_value(&snapshot).unwrap();
        value["state"]["sessions"][0]["active_window"] = serde_json::json!(99);
        let malformed: Snapshot = serde_json::from_value(value).unwrap();
        assert!(malformed.validate().is_err());
        let entry = snapshot.panes.get_mut(&pane).unwrap();
        Arc::make_mut(&mut entry.frame).rows.clear();
        assert!(snapshot.validate().is_err());
        snapshot.panes.clear();
        assert!(snapshot.validate().is_err());
    }
}
