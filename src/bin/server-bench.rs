//! Bounded diagnostics for local IPC payloads and input-to-owned-frame latency.
//! This measures the background client cache, not GPU presentation or compositor delay.
use anyhow::{Context, Result, ensure};
use rex_rs::{
    core::{
        Command, GridSize, PaneId, SplitAxis, Workspace,
        frame::{Cell, Cursor, Rgb, TerminalFrame},
    },
    server::{
        WorkspaceClient,
        protocol::{
            Operation, PROTOCOL_VERSION, PaneSnapshot, Request, Response, Snapshot, read_message,
            write_message,
        },
        run_default,
        updates::SnapshotUpdate,
    },
    terminal::PaneStatus,
};
use std::{
    collections::BTreeMap,
    fs,
    hint::black_box,
    os::unix::{fs::DirBuilderExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
const ITERATIONS: usize = 20;
const TIMEOUT: Duration = Duration::from_secs(15);

fn main() -> Result<()> {
    if std::env::args().any(|argument| argument == "--server") {
        return run_default();
    }
    println!(
        "# Debug/release profile applies to all measurements. 20 samples; no GPU/compositor timing."
    );
    if std::env::args().any(|argument| argument == "--contention") {
        println!("contention,echo_grid,p50_ms,p95_ms,backlog20_ms");
        echo_with_noisy_neighbor(false)?;
        echo_with_noisy_neighbor(true)?;
        return Ok(());
    }
    if std::env::args().any(|argument| argument == "--legacy-codec") {
        println!("legacy_codec,grid,bytes,encode_p50_us,decode_p50_us");
        for (columns, rows) in [(80, 24), (220, 120), (350, 160)] {
            legacy_codec(GridSize::new(columns, rows)?)?;
        }
    }
    println!(
        "compact_codec,grid,bytes,build_p50_us,encode_p50_us,decode_p50_us,decode_apply_p50_us"
    );
    for (columns, rows) in [(80, 24), (220, 120), (350, 160)] {
        compact_codec(GridSize::new(columns, rows)?)?;
    }
    println!("latency,grid,p50_ms,p95_ms,backlog20_ms");
    for (columns, rows) in [(80, 24), (220, 120)] {
        echo_latency(GridSize::new(columns, rows)?)?;
    }
    Ok(())
}
fn median(mut operation: impl FnMut() -> Result<()>) -> Result<Duration> {
    let mut timings = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        operation()?;
        timings.push(start.elapsed());
    }
    timings.sort_unstable();
    Ok(timings[ITERATIONS / 2])
}
fn response(size: GridSize) -> Result<Response> {
    let mut state = Workspace::default();
    state.apply(Command::CreateSession {
        title: "IPC benchmark".into(),
    })?;
    let pane = state.pane_ids()[0];
    let foreground = Rgb::from_hex(0x00d8_dce5);
    let background = Rgb::from_hex(0x0010_1216);
    let rows = (0..size.rows())
        .map(|row| {
            Arc::new(
                (0..size.columns())
                    .map(|column| Cell {
                        text: char::from(b'a' + ((row + column) % 26) as u8).to_string(),
                        foreground,
                        background,
                        ..Cell::default()
                    })
                    .collect(),
            )
        })
        .collect();
    Ok(Response {
        update: None,
        version: PROTOCOL_VERSION,
        server_id: "codec-fixture".into(),
        revision: 1,
        has_control: true,
        snapshot: Some(Snapshot {
            state,
            panes: BTreeMap::from([(
                pane,
                PaneSnapshot {
                    frame: Arc::new(TerminalFrame {
                        size,
                        rows,
                        cursor: Some(Cursor {
                            column: 0,
                            row: 0,
                            ..Cursor::default()
                        }),
                        foreground,
                        background,
                    }),
                    status: Some(PaneStatus::Running),
                    process_id: Some(123),
                },
            )]),
            restored_metadata: false,
        }),
        error: None,
    })
}
fn legacy_codec(size: GridSize) -> Result<()> {
    let mut value = response(size)?;
    for kind in ["full", "one_row", "cursor_only"] {
        if kind == "one_row" {
            let snapshot = value.snapshot.as_mut().context("codec snapshot")?;
            let pane = snapshot.panes.values_mut().next().context("codec pane")?;
            let frame = Arc::make_mut(&mut pane.frame);
            Arc::make_mut(&mut frame.rows[0])[0].text = "X".into();
        } else if kind == "cursor_only" {
            let snapshot = value.snapshot.as_mut().context("codec snapshot")?;
            let pane = snapshot.panes.values_mut().next().context("codec pane")?;
            Arc::make_mut(&mut pane.frame).cursor = Some(Cursor {
                column: 1,
                row: 0,
                ..Cursor::default()
            });
        }
        let mut bytes = Vec::new();
        write_message(&mut bytes, &value)?;
        let encoded = median(|| {
            let mut buffer = Vec::new();
            write_message(&mut buffer, &value)?;
            black_box(buffer);
            Ok(())
        })?;
        let decoded = median(|| {
            black_box(read_message::<Response>(&mut bytes.as_slice())?);
            Ok(())
        })?;
        println!(
            "{kind},{}x{},{},{:.2},{:.2}",
            size.columns(),
            size.rows(),
            bytes.len(),
            encoded.as_secs_f64() * 1e6,
            decoded.as_secs_f64() * 1e6
        );
    }
    Ok(())
}
fn changed_snapshot(base: &Snapshot, kind: &str) -> Result<Snapshot> {
    let mut current = base.clone();
    let pane = current.panes.values_mut().next().context("codec pane")?;
    let frame = Arc::make_mut(&mut pane.frame);
    match kind {
        "one_row" => Arc::make_mut(&mut frame.rows[0])[0].text = "X".into(),
        "cursor_only" => {
            frame.cursor = Some(Cursor {
                column: 1,
                row: 0,
                ..Cursor::default()
            });
        }
        _ => {}
    }
    Ok(current)
}

fn compact_codec(size: GridSize) -> Result<()> {
    let fixture = response(size)?;
    let base = fixture.snapshot.as_ref().context("codec snapshot")?;
    for kind in ["full", "one_row", "cursor_only"] {
        let current = changed_snapshot(base, kind)?;
        let previous = (kind != "full").then_some((base, 1));
        let built = median(|| {
            black_box(SnapshotUpdate::between(&current, previous));
            Ok(())
        })?;
        let value = Response {
            version: PROTOCOL_VERSION,
            server_id: "codec-fixture".into(),
            revision: 2,
            has_control: true,
            snapshot: None,
            error: None,
            update: Some(SnapshotUpdate::between(&current, previous)),
        };
        let mut bytes = Vec::new();
        write_message(&mut bytes, &value)?;
        let encoded = median(|| {
            let mut buffer = Vec::new();
            write_message(&mut buffer, &value)?;
            black_box(buffer);
            Ok(())
        })?;
        let decoded = median(|| {
            black_box(read_message::<Response>(&mut bytes.as_slice())?);
            Ok(())
        })?;
        let applied = median(|| {
            let response: Response = read_message(&mut bytes.as_slice())?;
            black_box(response.update.context("compact update")?.apply(previous)?);
            Ok(())
        })?;
        println!(
            "{kind},{}x{},{},{:.2},{:.2},{:.2},{:.2}",
            size.columns(),
            size.rows(),
            bytes.len(),
            built.as_secs_f64() * 1e6,
            encoded.as_secs_f64() * 1e6,
            decoded.as_secs_f64() * 1e6,
            applied.as_secs_f64() * 1e6
        );
    }
    Ok(())
}

fn wait_client(
    client: &mut WorkspaceClient,
    predicate: impl Fn(&WorkspaceClient) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        client.poll()?;
        if predicate(client) {
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "benchmark client timed out");
        thread::sleep(Duration::from_millis(1));
    }
}
fn row_starts(client: &WorkspaceClient, pane: PaneId, text: &str) -> bool {
    client
        .frame(pane)
        .and_then(|frame| frame.rows.first())
        .is_some_and(|row| {
            text.chars().enumerate().all(|(index, character)| {
                row.get(index)
                    .is_some_and(|cell| cell.text == character.to_string())
            })
        })
}
fn echo_latency(size: GridSize) -> Result<()> {
    let mut fixture = Fixture::new()?;
    let mut client = WorkspaceClient::connect(fixture.path.clone());
    wait_client(&mut client, |client| {
        client.connected() && client.has_control()
    })?;
    client.apply(Command::CreateSession {
        title: "Echo benchmark".into(),
    })?;
    wait_client(&mut client, |client| !client.state().pane_ids().is_empty())?;
    let pane = client.state().pane_ids()[0];
    client.resize(pane, size, 8, 16)?;
    wait_client(&mut client, |client| {
        client.frame(pane).is_some_and(|frame| frame.size == size)
    })?;
    client.write_input(
        pane,
        b"stty raw -echo; printf '\\033[2J\\033[HREADY'; exec cat\n",
    )?;
    wait_client(&mut client, |client| row_starts(client, pane, "READY"))?;
    let mut expected = String::from("READY");
    let mut timings = Vec::new();
    for byte in b'a'..=b't' {
        expected.push(char::from(byte));
        let start = Instant::now();
        client.write_input(pane, &[byte])?;
        wait_client(&mut client, |client| row_starts(client, pane, &expected))?;
        timings.push(start.elapsed());
    }
    let start = Instant::now();
    for byte in b'A'..=b'T' {
        expected.push(char::from(byte));
        client.write_input(pane, &[byte])?;
    }
    wait_client(&mut client, |client| row_starts(client, pane, &expected))?;
    let backlog = start.elapsed();
    timings.sort_unstable();
    println!(
        "input_to_cache,{}x{},{:.2},{:.2},{:.2}",
        size.columns(),
        size.rows(),
        timings[ITERATIONS / 2].as_secs_f64() * 1e3,
        timings[ITERATIONS * 95 / 100].as_secs_f64() * 1e3,
        backlog.as_secs_f64() * 1e3
    );
    drop(client);
    fixture.stop()?;
    Ok(())
}

