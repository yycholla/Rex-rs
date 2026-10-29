//! Local socket transport. Only the owner loop touches terminal handles.
use super::{
    default_socket_path,
    protocol::{
        MAX_GRID_CELLS, MAX_INPUT_BYTES, Operation, PROTOCOL_VERSION, PaneSnapshot, Request,
        Response, Snapshot, read_message, write_message,
    },
    storage::{MetadataStore, default_state_path},
    updates::SnapshotUpdate,
};
use crate::{
    core::{Command, PaneId, Workspace, frame::TerminalFrame},
    terminal::{PaneStatus, ShellConfig, TerminalConfig, TerminalWorkspace},
};
use anyhow::{Context, Result, bail, ensure};
use nix::{
    errno::Errno,
    unistd::{geteuid, getpgrp, getpid, setsid},
};
use std::{
    collections::BTreeMap,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{ErrorKind, Write},
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_CLIENTS: usize = 16;
const MAX_PANES: usize = 16;
const MAX_LABEL_CHARACTERS: usize = 128;
const REQUEST_QUEUE: usize = 32;
const TICK: Duration = Duration::from_millis(8);
const SOCKET_TIMEOUT: Duration = Duration::from_secs(2);

struct PendingRequest {
    request: Request,
    client_id: u64,
    alive: Arc<AtomicBool>,
    response: SyncSender<Response>,
    delivered: Receiver<()>,
}

#[derive(Default)]
struct ControlState {
    next_client: AtomicU64,
    owner: AtomicU64,
}

struct Endpoint {
    listener: UnixListener,
    path: PathBuf,
    device: u64,
    inode: u64,
    // The open file owns the process lock through socket cleanup.
    _lock: File,
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        }) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl Endpoint {
    fn bind(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "server socket path must be absolute");
        let parent = path
            .parent()
            .context("server socket needs a parent directory")?;
        private_directory(parent)?;
        let mut lock_name = path
            .file_name()
            .context("server socket needs a file name")?
            .to_os_string();
        lock_name.push(".lock");
        let lock_path = path.with_file_name(lock_name);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(&lock_path)
            .with_context(|| format!("open server lock {}", lock_path.display()))?;
        let lock_metadata = lock.metadata()?;
        ensure!(
            lock_metadata.is_file()
                && lock_metadata.uid() == geteuid().as_raw()
                && lock_metadata.permissions().mode() & 0o777 == 0o600,
            "server lock must be a private regular file owned by this user"
        );
        lock.try_lock()
            .context("another server owns this socket lock")?;
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_socket() && metadata.uid() == geteuid().as_raw(),
                    "refusing to replace a non-socket or foreign socket"
                );
                // A live listener may predate the lock convention. Never unlink it.
                match UnixStream::connect(path) {
                    Ok(_) => bail!("another server is already listening"),
                    Err(error) if error.kind() == ErrorKind::ConnectionRefused => {
                        fs::remove_file(path).context("remove stale server socket")?;
                    }
                    Err(error) => return Err(error).context("check existing server socket"),
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect server socket"),
        }
        let listener = UnixListener::bind(path).context("bind local server socket")?;
        let metadata = fs::symlink_metadata(path)?;
        let endpoint = Self {
            listener,
            path: path.to_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
            _lock: lock,
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        endpoint.listener.set_nonblocking(true)?;
        Ok(endpoint)
    }
}

fn private_directory(path: &Path) -> Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("create private server directory"),
    }
    let metadata = fs::symlink_metadata(path).context("inspect private server directory")?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == geteuid().as_raw()
            && metadata.permissions().mode() & 0o777 == 0o700,
        "server socket directory must be mode 0700 and owned by this user"
    );
    Ok(())
}

/// Run the local server with its terminal owner on the calling thread.
/// Client connection loss never removes panes or terminates their processes.
///
/// # Errors
/// Returns an error for an unsafe/busy endpoint, thread creation, terminal setup,
/// or a fatal owner-loop failure. Explicit shutdown drops the terminal workspace.
pub fn serve(path: &Path, config: TerminalConfig, shell: ShellConfig) -> Result<()> {
    serve_with_store(path, config, shell, None)
}

fn serve_with_store(
    path: &Path,
    config: TerminalConfig,
    shell: ShellConfig,
    store: Option<&MetadataStore>,
) -> Result<()> {
    ensure!(
        grid_cells(config.size) <= MAX_GRID_CELLS,
        "initial grid is too large"
    );
    let endpoint = Endpoint::bind(path)?;
    let recovered = store.map(MetadataStore::load).transpose()?.flatten();
    let restored_metadata = recovered.is_some();
    let mut runtime = match recovered {
        Some(state) => {
            validate_restored_state(&state, config)?;
            TerminalWorkspace::restore(state, config, shell)?
        }
        None => TerminalWorkspace::live(config, shell),
    };
    runtime.defer_hidden_frame_updates();
    let server_id = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let (sender, receiver) = mpsc::sync_channel(REQUEST_QUEUE);
    let connected = Arc::new(AtomicUsize::new(0));
    let stopping = Arc::new(AtomicBool::new(false));
    let control = Arc::new(ControlState::default());
    let result = owner_loop(
        &endpoint.listener,
        &mut runtime,
        config,
        &server_id,
        &sender,
        &receiver,
        &connected,
        &stopping,
        &control,
        store,
        restored_metadata,
    );
    stopping.store(true, Ordering::Release);
    let cleanup = runtime.shutdown(SOCKET_TIMEOUT);
    result.and(cleanup)
}

