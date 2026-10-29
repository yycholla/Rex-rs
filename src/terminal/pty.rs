//! Blocking PTY IO stays on workers; the application only drains bounded queues.
use crate::core::GridSize;
use anyhow::{Context, Result, bail, ensure};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::{Pid, getpgid, getpgrp, getsid},
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    collections::VecDeque,
    ffi::OsString,
    io::{ErrorKind, Read, Write},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, PoisonError, TryLockError,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread,
    time::Duration,
};

const OUTPUT_CHUNK_BYTES: usize = 8 * 1024;
const OUTPUT_QUEUE_CHUNKS: usize = 64;
const INPUT_QUEUE_MESSAGES: usize = 32;
const MAX_INPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct ShellConfig {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
}

impl Default for ShellConfig {
    fn default() -> Self {
        let program = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .filter(|path| {
                path.is_absolute()
                    && path.metadata().is_ok_and(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    })
            })
            .unwrap_or_else(|| PathBuf::from("/bin/sh"));
        Self {
            program,
            args: Vec::new(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PaneStatus {
    Running,
    Exited { code: u32 },
    Failed { message: String },
}

#[derive(Default)]
struct OutputBuffer {
    chunks: VecDeque<Vec<u8>>,
    error: Option<String>,
    eof: bool,
}

struct OutputQueue {
    buffer: Mutex<OutputBuffer>,
    space_available: Condvar,
    receiver_alive: AtomicBool,
}

struct OutputReceiver(Arc<OutputQueue>);

impl Drop for OutputReceiver {
    fn drop(&mut self) {
        self.0.close_receiver();
    }
}

#[derive(Default)]
struct OutputDrain {
    chunks: Vec<Vec<u8>>,
    error: Option<String>,
    closed: bool,
}

impl OutputQueue {
    fn new() -> Self {
        Self {
            buffer: Mutex::new(OutputBuffer::default()),
            space_available: Condvar::new(),
            receiver_alive: AtomicBool::new(true),
        }
    }

    fn push(&self, mut bytes: &[u8]) -> bool {
        let mut buffer = self.buffer.lock().unwrap_or_else(PoisonError::into_inner);
        while !bytes.is_empty() {
            if !self.receiver_alive.load(Ordering::Acquire) {
                return false;
            }
            // Coalesce tiny reads into the previous chunk rather than imposing
            // a UI scheduling round-trip for every individual shell write.
            if let Some(last) = buffer.chunks.back_mut()
                && last.len() < OUTPUT_CHUNK_BYTES
            {
                let count = bytes.len().min(OUTPUT_CHUNK_BYTES - last.len());
                last.extend_from_slice(&bytes[..count]);
                bytes = &bytes[count..];
            } else if buffer.chunks.len() < OUTPUT_QUEUE_CHUNKS {
                let count = bytes.len().min(OUTPUT_CHUNK_BYTES);
                let mut chunk = Vec::with_capacity(OUTPUT_CHUNK_BYTES);
                chunk.extend_from_slice(&bytes[..count]);
                buffer.chunks.push_back(chunk);
                bytes = &bytes[count..];
            } else {
                // Only the reader worker waits. Timeout also makes shutdown
                // robust if the receiver closes between the alive check and wait.
                buffer = self
                    .space_available
                    .wait_timeout(buffer, Duration::from_millis(20))
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        }
        true
    }

    fn drain(&self, max_bytes: usize) -> OutputDrain {
        // The application never waits for a worker's lock or copies/parses
        // terminal output while holding this mutex.
        let mut buffer = match self.buffer.try_lock() {
            Ok(buffer) => buffer,
            Err(TryLockError::WouldBlock) => return OutputDrain::default(),
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
        };
        let mut chunks = Vec::new();
        let mut bytes = 0;
        while bytes < max_bytes {
            let Some(mut chunk) = buffer.chunks.pop_front() else {
                break;
            };
            let remaining = max_bytes - bytes;
            if chunk.len() > remaining {
                let rest = chunk.split_off(remaining);
                buffer.chunks.push_front(rest);
            }
            bytes += chunk.len();
            chunks.push(chunk);
        }
        let drain = OutputDrain {
            closed: buffer.eof && buffer.chunks.is_empty(),
            error: buffer.error.take(),
            chunks,
        };
        drop(buffer);
        if bytes != 0 {
            self.space_available.notify_one();
        }
        drain
    }

    fn fail(&self, message: String) {
        let mut buffer = self.buffer.lock().unwrap_or_else(PoisonError::into_inner);
        if buffer.error.is_none() {
            buffer.error = Some(message);
        }
    }

    fn finish(&self) {
        self.buffer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .eof = true;
    }

    fn close_receiver(&self) {
        self.receiver_alive.store(false, Ordering::Release);
        self.space_available.notify_all();
    }
}

pub(super) struct PtyUpdate {
    pub output: Vec<Vec<u8>>,
    pub changed: bool,
}

pub(super) struct PtySession {
    master: Box<dyn MasterPty + Send>,
    input: SyncSender<Vec<u8>>,
    output: OutputReceiver,
    completion: Receiver<PaneStatus>,
    shutdown: SyncSender<Option<Pid>>,
    status: PaneStatus,
    process_id: Option<u32>,
    session_id: Option<Pid>,
    pending_completion: Option<PaneStatus>,
    output_closed: bool,
}

impl PtySession {
    pub fn spawn(config: &ShellConfig, size: GridSize) -> Result<Self> {
        ensure!(
            config.program.is_absolute(),
            "shell program must be an absolute path"
        );
        let pair = native_pty_system()
            .openpty(pty_size(size, 0, 0))
            .context("open pseudo-terminal")?;
        let reader = pair.master.try_clone_reader().context("clone PTY reader")?;
        let writer = pair.master.take_writer().context("take PTY writer")?;
        let mut command = CommandBuilder::new(&config.program);
        command.args(&config.args);
        command.cwd(&config.cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "rex-rs");
        command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        let child = pair
            .slave
            .spawn_command(command)
            .with_context(|| format!("spawn shell {}", config.program.display()))?;
        let process_id = child.process_id();
        // portable-pty establishes a new session before exec. Verify the group
        // belongs to this child before allowing any group signal.
        let group = owned_group(process_id);
        let (input, input_receiver) = mpsc::sync_channel(INPUT_QUEUE_MESSAGES);
        let output = OutputReceiver(Arc::new(OutputQueue::new()));
        let output_sender = Arc::clone(&output.0);
        let (shutdown, shutdown_receiver) = mpsc::sync_channel(1);
        let (completion_sender, completion) = mpsc::sync_channel(1);
        let mut emergency_killer = child.clone_killer();
        // Start the reaper first: if a later worker fails to start, dropping the
        // shutdown sender tells this worker to terminate and reap the child.
        if let Err(error) = thread::Builder::new()
            .name("rex-pty-reaper".into())
            .spawn(move || {
                reap(child, group, &shutdown_receiver, &completion_sender);
            })
        {
            // Signalling cannot block on child exit. Thread-resource exhaustion
            // leaves no worker available, so at least terminate the owned child.
            let _ = emergency_killer.kill();
            return Err(error).context("start PTY reaper");
        }
        let writer_output = Arc::clone(&output_sender);
        thread::Builder::new()
            .name("rex-pty-reader".into())
            .spawn(move || read_output(reader, &output_sender))
            .context("start PTY reader")?;
        thread::Builder::new()
            .name("rex-pty-writer".into())
            .spawn(move || write_input(writer, &input_receiver, &writer_output))
            .context("start PTY writer")?;
        drop(pair.slave);
        Ok(Self {
            master: pair.master,
            input,
            output,
            completion,
            shutdown,
            status: PaneStatus::Running,
            process_id,
            session_id: group,
            pending_completion: None,
            output_closed: false,
        })
    }

    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        ensure!(
            matches!(self.status, PaneStatus::Running),
            "terminal process is no longer running"
        );
        ensure!(
            bytes.len() <= MAX_INPUT_BYTES,
            "terminal input exceeds the 64 KiB message limit"
        );
        if bytes.is_empty() {
            return Ok(());
        }
        match self.input.try_send(bytes.to_vec()) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => bail!("terminal input queue is full; try again"),
            Err(TrySendError::Disconnected(_)) => bail!("terminal input writer has stopped"),
        }
    }

    pub fn resize(&self, size: GridSize, cell_width: u32, cell_height: u32) -> Result<()> {
        self.master
            .resize(pty_size(size, cell_width, cell_height))
            .context("resize pseudo-terminal")
    }

    pub fn poll(&mut self, max_bytes: usize) -> PtyUpdate {
        let drain = self.output.0.drain(max_bytes);
        let output = drain.chunks;
        let mut changed = !output.is_empty();
        self.output_closed |= drain.closed;
        if let Some(message) = drain.error {
            self.status = PaneStatus::Failed { message };
            let _ = self.shutdown.try_send(self.foreground_group());
            changed = true;
        }
        if let Ok(status) = self.completion.try_recv() {
            self.pending_completion = Some(status);
        }
        // Publish normal exit only after the reader's EOF marker. A fast child
        // can exit before its last bytes reach the bounded output queue.
        if self.output_closed
            && let Some(status) = self.pending_completion.take()
        {
            if !matches!(self.status, PaneStatus::Failed { .. }) {
                self.status = status;
            }
            changed = true;
        }
        PtyUpdate { output, changed }
    }

    pub fn status(&self) -> &PaneStatus {
        &self.status
    }
    pub fn process_id(&self) -> Option<u32> {
        self.process_id
    }

    pub fn begin_shutdown(&self) {
        self.output.0.close_receiver();
        let _ = self.shutdown.try_send(self.foreground_group());
    }

    pub fn is_reaped(&mut self) -> bool {
        if self.pending_completion.is_some() || matches!(self.status, PaneStatus::Exited { .. }) {
            return true;
        }
        match self.completion.try_recv() {
            Ok(status) => {
                self.pending_completion = Some(status);
                true
            }
            Err(mpsc::TryRecvError::Disconnected) => true,
            Err(mpsc::TryRecvError::Empty) => false,
        }
    }

    /// Only the standalone server's final shutdown may wait for reaping.
    /// Normal pane close and Drop remain nonblocking.
    pub fn wait_for_shutdown(&mut self, timeout: Duration) -> Result<()> {
        if let Some(PaneStatus::Failed { message }) = &self.pending_completion {
            bail!("{message}");
        }
        if self.pending_completion.is_some() || matches!(self.status, PaneStatus::Exited { .. }) {
            return Ok(());
        }
        match self.completion.recv_timeout(timeout) {
            Ok(PaneStatus::Failed { message }) => bail!(message),
            Ok(status) => {
                self.status = status;
                Ok(())
            }
            Err(RecvTimeoutError::Disconnected) => Ok(()),
            Err(RecvTimeoutError::Timeout) => bail!("timed out reaping terminal process"),
        }
    }

    fn foreground_group(&self) -> Option<Pid> {
        let session = self.session_id?;
        let foreground = Pid::from_raw(self.master.process_group_leader()?);
        (foreground.as_raw() > 0
            && foreground != getpgrp()
            && getsid(Some(foreground)).ok() == Some(session))
        .then_some(foreground)
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Never join a reader, writer, or child wait from the application thread.
        let _ = self.shutdown.try_send(self.foreground_group());
    }
}

fn pty_size(size: GridSize, cell_width: u32, cell_height: u32) -> PtySize {
    PtySize {
        rows: size.rows(),
        cols: size.columns(),
        pixel_width: u16::try_from(u32::from(size.columns()).saturating_mul(cell_width))
            .unwrap_or(u16::MAX),
        pixel_height: u16::try_from(u32::from(size.rows()).saturating_mul(cell_height))
            .unwrap_or(u16::MAX),
    }
}

fn owned_group(process_id: Option<u32>) -> Option<Pid> {
    let process = Pid::from_raw(i32::try_from(process_id?).ok()?);
    (process != getpgrp()
        && getpgid(Some(process)).ok() == Some(process)
        && getsid(Some(process)).ok() == Some(process))
    .then_some(process)
}

fn terminate(child: &mut (dyn Child + Send + Sync), group: Option<Pid>, foreground: Option<Pid>) {
    if let Some(foreground) = foreground.filter(|foreground| Some(*foreground) != group) {
        let _ = killpg(foreground, Signal::SIGKILL);
    }
    if let Some(group) = group {
        let _ = killpg(group, Signal::SIGKILL);
    } else {
        let _ = child.kill();
    }
}

fn reap(
    mut child: Box<dyn Child + Send + Sync>,
    group: Option<Pid>,
    shutdown: &Receiver<Option<Pid>>,
    completion: &SyncSender<PaneStatus>,
) {
    loop {
        let shutdown_group = match shutdown.recv_timeout(Duration::from_millis(20)) {
            Ok(foreground) => Some(foreground),
            Err(RecvTimeoutError::Disconnected) => Some(None),
            Err(RecvTimeoutError::Timeout) => None,
        };
        if let Some(foreground) = shutdown_group {
            terminate(child.as_mut(), group, foreground);
            let status = child.wait().map_or_else(
                |error| PaneStatus::Failed {
                    message: format!("wait for terminal process: {error}"),
                },
                |status| PaneStatus::Exited {
                    code: status.exit_code(),
                },
            );
            let _ = completion.send(status);
            return;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // A shell that exits must not leave its background jobs holding
                // the PTY open. Signal only its previously verified own group.
                if let Some(group) = group {
                    let _ = killpg(group, Signal::SIGKILL);
                }
                let _ = completion.send(PaneStatus::Exited {
                    code: status.exit_code(),
                });
                return;
            }
            Ok(None) => {}
            Err(error) => {
                terminate(child.as_mut(), group, None);
                let _ = child.wait();
                let _ = completion.send(PaneStatus::Failed {
                    message: format!("poll terminal process: {error}"),
                });
                return;
            }
        }
    }
}

fn read_output(mut reader: Box<dyn Read + Send>, output: &OutputQueue) {
    let mut buffer = [0; OUTPUT_CHUNK_BYTES];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if !output.push(&buffer[..count]) {
                    return;
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            // Linux PTY masters report EIO when the last slave closes.
            Err(error) if error.raw_os_error() == Some(nix::libc::EIO) => break,
            Err(error) => {
                output.fail(format!("read terminal output: {error}"));
                break;
            }
        }
    }
    output.finish();
}

