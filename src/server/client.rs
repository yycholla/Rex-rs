//! Nonblocking desktop cache backed by a reconnecting local socket worker.
use super::{
    default_socket_path,
    protocol::{
        MAX_GRID_CELLS, MAX_INPUT_BYTES, Operation, PROTOCOL_VERSION, PaneSnapshot, Request,
        Response, Snapshot, read_message, write_message,
    },
};
use crate::{
    core::{Command, GridSize, PaneId, Workspace, frame::TerminalFrame, input::KeyInput},
    terminal::PaneStatus,
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    io::ErrorKind,
    os::unix::net::UnixStream,
    path::PathBuf,
    pin::Pin,
    process::{Command as ProcessCommand, Stdio},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    task::{Context as TaskContext, Poll, Waker},
    thread,
    time::{Duration, Instant},
};

const COMMAND_CAPACITY: usize = 64;
const ERROR_CAPACITY: usize = 16;
const POLL_INTERVAL: Duration = Duration::from_millis(16);
const FAST_POLL_INTERVAL: Duration = Duration::from_millis(1);
const OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(4);
const INPUT_SETTLE_WINDOW: Duration = Duration::from_millis(50);
const OUTPUT_SETTLE_WINDOW: Duration = Duration::from_millis(100);
const RECONNECT_INTERVAL: Duration = Duration::from_millis(200);
const SOCKET_TIMEOUT: Duration = Duration::from_secs(2);

struct QueuedOperation {
    server_id: String,
    operation: Operation,
}

#[derive(Default)]
struct Mailbox {
    latest: Option<Response>,
    errors: VecDeque<(String, bool)>,
    connected: bool,
    has_control: bool,
    waiter: Option<Waker>,
}

struct WorkspaceUpdate {
    mailbox: Arc<Mutex<Mailbox>>,
    connected: bool,
    has_control: bool,
    registered: Option<Waker>,
}

impl Future for WorkspaceUpdate {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        let this = self.get_mut();
        let mut mailbox = this.mailbox.lock().unwrap_or_else(PoisonError::into_inner);
        if mailbox.latest.is_some()
            || !mailbox.errors.is_empty()
            || mailbox.connected != this.connected
            || (mailbox.connected && mailbox.has_control) != this.has_control
        {
            return Poll::Ready(());
        }
        // Inspect readiness and register under the same lock. A worker cannot
        // publish between those operations and leave an update asleep.
        if mailbox
            .waiter
            .as_ref()
            .is_none_or(|waiter| !waiter.will_wake(cx.waker()))
        {
            mailbox.waiter = Some(cx.waker().clone());
        }
        this.registered = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for WorkspaceUpdate {
    fn drop(&mut self) {
        let Some(registered) = &self.registered else {
            return;
        };
        let mut mailbox = self.mailbox.lock().unwrap_or_else(PoisonError::into_inner);
        if mailbox
            .waiter
            .as_ref()
            .is_some_and(|waiter| waiter.will_wake(registered))
        {
            mailbox.waiter = None;
        }
    }
}

/// A cached workspace whose IO and reconnect waits never run on the UI thread.
/// Dropping this client detaches it; it does not terminate the server or its PTYs.
pub struct WorkspaceClient {
    state: Workspace,
    panes: BTreeMap<PaneId, PaneSnapshot>,
    server_id: Option<String>,
    revision: u64,
    state_revision: u64,
    restored_metadata: bool,
    connected: bool,
    has_control: bool,
    commands: SyncSender<QueuedOperation>,
    mailbox: Arc<Mutex<Mailbox>>,
    stopped: Arc<AtomicBool>,
}

impl Default for WorkspaceClient {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceClient {
    /// Attach asynchronously, launching the current executable in server mode
    /// if the private local socket is missing. Initialize an empty server once.
    #[must_use]
    pub fn new() -> Self {
        match default_socket_path() {
            Ok(path) => Self::start(path, true),
            Err(error) => {
                let client = Self::start(PathBuf::new(), false);
                client.publish_error(format!("resolve local server socket: {error:#}"));
                client.stopped.store(true, Ordering::Release);
                client
            }
        }
    }

