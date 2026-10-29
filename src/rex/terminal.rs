//! Rex terminal data channel. Construct and poll on the terminal owner worker.
use super::{endpoint::RexStream, transport::open_websocket};
use crate::{
    core::{
        GridSize,
        frame::TerminalFrame,
        input::MouseTracking,
        interaction::{
            MAX_SEARCH_QUERY_BYTES, SearchCommand, SelectionCommand, TerminalAnnotations,
        },
    },
    terminal::GhosttyTerminal,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    fmt::Write as _,
    io::ErrorKind,
    sync::Arc,
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket};

const SUBPROTOCOL: &str = "rex.data.terminal.v1";
const MAX_CHUNK_BYTES: usize = 65_536;
const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
const MAX_APC_BYTES: usize = 65_536;
const MAX_GRID_CELLS: usize = 65_536;
const MAX_BATCH_BYTES: usize = 256 * 1024;
const MAX_BATCH_MESSAGES: usize = 8;
const BATCH_TIME: Duration = Duration::from_millis(1);
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Data handshake identity. Resume tokens are private and deliberately not Debug.
struct Welcome {
    token: [u8; 16],
    nonce: u64,
}

impl Welcome {
    fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.first() == Some(&0x82) {
            ensure!(bytes.len() == 2, "malformed Rex data rejection");
            bail!(
                "Rex data handshake rejected (reason 0x{:02x}); request a new ticket",
                bytes[1]
            );
        }
        ensure!(
            bytes.len() == 32 && bytes[0] == 0x81,
            "invalid Rex data welcome"
        );
        ensure!(
            bytes[17..20] == [0, 0, 0],
            "reserved Rex data welcome bytes are nonzero"
        );
        let token = bytes[1..17].try_into().context("invalid reconnect token")?;
        let nonce = u64::from_le_bytes(bytes[24..32].try_into().context("invalid nonce")?);
        ensure!(nonce != 0, "Rex data nonce must be nonzero");
        Ok(Self { token, nonce })
    }
}

/// A decoded terminal and its ordered, resumable data channel.
/// Ghostty stays on this object's owning thread; only owned frames leave it.
pub struct TerminalStream {
    socket: WebSocket<RexStream>,
    endpoint: String,
    welcome: Welcome,
    received: u64,
    decoder: StreamDecoder,
    resumable: bool,
    last_materialized: Option<Instant>,
}

impl TerminalStream {
    /// Open a new data stream using a fresh ticket from the attached control connection.
    ///
    /// # Errors
    /// Reports invalid tickets, connection and handshake rejection.
    pub fn connect(endpoint: &str, ticket: &str) -> Result<Self> {
        let mut hello = vec![0x01];
        hello.extend_from_slice(&decode_ticket(ticket)?);
        let (socket, welcome) = handshake(endpoint, hello)?;
        let decoder = StreamDecoder::new(welcome.nonce);
        Ok(Self {
            socket,
            endpoint: endpoint.to_owned(),
            welcome,
            received: 0,
            decoder,
            resumable: true,
            last_materialized: None,
        })
    }

    /// Read a bounded batch and publish at most one frame at display cadence.
    ///
    /// # Errors
    /// Reports malformed state, unsupported snapshot data or transport failure.
    pub fn poll(&mut self, timeout: Duration) -> Result<Option<Arc<TerminalFrame>>> {
        self.poll_with_frame(timeout, true)
    }

    /// Parse hidden terminals without copying their display cells. Revealing one
    /// materializes pending state even when the server sends no additional byte.
    /// A zero timeout makes the entire read batch nonblocking.
    ///
    /// # Errors
    /// Reports malformed state, unsupported snapshot data or transport failure.
    pub fn poll_with_frame(
        &mut self,
        timeout: Duration,
        materialize: bool,
    ) -> Result<Option<Arc<TerminalFrame>>> {
        // Visibility controls expensive native highlight extraction independently
        // of the visible frame cadence. Apply it before restoring a snapshot.
        self.set_interaction_visible(materialize);
        if self.decoder.exit_code.is_none() {
            self.socket.get_mut().set_nonblocking(timeout.is_zero())?;
            if !timeout.is_zero() {
                self.socket
                    .get_mut()
                    .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
            }
            let result = self.read_batch();
            let restored = self.socket.get_mut().set_nonblocking(false);
            result?;
            restored?;
        }
        // Annotation progress belongs to the owner loop even for hidden or
        // exited replicas; it does not depend on incoming VT or cell copies.
        self.decoder.interaction_tick()?;
        if materialize
            && self.decoder.exit_code.is_none()
            && let Some(terminal) = &mut self.decoder.terminal
            && terminal.graphics_tick()?
        {
            self.decoder.frame_pending = true;
        }
        if !materialize
            || (!self.decoder.publication.immediate_frame
                && self.decoder.exit_code.is_none()
                && self
                    .last_materialized
                    .is_some_and(|last| last.elapsed() < FRAME_INTERVAL))
        {
            self.decoder.refresh_pending_annotations()?;
            return Ok(None);
        }
        let inspected = self.decoder.frame_pending;
        let frame = match self.decoder.materialize() {
            Ok(frame) => frame,
            Err(error) => {
                self.resumable = false;
                return Err(error);
            }
        };
        if inspected {
            self.last_materialized = Some(Instant::now());
        }
        Ok(frame)
    }