fn write_input(mut writer: Box<dyn Write + Send>, input: &Receiver<Vec<u8>>, output: &OutputQueue) {
    while let Ok(bytes) = input.recv() {
        if let Err(error) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
            output.fail(format!("write terminal input: {error}"));
            return;
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests inspect owned queue fixtures; poison means the test must fail."
)]
mod output_tests {
    use super::*;

    #[test]
    fn byte_budgets_preserve_output_order_and_delay_eof_until_fully_drained() {
        let queue = OutputQueue::new();
        let expected: Vec<_> = (0..OUTPUT_CHUNK_BYTES * 3 + 7)
            .map(|index| u8::try_from(index % 251).unwrap())
            .collect();
        assert!(queue.push(&expected));
        queue.finish();
        let status_only = queue.drain(0);
        assert_eq!(status_only.chunks, [] as [Vec<u8>; 0]);
        assert!(!status_only.closed);
        let mut actual = Vec::new();
        for budget in [1, 7, OUTPUT_CHUNK_BYTES + 1, 13, 2, OUTPUT_CHUNK_BYTES * 3] {
            let update = queue.drain(budget);
            let count: usize = update.chunks.iter().map(Vec::len).sum();
            assert!(count <= budget);
            actual.extend(update.chunks.into_iter().flatten());
            assert_eq!(update.closed, actual.len() == expected.len());
        }
        assert_eq!(actual, expected);
        assert!(queue.drain(0).closed);
    }