#[allow(
    clippy::too_many_arguments,
    reason = "The owner loop borrows transport and terminal state; none may escape to IO workers."
)]
fn owner_loop(
    listener: &UnixListener,
    runtime: &mut TerminalWorkspace,
    config: TerminalConfig,
    server_id: &str,
    sender: &SyncSender<PendingRequest>,
    receiver: &Receiver<PendingRequest>,
    connected: &Arc<AtomicUsize>,
    stopping: &Arc<AtomicBool>,
    control: &Arc<ControlState>,
    store: Option<&MetadataStore>,
    restored_metadata: bool,
) -> Result<()> {
    let mut revision = 0_u64;
    let mut previous_controller = 0;
    let mut metadata_dirty = false;
    let mut previous_poll_error = None;
    let mut waiting = None;
    loop {
        accept_clients(listener, sender, connected, stopping, control)?;
        let current_controller = control.owner.load(Ordering::Acquire);
        if previous_controller != current_controller {
            revision = revision
                .checked_add(1)
                .context("server revision exhausted")?;
            previous_controller = current_controller;
        }
        let before_poll = Marker::capture(runtime);
        let poll = runtime.poll();
        if before_poll.changed(runtime) {
            revision = revision
                .checked_add(1)
                .context("server revision exhausted")?;
        }
        if let Err(error) = poll {
            let message = format!("{error:#}");
            if previous_poll_error.as_ref() != Some(&message) {
                eprintln!("Terminal polling failed: {message}");
            }
            previous_poll_error = Some(message);
        } else {
            previous_poll_error = None;
        }
        for _ in 0..REQUEST_QUEUE {
            let Some(pending) = waiting.take().or_else(|| receiver.try_recv().ok()) else {
                break;
            };
            let before = Marker::capture(runtime);
            let mut result = dispatch_request(runtime, &pending, config, server_id, control);
            // Echo can arrive after the first owner poll or while waiting for a
            // request. Include it in this response instead of another UI cycle.
            if let Err(error) = runtime.poll()
                && result.is_ok()
            {
                result = Err(error);
            }
            // Automation explicitly requests authoritative text for every pane.
            // Desktops need only the visible layout's materialized cells.
            if matches!(pending.request.operation, Operation::Snapshot)
                && result.is_ok()
                && let Err(error) = runtime.refresh_all_frames()
            {
                result = Err(error);
            }
            if before.state != *runtime.state() {
                metadata_dirty = true;
            }
            if metadata_dirty
                && result.is_ok()
                && matches!(
                    pending.request.operation,
                    Operation::Apply(_) | Operation::Initialize { .. } | Operation::Shutdown
                )
                && let Some(store) = store
            {
                match store.save(runtime.state()) {
                    Ok(()) => metadata_dirty = false,
                    Err(error) => {
                        result = Err(error)
                            .context("command applied in memory but durable metadata save failed");
                    }
                }
            }
            let current_controller = control.owner.load(Ordering::Acquire);
            if before.changed(runtime) || previous_controller != current_controller {
                previous_controller = current_controller;
                revision = revision
                    .checked_add(1)
                    .context("server revision exhausted")?;
            }
            let shutdown = result.as_ref().is_ok_and(|shutdown| *shutdown);
            let snapshot = (pending.request.known_revision != Some(revision)
                || pending.request.server_id.as_deref() != Some(server_id))
            .then(|| snapshot(runtime, restored_metadata));
            let response = Response {
                version: PROTOCOL_VERSION,
                server_id: server_id.to_owned(),
                revision,
                has_control: control.owner.load(Ordering::Acquire) == pending.client_id,
                snapshot,
                update: None,
                error: result.err().map(|error| format!("{error:#}")),
            };
            // A disconnected or slow client cannot hold the terminal owner.
            let _ = pending.response.try_send(response);
            if shutdown {
                // Final shutdown is the only place the owner waits for socket IO:
                // do not let process exit kill the worker before it writes the ack.
                pending
                    .delivered
                    .recv_timeout(SOCKET_TIMEOUT)
                    .context("deliver shutdown response before process exit")?;
                return Ok(());
            }
        }
        // Existing clients wake the owner as soon as input arrives. The timeout
        // only schedules output/accept polling when no requests are pending.
        waiting = receiver.recv_timeout(TICK).ok();
    }
}

fn accept_clients(
    listener: &UnixListener,
    sender: &SyncSender<PendingRequest>,
    connected: &Arc<AtomicUsize>,
    stopping: &Arc<AtomicBool>,
    control: &Arc<ControlState>,
) -> Result<()> {
    // A connection flood gets a bounded portion of an owner tick.
    for _ in 0..MAX_CLIENTS {
        let (stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("accept local client"),
        };
        if connected.load(Ordering::Acquire) >= MAX_CLIENTS {
            continue;
        }
        stream.set_read_timeout(Some(SOCKET_TIMEOUT))?;
        stream.set_write_timeout(Some(SOCKET_TIMEOUT))?;
        let sender = sender.clone();
        let connected = Arc::clone(connected);
        let stopping = Arc::clone(stopping);
        connected.fetch_add(1, Ordering::AcqRel);
        let count = Arc::clone(&connected);
        let control = Arc::clone(control);
        let client_id = control.next_client.fetch_add(1, Ordering::AcqRel) + 1;
        let alive = Arc::new(AtomicBool::new(true));
        if let Err(error) = thread::Builder::new()
            .name("rex-client-io".into())
            .spawn(move || {
                let _guard = ConnectionCount {
                    connected,
                    control,
                    client_id,
                    alive: Arc::clone(&alive),
                };
                serve_connection(stream, &sender, &stopping, client_id, &alive);
            })
        {
            count.fetch_sub(1, Ordering::AcqRel);
            return Err(error).context("start client IO worker");
        }
    }
    Ok(())
}