    fn read_batch(&mut self) -> Result<()> {
        let start = Instant::now();
        let mut accepted = 0;
        for _ in 0..MAX_BATCH_MESSAGES {
            match self.socket.read() {
                Ok(Message::Binary(bytes)) => {
                    if bytes.is_empty() || bytes.len() > MAX_CHUNK_BYTES {
                        self.resumable = false;
                        bail!("invalid Rex data chunk size");
                    }
                    if let Err(error) = self.decoder.push_deferred(&bytes) {
                        // Applying a chunk can change the replica before failure;
                        // never resume by replaying that chunk into partial state.
                        self.resumable = false;
                        return Err(error);
                    }
                    self.received = self
                        .received
                        .checked_add(u64::try_from(bytes.len())?)
                        .context("Rex data byte count overflow")?;
                    accepted += bytes.len();
                    // child_exited is ordered after all program output. The
                    // A later channel close cannot lose program output and must
                    // not trigger resume. Preserve the final frame for browsing.
                    if self.decoder.exit_code.is_some() {
                        break;
                    }
                }
                Ok(Message::Ping(_) | Message::Pong(_)) => {
                    self.socket.flush()?;
                }
                Ok(Message::Close(_)) => bail!("Rex terminal data channel closed"),
                Ok(_) => {
                    self.resumable = false;
                    bail!("unexpected nonbinary Rex terminal data");
                }
                Err(tungstenite::Error::Io(error))
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    break;
                }
                Err(error) => return Err(error).context("read Rex terminal data"),
            }
            if accepted >= MAX_BATCH_BYTES || start.elapsed() >= BATCH_TIME {
                break;
            }
            // Only the first read may wait. Small WebSocket messages already
            // queued by Rex are coalesced without waiting once per chunk.
            self.socket.get_mut().set_nonblocking(true)?;
        }
        Ok(())
    }

    /// Resume exactly after the last chunk accepted by the decoder.
    ///
    /// # Errors
    /// A refused resume requires discarding this decoder and obtaining a fresh ticket.
    pub fn resume(&mut self) -> Result<()> {
        ensure!(
            self.decoder.exit_code.is_none(),
            "Rex terminal program exited"
        );
        ensure!(
            self.resumable,
            "Rex terminal decoder failed; obtain a fresh data ticket"
        );
        // Stop the previous transport before replacing it. Read-ahead bytes
        // inside its WebSocket buffer were not accepted and are not counted.
        let _ = self.socket.get_mut().shutdown(std::net::Shutdown::Both);
        let mut request = vec![0x02];
        request.extend_from_slice(&self.welcome.token);
        request.extend_from_slice(&self.received.to_le_bytes());
        let (socket, welcome) = handshake(&self.endpoint, request)?;
        ensure!(
            welcome.nonce == self.welcome.nonce,
            "Rex resume changed channel nonce"
        );
        self.socket = socket;
        self.welcome = welcome;
        Ok(())
    }

    /// Scroll this client's restored history without changing the server viewport.
    ///
    /// # Errors
    /// Reports terminal history or rendering errors.
    pub fn scroll(&mut self, lines: i32) -> Result<Option<Arc<TerminalFrame>>> {
        let Some(terminal) = &mut self.decoder.terminal else {
            return Ok(None);
        };
        terminal.scroll_lines(lines)?;
        self.decoder.frame_pending = true;
        let frame = self.decoder.materialize()?;
        self.last_materialized = Some(Instant::now());
        Ok(frame)
    }

    /// Return this client's viewport to live output before sending input.
    ///
    /// # Errors
    /// Reports terminal viewport or rendering errors.
    pub fn scroll_to_bottom(&mut self) -> Result<Option<Arc<TerminalFrame>>> {
        let Some(terminal) = &mut self.decoder.terminal else {
            return Ok(None);
        };
        if terminal.is_at_bottom()? {
            return Ok(None);
        }
        terminal.scroll_to_bottom();
        self.decoder.frame_pending = true;
        let frame = self.decoder.materialize()?;
        self.last_materialized = Some(Instant::now());
        Ok(frame)
    }

    /// Set current layout visibility before a local operation or stream poll.
    pub(crate) fn set_interaction_visible(&mut self, visible: bool) {
        self.decoder.set_interaction_visible(visible);
    }

    /// Change this replica's local history search. Set/cancel intent can precede
    /// the initial snapshot and will be installed on the new native terminal.
    ///
    /// # Errors
    /// Rejects oversized queries, navigation before snapshot restore, or native
    /// search failures. Neither this method nor selection sends process input.
    pub fn search(&mut self, command: SearchCommand) -> Result<()> {
        self.decoder.search(command)
    }

    /// Apply a local selection gesture, including on exited/read-only replicas.
    ///
    /// # Errors
    /// Reports unavailable snapshots, stale viewport tokens or native failures.
    pub fn select(&mut self, command: SelectionCommand) -> Result<()> {
        self.decoder.select(command)
    }

    /// Snapshot owned annotations independently of the terminal frame identity.
    /// Compare their revision even when polling returns no changed cell frame.
    #[must_use]
    pub fn annotations(&self) -> Arc<TerminalAnnotations> {
        Arc::clone(&self.decoder.annotations)
    }

    /// Copy the native logical selection without sending a clipboard request to
    /// Rex. Empty selection and an unrestored replica return None.
    ///
    /// # Errors
    /// Reports native selection-formatting failures or copy-size bounds.
    pub fn copy_selection(&self) -> Result<Option<String>> {
        self.decoder
            .terminal
            .as_ref()
            .map_or(Ok(None), GhosttyTerminal::copy_selection)
    }

    /// Resolve an OSC 8 link against the displayed viewport on its owner thread.
    ///
    /// # Errors
    /// Rejects stale viewport tokens, invalid cells and oversized native URIs.
    pub fn hyperlink_at(
        &mut self,
        column: u16,
        row: u16,
        viewport_token: u64,
    ) -> Result<Option<String>> {
        self.decoder
            .terminal
            .as_mut()
            .context("Local terminal history is unavailable")?
            .hyperlink_at(column, row, viewport_token)
    }

    /// Whether an application requested mouse input instead of local history.
    ///
    /// # Errors
    /// Reports native terminal mode query failures.
    pub fn uses_mouse_reporting(&self) -> Result<bool> {
        self.decoder
            .terminal
            .as_ref()
            .map_or(Ok(false), GhosttyTerminal::uses_mouse_reporting)
    }

    /// Requested pointer traffic, independent of frame materialization cadence.
    ///
    /// # Errors
    /// Reports native terminal mode query failures.
    pub fn mouse_tracking(&self) -> Result<MouseTracking> {
        self.decoder
            .terminal
            .as_ref()
            .map_or(Ok(MouseTracking::None), GhosttyTerminal::mouse_tracking)
    }

    /// Whether a full-screen application owns the alternate screen.
    ///
    /// # Errors
    /// Reports native terminal screen query failures.
    pub fn alternate_screen(&self) -> Result<bool> {
        self.decoder
            .terminal
            .as_ref()
            .map_or(Ok(false), GhosttyTerminal::alternate_screen)
    }

    /// Whether applying stream bytes failed and this replica must be discarded.
    #[must_use]
    pub const fn is_decode_failed(&self) -> bool {
        !self.resumable
    }

    /// Input is safe once this replica has restored its initial terminal state.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        self.resumable && self.decoder.terminal.is_some() && self.decoder.exit_code.is_none()
    }

    #[must_use]
    pub const fn exit_code(&self) -> Option<i32> {
        self.decoder.exit_code
    }
}