    /// Attach to an existing server without launching or creating a session.
    #[must_use]
    pub fn connect(path: PathBuf) -> Self {
        Self::start(path, false)
    }

    fn start(path: PathBuf, launch: bool) -> Self {
        let (commands, incoming) = mpsc::sync_channel(COMMAND_CAPACITY);
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_mailbox = Arc::clone(&mailbox);
        let worker_stopped = Arc::clone(&stopped);
        let worker = thread::Builder::new()
            .name("rex-server-client".into())
            .spawn(move || worker(&path, launch, &incoming, &worker_mailbox, &worker_stopped));
        let client = Self {
            state: Workspace::default(),
            panes: BTreeMap::new(),
            server_id: None,
            revision: 0,
            state_revision: 0,
            connected: false,
            has_control: false,
            restored_metadata: false,
            commands,
            mailbox,
            stopped,
        };
        if let Err(error) = worker {
            client.publish_error(format!("start local server client: {error}"));
        }
        client
    }

    fn publish_error(&self, error: String) {
        record_error(&self.mailbox, error);
    }

    #[must_use]
    pub fn state(&self) -> &Workspace {
        &self.state
    }

    #[must_use]
    pub fn frame(&self, pane: PaneId) -> Option<&Arc<TerminalFrame>> {
        self.panes.get(&pane).map(|entry| &entry.frame)
    }

    #[must_use]
    pub fn status(&self, pane: PaneId) -> Option<&PaneStatus> {
        self.panes.get(&pane)?.status.as_ref()
    }

    #[must_use]
    pub fn process_id(&self, pane: PaneId) -> Option<u32> {
        self.panes.get(&pane)?.process_id
    }

    #[must_use]
    pub const fn connected(&self) -> bool {
        self.connected
    }

    #[must_use]
    pub const fn has_control(&self) -> bool {
        self.connected && self.has_control
    }

    #[must_use]
    pub const fn restored_metadata(&self) -> bool {
        self.restored_metadata
    }

    /// Request terminal authority; explicit takeover revokes the old controller.
    ///
    /// # Errors
    /// Returns an error if detached or the bounded command queue is full.
    pub fn take_control(&mut self, force: bool) -> Result<()> {
        self.enqueue(Operation::AcquireControl { force })
    }

    /// Release terminal authority without closing any processes.
    ///
    /// # Errors
    /// Returns an error if detached or the bounded command queue is full.
    pub fn release_control(&mut self) -> Result<()> {
        self.enqueue(Operation::ReleaseControl)
    }

    #[must_use]
    pub fn server_id(&self) -> Option<&str> {
        self.server_id.as_deref()
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Local generation changes only when installed workspace metadata changes.
    #[must_use]
    pub const fn state_revision(&self) -> u64 {
        self.state_revision
    }

    /// Wait for a cache, error or connection/control change without polling IO.
    /// The owned future does not borrow this client and permits one outstanding
    /// waiter. Readiness persists until [`Self::poll`] drains the mailbox.
    pub fn wait_for_update(&self) -> impl Future<Output = ()> + Send + 'static + use<> {
        WorkspaceUpdate {
            mailbox: Arc::clone(&self.mailbox),
            connected: self.connected,
            has_control: self.has_control,
            registered: None,
        }
    }

    /// Drain the newest authoritative snapshot without waiting for socket IO.
    ///
    /// # Errors
    /// Reports queued server/transport errors once. Delivery failures are never
    /// replayed because a mutation may already have reached the server.
    pub fn poll(&mut self) -> Result<bool> {
        let Ok(mut mailbox) = self.mailbox.try_lock() else {
            return Ok(false);
        };
        let connection_changed = self.connected != mailbox.connected
            || self.has_control != (mailbox.connected && mailbox.has_control);
        self.has_control = mailbox.connected && mailbox.has_control;
        self.connected = mailbox.connected;
        if let Some((error, _)) = mailbox.errors.pop_front() {
            bail!(error);
        }
        let response = mailbox.latest.take();
        drop(mailbox);
        let Some(response) = response else {
            return Ok(connection_changed);
        };
        let epoch_changed = self.server_id.as_deref() != Some(response.server_id.as_str());
        self.server_id = Some(response.server_id);
        self.revision = response.revision;
        let Some(Snapshot {
            state,
            panes,
            restored_metadata,
        }) = response.snapshot
        else {
            return Ok(connection_changed || epoch_changed);
        };
        if epoch_changed || self.state != state {
            self.state_revision = self.state_revision.saturating_add(1);
        }
        self.restored_metadata = restored_metadata;
        self.state = state;
        self.panes = panes;
        Ok(true)
    }