/// Stress an independently running pane while checking ordered echo in another.
/// Hidden and visible neighbors exercise different publication paths.
fn echo_with_noisy_neighbor(visible: bool) -> Result<()> {
    let mut fixture = Fixture::new()?;
    let mut client = WorkspaceClient::connect(fixture.path.clone());
    wait_client(&mut client, |client| {
        client.connected() && client.has_control()
    })?;
    client.apply(Command::CreateSession {
        title: "Noisy neighbor".into(),
    })?;
    wait_client(&mut client, |client| client.state().pane_ids().len() == 1)?;
    let noisy = client.state().pane_ids()[0];
    let noisy_size = GridSize::new(220, 120)?;
    client.resize(noisy, noisy_size, 8, 16)?;
    wait_client(&mut client, |client| {
        client
            .frame(noisy)
            .is_some_and(|frame| frame.size == noisy_size)
    })?;
    client.write_input(noisy, b"stty -echo; printf '\\033[2J\\033[H'; while :; do printf 'NOISY-abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789\\r\\n'; done\n")?;
    wait_client(&mut client, |client| row_starts(client, noisy, "NOISY"))?;
    client.apply(if visible {
        Command::SplitPane {
            pane: noisy,
            axis: SplitAxis::Horizontal,
        }
    } else {
        Command::CreateSession {
            title: "Interactive echo".into(),
        }
    })?;
    wait_client(&mut client, |client| client.state().pane_ids().len() == 2)?;
    let echo = client
        .state()
        .active_session()
        .context("active echo session")?
        .active_pane()
        .context("fixture session must have an attached pane")?;
    ensure!(echo != noisy, "echo pane did not receive focus");
    client.write_input(
        echo,
        b"stty raw -echo; printf '\\033[2J\\033[HREADY'; exec cat\n",
    )?;
    wait_client(&mut client, |client| row_starts(client, echo, "READY"))?;
    let mut expected = String::from("READY");
    let mut timings = Vec::with_capacity(ITERATIONS);
    for byte in b'a'..=b't' {
        expected.push(char::from(byte));
        let started = Instant::now();
        client.write_input(echo, &[byte])?;
        wait_client(&mut client, |client| row_starts(client, echo, &expected))?;
        timings.push(started.elapsed());
    }
    let started = Instant::now();
    for byte in b'A'..=b'T' {
        expected.push(char::from(byte));
        client.write_input(echo, &[byte])?;
    }
    wait_client(&mut client, |client| row_starts(client, echo, &expected))?;
    let backlog = started.elapsed();
    timings.sort_unstable();
    println!(
        "{},80x24,{:.2},{:.2},{:.2}",
        if visible {
            "visible_noisy_neighbor"
        } else {
            "hidden_noisy_neighbor"
        },
        timings[ITERATIONS / 2].as_secs_f64() * 1e3,
        timings[ITERATIONS * 95 / 100].as_secs_f64() * 1e3,
        backlog.as_secs_f64() * 1e3
    );
    drop(client);
    fixture.stop()
}
struct Fixture {
    directory: PathBuf,
    path: PathBuf,
    child: Child,
}
impl Fixture {
    fn new() -> Result<Self> {
        let directory = std::env::temp_dir().join(format!(
            "rex-server-bench-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let path = directory.join("server.sock");
        let child = ProcessCommand::new(std::env::current_exe()?)
            .args(["--server", "--ephemeral", "--socket"])
            .arg(&path)
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let mut fixture = Self {
            directory,
            path,
            child,
        };
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if UnixStream::connect(&fixture.path).is_ok() {
                return Ok(fixture);
            }
            ensure!(
                fixture.child.try_wait()?.is_none(),
                "benchmark server exited"
            );
            ensure!(
                Instant::now() < deadline,
                "benchmark server startup timeout"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn stop(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        shutdown(&self.path)?;
        let deadline = Instant::now() + TIMEOUT;
        while self.child.try_wait()?.is_none() {
            ensure!(
                Instant::now() < deadline,
                "benchmark server shutdown timeout"
            );
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}
fn shutdown(path: &Path) -> Result<()> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut request = Request {
        version: PROTOCOL_VERSION,
        server_id: None,
        known_revision: None,
        operation: Operation::Snapshot,
    };
    write_message(&mut stream, &request)?;
    let response: Response = read_message(&mut stream)?;
    request.server_id = Some(response.server_id);
    request.known_revision = Some(response.revision);
    request.operation = Operation::Shutdown;
    write_message(&mut stream, &request)?;
    let response: Response = read_message(&mut stream)?;
    ensure!(
        response.error.is_none(),
        "benchmark shutdown rejected: {:?}",
        response.error
    );
    Ok(())
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.stop().is_err() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}