impl Drop for TerminalStream {
    fn drop(&mut self) {
        // A deliberate detach ends this data stream. Abrupt transport failures
        // use RESUME instead; do not leave a dropped decoder waiting for one.
        let _ = self
            .socket
            .get_mut()
            .set_write_timeout(Some(Duration::from_millis(100)));
        let _ = self.socket.close(None);
        let _ = self.socket.get_mut().shutdown(std::net::Shutdown::Both);
    }
}

fn handshake(endpoint: &str, request: Vec<u8>) -> Result<(WebSocket<RexStream>, Welcome)> {
    let mut socket = open_websocket(endpoint, "/api/data", SUBPROTOCOL)?;
    socket.set_config(|config| {
        config.max_message_size = Some(MAX_CHUNK_BYTES);
        config.max_frame_size = Some(MAX_CHUNK_BYTES);
    });
    let _deadline = socket.get_ref().handshake_guard(Duration::from_secs(3))?;
    socket.send(Message::Binary(request.into()))?;
    let Message::Binary(bytes) = socket.read()? else {
        bail!("Rex data handshake did not return binary welcome");
    };
    let welcome = Welcome::parse(&bytes)?;
    Ok((socket, welcome))
}

fn decode_ticket(ticket: &str) -> Result<[u8; 16]> {
    ensure!(
        ticket.len() == 22,
        "Rex ticket must contain 22 base64url characters"
    );
    let mut output = [0_u8; 16];
    let mut accumulator = 0_u32;
    let mut bits = 0_u32;
    let mut index = 0;
    for byte in ticket.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => bail!("invalid Rex ticket encoding"),
        };
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output[index] = (accumulator >> bits) as u8;
            index += 1;
        }
    }
    ensure!(
        accumulator.trailing_zeros() >= 4,
        "noncanonical Rex ticket encoding"
    );
    Ok(output)
}