    /// Queue the same typed mutation used by buttons and shortcuts.
    ///
    /// # Errors
    /// Rejects invalid cached commands, detached clients, and queue exhaustion.
    /// Successful enqueue is not a server acknowledgement; poll reports errors.
    pub fn apply(&mut self, command: Command) -> Result<()> {
        let mut candidate = self.state.clone();
        candidate.apply(command.clone())?;
        self.enqueue(Operation::Apply(command))
    }

    /// Queue raw input for a live pane.
    ///
    /// # Errors
    /// Rejects missing panes, oversized input, disconnection or a full queue.
    pub fn write_input(&mut self, pane: PaneId, bytes: &[u8]) -> Result<()> {
        ensure!(
            bytes.len() <= MAX_INPUT_BYTES,
            "terminal input exceeds 64 KiB"
        );
        self.check_pane(pane)?;
        self.enqueue(Operation::Input {
            pane,
            bytes: bytes.to_vec(),
        })
    }

    /// Queue a mode-dependent terminal key.
    ///
    /// # Errors
    /// Rejects missing panes, oversized keys, disconnection or a full queue.
    pub fn send_key(&mut self, pane: PaneId, input: &KeyInput) -> Result<()> {
        self.check_pane(pane)?;
        ensure!(
            input.key.len() <= MAX_INPUT_BYTES
                && input
                    .text
                    .as_ref()
                    .is_none_or(|text| text.len() <= MAX_INPUT_BYTES),
            "terminal key exceeds input limit"
        );
        self.enqueue(Operation::Key {
            pane,
            input: input.clone(),
        })
    }

    /// Queue a paste for server-side terminal mode encoding.
    ///
    /// # Errors
    /// Rejects missing panes, oversized text, disconnection or a full queue.
    pub fn paste(&mut self, pane: PaneId, text: &str) -> Result<()> {
        self.check_pane(pane)?;
        ensure!(
            text.len() <= MAX_INPUT_BYTES.saturating_sub(12),
            "paste exceeds server byte limit"
        );
        self.enqueue(Operation::Paste {
            pane,
            text: text.to_owned(),
        })
    }

    /// Queue a history viewport movement.
    ///
    /// # Errors
    /// Rejects missing panes, disconnection or a full queue.
    pub fn scroll(&mut self, pane: PaneId, lines: i32) -> Result<()> {
        self.check_pane(pane)?;
        self.enqueue(Operation::Scroll { pane, lines })
    }

    /// Queue a measured terminal and PTY resize; response bytes stay server-side.
    ///
    /// # Errors
    /// Rejects excessive grids, missing panes, disconnection or a full queue.
    pub fn resize(
        &mut self,
        pane: PaneId,
        size: GridSize,
        cell_width: u32,
        cell_height: u32,
    ) -> Result<Vec<u8>> {
        self.check_pane(pane)?;
        ensure!(
            usize::from(size.columns()) * usize::from(size.rows()) <= MAX_GRID_CELLS,
            "terminal viewport exceeds server cell limit"
        );
        self.enqueue(Operation::Resize {
            pane,
            size,
            cell_width,
            cell_height,
        })?;
        Ok(Vec::new())
    }

    fn check_pane(&self, pane: PaneId) -> Result<()> {
        ensure!(
            self.has_control,
            "this client is an observer; take terminal control first"
        );
        ensure!(
            self.panes.contains_key(&pane),
            "pane {pane} no longer exists"
        );
        Ok(())
    }

