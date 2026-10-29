//! Real-process checks for server-owned PTYs, detached clients and epoch guards.
use anyhow::{Context, Result, ensure};
use rex_rs::{
    core::{
        Command, Direction, GridSize, Layout, PaneId, SessionId, SplitAxis, Workspace,
        WorkspaceWindow,
    },
    server::{
        WorkspaceClient,
        protocol::{
            MAX_INPUT_BYTES, MAX_MESSAGE_BYTES, Operation, PROTOCOL_VERSION, Request, Response,
            Snapshot, read_message, write_message,
        },
        run_default,
    },
};
use std::{
    fs,
    io::Write,
    os::unix::{
        fs::{DirBuilderExt, symlink},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const TIMEOUT: Duration = Duration::from_secs(8);

fn main() -> Result<()> {
    if std::env::args().any(|argument| argument == "--server") {
        return run_default();
    }
    let mut fixture = ServerFixture::new()?;
    check_lifetime(&fixture.path)?;
    check_protocol_boundaries(&fixture.path)?;
    check_controller_policy(&fixture.path)?;
    check_shared_layout(&fixture.path)?;
    check_async_client(&fixture.path)?;
    check_server_restart(&mut fixture)?;
    fixture.shutdown()?;
    check_metadata_recovery()?;
    check_failed_metadata_retry()?;
    println!(
        "PASS: server lifetime, reconnect, shared state, epoch guards, bounded IPC, async cache and explicit close"
    );
    Ok(())
}

struct Connection {
    stream: UnixStream,
    server_id: String,
    revision: u64,
    snapshot: Snapshot,
    has_control: bool,
}

impl Connection {
    fn open(path: &Path) -> Result<Self> {
        let mut stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        write_message(
            &mut stream,
            &Request {
                version: PROTOCOL_VERSION,
                server_id: None,
                known_revision: None,
                operation: Operation::Snapshot,
            },
        )?;
        let response: Response = read_message(&mut stream)?;
        ensure!(response.version == PROTOCOL_VERSION);
        ensure!(
            response.error.is_none(),
            "attach failed: {:?}",
            response.error
        );
        let snapshot = response
            .snapshot
            .context("attach must return a full snapshot")?;
        snapshot.validate()?;
        let mut connection = Self {
            stream,
            server_id: response.server_id,
            revision: response.revision,
            snapshot,
            has_control: response.has_control,
        };
        connection.apply(Operation::AcquireControl { force: false })?;
        Ok(connection)
    }

    fn request(&mut self, operation: Operation) -> Result<Response> {
        write_message(
            &mut self.stream,
            &Request {
                version: PROTOCOL_VERSION,
                server_id: Some(self.server_id.clone()),
                known_revision: Some(self.revision),
                operation,
            },
        )?;
        let response: Response = read_message(&mut self.stream)?;
        ensure!(response.version == PROTOCOL_VERSION);
        ensure!(
            response.server_id == self.server_id,
            "server unexpectedly restarted"
        );
        self.has_control = response.has_control;
        self.revision = response.revision;
        if let Some(snapshot) = &response.snapshot {
            snapshot.validate()?;
            self.snapshot = snapshot.clone();
        }
        Ok(response)
    }

    fn apply(&mut self, operation: Operation) -> Result<()> {
        let response = self.request(operation)?;
        ensure!(
            response.error.is_none(),
            "request rejected: {:?}",
            response.error
        );
        Ok(())
    }

    fn active_pane(&self) -> Result<PaneId> {
        self.snapshot
            .state
            .active_session()
            .and_then(rex_rs::core::Session::active_pane)
            .context("fixture session must exist")
    }

    fn active_window(&self) -> Result<&WorkspaceWindow> {
        self.snapshot
            .state
            .active_session()
            .and_then(rex_rs::core::Session::active_window)
            .context("fixture session must exist")
    }

    fn pane_size(&self, pane: PaneId) -> Result<GridSize> {
        self.snapshot
            .panes
            .get(&pane)
            .map(|entry| entry.frame.size)
            .context("fixture pane must exist")
    }

    fn wait_text(&mut self, pane: PaneId, needle: &str) -> Result<()> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.apply(Operation::Snapshot)?;
            let frame = &self
                .snapshot
                .panes
                .get(&pane)
                .context("fixture pane missing")?
                .frame;
            if frame.text_lines().join("\n").contains(needle) {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "terminal never produced {needle:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn check_lifetime(path: &Path) -> Result<()> {
    let mut first = Connection::open(path)?;
    first.apply(Operation::Initialize {
        title: "Persistent".into(),
    })?;
    let session = first
        .snapshot
        .state
        .active_session()
        .context("session")?
        .id();
    let original = first.active_pane()?;
    let pid = first
        .snapshot
        .panes
        .get(&original)
        .and_then(|pane| pane.process_id)
        .context("PTY pid")?;
    first.apply(Operation::Apply(Command::RenameSession {
        session,
        title: "Kept label 界".into(),
    }))?;
    first.apply(Operation::Apply(Command::CreateWindow {
        session,
        title: "Kept tab".into(),
    }))?;
    let state = first.snapshot.state.clone();
    let processes: Vec<_> = first
        .snapshot
        .panes
        .iter()
        .map(|(id, pane)| (*id, pane.process_id))
        .collect();
    first.apply(Operation::Input {
        pane: original,
        bytes: b"stty -echo; sleep 0.3; printf '%s%s\\n' OFFLINE _DONE\n".to_vec(),
    })?;
    let epoch = first.server_id.clone();
    drop(first);
    thread::sleep(Duration::from_millis(450));
    let mut second = Connection::open(path)?;
    ensure!(
        second.server_id == epoch,
        "client disconnect restarted server"
    );
    ensure!(
        second.snapshot.state == state,
        "layout or labels changed during detach"
    );
    for (id, process) in processes {
        ensure!(
            second
                .snapshot
                .panes
                .get(&id)
                .and_then(|pane| pane.process_id)
                == process,
            "reconnect replaced shell for {id}"
        );
    }
    second.wait_text(original, "OFFLINE_DONE")?;
    check_shared_epoch_guard(&mut second, path, session)?;
    second.apply(Operation::Apply(Command::ClosePane(original)))?;
    let deadline = Instant::now() + TIMEOUT;
    while PathBuf::from(format!("/proc/{pid}")).exists() {
        ensure!(
            Instant::now() < deadline,
            "explicit close did not reap shell {pid}"
        );
        thread::sleep(Duration::from_millis(20));
    }
    println!(
        "PASS: detach retains shell PID, output, tabs and labels; two clients share state; stale epochs rejected; explicit close reaps shell"
    );
    Ok(())
}

fn check_shared_epoch_guard(
    second: &mut Connection,
    path: &Path,
    session: SessionId,
) -> Result<()> {
    let mut observer = Connection::open(path)?;
    second.apply(Operation::Apply(Command::RenameSession {
        session,
        title: "Shared state".into(),
    }))?;
    observer.apply(Operation::Snapshot)?;
    ensure!(
        observer.snapshot.state == second.snapshot.state,
        "clients disagree on authoritative state"
    );
    ensure!(
        observer.revision >= second.revision,
        "observer regressed revision"
    );
    let previous = second.snapshot.state.clone();
    write_message(
        &mut observer.stream,
        &Request {
            version: PROTOCOL_VERSION,
            server_id: Some("previous-server-instance".into()),
            known_revision: Some(observer.revision),
            operation: Operation::Apply(Command::CloseSession(session)),
        },
    )?;
    let rejected: Response = read_message(&mut observer.stream)?;
    ensure!(
        rejected.error.is_some(),
        "stale epoch mutation was accepted"
    );
    second.apply(Operation::Snapshot)?;
    ensure!(
        second.snapshot.state == previous,
        "stale epoch changed workspace"
    );
    Ok(())
}

fn check_protocol_boundaries(path: &Path) -> Result<()> {
    for payload in [
        ((MAX_MESSAGE_BYTES + 1) as u32).to_be_bytes().to_vec(),
        [4u32.to_be_bytes().as_slice(), b"nope"].concat(),
    ] {
        let mut stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.write_all(&payload)?;
        ensure!(
            read_message::<Response>(&mut stream).is_err(),
            "malformed IPC was accepted"
        );
    }
    let mut connection = Connection::open(path)?;
    connection.apply(Operation::AcquireControl { force: true })?;
    let pane = connection.active_pane()?;
    let state = connection.snapshot.state.clone();
    let rejected = connection.request(Operation::Input {
        pane,
        bytes: vec![b'x'; MAX_INPUT_BYTES + 1],
    })?;
    ensure!(rejected.error.is_some(), "oversized input was accepted");
    let rejected = connection.request(Operation::Resize {
        pane,
        size: GridSize::new(u16::MAX, u16::MAX)?,
        cell_width: 8,
        cell_height: 16,
    })?;
    ensure!(rejected.error.is_some(), "oversized viewport was accepted");
    ensure!(
        connection.snapshot.state == state,
        "invalid requests changed workspace"
    );
    println!("PASS: oversized/malformed IPC, input and grids rejected without stopping server");
    Ok(())
}

fn check_controller_policy(path: &Path) -> Result<()> {
    let mut controller = Connection::open(path)?;
    controller.apply(Operation::AcquireControl { force: true })?;
    let pane = controller.active_pane()?;
    let mut observer = Connection::open(path)?;
    ensure!(
        !observer.has_control,
        "nonforcing observer stole controller lease"
    );
    let original = observer.snapshot.state.clone();
    let size = observer.pane_size(pane)?;
    for operation in [
        Operation::Resize {
            pane,
            size: GridSize::new(97, 29)?,
            cell_width: 8,
            cell_height: 16,
        },
        Operation::Input {
            pane,
            bytes: b"LEASE_INPUT_MUST_NOT_RUN\n".to_vec(),
        },
        Operation::Scroll { pane, lines: 10 },
    ] {
        ensure!(
            observer.request(operation)?.error.is_some(),
            "observer terminal mutation was accepted"
        );
    }
    controller.apply(Operation::Snapshot)?;
    ensure!(
        controller.snapshot.state == original,
        "denied observer request changed layout"
    );
    ensure!(
        controller.pane_size(pane)? == size,
        "observer resized terminal"
    );
    observer.apply(Operation::AcquireControl { force: true })?;
    ensure!(
        observer.has_control,
        "explicit takeover did not acquire lease"
    );
    ensure!(
        controller
            .request(Operation::Scroll { pane, lines: 1 })?
            .error
            .is_some(),
        "previous controller retained authority"
    );
    ensure!(
        !controller.has_control,
        "revoked controller reported stale authority"
    );
    observer.apply(Operation::Resize {
        pane,
        size: GridSize::new(93, 27)?,
        cell_width: 8,
        cell_height: 16,
    })?;
    ensure!(
        observer.pane_size(pane)? == GridSize::new(93, 27)?,
        "controller resize did not apply"
    );
    drop(observer);
    let deadline = Instant::now() + TIMEOUT;
    while !controller.has_control {
        controller.apply(Operation::AcquireControl { force: false })?;
        ensure!(
            Instant::now() < deadline,
            "disconnect did not release controller lease"
        );
        thread::sleep(Duration::from_millis(10));
    }
    controller.apply(Operation::ReleaseControl)?;
    ensure!(
        !controller.has_control,
        "release retained controller authority"
    );
    let mut reconnected = Connection::open(path)?;
    ensure!(
        reconnected.has_control,
        "new connection failed to acquire released lease"
    );
    reconnected.apply(Operation::Scroll { pane, lines: 0 })?;
    println!(
        "PASS: observers cannot alter terminal input/size/scroll; explicit takeover, disconnect release and reacquire work"
    );
    Ok(())
}

fn check_shared_layout(path: &Path) -> Result<()> {
    let mut controller = Connection::open(path)?;
    controller.apply(Operation::AcquireControl { force: true })?;
    controller.apply(Operation::Apply(Command::CreateSession {
        title: "Shared layout".into(),
    }))?;
    let session = controller
        .snapshot
        .state
        .active_session()
        .context("layout session")?
        .id();
    let window = controller.active_window()?.id();
    let left = controller.active_pane()?;
    controller.apply(Operation::Apply(Command::SplitPane {
        pane: left,
        axis: SplitAxis::Horizontal,
    }))?;
    let right = controller.active_pane()?;
    let split = controller
        .snapshot
        .state
        .active_session()
        .context("layout session")?
        .layout()
        .context("fixture session must have a layout")?
        .split_ids()[0];
    controller.apply(Operation::Apply(Command::SplitPane {
        pane: right,
        axis: SplitAxis::Vertical,
    }))?;
    let processes = shell_pids(&controller.snapshot);
    controller.apply(Operation::Apply(Command::ResizeSplit { split, ratio: 650 }))?;
    controller.apply(Operation::Apply(Command::ToggleZoom(left)))?;
    let mut observer = Connection::open(path)?;
    ensure!(
        observer.snapshot.state == controller.snapshot.state,
        "observer missed shared zoom or ratio"
    );
    let active = observer.active_window()?;
    ensure!(
        active.zoomed_pane() == Some(left),
        "zoom did not become authoritative"
    );
    ensure!(
        matches!(active.layout(), Layout::Split { id, ratio: 650, .. } if *id == split),
        "split ratio/ID did not survive snapshot"
    );
    controller.apply(Operation::Apply(Command::CreateWindow {
        session,
        title: "Independent tab".into(),
    }))?;
    controller.apply(Operation::Apply(Command::SelectWindow(window)))?;
    ensure!(
        controller.active_window()?.zoomed_pane() == Some(left),
        "tab switch lost shared zoom"
    );
    observer.apply(Operation::Apply(Command::FocusDirection {
        direction: Direction::Right,
        wrap: false,
    }))?;
    controller.apply(Operation::Snapshot)?;
    ensure!(
        controller.active_pane()? == right,
        "directional focus did not choose geometric right neighbor"
    );
    ensure!(
        controller.snapshot.state == observer.snapshot.state,
        "clients disagree after directional focus"
    );
    ensure!(
        controller.active_window()?.zoomed_pane().is_none(),
        "focus change retained stale zoom"
    );
    for (pane, pid) in processes {
        ensure!(
            controller
                .snapshot
                .panes
                .get(&pane)
                .and_then(|entry| entry.process_id)
                == Some(pid),
            "layout command restarted pane {pane}"
        );
    }
    controller.apply(Operation::Apply(Command::CloseSession(session)))?;
    println!(
        "PASS: zoom, ratios, directional focus and stable IDs are shared without restarting shells"
    );
    Ok(())
}

fn shell_pids(snapshot: &Snapshot) -> Vec<(PaneId, u32)> {
    snapshot
        .panes
        .iter()
        .filter_map(|(pane, entry)| entry.process_id.map(|pid| (*pane, pid)))
        .collect()
}

fn check_metadata_recovery() -> Result<()> {
    let mut fixture = ServerFixture::with_metadata(true)?;
    let mut first = Connection::open(&fixture.path)?;
    ensure!(
        !first.snapshot.restored_metadata,
        "fresh server claimed metadata recovery"
    );
    first.apply(Operation::Initialize {
        title: "Saved label 界".into(),
    })?;
    let session = first
        .snapshot
        .state
        .active_session()
        .context("durable session")?
        .id();
    let original = first.active_pane()?;
    first.apply(Operation::Apply(Command::SplitPane {
        pane: original,
        axis: SplitAxis::Horizontal,
    }))?;
    let split = first
        .snapshot
        .state
        .active_session()
        .context("durable session")?
        .layout()
        .context("fixture session must have a layout")?
        .split_ids()[0];
    first.apply(Operation::Apply(Command::ResizeSplit { split, ratio: 700 }))?;
    first.apply(Operation::Apply(Command::CreateWindow {
        session,
        title: "Saved tab".into(),
    }))?;
    let active = first.active_pane()?;
    first.apply(Operation::Input {
        pane: active,
        bytes: b"stty -echo; printf '%s%s\\n' DURABLE_OUTPUT_ MUST_NOT_RETURN\n".to_vec(),
    })?;
    first.wait_text(active, "DURABLE_OUTPUT_MUST_NOT_RETURN")?;
    first.apply(Operation::Apply(Command::ToggleZoom(active)))?;
    let saved = first.snapshot.state.clone();
    let pids = shell_pids(&first.snapshot);
    ensure!(pids.len() == 3, "fixture requires three real PTY shells");
    drop(first);
    fixture.shutdown()?;
    let contents = fs::read_to_string(fixture.state_file.as_ref().context("metadata file")?)?;
    let document: serde_json::Value = serde_json::from_str(&contents)?;
    ensure!(
        document.get("workspace").is_some(),
        "metadata has no workspace document"
    );
    ensure!(
        !contents.contains("process_id") && !contents.contains("DURABLE_OUTPUT_MUST_NOT_RETURN"),
        "metadata persisted live process/output data"
    );
    fixture.restart()?;
    let mut restored = Connection::open(&fixture.path)?;
    ensure!(
        restored.snapshot.restored_metadata,
        "restored layout was not disclosed"
    );
    ensure!(
        restored.snapshot.state == saved,
        "metadata recovery lost labels/layout/ratios/zoom/allocator state"
    );
    ensure!(
        restored.snapshot.panes.values().all(|entry| entry
            .process_id
            .is_some_and(|pid| pids.iter().all(|(_, old)| *old != pid))),
        "recovery reused old live shell identity"
    );
    ensure!(
        restored.snapshot.panes.values().all(|entry| !entry
            .frame
            .text_lines()
            .join("\n")
            .contains("DURABLE_OUTPUT_MUST_NOT_RETURN")),
        "fresh shells claimed old terminal contents"
    );
    check_recovered_allocator(&mut restored, &saved)?;
    drop(restored);
    fixture.shutdown()?;
    println!(
        "PASS: restart restores labels/tabs/layout/ratios/zoom and allocator; discloses fresh shells without old output/PIDs"
    );
    Ok(())
}

fn check_recovered_allocator(connection: &mut Connection, previous: &Workspace) -> Result<()> {
    let pane = connection.active_pane()?;
    let max_pane = previous
        .pane_ids()
        .into_iter()
        .map(PaneId::value)
        .max()
        .context("saved panes")?;
    connection.apply(Operation::Apply(Command::SplitPane {
        pane,
        axis: SplitAxis::Vertical,
    }))?;
    ensure!(
        connection.active_pane()?.value() > max_pane,
        "recovered allocator reused an old pane ID"
    );
    Ok(())
}

fn check_failed_metadata_retry() -> Result<()> {
    let mut fixture = ServerFixture::with_metadata(true)?;
    let mut connection = Connection::open(&fixture.path)?;
    connection.apply(Operation::Initialize {
        title: "Before failed save".into(),
    })?;
    let session = connection
        .snapshot
        .state
        .active_session()
        .context("retry session")?
        .id();
    let path = fixture.state_file.as_ref().context("retry metadata file")?;
    let rename = Command::RenameSession {
        session,
        title: "Recovered durable rename".into(),
    };
    check_blocked_metadata_save(&mut connection, path, rename.clone())?;
    let in_memory = connection.snapshot.state.clone();
    connection.apply(Operation::Apply(rename))?;
    ensure!(
        connection.snapshot.state == in_memory,
        "no-op retry changed in-memory layout"
    );
    let document: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
    ensure!(
        document["workspace"]["sessions"][0]["title"] == "Recovered durable rename",
        "successful no-op retry left stale metadata on disk"
    );
    drop(connection);
    fixture.shutdown()?;
    fixture.restart()?;
    let restored = Connection::open(&fixture.path)?;
    ensure!(
        restored.snapshot.state == in_memory,
        "retry acknowledged success without durable restart recovery"
    );
    drop(restored);
    fixture.shutdown()?;
    println!(
        "PASS: failed metadata save preserves protected files; no-op command retry persists dirty state and survives restart"
    );
    Ok(())
}

fn check_blocked_metadata_save(
    connection: &mut Connection,
    path: &Path,
    rename: Command,
) -> Result<()> {
    let original = fs::read(path)?;
    let backup = path.with_extension("original-fixture");
    let sentinel = path.with_extension("protected-fixture");
    fs::write(&sentinel, b"protected sentinel")?;
    fs::rename(path, &backup)?;
    // Restore the regular saved file even if an assertion fails, so fixture
    // shutdown can still stop and reap the live shells on an unsuccessful test.
    let result = (|| -> Result<()> {
        symlink(&sentinel, path)?;
        let rejected = connection.request(Operation::Apply(rename))?;
        ensure!(
            rejected
                .error
                .as_deref()
                .is_some_and(|error| error.contains("applied in memory")),
            "failed durable save was not reported explicitly"
        );
        ensure!(
            connection
                .snapshot
                .state
                .active_session()
                .context("failed-save session")?
                .title()
                == "Recovered durable rename",
            "save failure fixture did not apply rename in memory"
        );
        ensure!(
            fs::read(&sentinel)? == b"protected sentinel",
            "metadata save changed protected symlink target"
        );
        ensure!(
            fs::read(&backup)? == original,
            "metadata save changed original saved bytes"
        );
        ensure!(
            fs::symlink_metadata(path)?.file_type().is_symlink(),
            "failed save replaced unsafe destination"
        );
        Ok(())
    })();
    let restore = (|| -> Result<()> {
        if fs::symlink_metadata(path).is_ok() {
            fs::remove_file(path)?;
        }
        fs::rename(&backup, path)?;
        Ok(())
    })();
    result?;
    restore?;
    Ok(())
}

fn check_async_client(path: &Path) -> Result<()> {
    let mut client = WorkspaceClient::connect(path.to_owned());
    wait_client(&mut client, |client| {
        client.connected() && !client.state().sessions().is_empty()
    })?;
    client.take_control(true)?;
    wait_client(&mut client, WorkspaceClient::has_control)?;
    let pane = client
        .state()
        .active_session()
        .context("cached session")?
        .active_pane()
        .context("fixture session must have an attached pane")?;
    let pid = client.process_id(pane).context("cached live PTY PID")?;
    client.write_input(pane, b"stty -echo; printf '%s%s\\n' ASYNC _READY\n")?;
    wait_client(&mut client, |client| {
        client
            .frame(pane)
            .is_some_and(|frame| frame.text_lines().join("\n").contains("ASYNC_READY"))
    })?;
    wait_quiescent(&mut client)?;
    let frame = client.frame(pane).map(Arc::clone).context("cached frame")?;
    let revision = client.revision();
    for _ in 0..10 {
        thread::sleep(Duration::from_millis(10));
        client.poll()?;
        ensure!(
            client.revision() == revision,
            "idle polling changed server revision"
        );
        ensure!(
            Arc::ptr_eq(&frame, client.frame(pane).context("cached frame")?),
            "idle polling replaced identical frame"
        );
    }
    drop(client);
    let mut reattached = WorkspaceClient::connect(path.to_owned());
    wait_client(&mut reattached, |client| {
        client.connected() && client.process_id(pane) == Some(pid)
    })?;
    println!("PASS: background client cache/input and detach/reattach preserve server work");
    Ok(())
}

fn wait_quiescent(client: &mut WorkspaceClient) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    let mut stable_since = Instant::now();
    let mut revision = client.revision();
    loop {
        client.poll()?;
        if client.revision() != revision {
            revision = client.revision();
            stable_since = Instant::now();
        }
        if stable_since.elapsed() >= Duration::from_millis(200) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "terminal output never became quiescent"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_client(
    client: &mut WorkspaceClient,
    predicate: impl Fn(&WorkspaceClient) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let start = Instant::now();
        client.poll()?;
        ensure!(
            start.elapsed() < Duration::from_millis(100),
            "UI cache polling blocked"
        );
        if predicate(client) {
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "background client timed out");
        thread::sleep(Duration::from_millis(10));
    }
}

fn check_server_restart(fixture: &mut ServerFixture) -> Result<()> {
    let mut client = WorkspaceClient::connect(fixture.path.clone());
    wait_client(&mut client, |client| {
        client.connected() && !client.state().sessions().is_empty()
    })?;
    let original_epoch = client
        .server_id()
        .context("attached server epoch")?
        .to_owned();
    let jobs = prepare_shutdown_jobs(&fixture.path)?;
    fixture.shutdown()?;
    wait_processes_stopped(&jobs)?;
    // A UI may enqueue before it learns that a socket died. The old epoch must
    // prevent this mutation from applying if pane/session IDs are reused.
    client
        .apply(Command::CreateSession {
            title: "Must not replay".into(),
        })
        .context("enqueue old-epoch mutation before consuming disconnection")?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let _ = client.poll();
        if !client.connected() {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "client failed to detect server exit"
        );
        thread::sleep(Duration::from_millis(10));
    }
    ensure!(
        !client.state().sessions().is_empty(),
        "connection loss discarded cached workspace"
    );
    fixture.restart()?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        // Transport loss and discarded stale work are deliberately reported.
        let _ = client.poll();
        if client.connected()
            && client
                .server_id()
                .is_some_and(|epoch| epoch != original_epoch)
        {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "client did not attach to new server epoch"
        );
        thread::sleep(Duration::from_millis(10));
    }
    ensure!(
        client.state().sessions().is_empty(),
        "old mutation replayed into restarted server"
    );
    println!("PASS: server restart changes epoch, replaces cache, and never replays old mutations");
    Ok(())
}

fn prepare_shutdown_jobs(path: &Path) -> Result<Vec<u32>> {
    let mut connection = Connection::open(path)?;
    connection.apply(Operation::AcquireControl { force: true })?;
    let pane = connection.active_pane()?;
    connection.apply(Operation::Input {
        pane,
        bytes: b"stty -echo; /bin/sh -c 'trap \"\" HUP; sleep 30 & bg=$!; printf \"\\nSERVER-JOBS:%s:%s:DONE\\n\" \"$$\" \"$bg\"; wait'\n".to_vec(),
    })?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        connection.apply(Operation::Snapshot)?;
        let text = connection
            .snapshot
            .panes
            .get(&pane)
            .context("job fixture pane")?
            .frame
            .text_lines()
            .join("\n");
        for line in text.lines() {
            if let Some(pids) = line
                .strip_prefix("SERVER-JOBS:")
                .and_then(|value| value.strip_suffix(":DONE"))
            {
                let jobs: Vec<u32> = pids
                    .split(':')
                    .map(str::parse)
                    .collect::<std::result::Result<_, _>>()?;
                ensure!(
                    jobs.len() == 2 && jobs[0] != jobs[1],
                    "fixture must report distinct foreground/background PIDs"
                );
                let foreground = nix::unistd::Pid::from_raw(i32::try_from(jobs[0])?);
                ensure!(
                    nix::unistd::getpgid(Some(foreground))? == foreground,
                    "fixture requires a separate foreground group"
                );
                ensure!(
                    jobs.iter().all(|pid| process_running(*pid)),
                    "fixture jobs must be running before shutdown"
                );
                return Ok(jobs);
            }
        }
        ensure!(
            Instant::now() < deadline,
            "shutdown job fixture never reported PIDs"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn process_running(pid: u32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // Orphaned grandchildren may briefly remain zombies until host init reaps
    // them. They are stopped; direct PTY children must be fully reaped below.
    stat.rsplit_once(") ")
        .is_some_and(|(_, fields)| !fields.starts_with('Z'))
}

fn wait_processes_stopped(pids: &[u32]) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    while pids.iter().any(|pid| process_running(*pid)) {
        ensure!(
            Instant::now() < deadline,
            "server shutdown left jobs running: {pids:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
    println!("PASS: server shutdown stops ignored-HUP foreground job and its background child");
    Ok(())
}

struct ServerFixture {
    directory: PathBuf,
    path: PathBuf,
    child: Child,
    state_file: Option<PathBuf>,
}

fn spawn_server(path: &Path, state_file: Option<&Path>) -> Result<Child> {
    // Keep using this running image if another Cargo invocation replaces its
    // pathname while the fixture is restarting its private server.
    let mut command = ProcessCommand::new(format!("/proc/{}/exe", std::process::id()));
    command.args(["--server", "--socket"]).arg(path);
    if let Some(path) = state_file {
        command.arg("--state-file").arg(path);
    } else {
        command.arg("--ephemeral");
    }
    command
        .env("SHELL", "/bin/sh")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("start fixture server")
}

impl ServerFixture {
    fn new() -> Result<Self> {
        Self::with_metadata(false)
    }

    fn with_metadata(durable: bool) -> Result<Self> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let directory =
            std::env::temp_dir().join(format!("rex-server-smoke-{}-{nonce}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let path = directory.join("server.sock");
        let state_file = durable.then(|| directory.join("workspace.json"));
        let child = spawn_server(&path, state_file.as_deref())?;
        let mut fixture = Self {
            directory,
            path,
            child,
            state_file,
        };
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if UnixStream::connect(&fixture.path).is_ok() {
                return Ok(fixture);
            }
            ensure!(
                fixture.child.try_wait()?.is_none(),
                "server exited during startup"
            );
            ensure!(Instant::now() < deadline, "server did not bind its socket");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn restart(&mut self) -> Result<()> {
        ensure!(
            self.child.try_wait()?.is_some(),
            "restart requires stopped server"
        );
        self.child = spawn_server(&self.path, self.state_file.as_deref())?;
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if UnixStream::connect(&self.path).is_ok() {
                return Ok(());
            }
            ensure!(self.child.try_wait()?.is_none(), "restarted server exited");
            ensure!(
                Instant::now() < deadline,
                "restarted server never bound socket"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn shutdown(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        let mut connection = Connection::open(&self.path)?;
        let pids: Vec<_> = connection
            .snapshot
            .panes
            .values()
            .filter_map(|pane| pane.process_id)
            .collect();
        connection
            .apply(Operation::Shutdown)
            .context("server must acknowledge explicit shutdown")?;
        let deadline = Instant::now() + TIMEOUT;
        while self.child.try_wait()?.is_none() {
            ensure!(
                Instant::now() < deadline,
                "server did not stop after explicit shutdown"
            );
            thread::sleep(Duration::from_millis(20));
        }
        let deadline = Instant::now() + TIMEOUT;
        while pids
            .iter()
            .any(|pid| PathBuf::from(format!("/proc/{pid}")).exists())
        {
            ensure!(
                Instant::now() < deadline,
                "server exited without reaping PTY shells: {pids:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }
}

impl Drop for ServerFixture {
    fn drop(&mut self) {
        if self.shutdown().is_err() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}