struct InteractionPublication {
    immediate_frame: bool,
    annotations_pending: bool,
    last_annotated: Option<Instant>,
    visible: bool,
}

impl Default for InteractionPublication {
    fn default() -> Self {
        Self {
            immediate_frame: false,
            annotations_pending: false,
            last_annotated: None,
            visible: true,
        }
    }
}

struct StreamDecoder {
    nonce: u64,
    snapshot: Vec<u8>,
    record_offset: usize,
    terminal: Option<GhosttyTerminal>,
    pending: Vec<u8>,
    exit_code: Option<i32>,
    passthrough_apc: bool,
    frame_pending: bool,
    last_frame: Option<Arc<TerminalFrame>>,
    annotations: Arc<TerminalAnnotations>,
    search_intent: Option<SearchCommand>,
    publication: InteractionPublication,
}

impl StreamDecoder {
    fn new(nonce: u64) -> Self {
        Self {
            nonce,
            snapshot: Vec::new(),
            record_offset: 10,
            terminal: None,
            pending: Vec::new(),
            exit_code: None,
            passthrough_apc: false,
            frame_pending: false,
            last_frame: None,
            annotations: Arc::new(TerminalAnnotations::default()),
            search_intent: None,
            publication: InteractionPublication::default(),
        }
    }

    #[cfg(test)]
    fn push(&mut self, bytes: &[u8]) -> Result<Option<Arc<TerminalFrame>>> {
        self.push_deferred(bytes)?;
        self.materialize()
    }

    fn push_deferred(&mut self, bytes: &[u8]) -> Result<()> {
        let mut bytes = bytes;
        let mut tail = Vec::new();
        if self.terminal.is_none() {
            ensure!(
                self.snapshot.len().saturating_add(bytes.len()) <= MAX_SNAPSHOT_BYTES,
                "Rex snapshot exceeds client limit"
            );
            self.snapshot.extend_from_slice(bytes);
            let Some(end) = snapshot_end(&self.snapshot, &mut self.record_offset)? else {
                return Ok(());
            };
            let terminal = GhosttyTerminal::from_snapshot(&self.snapshot[..end])?;
            self.install_terminal(terminal)?;
            tail.extend_from_slice(&self.snapshot[end..]);
            self.snapshot.clear();
            self.snapshot.shrink_to_fit();
            bytes = &tail;
        }
        self.feed_live(bytes)?;
        let terminal = self
            .terminal
            .as_mut()
            .context("missing restored Rex terminal")?;
        // Rex's authoritative terminal already answers device queries. Never echo
        // the presentation replica's responses back to the process.
        let _ = terminal.take_pending_output();
        self.frame_pending = true;
        self.publication.annotations_pending = true;
        Ok(())
    }

    fn materialize(&mut self) -> Result<Option<Arc<TerminalFrame>>> {
        if !self.frame_pending {
            return Ok(None);
        }
        let Some(terminal) = &mut self.terminal else {
            return Ok(None);
        };
        let frame = terminal.snapshot_shared()?;
        // Full spans/tokens are paired only after obtaining the current frame.
        self.refresh_annotations(true)?;
        self.frame_pending = false;
        self.publication.immediate_frame = false;
        if self
            .last_frame
            .as_ref()
            .is_some_and(|last| Arc::ptr_eq(last, &frame))
        {
            return Ok(None);
        }
        self.last_frame = Some(Arc::clone(&frame));
        Ok(Some(frame))
    }