    #[test]
    fn partial_chunk_service_keeps_queue_capacity_and_reopens_space() {
        let queue = OutputQueue::new();
        assert!(queue.push(&vec![b'x'; OUTPUT_CHUNK_BYTES * OUTPUT_QUEUE_CHUNKS]));
        assert_eq!(
            queue.buffer.lock().unwrap().chunks.len(),
            OUTPUT_QUEUE_CHUNKS
        );
        assert_eq!(queue.drain(1).chunks.concat(), [b'x']);
        assert_eq!(
            queue.buffer.lock().unwrap().chunks.len(),
            OUTPUT_QUEUE_CHUNKS
        );
        assert_eq!(
            queue.drain(OUTPUT_CHUNK_BYTES - 1).chunks.concat().len(),
            OUTPUT_CHUNK_BYTES - 1
        );
        assert_eq!(
            queue.buffer.lock().unwrap().chunks.len(),
            OUTPUT_QUEUE_CHUNKS - 1
        );
        assert!(queue.push(b"tail"));
        queue.finish();
        let remaining = queue.drain(usize::MAX);
        let bytes = remaining.chunks.concat();
        assert_eq!(
            bytes.len(),
            OUTPUT_CHUNK_BYTES * (OUTPUT_QUEUE_CHUNKS - 1) + 4
        );
        assert!(bytes.ends_with(b"tail"));
        assert!(remaining.closed);
    }

    #[test]
    fn status_only_poll_reports_errors_without_consuming_output() {
        let queue = OutputQueue::new();
        assert!(queue.push(b"final output"));
        queue.fail("writer failed".into());
        let update = queue.drain(0);
        assert_eq!(update.error.as_deref(), Some("writer failed"));
        assert_eq!(update.chunks, [] as [Vec<u8>; 0]);
        assert_eq!(queue.drain(64).chunks.concat(), b"final output");
    }
}