struct ConnectionCount {
    connected: Arc<AtomicUsize>,
    control: Arc<ControlState>,
    client_id: u64,
    alive: Arc<AtomicBool>,
}
impl Drop for ConnectionCount {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        let _ = self.control.owner.compare_exchange(
            self.client_id,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.connected.fetch_sub(1, Ordering::AcqRel);
    }
}

fn serve_connection(
    mut stream: UnixStream,
    sender: &SyncSender<PendingRequest>,
    stopping: &AtomicBool,
    client_id: u64,
    alive: &Arc<AtomicBool>,
) {
    let mut incremental = false;
    let mut baseline: Option<(Snapshot, u64)> = None;
    while !stopping.load(Ordering::Acquire) {
        let Ok(request) = read_message::<Request>(&mut stream) else {
            break;
        };
        incremental |= matches!(request.operation, Operation::Poll);
        let compact = incremental && !matches!(request.operation, Operation::Snapshot);
        let known = request.known_revision;
        let (response, receiver) = mpsc::sync_channel(1);
        let (delivered, completion) = mpsc::sync_channel(1);
        if sender
            .try_send(PendingRequest {
                request,
                client_id,
                alive: Arc::clone(alive),
                response,
                delivered: completion,
            })
            .is_err()
        {
            break;
        }
        let Ok(mut response) = receiver.recv_timeout(SOCKET_TIMEOUT) else {
            break;
        };
        let next = prepare_snapshot_response(&mut response, compact, known, baseline.as_ref());
        let mut encoded = Vec::new();
        let encoded_snapshot = match write_message(&mut encoded, &response) {
            Ok(()) => true,
            Err(error) => {
                // The failed payload never becomes a connection baseline.
                encoded.clear();
                response.snapshot = None;
                response.update = None;
                response.error = Some(format!("cannot encode server snapshot: {error:#}"));
                if write_message(&mut encoded, &response).is_err() {
                    break;
                }
                false
            }
        };
        if stream.write_all(&encoded).is_err() {
            break;
        }
        if encoded_snapshot && let Some(next) = next {
            baseline = Some(next);
        }
        let _ = delivered.try_send(());
    }
}

/// Compact desktops need current text only for the shared visible layout. Hidden
/// terminals keep running; their status and metadata still cross every update.
/// Return precisely the projected snapshot whose successful delivery can become
/// the connection baseline. Explicit snapshots and baseline resets stay complete.
fn prepare_snapshot_response(
    response: &mut Response,
    compact: bool,
    known: Option<u64>,
    baseline: Option<&(Snapshot, u64)>,
) -> Option<(Snapshot, u64)> {
    let mut snapshot = response.snapshot.take()?;
    if compact {
        let previous = baseline
            .filter(|(_, revision)| Some(*revision) == known)
            .map(|(snapshot, revision)| (snapshot, *revision));
        if let Some((previous, _)) = previous {
            let visible = snapshot
                .state
                .active_session()
                .and_then(crate::core::Session::active_window)
                .map_or_else(Vec::new, |window| {
                    window
                        .zoomed_pane()
                        .map_or_else(|| window.layout().panes(), |pane| vec![pane])
                });
            for (id, pane) in &mut snapshot.panes {
                if !visible.contains(id)
                    && let Some(before) = previous.panes.get(id)
                    // Preserve the current aggregate grid budget. A hidden
                    // resize needs one complete frame at its new dimensions.
                    && before.frame.size == pane.frame.size
                {
                    pane.frame = Arc::clone(&before.frame);
                }
            }
        }
        response.update = Some(SnapshotUpdate::between(&snapshot, previous));
    } else {
        response.snapshot = Some(snapshot.clone());
    }
    Some((snapshot, response.revision))
}

struct Marker {
    state: Workspace,
    panes: BTreeMap<PaneId, (Arc<TerminalFrame>, Option<PaneStatus>)>,
}

impl Marker {
    fn capture(runtime: &TerminalWorkspace) -> Self {
        Self {
            state: runtime.state().clone(),
            panes: runtime
                .state()
                .pane_ids()
                .into_iter()
                .filter_map(|pane| {
                    Some((
                        pane,
                        (
                            Arc::clone(runtime.frame(pane)?),
                            runtime.status(pane).cloned(),
                        ),
                    ))
                })
                .collect(),
        }
    }

    fn changed(&self, runtime: &TerminalWorkspace) -> bool {
        self.state != *runtime.state()
            || self.panes.iter().any(|(pane, (frame, status))| {
                runtime
                    .frame(*pane)
                    .is_none_or(|next| !Arc::ptr_eq(frame, next))
                    || runtime.status(*pane) != status.as_ref()
            })
    }
}