    fn install_terminal(&mut self, mut terminal: GhosttyTerminal) -> Result<()> {
        terminal.set_interaction_visible(self.publication.visible);
        // Search objects belong to exactly one native terminal. Recreate them
        // from owned intent; fresh snapshots deliberately have no old selection.
        if let Some(command) = &self.search_intent {
            terminal.search(command.clone())?;
        }
        self.terminal = Some(terminal);
        self.last_frame = None;
        self.frame_pending = true;
        self.publication.immediate_frame = true;
        self.publication.annotations_pending = true;
        self.refresh_annotations(false)?;
        Ok(())
    }

    fn set_interaction_visible(&mut self, visible: bool) {
        if self.publication.visible == visible {
            return;
        }
        self.publication.visible = visible;
        self.publication.annotations_pending = true;
        if let Some(terminal) = &mut self.terminal {
            terminal.set_interaction_visible(visible);
        }
        if visible {
            // A complete hidden search may produce no further progress or VT.
            // Re-pair its full highlights with the frame immediately on reveal.
            self.frame_pending = true;
            self.publication.immediate_frame = true;
        }
    }

    fn refresh_annotations(&mut self, paired_frame: bool) -> Result<bool> {
        let Some(terminal) = &mut self.terminal else {
            return Ok(false);
        };
        let live = terminal.annotations()?;
        let mut annotations = (*live).clone();
        if !paired_frame {
            let changed_viewport = live.viewport_token != self.annotations.viewport_token;
            let changed_query = live.search.as_ref().map(|search| search.generation)
                != self
                    .annotations
                    .search
                    .as_ref()
                    .map(|search| search.generation);
            if changed_viewport {
                // Publish hidden search progress/counts without presenting spans
                // against a frame from another viewport, or copying hidden cells.
                annotations.viewport_token = self.annotations.viewport_token;
                annotations.selection.clear();
            }
            if (changed_viewport || changed_query)
                && let Some(search) = &mut annotations.search
            {
                search.matches.clear();
                search.selected_match.clear();
            }
        }
        annotations.revision = self.annotations.revision;
        self.publication.annotations_pending = false;
        self.publication.last_annotated = Some(Instant::now());
        if annotations == *self.annotations {
            return Ok(false);
        }
        annotations.revision = annotations
            .revision
            .checked_add(1)
            .context("terminal annotation revision overflow")?;
        self.annotations = Arc::new(annotations);
        Ok(true)
    }

    fn refresh_pending_annotations(&mut self) -> Result<bool> {
        if self.publication.annotations_pending
            && self
                .publication
                .last_annotated
                .is_none_or(|last| last.elapsed() >= FRAME_INTERVAL)
        {
            return self.refresh_annotations(false);
        }
        Ok(false)
    }

    fn interaction_tick(&mut self) -> Result<bool> {
        let Some(terminal) = &mut self.terminal else {
            return Ok(false);
        };
        let progressed = terminal.interaction_tick()?;
        if progressed {
            self.publication.annotations_pending = true;
            // A tick may autoscroll a selection or reveal a search result. The
            // next visible publication snapshots cells even with no VT bytes.
            self.frame_pending = true;
        }
        Ok(progressed)
    }

    fn search(&mut self, command: SearchCommand) -> Result<()> {
        if let SearchCommand::Set { query, .. } = &command {
            ensure!(
                query.len() <= MAX_SEARCH_QUERY_BYTES,
                "terminal search query exceeds limit"
            );
        }
        let generation = search_generation(&command);
        if self
            .search_intent
            .as_ref()
            .is_some_and(|previous| generation < search_generation(previous))
        {
            return Ok(());
        }
        let intent = matches!(
            command,
            SearchCommand::Set { .. } | SearchCommand::Cancel { .. }
        );
        if let Some(terminal) = &mut self.terminal {
            terminal.search(command.clone())?;
            self.frame_pending = true;
            self.publication.immediate_frame = true;
            self.refresh_annotations(false)?;
        } else {
            ensure!(
                intent,
                "terminal search navigation requires a restored snapshot"
            );
        }
        if intent {
            self.search_intent = Some(command);
        }
        Ok(())
    }

    fn select(&mut self, command: SelectionCommand) -> Result<()> {
        self.terminal
            .as_mut()
            .context("terminal selection requires a restored snapshot")?
            .select(command)?;
        self.frame_pending = true;
        self.publication.immediate_frame = true;
        self.refresh_annotations(false)?;
        Ok(())
    }