    fn enqueue(&self, operation: Operation) -> Result<()> {
        ensure!(self.connected, "local server is disconnected; reconnecting");
        let server_id = self
            .server_id
            .clone()
            .context("local server has not attached")?;
        self.commands
            .try_send(QueuedOperation {
                server_id,
                operation,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => anyhow::anyhow!("local server command queue is full"),
                TrySendError::Disconnected(_) => anyhow::anyhow!("local server worker stopped"),
            })
    }
}

impl Drop for WorkspaceClient {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

fn record_error(mailbox: &Mutex<Mailbox>, message: String) {
    record_error_kind(mailbox, message, false);
}

fn record_transport_error(mailbox: &Mutex<Mailbox>, message: String) {
    record_error_kind(mailbox, message, true);
}

fn record_error_kind(mailbox: &Mutex<Mailbox>, mut message: String, transport: bool) {
    if message.len() > 4096 {
        let mut end = 4096;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    let mut mailbox = mailbox.lock().unwrap_or_else(PoisonError::into_inner);
    if mailbox
        .errors
        .back()
        .is_some_and(|(last, kind)| last == &message && *kind == transport)
    {
        return;
    }
    if mailbox.errors.len() == ERROR_CAPACITY {
        mailbox.errors.pop_front();
    }
    mailbox.errors.push_back((message, transport));
    let waiter = mailbox.waiter.take();
    drop(mailbox);
    if let Some(waiter) = waiter {
        waiter.wake();
    }
}

fn publish(mailbox: &Mutex<Mailbox>, response: Response) {
    let mut mailbox = mailbox.lock().unwrap_or_else(PoisonError::into_inner);
    let changed = !mailbox.connected
        || mailbox.has_control != response.has_control
        || response.snapshot.is_some();
    mailbox.connected = true;
    mailbox.has_control = response.has_control;
    // A successful attach supersedes old connectivity errors, while command
    // rejection and uncertain-delivery errors remain visible.
    mailbox.errors.retain(|(_, transport)| !transport);
    let obsolete = if response.snapshot.is_some() {
        mailbox.latest.replace(response)
    } else {
        None
    };
    let waiter = changed.then(|| mailbox.waiter.take()).flatten();
    drop(mailbox);
    if let Some(waiter) = waiter {
        waiter.wake();
    }
    // A slow UI can leave an unconsumed screen here. Destroy its cell strings
    // outside the mutex so a cache poll cannot miss a frame while they are freed.
    drop(obsolete);
}

fn mark_disconnected(mailbox: &Mutex<Mailbox>) -> bool {
    let mut mailbox = mailbox.lock().unwrap_or_else(PoisonError::into_inner);
    let connected = mailbox.connected;
    mailbox.connected = false;
    mailbox.has_control = false;
    let waiter = connected.then(|| mailbox.waiter.take()).flatten();
    drop(mailbox);
    if let Some(waiter) = waiter {
        waiter.wake();
    }
    connected
}

fn exchange(stream: &mut UnixStream, request: &Request) -> Result<Response> {
    write_message(stream, request)?;
    let response: Response = read_message(stream)?;
    if let Some(snapshot) = &response.snapshot {
        snapshot.validate()?;
    }
    ensure!(
        !response.server_id.is_empty() && response.server_id.len() <= 128,
        "invalid local server identity"
    );
    ensure!(
        response.version == PROTOCOL_VERSION,
        "unsupported local server protocol version"
    );
    Ok(response)
}

fn exchange_cached(
    stream: &mut UnixStream,
    request: &Request,
    baseline: &mut Option<(Snapshot, u64)>,
) -> Result<Response> {
    let mut response = exchange(stream, request)?;
    if let Some(update) = response.update.take() {
        ensure!(
            response.snapshot.is_none(),
            "response contains both a snapshot and an update"
        );
        let previous = baseline
            .as_ref()
            .map(|(snapshot, revision)| (snapshot, *revision));
        response.snapshot = Some(update.apply(previous)?);
    }
    if let Some(snapshot) = &response.snapshot {
        *baseline = Some((snapshot.clone(), response.revision));
    }
    Ok(response)
}

struct Attachment {
    epoch: String,
    revision: u64,
    baseline: Option<(Snapshot, u64)>,
}

fn launch_server(path: &std::path::Path) -> Result<()> {
    let executable = std::env::current_exe().context("locate desktop executable")?;
    let mut child = ProcessCommand::new(executable)
        .arg("--server")
        .arg("--socket")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("launch local server")?;
    // The detached reaper does not own terminal resources or terminate the child.
    thread::Builder::new()
        .name("rex-server-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .context("start server process reaper")?;
    Ok(())
}

fn worker(
    path: &std::path::Path,
    launch: bool,
    commands: &Receiver<QueuedOperation>,
    mailbox: &Mutex<Mailbox>,
    stopped: &AtomicBool,
) {
    let mut launched = false;
    let mut initialize = launch;
    let mut reported_disconnection = false;
    while !stopped.load(Ordering::Acquire) {
        let connection = UnixStream::connect(path);
        let mut stream = match connection {
            Ok(stream) => stream,
            Err(error) => {
                if launch
                    && !launched
                    && matches!(
                        error.kind(),
                        ErrorKind::NotFound | ErrorKind::ConnectionRefused
                    )
                {
                    launched = true;
                    if let Err(error) = launch_server(path) {
                        record_transport_error(mailbox, format!("{error:#}"));
                    }
                } else if !reported_disconnection {
                    record_transport_error(
                        mailbox,
                        format!("local server disconnected: {error}; reconnecting"),
                    );
                    reported_disconnection = true;
                }
                thread::sleep(RECONNECT_INTERVAL);
                continue;
            }
        };
        let result = run_connection(&mut stream, commands, mailbox, stopped, &mut initialize);
        let was_connected = mark_disconnected(mailbox);
        if was_connected {
            reported_disconnection = false;
        }
        if stopped.load(Ordering::Acquire) {
            break;
        }
        if let Err(error) = result {
            if !reported_disconnection {
                record_transport_error(
                    mailbox,
                    format!("local server disconnected: {error:#}; reconnecting"),
                );
            }
            reported_disconnection = true;
        } else {
            reported_disconnection = false;
        }
        thread::sleep(RECONNECT_INTERVAL);
    }
}

fn attach(
    stream: &mut UnixStream,
    mailbox: &Mutex<Mailbox>,
    initialize: &mut bool,
) -> Result<Attachment> {
    let mut baseline = None;
    let mut response = exchange_cached(
        stream,
        &Request {
            version: PROTOCOL_VERSION,
            server_id: None,
            known_revision: None,
            operation: Operation::Poll,
        },
        &mut baseline,
    )?;
    if let Some(error) = response.error.take() {
        bail!(error);
    }
    ensure!(
        response.snapshot.is_some(),
        "attach did not return a full snapshot"
    );
    let epoch = response.server_id.clone();
    let mut revision = response.revision;
    let initialize_workspace = *initialize
        && !response
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.restored_metadata);
    // A recovered empty workspace is intentional, and must stay empty.
    *initialize = false;
    publish(mailbox, response);
    if initialize_workspace {
        // A lost acknowledgement must not replay initialization on reconnect.
        let mut response = exchange_cached(
            stream,
            &Request {
                version: PROTOCOL_VERSION,
                server_id: Some(epoch.clone()),
                known_revision: Some(revision),
                operation: Operation::Initialize {
                    title: "Development".into(),
                },
            },
            &mut baseline,
        )?;
        ensure!(
            response.server_id == epoch && response.revision >= revision,
            "initialization returned inconsistent server identity or revision"
        );
        if response.snapshot.is_some() {
            revision = response.revision;
        }
        if let Some(error) = response.error.take() {
            record_error(mailbox, error);
        }
        publish(mailbox, response);
    }
    let mut response = exchange_cached(
        stream,
        &Request {
            version: PROTOCOL_VERSION,
            server_id: Some(epoch.clone()),
            known_revision: Some(revision),
            operation: Operation::AcquireControl { force: false },
        },
        &mut baseline,
    )?;
    ensure!(
        response.server_id == epoch && response.revision >= revision,
        "control attachment changed server identity or revision"
    );
    if response.snapshot.is_some() {
        revision = response.revision;
    }
    if let Some(error) = response.error.take() {
        record_error(mailbox, error);
    }
    publish(mailbox, response);
    Ok(Attachment {
        epoch,
        revision,
        baseline,
    })
}

fn run_connection(
    stream: &mut UnixStream,
    commands: &Receiver<QueuedOperation>,
    mailbox: &Mutex<Mailbox>,
    stopped: &AtomicBool,
    initialize: &mut bool,
) -> Result<()> {
    stream.set_read_timeout(Some(SOCKET_TIMEOUT))?;
    stream.set_write_timeout(Some(SOCKET_TIMEOUT))?;
    let Attachment {
        epoch,
        mut revision,
        mut baseline,
    } = attach(stream, mailbox, initialize)?;
    let mut fast_until = Instant::now();
    let mut output_until = fast_until;
    while !stopped.load(Ordering::Acquire) {
        let interval = poll_interval(Instant::now(), fast_until, output_until);
        let operation = match commands.recv_timeout(interval) {
            Ok(command) => {
                if command.server_id != epoch {
                    record_error(
                        mailbox,
                        "discarded queued command for a previous server instance".into(),
                    );
                    continue;
                }
                command.operation
            }
            Err(RecvTimeoutError::Timeout) => Operation::Poll,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        let mutation = !matches!(operation, Operation::Snapshot | Operation::Poll);
        if matches!(
            operation,
            Operation::Input { .. } | Operation::Key { .. } | Operation::Paste { .. }
        ) {
            fast_until = Instant::now() + INPUT_SETTLE_WINDOW;
        }
        let result = exchange_cached(
            stream,
            &Request {
                version: PROTOCOL_VERSION,
                server_id: Some(epoch.clone()),
                known_revision: Some(revision),
                operation,
            },
            &mut baseline,
        );
        let mut response = match result {
            Ok(response) => response,
            Err(error) => {
                if mutation {
                    record_error(
                        mailbox,
                        "command delivery is unknown after connection loss; it was not replayed"
                            .into(),
                    );
                }
                return Err(error);
            }
        };
        ensure!(
            response.server_id == epoch && response.revision >= revision,
            "server identity or revision changed inconsistently during attachment"
        );
        if response.snapshot.is_some() {
            revision = response.revision;
            // Continuous output must not pay an idle 16 ms delay after every
            // encode/decode round trip. Return to idle promptly once it settles.
            output_until = Instant::now() + OUTPUT_SETTLE_WINDOW;
        }
        if let Some(error) = response.error.take() {
            record_error(mailbox, error);
        }
        publish(mailbox, response);
    }
    Ok(())
}

fn poll_interval(now: Instant, input_until: Instant, output_until: Instant) -> Duration {
    if now < input_until {
        FAST_POLL_INTERVAL
    } else if now < output_until {
        OUTPUT_POLL_INTERVAL
    } else {
        POLL_INTERVAL
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests fail immediately when protocol fixture construction fails."
)]
mod tests {
    use super::*;
    use crate::core::frame::Rgb;
    use std::{sync::atomic::AtomicUsize, task::Wake};

    struct MailboxWake {
        mailbox: Arc<Mutex<Mailbox>>,
        count: AtomicUsize,
    }

    impl Wake for MailboxWake {
        fn wake(self: Arc<Self>) {
            assert!(
                self.mailbox.try_lock().is_ok(),
                "wake must release the mailbox lock"
            );
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn update_waiter(mailbox: &Arc<Mutex<Mailbox>>, connected: bool) -> WorkspaceUpdate {
        WorkspaceUpdate {
            mailbox: Arc::clone(mailbox),
            connected,
            has_control: false,
            registered: None,
        }
    }

    fn empty_response(control: bool) -> Response {
        Response {
            version: PROTOCOL_VERSION,
            server_id: "wake-test".into(),
            revision: 0,
            has_control: control,
            update: None,
            snapshot: None,
            error: None,
        }
    }

    #[test]
    fn cache_notifications_check_readiness_before_and_after_registration() {
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        let counter = Arc::new(MailboxWake {
            mailbox: Arc::clone(&mailbox),
            count: AtomicUsize::new(0),
        });
        let waker = Waker::from(Arc::clone(&counter));
        let mut cx = TaskContext::from_waker(&waker);
        let mut waiter = update_waiter(&mailbox, false);
        assert!(Pin::new(&mut waiter).poll(&mut cx).is_pending());
        record_error(&mailbox, "new error".into());
        assert_eq!(counter.count.load(Ordering::Relaxed), 1);
        assert!(Pin::new(&mut waiter).poll(&mut cx).is_ready());
        drop(waiter);
        // An update predating registration is ready without requiring a wake.
        let mut waiter = update_waiter(&mailbox, false);
        assert!(Pin::new(&mut waiter).poll(&mut cx).is_ready());
        assert_eq!(counter.count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn idle_acknowledgements_do_not_wake_but_control_changes_do() {
        let mailbox = Arc::new(Mutex::new(Mailbox {
            connected: true,
            ..Mailbox::default()
        }));
        let counter = Arc::new(MailboxWake {
            mailbox: Arc::clone(&mailbox),
            count: AtomicUsize::new(0),
        });
        let waker = Waker::from(Arc::clone(&counter));
        let mut cx = TaskContext::from_waker(&waker);
        let mut waiter = update_waiter(&mailbox, true);
        assert!(Pin::new(&mut waiter).poll(&mut cx).is_pending());
        publish(&mailbox, empty_response(false));
        assert_eq!(counter.count.load(Ordering::Relaxed), 0);
        assert!(Pin::new(&mut waiter).poll(&mut cx).is_pending());
        publish(&mailbox, empty_response(true));
        assert_eq!(counter.count.load(Ordering::Relaxed), 1);
        assert!(Pin::new(&mut waiter).poll(&mut cx).is_ready());
    }

    #[test]
    fn cancelled_cache_waiter_releases_its_waker() {
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        let counter = Arc::new(MailboxWake {
            mailbox: Arc::clone(&mailbox),
            count: AtomicUsize::new(0),
        });
        let waker = Waker::from(counter);
        let mut cx = TaskContext::from_waker(&waker);
        let mut waiter = update_waiter(&mailbox, false);
        assert!(Pin::new(&mut waiter).poll(&mut cx).is_pending());
        assert!(mailbox.lock().unwrap().waiter.is_some());
        drop(waiter);
        assert!(mailbox.lock().unwrap().waiter.is_none());
    }

    #[test]
    fn screen_publication_and_disconnect_wake_outside_the_mailbox_lock() {
        let mailbox = Arc::new(Mutex::new(Mailbox {
            connected: true,
            ..Mailbox::default()
        }));
        let counter = Arc::new(MailboxWake {
            mailbox: Arc::clone(&mailbox),
            count: AtomicUsize::new(0),
        });
        let waker = Waker::from(Arc::clone(&counter));
        let mut cx = TaskContext::from_waker(&waker);
        let mut waiter = update_waiter(&mailbox, true);
        assert!(Pin::new(&mut waiter).poll(&mut cx).is_pending());
        let mut response = empty_response(false);
        response.snapshot = Some(Snapshot {
            state: Workspace::default(),
            panes: BTreeMap::new(),
            restored_metadata: false,
        });
        publish(&mailbox, response);
        assert_eq!(counter.count.load(Ordering::Relaxed), 1);
        assert!(Pin::new(&mut waiter).poll(&mut cx).is_ready());
        drop(waiter);
        mailbox.lock().unwrap().latest.take();
        let mut waiter = update_waiter(&mailbox, true);
        assert!(Pin::new(&mut waiter).poll(&mut cx).is_pending());
        assert!(mark_disconnected(&mailbox));
        assert_eq!(counter.count.load(Ordering::Relaxed), 2);
        assert!(Pin::new(&mut waiter).poll(&mut cx).is_ready());
    }

    #[test]
    fn active_output_polling_expires_and_preserves_input_priority() {
        let now = Instant::now();
        let input_until = now + INPUT_SETTLE_WINDOW;
        let output_until = now + OUTPUT_SETTLE_WINDOW;
        assert_eq!(
            poll_interval(now, input_until, output_until),
            FAST_POLL_INTERVAL
        );
        assert_eq!(
            poll_interval(input_until, input_until, output_until),
            OUTPUT_POLL_INTERVAL
        );
        assert_eq!(
            poll_interval(output_until, input_until, output_until),
            POLL_INTERVAL
        );
    }

    #[test]
    fn restored_empty_workspace_is_not_automatically_initialized() {
        let (mut client, mut peer) = UnixStream::pair().unwrap();
        client.set_read_timeout(Some(SOCKET_TIMEOUT)).unwrap();
        let fake_server = thread::spawn(move || {
            let request: Request = read_message(&mut peer).unwrap();
            assert!(matches!(request.operation, Operation::Poll));
            write_message(
                &mut peer,
                &Response {
                    version: PROTOCOL_VERSION,
                    server_id: "recovered".into(),
                    revision: 0,
                    has_control: false,
                    update: None,
                    snapshot: Some(Snapshot {
                        state: Workspace::default(),
                        panes: BTreeMap::new(),
                        restored_metadata: true,
                    }),
                    error: None,
                },
            )
            .unwrap();
            let request: Request = read_message(&mut peer).unwrap();
            assert!(matches!(
                request.operation,
                Operation::AcquireControl { force: false }
            ));
            write_message(
                &mut peer,
                &Response {
                    version: PROTOCOL_VERSION,
                    server_id: "recovered".into(),
                    revision: 1,
                    has_control: true,
                    update: None,
                    snapshot: None,
                    error: None,
                },
            )
            .unwrap();
        });
        let mailbox = Mutex::new(Mailbox::default());
        let mut initialize = true;
        attach(&mut client, &mailbox, &mut initialize).unwrap();
        assert!(!initialize);
        let mailbox = mailbox.lock().unwrap();
        assert_eq!(
            mailbox
                .latest
                .as_ref()
                .unwrap()
                .snapshot
                .as_ref()
                .unwrap()
                .state
                .sessions(),
            []
        );
        fake_server.join().unwrap();
    }

    #[test]
    fn successful_attach_discards_only_obsolete_transport_errors() {
        let mailbox = Mutex::new(Mailbox::default());
        record_transport_error(&mailbox, "disconnected".into());
        record_error(&mailbox, "command delivery unknown".into());
        publish(
            &mailbox,
            Response {
                version: PROTOCOL_VERSION,
                server_id: "reattached".into(),
                revision: 0,
                has_control: false,
                update: None,
                snapshot: Some(Snapshot {
                    restored_metadata: false,
                    state: Workspace::default(),
                    panes: BTreeMap::new(),
                }),
                error: None,
            },
        );
        let mailbox = mailbox.lock().unwrap();
        assert!(mailbox.connected);
        assert_eq!(mailbox.errors.len(), 1);
        assert_eq!(mailbox.errors[0].0, "command delivery unknown");
        assert!(mailbox.latest.is_some());
    }

    #[test]
    fn malformed_peer_snapshot_is_rejected_before_publication() {
        let (mut client, mut peer) = UnixStream::pair().unwrap();
        client.set_read_timeout(Some(SOCKET_TIMEOUT)).unwrap();
        let fake_server = thread::spawn(move || {
            let _: Request = read_message(&mut peer).unwrap();
            let mut state = Workspace::default();
            state
                .apply(Command::CreateSession {
                    title: "malformed peer".into(),
                })
                .unwrap();
            let pane = state.pane_ids()[0];
            let snapshot = Snapshot {
                restored_metadata: false,
                state,
                panes: BTreeMap::from([(
                    pane,
                    PaneSnapshot {
                        frame: Arc::new(TerminalFrame {
                            size: GridSize::default(),
                            rows: Vec::new(),
                            cursor: None,
                            foreground: Rgb::default(),
                            background: Rgb::default(),
                        }),
                        status: None,
                        process_id: None,
                    },
                )]),
            };
            write_message(
                &mut peer,
                &Response {
                    version: PROTOCOL_VERSION,
                    server_id: "fake-peer".into(),
                    revision: 1,
                    has_control: false,
                    update: None,
                    snapshot: Some(snapshot),
                    error: None,
                },
            )
            .unwrap();
        });
        let response = exchange(
            &mut client,
            &Request {
                version: PROTOCOL_VERSION,
                server_id: None,
                known_revision: None,
                operation: Operation::Snapshot,
            },
        );
        assert!(response.is_err());
        fake_server.join().unwrap();
    }
}