fn snapshot(runtime: &TerminalWorkspace, restored_metadata: bool) -> Snapshot {
    Snapshot {
        restored_metadata,
        state: runtime.state().clone(),
        panes: runtime
            .state()
            .pane_ids()
            .into_iter()
            .filter_map(|pane| {
                Some((
                    pane,
                    PaneSnapshot {
                        frame: Arc::clone(runtime.frame(pane)?),
                        status: runtime.status(pane).cloned(),
                        process_id: runtime.process_id(pane),
                    },
                ))
            })
            .collect(),
    }
}

fn dispatch_request(
    runtime: &mut TerminalWorkspace,
    pending: &PendingRequest,
    config: TerminalConfig,
    server_id: &str,
    control: &ControlState,
) -> Result<bool> {
    let request = &pending.request;
    ensure!(
        request.version == PROTOCOL_VERSION,
        "unsupported local protocol version"
    );
    if !matches!(request.operation, Operation::Snapshot | Operation::Poll) {
        ensure!(
            request.server_id.as_deref() == Some(server_id),
            "server identity changed; attach again before sending commands"
        );
    }
    match &request.operation {
        Operation::AcquireControl { force } => {
            ensure!(
                pending.alive.load(Ordering::Acquire),
                "client disconnected before acquiring control"
            );
            if *force {
                control.owner.store(pending.client_id, Ordering::Release);
            } else {
                let _ = control.owner.compare_exchange(
                    0,
                    pending.client_id,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            if !pending.alive.load(Ordering::Acquire) {
                let _ = control.owner.compare_exchange(
                    pending.client_id,
                    0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                bail!("client disconnected while acquiring control");
            }
            Ok(false)
        }
        Operation::ReleaseControl => {
            let _ = control.owner.compare_exchange(
                pending.client_id,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            Ok(false)
        }
        Operation::Input { .. }
        | Operation::Key { .. }
        | Operation::Paste { .. }
        | Operation::Scroll { .. }
        | Operation::Resize { .. } => {
            ensure!(
                pending.alive.load(Ordering::Acquire)
                    && control.owner.load(Ordering::Acquire) == pending.client_id,
                "terminal control belongs to another client; acquire control explicitly"
            );
            apply_request(runtime, request, config, server_id)
        }
        _ => apply_request(runtime, request, config, server_id),
    }
}

fn validate_restored_state(state: &Workspace, config: TerminalConfig) -> Result<()> {
    ensure!(state.is_valid(), "saved workspace is invalid");
    let count = state.pane_ids().len();
    ensure!(
        count <= MAX_PANES && count * grid_cells(config.size) <= MAX_GRID_CELLS,
        "saved workspace exceeds server limits"
    );
    Ok(())
}

fn apply_request(
    runtime: &mut TerminalWorkspace,
    request: &Request,
    config: TerminalConfig,
    server_id: &str,
) -> Result<bool> {
    ensure!(
        request.version == PROTOCOL_VERSION,
        "unsupported local protocol version"
    );
    if !matches!(request.operation, Operation::Snapshot | Operation::Poll) {
        ensure!(
            request.server_id.as_deref() == Some(server_id),
            "server identity changed; attach again before sending commands"
        );
    }
    match &request.operation {
        Operation::AcquireControl { .. } | Operation::ReleaseControl => {
            bail!("control request must use connection dispatch")
        }
        Operation::Snapshot | Operation::Poll => {}
        Operation::Initialize { title } => {
            validate_label(title)?;
            if runtime.state().sessions().is_empty() {
                let command = Command::CreateSession {
                    title: title.clone(),
                };
                validate_command(runtime, &command, config)?;
                runtime.apply(command)?;
            }
        }
        Operation::Apply(command) => {
            validate_command(runtime, command, config)?;
            runtime.apply(command.clone())?;
        }
        Operation::Input { pane, bytes } => {
            ensure!(
                bytes.len() <= MAX_INPUT_BYTES,
                "input exceeds the per-message limit"
            );
            runtime.write_input(*pane, bytes)?;
        }
        Operation::Key { pane, input } => {
            ensure!(
                input.key.len() + input.text.as_ref().map_or(0, String::len) <= MAX_INPUT_BYTES,
                "key input exceeds the per-message limit"
            );
            runtime.send_key(*pane, input)?;
        }
        Operation::Paste { pane, text } => {
            ensure!(
                text.len() <= MAX_INPUT_BYTES.saturating_sub(12),
                "paste exceeds the per-message limit"
            );
            runtime.paste(*pane, text)?;
        }
        Operation::Scroll { pane, lines } => runtime.scroll(*pane, *lines)?,
        Operation::Resize {
            pane,
            size,
            cell_width,
            cell_height,
        } => {
            ensure!(
                *cell_width <= 1024 && *cell_height <= 1024,
                "cell pixel dimensions are too large"
            );
            let existing = runtime.state().pane_ids();
            ensure!(existing.contains(pane), "pane no longer exists");
            let total = existing.into_iter().try_fold(0_usize, |total, id| {
                let cells = if id == *pane {
                    grid_cells(*size)
                } else {
                    runtime.frame(id).map_or(0, |frame| grid_cells(frame.size))
                };
                total.checked_add(cells).context("grid cell count overflow")
            })?;
            ensure!(
                total <= MAX_GRID_CELLS,
                "workspace grid cell budget exceeded"
            );
            runtime.resize(*pane, *size, *cell_width, *cell_height)?;
        }
        Operation::Shutdown => return Ok(true),
    }
    Ok(false)
}

fn grid_cells(size: crate::core::GridSize) -> usize {
    usize::from(size.columns()) * usize::from(size.rows())
}

fn validate_command(
    runtime: &TerminalWorkspace,
    command: &Command,
    config: TerminalConfig,
) -> Result<()> {
    match command {
        Command::CreateSession { title }
        | Command::CreateWindow { title, .. }
        | Command::RenameSession { title, .. }
        | Command::RenameWindow { title, .. } => validate_label(title)?,
        _ => {}
    }
    let mut candidate = runtime.state().clone();
    candidate.apply(command.clone())?;
    let panes = candidate.pane_ids();
    ensure!(panes.len() <= MAX_PANES, "workspace pane limit reached");
    let total = panes.into_iter().try_fold(0_usize, |total, pane| {
        let size = runtime.frame(pane).map_or(config.size, |frame| frame.size);
        total
            .checked_add(grid_cells(size))
            .context("grid cell count overflow")
    })?;
    ensure!(
        total <= MAX_GRID_CELLS,
        "workspace grid cell budget exceeded"
    );
    Ok(())
}

fn validate_label(title: &str) -> Result<()> {
    ensure!(
        title.chars().count() <= MAX_LABEL_CHARACTERS,
        "labels must be at most {MAX_LABEL_CHARACTERS} characters"
    );
    Ok(())
}

/// Parse standalone server arguments and run until explicit shutdown.
/// The desktop executable also invokes this entry point for `--server`.
///
/// # Errors
/// Returns an error for invalid arguments or a failed server lifetime.
pub fn run_default() -> Result<()> {
    let mut path = None;
    let mut shutdown = false;
    let mut state_file = None;
    let mut ephemeral = false;
    let mut args = std::env::args_os().skip(1);
    while let Some(argument) = args.next() {
        if argument == "--server" {
            continue;
        }
        if argument == "--socket" {
            ensure!(path.is_none(), "--socket may only be supplied once");
            path = Some(PathBuf::from(
                args.next().context("--socket requires a path")?,
            ));
        } else if argument == "--shutdown" {
            ensure!(!shutdown, "--shutdown may only be supplied once");
            shutdown = true;
        } else if argument == "--state-file" {
            ensure!(
                state_file.is_none(),
                "--state-file may only be supplied once"
            );
            state_file = Some(PathBuf::from(
                args.next().context("--state-file requires a path")?,
            ));
        } else if argument == "--ephemeral" {
            ensure!(!ephemeral, "--ephemeral may only be supplied once");
            ephemeral = true;
        } else if argument == "--help" || argument == "-h" {
            println!("rex-server [--socket PATH] [--state-file PATH | --ephemeral] [--shutdown]");
            return Ok(());
        } else {
            bail!("unknown server argument: {}", argument.to_string_lossy());
        }
    }
    ensure!(
        !ephemeral || state_file.is_none(),
        "--ephemeral conflicts with --state-file"
    );
    let path = path.map_or_else(default_socket_path, Ok)?;
    if shutdown {
        return request_shutdown(&path);
    }
    // A UI-launched child must not share the terminal's session or foreground
    // process group. Interactive standalone servers may already be group leaders.
    match setsid() {
        Ok(_) => {}
        Err(Errno::EPERM) if getpgrp() == getpid() => {}
        Err(error) => return Err(error).context("detach local server process session"),
    }
    let store = if ephemeral {
        None
    } else {
        let state_path = state_file.map_or_else(|| state_path_for_socket(&path), Ok)?;
        Some(MetadataStore::open(state_path)?)
    };
    serve_with_store(
        &path,
        TerminalConfig::default(),
        ShellConfig::default(),
        store.as_ref(),
    )
}

fn state_path_for_socket(path: &Path) -> Result<PathBuf> {
    if default_socket_path().is_ok_and(|default| default == path) {
        default_state_path()
    } else {
        let mut name = path
            .file_name()
            .context("socket needs a file name")?
            .to_os_string();
        name.push(".workspace.json");
        Ok(path.with_file_name(name))
    }
}

fn request_shutdown(path: &Path) -> Result<()> {
    let mut stream = UnixStream::connect(path).context("connect to local server for shutdown")?;
    stream.set_read_timeout(Some(SOCKET_TIMEOUT))?;
    stream.set_write_timeout(Some(SOCKET_TIMEOUT))?;
    let mut request = Request {
        version: PROTOCOL_VERSION,
        server_id: None,
        known_revision: None,
        operation: Operation::Snapshot,
    };
    write_message(&mut stream, &request)?;
    let mut response: Response = read_message(&mut stream)?;
    if response.version == 2 && PROTOCOL_VERSION != 2 {
        // Upgrade management only: old servers understand Snapshot/Shutdown,
        // but not desktop row updates. Do not auto-stop a user's live server.
        request.version = 2;
        write_message(&mut stream, &request)?;
        response = read_message(&mut stream)?;
    }
    ensure!(
        response.version == request.version,
        "unsupported local server version"
    );
    ensure!(
        response.error.is_none(),
        "cannot attach for shutdown: {:?}",
        response.error
    );
    let server_id = response.server_id;
    request.server_id = Some(server_id.clone());
    request.known_revision = Some(response.revision);
    request.operation = Operation::Shutdown;
    write_message(&mut stream, &request)?;
    let response: Response = read_message(&mut stream)?;
    ensure!(
        response.version == request.version && response.server_id == server_id,
        "server changed during shutdown request"
    );
    ensure!(
        response.error.is_none(),
        "server rejected shutdown: {:?}",
        response.error
    );
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests assert fixture preconditions and fail immediately when setup breaks."
)]
mod tests {
    use super::*;
    use crate::core::{
        GridSize, SplitAxis,
        frame::{Cell, Rgb},
    };
    use std::os::unix::fs::symlink;

    struct FixtureDirectory(PathBuf);
    impl FixtureDirectory {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "rex-host-test-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
        fn socket(&self) -> PathBuf {
            self.0.join("server.sock")
        }
    }
    impl Drop for FixtureDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn request(operation: Operation) -> Request {
        Request {
            version: PROTOCOL_VERSION,
            server_id: Some("fixture".into()),
            known_revision: None,
            operation,
        }
    }

    fn text_frame(text: &str, columns: u16) -> Arc<TerminalFrame> {
        let mut cells = vec![Cell::default(); usize::from(columns)];
        cells[0].text = text.into();
        Arc::new(TerminalFrame {
            images: Arc::default(),
            graphics_warning: None,
            size: GridSize::new(columns, 1).unwrap(),
            rows: vec![Arc::new(cells)],
            cursor: None,
            foreground: Rgb::default(),
            background: Rgb::default(),
        })
    }

    fn two_tab_snapshot() -> Snapshot {
        let mut state = Workspace::default();
        state
            .apply(Command::CreateSession {
                title: "first".into(),
            })
            .unwrap();
        state
            .apply(Command::CreateWindow {
                session: state.active_session().unwrap().id(),
                title: "second".into(),
            })
            .unwrap();
        let panes = state
            .pane_ids()
            .into_iter()
            .map(|id| {
                (
                    id,
                    PaneSnapshot {
                        frame: text_frame("old", 2),
                        status: Some(PaneStatus::Running),
                        process_id: Some(123),
                    },
                )
            })
            .collect();
        Snapshot {
            state,
            panes,
            restored_metadata: false,
        }
    }

    fn snapshot_response(snapshot: Snapshot) -> Response {
        Response {
            version: PROTOCOL_VERSION,
            server_id: "fixture".into(),
            revision: 8,
            has_control: true,
            snapshot: Some(snapshot),
            update: None,
            error: None,
        }
    }

    #[test]
    fn hidden_output_is_deferred_while_visible_text_and_hidden_status_stay_current() {
        let before = two_tab_snapshot();
        let hidden = before.state.pane_ids()[0];
        let visible = before
            .state
            .active_session()
            .unwrap()
            .active_pane()
            .unwrap();
        let baseline = (before, 7);
        let mut current = baseline.0.clone();
        current.panes.get_mut(&hidden).unwrap().frame = text_frame("hidden-new", 2);
        current.panes.get_mut(&hidden).unwrap().status = Some(PaneStatus::Exited { code: 3 });
        current.panes.get_mut(&hidden).unwrap().process_id = None;
        current.panes.get_mut(&visible).unwrap().frame = text_frame("visible-new", 2);
        let mut response = snapshot_response(current);
        let delivered =
            prepare_snapshot_response(&mut response, true, Some(7), Some(&baseline)).unwrap();
        let update = response.update.unwrap();
        assert!(update.panes[&hidden].rows.is_empty());
        assert!(!update.panes[&visible].rows.is_empty());
        let cached = update.apply(Some((&baseline.0, 7))).unwrap();
        assert!(Arc::ptr_eq(
            &cached.panes[&hidden].frame,
            &baseline.0.panes[&hidden].frame
        ));
        assert_eq!(
            cached.panes[&hidden].status,
            Some(PaneStatus::Exited { code: 3 })
        );
        assert_eq!(cached.panes[&hidden].process_id, None);
        assert_eq!(cached.panes[&visible].frame.rows[0][0].text, "visible-new");
        assert!(Arc::ptr_eq(
            &delivered.0.panes[&hidden].frame,
            &baseline.0.panes[&hidden].frame
        ));
        cached.validate().unwrap();
    }

    #[test]
    fn selecting_a_hidden_tab_publishes_its_latest_frame() {
        let initial = two_tab_snapshot();
        let first_window = initial.state.active_session().unwrap().windows()[0].id();
        let hidden = initial.state.pane_ids()[0];
        let baseline = (initial, 7);
        let mut current = baseline.0.clone();
        current.panes.get_mut(&hidden).unwrap().frame = text_frame("latest", 2);
        let mut hidden_response = snapshot_response(current.clone());
        let delivered =
            prepare_snapshot_response(&mut hidden_response, true, Some(7), Some(&baseline))
                .unwrap();
        assert!(hidden_response.update.unwrap().panes.is_empty());
        current
            .state
            .apply(Command::SelectWindow(first_window))
            .unwrap();
        let mut revealed = snapshot_response(current);
        revealed.revision = 9;
        prepare_snapshot_response(&mut revealed, true, Some(8), Some(&delivered)).unwrap();
        let update = revealed.update.unwrap();
        assert!(!update.panes[&hidden].rows.is_empty());
        let cached = update.apply(Some((&delivered.0, 8))).unwrap();
        assert_eq!(cached.panes[&hidden].frame.rows[0][0].text, "latest");
        assert_eq!(
            cached
                .state
                .active_session()
                .unwrap()
                .active_window()
                .unwrap()
                .id(),
            first_window
        );
    }

    #[test]
    fn unzooming_publishes_the_latest_frame_of_the_covered_split() {
        let mut initial = two_tab_snapshot();
        let first = initial
            .state
            .active_session()
            .unwrap()
            .active_pane()
            .unwrap();
        initial
            .state
            .apply(Command::SplitPane {
                pane: first,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        let second = initial
            .state
            .active_session()
            .unwrap()
            .active_pane()
            .unwrap();
        initial.panes.insert(second, initial.panes[&first].clone());
        initial.state.apply(Command::ToggleZoom(first)).unwrap();
        let baseline = (initial, 7);
        let mut current = baseline.0.clone();
        current.panes.get_mut(&second).unwrap().frame = text_frame("covered", 2);
        let mut hidden = snapshot_response(current.clone());
        let delivered =
            prepare_snapshot_response(&mut hidden, true, Some(7), Some(&baseline)).unwrap();
        assert!(hidden.update.unwrap().panes.is_empty());
        current.state.apply(Command::ToggleZoom(first)).unwrap();
        let mut revealed = snapshot_response(current);
        prepare_snapshot_response(&mut revealed, true, Some(8), Some(&delivered)).unwrap();
        let update = revealed.update.unwrap();
        assert!(!update.panes[&second].rows.is_empty());
        let cached = update.apply(Some((&delivered.0, 8))).unwrap();
        assert_eq!(cached.panes[&second].frame.rows[0][0].text, "covered");
    }

    #[test]
    fn hidden_resize_resets_and_explicit_snapshots_keep_current_frames() {
        let baseline = (two_tab_snapshot(), 7);
        let hidden = baseline.0.state.pane_ids()[0];
        let mut current = baseline.0.clone();
        current.panes.get_mut(&hidden).unwrap().frame = text_frame("resized", 3);
        let mut response = snapshot_response(current.clone());
        prepare_snapshot_response(&mut response, true, Some(7), Some(&baseline)).unwrap();
        let update = response.update.unwrap();
        assert!(!update.panes[&hidden].rows.is_empty());
        let cached = update.apply(Some((&baseline.0, 7))).unwrap();
        assert_eq!(cached.panes[&hidden].frame.size.columns(), 3);
        cached.validate().unwrap();

        current.panes.get_mut(&hidden).unwrap().frame = text_frame("fresh", 2);
        for (compact, known) in [(false, Some(7)), (true, None), (true, Some(6))] {
            let mut response = snapshot_response(current.clone());
            let delivered =
                prepare_snapshot_response(&mut response, compact, known, Some(&baseline)).unwrap();
            assert_eq!(delivered.0.panes[&hidden].frame.rows[0][0].text, "fresh");
            let received = if compact {
                let update = response.update.unwrap();
                assert_eq!(update.base_revision, None);
                update.apply(None).unwrap()
            } else {
                assert!(response.update.is_none());
                response.snapshot.unwrap()
            };
            assert_eq!(received.panes[&hidden].frame.rows[0][0].text, "fresh");
            received.validate().unwrap();
        }
    }

    #[test]
    fn a_new_hidden_pane_receives_a_complete_initial_frame() {
        let baseline = (two_tab_snapshot(), 7);
        let session = baseline.0.state.active_session().unwrap();
        let visible_window = session.active_window().unwrap().id();
        let mut current = baseline.0.clone();
        current
            .state
            .apply(Command::CreateWindow {
                session: session.id(),
                title: "new hidden".into(),
            })
            .unwrap();
        let pane = current
            .state
            .active_session()
            .unwrap()
            .active_pane()
            .unwrap();
        let mut added = baseline.0.panes.values().next().unwrap().clone();
        added.frame = text_frame("new-pane", 2);
        current.panes.insert(pane, added);
        current
            .state
            .apply(Command::SelectWindow(visible_window))
            .unwrap();
        let mut response = snapshot_response(current);
        prepare_snapshot_response(&mut response, true, Some(7), Some(&baseline)).unwrap();
        let update = response.update.unwrap();
        assert_eq!(update.panes[&pane].rows.len(), 1);
        let cached = update.apply(Some((&baseline.0, 7))).unwrap();
        assert_eq!(cached.panes[&pane].frame.rows[0][0].text, "new-pane");
        cached.validate().unwrap();
    }

    #[test]
    fn endpoint_lock_preserves_live_socket_and_recovers_stale_socket() {
        let directory = FixtureDirectory::new();
        let endpoint = Endpoint::bind(&directory.socket()).unwrap();
        let inode = fs::symlink_metadata(directory.socket()).unwrap().ino();
        assert!(Endpoint::bind(&directory.socket()).is_err());
        assert_eq!(
            fs::symlink_metadata(directory.socket()).unwrap().ino(),
            inode
        );
        drop(endpoint);
        assert!(!directory.socket().exists());
        // A dead listener leaves a socket that can be recovered under the lock.
        drop(UnixListener::bind(directory.socket()).unwrap());
        let recovered = Endpoint::bind(&directory.socket()).unwrap();
        assert!(UnixStream::connect(directory.socket()).is_ok());
        drop(recovered);
    }

    #[test]
    fn rejects_nonprivate_parent_and_preserves_non_socket_paths() {
        let directory = FixtureDirectory::new();
        fs::write(directory.socket(), b"keep me").unwrap();
        assert!(Endpoint::bind(&directory.socket()).is_err());
        assert_eq!(fs::read(directory.socket()).unwrap(), b"keep me");
        fs::remove_file(directory.socket()).unwrap();
        symlink(directory.0.join("target"), directory.socket()).unwrap();
        assert!(Endpoint::bind(&directory.socket()).is_err());
        assert!(
            fs::symlink_metadata(directory.socket())
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Endpoint::bind(&directory.socket()).is_err());
    }

    #[test]
    fn cleanup_does_not_unlink_a_replaced_socket_path() {
        let directory = FixtureDirectory::new();
        let endpoint = Endpoint::bind(&directory.socket()).unwrap();
        fs::remove_file(directory.socket()).unwrap();
        fs::write(directory.socket(), b"replacement").unwrap();
        drop(endpoint);
        assert_eq!(fs::read(directory.socket()).unwrap(), b"replacement");
    }

    #[test]
    fn stale_incarnation_and_wrong_version_cannot_mutate_state() {
        let config = TerminalConfig::default();
        let mut runtime = TerminalWorkspace::new(config);
        let invalid_label = request(Operation::Initialize {
            title: "界".repeat(MAX_LABEL_CHARACTERS + 1),
        });
        assert!(apply_request(&mut runtime, &invalid_label, config, "fixture").is_err());
        let mut stale = request(Operation::Initialize {
            title: "fixture".into(),
        });
        stale.server_id = Some("old-server".into());
        assert!(apply_request(&mut runtime, &stale, config, "fixture").is_err());
        stale.server_id = Some("fixture".into());
        stale.version = PROTOCOL_VERSION + 1;
        assert!(apply_request(&mut runtime, &stale, config, "fixture").is_err());
        assert_eq!(runtime.state().sessions(), []);
        let mut attach = request(Operation::Snapshot);
        attach.server_id = None;
        assert!(!apply_request(&mut runtime, &attach, config, "fixture").unwrap());
    }

    #[test]
    fn initialize_is_atomic_and_noop_commands_do_not_change_marker() {
        let config = TerminalConfig::default();
        let mut runtime = TerminalWorkspace::new(config);
        let initialize = request(Operation::Initialize {
            title: "fixture".into(),
        });
        apply_request(&mut runtime, &initialize, config, "fixture").unwrap();
        let original = Marker::capture(&runtime);
        apply_request(&mut runtime, &initialize, config, "fixture").unwrap();
        assert!(!original.changed(&runtime));
        let session = runtime.state().active_session().unwrap().id();
        apply_request(
            &mut runtime,
            &request(Operation::Apply(Command::SelectSession(session))),
            config,
            "fixture",
        )
        .unwrap();
        assert!(!original.changed(&runtime));
        snapshot(&runtime, false).validate().unwrap();
    }

    #[test]
    fn rejects_grid_and_pane_budgets_before_allocating() {
        let config = TerminalConfig::default();
        let mut runtime = TerminalWorkspace::new(config);
        apply_request(
            &mut runtime,
            &request(Operation::Initialize {
                title: "fixture".into(),
            }),
            config,
            "fixture",
        )
        .unwrap();
        let pane = runtime
            .state()
            .active_session()
            .unwrap()
            .active_pane()
            .unwrap();
        let original = Marker::capture(&runtime);
        let oversized = request(Operation::Resize {
            pane,
            size: GridSize::new(4096, 4096).unwrap(),
            cell_width: 8,
            cell_height: 16,
        });
        assert!(apply_request(&mut runtime, &oversized, config, "fixture").is_err());
        assert!(!original.changed(&runtime));
        for index in 1..MAX_PANES {
            runtime
                .apply(Command::CreateSession {
                    title: format!("fixture {index}"),
                })
                .unwrap();
        }
        let original = Marker::capture(&runtime);
        assert!(
            apply_request(
                &mut runtime,
                &request(Operation::Apply(Command::CreateSession {
                    title: "over budget".into()
                })),
                config,
                "fixture",
            )
            .is_err()
        );
        assert!(!original.changed(&runtime));
    }

    #[test]
    fn explicit_upgrade_shutdown_can_negotiate_previous_server_version() {
        let directory = FixtureDirectory::new();
        let path = directory.socket();
        let listener = UnixListener::bind(&path).unwrap();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            for (index, expected) in [PROTOCOL_VERSION, 2, 2].into_iter().enumerate() {
                let request: Request = read_message(&mut stream).unwrap();
                assert_eq!(request.version, expected);
                if index == 2 {
                    assert!(matches!(request.operation, Operation::Shutdown));
                    assert_eq!(request.server_id.as_deref(), Some("old-server"));
                } else {
                    assert!(matches!(request.operation, Operation::Snapshot));
                }
                write_message(
                    &mut stream,
                    &Response {
                        version: 2,
                        server_id: "old-server".into(),
                        revision: 4,
                        has_control: false,
                        snapshot: None,
                        update: None,
                        error: (index == 0).then(|| "unsupported local protocol version".into()),
                    },
                )
                .unwrap();
            }
        });
        request_shutdown(&path).unwrap();
        peer.join().unwrap();
    }
}