    fn feed_live(&mut self, bytes: &[u8]) -> Result<()> {
        self.pending.extend_from_slice(bytes);
        let mut consumed = 0;
        while consumed < self.pending.len() {
            let rest = &self.pending[consumed..];
            if self.passthrough_apc {
                let ending = rest.iter().enumerate().find_map(|(index, byte)| {
                    if matches!(byte, 0x18 | 0x1a | 0x9c) {
                        Some(index + 1)
                    } else if *byte == 0x1b && rest.get(index + 1) == Some(&b'\\') {
                        Some(index + 2)
                    } else {
                        None
                    }
                });
                let (length, ended) = ending.map_or_else(
                    || (rest.len() - usize::from(rest.last() == Some(&0x1b)), false),
                    |end| (end, true),
                );
                self.terminal
                    .as_mut()
                    .context("missing Rex terminal")?
                    .feed(&rest[..length]);
                consumed += length;
                self.passthrough_apc = !ended;
                if !ended {
                    break;
                }
                continue;
            }
            let Some(start) = rest.windows(2).position(|pair| pair == b"\x1b_") else {
                // Keep a final ESC so an APC can begin in the next network chunk.
                let length = rest.len() - usize::from(rest.last() == Some(&0x1b));
                self.terminal
                    .as_mut()
                    .context("missing Rex terminal")?
                    .feed(&rest[..length]);
                consumed += length;
                break;
            };
            self.terminal
                .as_mut()
                .context("missing Rex terminal")?
                .feed(&rest[..start]);
            consumed += start;
            let rest = &self.pending[consumed..];
            let prefix = format!("\x1b_rex1;{};", self.nonce);
            let compared = rest.len().min(prefix.len());
            if rest[..compared] != prefix.as_bytes()[..compared] {
                self.passthrough_apc = true;
                // Hand the introducer to Ghostty before entering passthrough.
                self.terminal
                    .as_mut()
                    .context("missing Rex terminal")?
                    .feed(&rest[..2]);
                consumed += 2;
                continue;
            }
            let Some(end) = rest[2..]
                .windows(2)
                .position(|pair| pair == b"\x1b\\")
                .map(|end| end + 4)
            else {
                ensure!(rest.len() <= MAX_APC_BYTES, "Rex APC exceeds client limit");
                break;
            };
            ensure!(end <= MAX_APC_BYTES, "Rex APC exceeds client limit");
            let apc = self.pending[consumed..consumed + end].to_vec();
            self.apply_apc(&apc)?;
            consumed += end;
        }
        self.pending.drain(..consumed);
        Ok(())
    }

    fn apply_apc(&mut self, bytes: &[u8]) -> Result<()> {
        let prefix = format!("\x1b_rex1;{};", self.nonce);
        if !bytes.starts_with(prefix.as_bytes()) {
            self.terminal
                .as_mut()
                .context("missing Rex terminal")?
                .feed(bytes);
            return Ok(());
        }
        let message: Value = serde_json::from_slice(&bytes[prefix.len()..bytes.len() - 2])
            .context("decode Rex terminal APC")?;
        match message["type"].as_str() {
            Some("resize") => {
                let columns =
                    u16::try_from(message["cols"].as_u64().context("missing Rex columns")?)?;
                let rows = u16::try_from(message["rows"].as_u64().context("missing Rex rows")?)?;
                ensure!(
                    usize::from(columns) * usize::from(rows) <= MAX_GRID_CELLS,
                    "Rex grid exceeds client limit"
                );
                let size = GridSize::new(columns, rows)?;
                let width = u32::try_from(message["cell_width_px"].as_u64().unwrap_or(0))?;
                let height = u32::try_from(message["cell_height_px"].as_u64().unwrap_or(0))?;
                self.terminal
                    .as_mut()
                    .context("missing Rex terminal")?
                    .resize(size, width, height)?;
            }
            Some("child_exited") => {
                self.exit_code = Some(i32::try_from(
                    message["exit_code"]
                        .as_i64()
                        .context("missing Rex exit code")?,
                )?);
            }
            Some("theme") => {
                self.apply_theme(&message)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn apply_theme(&mut self, message: &Value) -> Result<()> {
        // OSC color setters change only this presentation replica. Missing theme
        // colors reset to the terminal's built-in defaults as the protocol requires.
        let mut escapes = String::from("\x1b]110\x1b\\\x1b]111\x1b\\\x1b]112\x1b\\\x1b]104\x1b\\");
        for (name, code) in [("foreground", 10), ("background", 11), ("cursor", 12)] {
            if let Some(color) = message[name].as_str() {
                let color = checked_color(color)?;
                write!(escapes, "\x1b]{code};#{color}\x1b\\")?;
            }
        }
        if let Some(palette) = message["palette"].as_array() {
            ensure!(palette.len() <= 256, "Rex palette exceeds 256 colors");
            for entry in palette {
                let (index, color) = entry
                    .as_str()
                    .and_then(|value| value.split_once('='))
                    .context("invalid Rex palette entry")?;
                let index: u8 = index.parse()?;
                let color = checked_color(color)?;
                write!(escapes, "\x1b]4;{index};#{color}\x1b\\")?;
            }
        }
        self.terminal
            .as_mut()
            .context("missing Rex terminal")?
            .feed(escapes.as_bytes());
        Ok(())
    }
}

fn checked_color(color: &str) -> Result<&str> {
    let color = color.strip_prefix('#').unwrap_or(color);
    ensure!(
        color.len() == 6 && color.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid Rex theme color"
    );
    Ok(color)
}

fn search_generation(command: &SearchCommand) -> u64 {
    match command {
        SearchCommand::Set { generation, .. }
        | SearchCommand::Next { generation }
        | SearchCommand::Previous { generation }
        | SearchCommand::Cancel { generation } => *generation,
    }
}

/// Locate FINISH without interpreting terminal state. The native decoder validates
/// record order, CRC32C, state and continuation before publishing a terminal.
fn snapshot_end(bytes: &[u8], offset: &mut usize) -> Result<Option<usize>> {
    if bytes.len() < 10 {
        return Ok(None);
    }
    ensure!(
        &bytes[..8] == b"GHOSTSNP" && bytes[8..10] == [1, 0],
        "unsupported Rex Ghostty snapshot envelope"
    );
    if bytes.len() >= 24 {
        ensure!(
            bytes[10..12] == [1, 0],
            "Rex snapshot must begin with terminal state"
        );
        let columns = u16::from_le_bytes(bytes[20..22].try_into()?);
        let rows = u16::from_le_bytes(bytes[22..24].try_into()?);
        ensure!(
            columns != 0 && rows != 0 && usize::from(columns) * usize::from(rows) <= MAX_GRID_CELLS,
            "Rex snapshot grid exceeds client limit"
        );
    }
    while bytes.len().saturating_sub(*offset) >= 10 {
        let tag = u16::from_le_bytes(bytes[*offset..*offset + 2].try_into()?);
        let length = usize::try_from(u32::from_le_bytes(
            bytes[*offset + 2..*offset + 6].try_into()?,
        ))?;
        let end = offset
            .checked_add(10)
            .and_then(|start| start.checked_add(length))
            .context("Rex snapshot record overflow")?;
        ensure!(
            end <= MAX_SNAPSHOT_BYTES,
            "Rex snapshot record exceeds client limit"
        );
        if bytes.len() < end {
            return Ok(None);
        }
        *offset = end;
        if tag == 6 {
            ensure!(length == 0, "Rex snapshot FINISH has a payload");
            return Ok(Some(end));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_decoding_is_exact_and_rejects_noncanonical_padding() -> Result<()> {
        assert_eq!(
            decode_ticket("AAECAwQFBgcICQoLDA0ODw")?,
            std::array::from_fn(|index| index as u8)
        );
        assert!(decode_ticket("AAECAwQFBgcICQoLDA0ODx").is_err());
        assert!(decode_ticket("AAECAwQFBgcICQoLDA0OD=").is_err());
        assert!(decode_ticket("short").is_err());
        Ok(())
    }

    #[test]
    fn welcome_requires_exact_length_nonce_and_reserved_bytes() -> Result<()> {
        let mut bytes = [0_u8; 32];
        bytes[0] = 0x81;
        bytes[24] = 1;
        assert_eq!(Welcome::parse(&bytes)?.nonce, 1);
        assert!(Welcome::parse(&bytes[..31]).is_err());
        bytes[17] = 1;
        assert!(Welcome::parse(&bytes).is_err());
        bytes[17] = 0;
        bytes[24] = 0;
        assert!(Welcome::parse(&bytes).is_err());
        assert!(Welcome::parse(&[0x82, 7]).is_err());
        Ok(())
    }

    #[test]
    fn snapshot_boundary_survives_arbitrary_chunks_and_leaves_live_tail() -> Result<()> {
        let mut bytes = b"GHOSTSNP\x01\x00".to_vec();
        bytes.extend_from_slice(&[1, 0, 4, 0, 0, 0, 0, 0, 0, 0, 80, 0, 24, 0]);
        bytes.extend_from_slice(&[6, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        bytes.extend_from_slice(b"live");
        let mut offset = 10;
        for end in 0..34 {
            assert_eq!(snapshot_end(&bytes[..end], &mut offset)?, None);
        }
        assert_eq!(snapshot_end(&bytes, &mut offset)?, Some(34));
        assert_eq!(&bytes[34..], b"live");
        Ok(())
    }

    #[test]
    fn snapshot_rejects_oversized_records_before_buffering_payload() {
        let mut bytes = b"GHOSTSNP\x01\x00\x01\x00".to_vec();
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&[0; 4]);
        assert!(snapshot_end(&bytes, &mut 10).is_err());
        assert!(checked_color("aabbcc\x1b").is_err());
    }
    #[test]
    fn split_authenticated_apc_resizes_but_forged_nonce_does_not() -> Result<()> {
        let mut decoder = StreamDecoder::new(42);
        decoder.terminal = Some(GhosttyTerminal::new(
            crate::terminal::TerminalConfig::default(),
        )?);
        decoder.push(b"hello\x1b_rex1;4")?;
        decoder.push(b"2;{\"type\":\"resize\",\"cols\":10,\"rows\":3}\x1b")?;
        let frame = decoder
            .push(b"\\world")?
            .context("expected decoded frame")?;
        assert_eq!(frame.size, GridSize::new(10, 3)?);
        assert!(
            frame
                .text_lines()
                .iter()
                .any(|line| line.contains("helloworld"))
        );
        assert!(
            decoder
                .push(b"\x1b_rex1;41;{\"type\":\"resize\",\"cols\":1,\"rows\":1}\x1b\\")?
                .is_none()
        );
        assert_eq!(
            decoder
                .last_frame
                .as_ref()
                .context("last visible frame")?
                .size,
            GridSize::new(10, 3)?
        );
        decoder.push(b"\x1b_rex1;42;{\"type\":\"child_exited\",\"exit_code\":7}\x1b\\")?;
        assert_eq!(decoder.exit_code, Some(7));
        Ok(())
    }

    #[test]
    fn unknown_apc_streams_without_accumulating_large_payloads() -> Result<()> {
        let mut decoder = StreamDecoder::new(42);
        decoder.terminal = Some(GhosttyTerminal::new(
            crate::terminal::TerminalConfig::default(),
        )?);
        decoder.push(b"\x1b_G")?;
        let chunk = vec![b'x'; MAX_CHUNK_BYTES];
        for _ in 0..3 {
            decoder.push(&chunk)?;
            assert_eq!(decoder.pending, [] as [u8; 0]);
        }
        let frame = decoder
            .push(b"\x1b\\visible")?
            .context("expected decoded frame")?;
        assert!(
            frame
                .text_lines()
                .iter()
                .any(|line| line.contains("visible"))
        );
        Ok(())
    }
    #[test]
    fn cancelled_unknown_apc_does_not_hide_authenticated_resize() -> Result<()> {
        for terminator in [0x18, 0x1a, 0x9c] {
            let mut decoder = StreamDecoder::new(42);
            decoder.terminal = Some(GhosttyTerminal::new(
                crate::terminal::TerminalConfig::default(),
            )?);
            decoder.push(b"\x1b_not-rex")?;
            decoder.push(&[terminator])?;
            let frame = decoder
                .push(b"\x1b_rex1;42;{\"type\":\"resize\",\"cols\":10,\"rows\":3}\x1b\\")?
                .context("expected frame")?;
            assert_eq!(frame.size, GridSize::new(10, 3)?);
        }
        Ok(())
    }
    #[test]
    fn deferred_chunks_coalesce_and_nonvisual_updates_keep_frame_identity() -> Result<()> {
        let mut decoder = StreamDecoder::new(42);
        decoder.terminal = Some(GhosttyTerminal::new(
            crate::terminal::TerminalConfig::default(),
        )?);
        for _ in 0..10 {
            decoder.push_deferred(b"coalesced ")?;
        }
        assert!(decoder.last_frame.is_none());
        let frame = decoder.materialize()?.context("one coalesced frame")?;
        assert!(frame.text_lines().join("\n").contains("coalesced"));
        assert!(decoder.materialize()?.is_none());
        decoder.push_deferred(b"\x1b]2;title change\x1b\\")?;
        assert!(decoder.materialize()?.is_none());
        assert!(Arc::ptr_eq(
            decoder.last_frame.as_ref().context("last frame")?,
            &frame
        ));
        decoder.push_deferred(b"new visible text")?;
        let revealed = decoder
            .materialize()?
            .context("reveal pending state without another byte")?;
        assert!(!Arc::ptr_eq(&frame, &revealed));
        Ok(())
    }
}

#[cfg(test)]
mod interaction_tests;
