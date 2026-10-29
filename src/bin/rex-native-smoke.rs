//! Interoperability checks against an isolated instance of the installed Rex server.
use anyhow::{Context, Result, ensure};
use rex_rs::{
    core::{
        Command, GridSize, SplitAxis,
        input::{KeyInput, Modifiers, MouseAction, MouseButton, MouseInput},
    },
    rex::{
        ClientScript, WorkspaceClient,
        actions::{self, NativeAction},
        automation::{self, ActionContext, ActionError, ClientActionRequest},
        terminal::TerminalStream,
        transport::{ControlConnection, PendingCall},
        workspace::{ViewLayout as Layout, WorkspaceState},
    },
    terminal::PaneStatus,
};
use serde_json::{Value, json};
use std::{
    fs,
    future::Future,
    io::{Read, Write},
    net::{Shutdown, TcpListener},
    os::unix::{
        fs::DirBuilderExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, Stdio},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    task::{Context as TaskContext, Poll, Waker},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const TIMEOUT: Duration = Duration::from_secs(8);

#[path = "rex-native-smoke/close_races.rs"]
mod close_races;

#[path = "rex-native-smoke/detached.rs"]
mod detached;

#[path = "rex-native-smoke/management.rs"]
mod management;

#[path = "rex-native-smoke/fidelity.rs"]
mod fidelity;

#[path = "rex-native-smoke/search.rs"]
mod search;

#[path = "rex-native-smoke/pending.rs"]
mod pending;

#[path = "rex-native-smoke/network.rs"]
mod network;

#[path = "rex-native-smoke/routing.rs"]
mod routing;

const FIXTURE_CONFIG: &str = r#"-- Isolated interoperability fixture.
rex.action{
  name = "smoke_queue_pair",
  title = "Smoke Queue Pair",
  run = function(ctx, args)
    rex.session.set_label{ label = "Server action applied" }
    rex.client.queue("client.font.increase")
    rex.client.queue("client.sidebar.toggle")
    return { applied = true }
  end,
}
rex.action{
  name = "smoke_nested_copy",
  title = "Smoke Nested Copy",
  run = function(ctx, args)
    local result, err = rex.call("client.perform", {
      client_id = ctx.client_id, action = "client.copy", args = {},
      session_id = ctx.session_id, block_id = ctx.block_id, timeout_ms = 5000
    })
    if err ~= nil then error(err, 0) end
    return result
  end,
}
rex.action{
  name = "smoke_nested_split", title = "Smoke Nested Split",
  run = function(ctx, args)
    local result, err = rex.call("client.perform", {
      client_id=ctx.client_id, action="pane.split", args={direction="right"},
      session_id=ctx.session_id, block_id=ctx.block_id, timeout_ms=5000})
    if err ~= nil then error(err, 0) end
    return result
  end,
}
rex.action{
  name = "smoke_cross_server", title = "Smoke Cross Server",
  run = function(ctx, args)
    local result, err = rex.server(args.peer).call(args.method, args.payload)
    if err ~= nil then error(err, 0) end
    return result
  end,
}
rex.action{
  name = "smoke_cross_after_copy", title = "Smoke Stale Cross Server",
  run = function(ctx, args)
    local remote = rex.server(args.peer)
    local copied, err = rex.call("client.perform", {client_id=ctx.client_id,
      action="client.copy", args={}, timeout_ms=5000})
    if err ~= nil then error(err, 0) end
    local result, failed = remote.call("session.set_label", {
      session_id=args.session_id, label="Stale route mutated"})
    if failed ~= nil then error(failed, 0) end
    return result
  end,
}
"#;

fn main() -> Result<()> {
    let mut fixture = ServerFixture::start()?;
    check_native_client(&fixture.endpoint).context("native interoperability fixture")?;
    let mut actions = ControlConnection::connect(&fixture.endpoint)?;
    actions.call(
        "session.bootstrap",
        json!({"event_scope":"session","attach_session_ids":[]}),
    )?;
    check_client_actions(&mut actions, &fixture.endpoint).context("native client actions")?;
    management::check(&mut actions, &fixture.endpoint).context("native pane and tab management")?;
    detached::check(&mut actions, &fixture.endpoint)
        .context("native detached inventory and restoration")?;
    close_races::check(&mut actions, &fixture.endpoint).context("native pane close event races")?;
    search::check(&mut actions, &fixture.endpoint)
        .context("native local history search and selection")?;
    fidelity::check(&mut actions, &fixture.endpoint)
        .context("native terminal presentation fidelity")?;
    pending::check(&mut actions, &fixture.endpoint).context("pending Lua worker responsiveness")?;
    network::check(&fixture.network_endpoint)
        .context("native HTTP control and terminal transport")?;
    routing::check(&fixture.endpoint).context("named cross-server Lua routing")?;
    fixture.stop()?;
    println!(
        "PASS: native Rex bootstrap, ordered commands/events, terminal snapshot/Unicode/resume, normal shell exit, ownership and detached process lifetime"
    );
    Ok(())
}

struct ServerFixture {
    directory: PathBuf,
    endpoint: String,
    network_endpoint: String,
    child: Option<Child>,
}

impl ServerFixture {
    fn start() -> Result<Self> {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "rex-rs-native-smoke-{}-{stamp}",
            std::process::id()
        ));
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        fs::write(directory.join("init.lua"), FIXTURE_CONFIG)?;
        let endpoint = format!("unix://{}", directory.join("server.sock").display());
        let reservation = TcpListener::bind(("127.0.0.1", 0))?;
        let network_endpoint = format!("http://{}", reservation.local_addr()?);
        let log = fs::File::create(directory.join("server.log"))?;
        let mut fixture = Self {
            directory,
            endpoint,
            network_endpoint,
            child: None,
        };
        drop(reservation);
        fixture.child = Some(
            ProcessCommand::new("rex")
                .args([
                    "server",
                    "run",
                    "--listen",
                    &fixture.endpoint,
                    "--state-dir",
                ])
                .arg(fixture.directory.join("state"))
                .arg("--config")
                .arg(fixture.directory.join("init.lua"))
                .args([
                    "--log-format",
                    "json",
                    "--extra-listen",
                    &fixture.network_endpoint,
                    "--allow-anonymous",
                ])
                .env("SHELL", "/bin/sh")
                .stdin(Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log)
                .spawn()
                .context("start installed Rex (required for native interoperability checks)")?,
        );
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if ControlConnection::connect(&fixture.endpoint).is_ok() {
                return Ok(fixture);
            }
            ensure!(
                fixture
                    .child
                    .as_mut()
                    .context("Rex child")?
                    .try_wait()?
                    .is_none(),
                "isolated Rex exited before accepting connections"
            );
            ensure!(
                Instant::now() < deadline,
                "isolated Rex did not become ready"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn stop(&mut self) -> Result<()> {
        let Some(child) = &mut self.child else {
            return Ok(());
        };
        if child.try_wait()?.is_none() {
            let mut connection = ControlConnection::connect(&self.endpoint)?;
            connection.call("session.bootstrap", json!({"event_scope":"session"}))?;
            connection.call("server.stop", json!({}))?;
            let deadline = Instant::now() + TIMEOUT;
            while child.try_wait()?.is_none() {
                ensure!(Instant::now() < deadline, "isolated Rex did not stop");
                thread::sleep(Duration::from_millis(20));
            }
        }
        self.child = None;
        fs::remove_dir_all(&self.directory)?;
        Ok(())
    }
}

impl Drop for ServerFixture {
    fn drop(&mut self) {
        if self.stop().is_err() {
            if let Some(child) = &mut self.child {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

fn check_native_client(endpoint: &str) -> Result<()> {
    let mut control = ControlConnection::connect(endpoint)?;
    let bootstrap = control.call("session.bootstrap", json!({"event_scope":"all"}))?;
    let mut state = WorkspaceState::default();
    state.apply_bootstrap(&bootstrap)?;
    ensure!(state.sessions().is_empty(), "fixture must start empty");
    let initial_identity = control.client_id().to_owned();
    let created = control.call("session.create", json!({
        "label":"rex-rs-native-smoke", "initial_windows":[{
            "window_label":"Native", "layout":{"block":{"flavor":"com.superlogical.terminal.shell"}}
        }]
    }))?;
    let session = created["session_id"]
        .as_str()
        .context("created session ID")?
        .to_owned();
    let block = created["initial_windows"][0]["block_ids"][0]
        .as_str()
        .context("created block ID")?
        .to_owned();
    let attached = control.call("session.attach", json!({"session_id":session}))?;
    state.apply_view(&attached["view"])?;
    let pane = state.pane_ids().first().copied().context("native pane")?;
    ensure!(state.remote_pane(pane)? == block);
    let process = terminal_call(
        &mut control,
        &session,
        &block,
        "com.superlogical.terminal.process",
        json!({}),
    )?;
    let child_pid = process["child"]["pid"]
        .as_u64()
        .context("native shell PID")?;

    check_workspace_commands(&mut control, &mut state, &session, pane)
        .context("native workspace commands")?;
    check_block_ownership(&mut control, endpoint, &session, &block)
        .context("native ownership arbitration")?;
    check_terminal_stream(&mut control, endpoint, &session, &block)
        .context("native terminal streaming")?;

    let token = control
        .reconnect_token()
        .context("private reconnect token")?
        .to_owned();
    // A graceful desktop detach ends its identity. Resume models transport loss,
    // retaining the server's reconnect grace period instead of sending CLOSE.
    control
        .disconnect_for_resume()
        .context("interrupt native control transport")?;
    drop(control);
    let mut resumed = ControlConnection::connect_with_resume(endpoint, Some(&token))
        .context("resume native control identity")?;
    let bootstrap = resumed.call(
        "session.bootstrap",
        json!({"event_scope":"all","preferred_session_id":session}),
    )?;
    ensure!(
        resumed.client_id() == initial_identity,
        "native client identity changed across resume"
    );
    state.apply_bootstrap(&bootstrap)?;
    ensure!(
        state.remote_pane(pane)? == block,
        "same-instance reconnect remapped pane"
    );
    let process = terminal_call(
        &mut resumed,
        &session,
        &block,
        "com.superlogical.terminal.process",
        json!({}),
    )?;
    ensure!(
        process["child"]["pid"].as_u64() == Some(child_pid),
        "desktop detach restarted shell"
    );
    ensure!(process["last_exit"].is_null(), "shell exited during detach");
    check_cached_client(&mut resumed, endpoint, &session, &block, child_pid)
        .context("native cached desktop client")?;
    check_tab_broadcast(endpoint, &session).context("native bidirectional tab broadcasts")?;
    check_cached_reconnect(&mut resumed, endpoint, &session, &block, child_pid)
        .context("forced native cached control reconnect")?;
    check_titles_and_wheel(&mut resumed, endpoint, &session)
        .context("native live titles and wheel routing")?;
    check_focus_and_mouse(&mut resumed, endpoint, &session)
        .context("native focus and application mouse routing")?;
    check_pointer_reconnect(&mut resumed, endpoint, &session)
        .context("native held mouse gestures across reconnect")?;
    check_shell_exit(&mut resumed, endpoint, &session).context("native normal shell exit")?;
    resumed.call("session.destroy", json!({"session_id":session}))?;
    let sessions = resumed.call("session.list", json!({}))?;
    state.update_sessions(&sessions)?;
    ensure!(
        state.sessions().is_empty() && state.pane_ids().is_empty(),
        "destroyed fixture session retained in client"
    );
    Ok(())
}

fn check_shell_exit(control: &mut ControlConnection, endpoint: &str, session: &str) -> Result<()> {
    let created = control.call("session.new_window", json!({
        "session_id":session,"layout":{"block":{
            "flavor":"com.superlogical.terminal.shell","options":{
                "shell":"none","command":["/bin/sh","-c",
                    "printf 'EXIT_READY\\n'; read -r command; printf 'rex-exit-%s\\n' 'final-output'; exit 7"]
            }
        }}
    }))?;
    let block = created["block_ids"][0].as_str().context("exit block")?;
    let mut stream = open_terminal(control, endpoint, session, block)?;
    wait_text(&mut stream, "EXIT_READY")?;
    let mut client = WorkspaceClient::connect(endpoint.to_owned());
    wait_cache(&mut client, |client| {
        cached_text(client, block, "EXIT_READY")
    })?;
    let pane = client
        .state()
        .pane_ids()
        .into_iter()
        .find(|pane| {
            client
                .state()
                .pane(*pane)
                .is_some_and(|entry| entry.remote_id == block)
        })
        .context("cached exit pane")?;
    terminal_call(
        control,
        session,
        block,
        "com.superlogical.terminal.write",
        json!({"data":"exit\n"}),
    )?;
    let deadline = Instant::now() + TIMEOUT;
    let mut final_output = false;
    while stream.exit_code().is_none() || !final_output {
        ensure!(
            Instant::now() < deadline,
            "terminal did not publish final output and exit status"
        );
        if let Some(frame) = stream.poll(Duration::from_millis(20))? {
            final_output |= frame
                .text_lines()
                .join("\n")
                .contains("rex-exit-final-output");
        }
    }
    ensure!(stream.exit_code() == Some(7), "wrong shell exit code");
    ensure!(!stream.is_ready(), "exited stream still accepts input");
    wait_cache(&mut client, |client| {
        client.status(pane) == Some(&PaneStatus::Exited { code: 7 })
            && cached_text(client, block, "rex-exit-final-output")
    })?;
    ensure!(
        client.write_input(pane, b"ignored").is_err(),
        "exited pane accepted process input"
    );
    let frame = Arc::clone(client.frame(pane).context("final cached exit frame")?);
    // Stay attached past the reported five-second channel close and the old
    // retry deadline. Completion must not attempt RESUME or a fresh ticket.
    let settled = Instant::now() + Duration::from_secs(6);
    while Instant::now() < settled {
        ensure!(
            stream.poll(Duration::ZERO)?.is_none(),
            "exited stream republished a frame"
        );
        client.poll()?;
        ensure!(client.connected(), "shell exit disconnected workspace");
        ensure!(
            client.status(pane) == Some(&PaneStatus::Exited { code: 7 }),
            "shell exit changed to failure/retry status"
        );
        ensure!(
            Arc::ptr_eq(client.frame(pane).context("retained exit frame")?, &frame),
            "final exit frame was replaced"
        );
        thread::sleep(Duration::from_millis(10));
    }
    drop(client);
    let mut reattached = WorkspaceClient::connect(endpoint.to_owned());
    wait_cache(&mut reattached, |client| {
        client.state().pane_ids().into_iter().any(|pane| {
            client
                .state()
                .pane(pane)
                .is_some_and(|entry| entry.remote_id == block)
                && client.status(pane) == Some(&PaneStatus::Exited { code: 7 })
        }) && cached_text(client, block, "rex-exit-final-output")
    })?;
    drop(reattached);
    control.call(
        "session.close_window",
        json!({"session_id":session,"window_id":created["window_id"]}),
    )?;
    Ok(())
}

fn check_workspace_commands(
    control: &mut ControlConnection,
    state: &mut WorkspaceState,
    session: &str,
    pane: rex_rs::core::PaneId,
) -> Result<()> {
    let session_id = state.active_session().context("active session")?.id();
    run_command(
        control,
        state,
        &Command::RenameSession {
            session: session_id,
            title: "Native 界".into(),
        },
    )?;
    run_command(
        control,
        state,
        &Command::SplitPane {
            pane,
            axis: SplitAxis::Horizontal,
        },
    )?;
    refresh_view(control, state, session)?;
    ensure!(state.active_session().context("active session")?.title() == "Native 界");
    ensure!(
        state.pane_ids().len() == 2,
        "native split did not create a block"
    );
    let window = state
        .active_session()
        .context("session")?
        .active_window()
        .context("window")?;
    let split = match window.layout().context("tiled native layout")? {
        Layout::Split { id, .. } => *id,
        Layout::Pane(_) => anyhow::bail!("expected native split"),
    };
    run_command(control, state, &Command::ResizeSplit { split, ratio: 650 })?;
    refresh_view(control, state, session)?;
    let window = state
        .active_session()
        .context("session")?
        .active_window()
        .context("window")?;
    ensure!(
        matches!(window.layout(), Some(Layout::Split { ratio, .. }) if (*ratio - 0.65).abs() < f64::EPSILON),
        "native resize was not reflected"
    );
    wait_view_event(control, session)?;
    Ok(())
}

fn run_command(
    control: &mut ControlConnection,
    state: &WorkspaceState,
    command: &Command,
) -> Result<()> {
    let call = state.command(command)?.context("remote command")?;
    control.call(call.method, call.params)?;
    Ok(())
}

fn refresh_view(
    control: &mut ControlConnection,
    state: &mut WorkspaceState,
    session: &str,
) -> Result<()> {
    state.apply_view(&control.call("session.view", json!({"session_id":session}))?)?;
    Ok(())
}

fn wait_view_event(control: &mut ControlConnection, session: &str) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    let mut previous = None;
    loop {
        ensure!(
            Instant::now() < deadline,
            "missing native session view event"
        );
        if let Some(event) = control.poll_event(Duration::from_millis(50))? {
            let sequence = event["seq"].as_u64().context("event sequence")?;
            if let Some(previous) = previous {
                ensure!(sequence == previous + 1, "native event sequence gap");
            }
            previous = Some(sequence);
            let payload = &event["payload"];
            if payload["type"] == "session_event"
                && payload["payload"]["session_id"] == session
                && payload["payload"]["event"]["type"] == "session_view_changed"
            {
                return Ok(());
            }
        }
    }
}

fn check_terminal_stream(
    control: &mut ControlConnection,
    endpoint: &str,
    session: &str,
    block: &str,
) -> Result<()> {
    paste(control, session, block, "printf 'rex-native-baseline\\n'\n")?;
    let mut stream =
        open_terminal(control, endpoint, session, block).context("open baseline terminal data")?;
    wait_texts(
        &mut stream,
        &["rex-native-baseline", "rex-native-observer-input"],
    )?;
    terminal_call(
        control,
        session,
        block,
        "com.superlogical.terminal.resize",
        json!({"columns":93,"rows":31,"cell_width_px":8,"cell_height_px":16}),
    )?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        ensure!(
            Instant::now() < deadline,
            "native resize did not reach stream"
        );
        if let Some(frame) = stream.poll(Duration::from_millis(50))?
            && frame.size.columns() == 93
            && frame.size.rows() == 31
        {
            break;
        }
    }
    paste(
        control,
        session,
        block,
        "printf 'rex-native-stream-\\344\\270\\226\\347\\225\\214\\n'\n",
    )?;
    wait_text(&mut stream, "rex-native-stream-世界")?;
    key(
        control,
        session,
        block,
        "unidentified",
        "printf 'rex-native-key\\n'",
    )?;
    key(control, session, block, "enter", "")?;
    wait_text(&mut stream, "rex-native-key")?;
    settle_stream(&mut stream)?;
    stream.resume().context("resume idle terminal data")?;
    paste(
        control,
        session,
        block,
        "printf 'rex-native-resumed-\\344\\270\\226\\347\\225\\214\\n'\n",
    )?;
    wait_text(&mut stream, "rex-native-resumed-世界")?;
    // Existing terminals may retain Kitty images, adding optional snapshot
    // records that fresh empty terminals never exercise. Only this isolated
    // fixture receives the one-pixel image; its text must still restore.
    paste(
        control,
        session,
        block,
        r"printf '\033_Ga=T,f=24,s=1,v=1,i=4242,q=2;AAAA\033\\'; printf 'rex-native-%s\n' 'graphics-ready'",
    )?;
    wait_text(&mut stream, "rex-native-graphics-ready")?;
    drop(stream);
    let mut fresh = open_terminal(control, endpoint, session, block)
        .context("open fresh terminal snapshot after stream drop")?;
    wait_texts(
        &mut fresh,
        &["rex-native-resumed-世界", "rex-native-graphics-ready"],
    )?;
    Ok(())
}

fn check_block_ownership(
    owner: &mut ControlConnection,
    endpoint: &str,
    session: &str,
    block: &str,
) -> Result<()> {
    let mut observer = ControlConnection::connect(endpoint)?;
    observer.call(
        "session.bootstrap",
        json!({"event_scope":"session","preferred_session_id":session}),
    )?;
    let claims = observer.call(
        "session.own_block",
        json!({"session_id":session,"block_ids":[block],"mode":"request"}),
    )?;
    ensure!(
        claims["blocks"][0]["owner"] == owner.client_id(),
        "requesting ownership displaced active owner"
    );
    terminal_call(
        owner,
        session,
        block,
        "com.superlogical.terminal.resize",
        json!({"columns":81,"rows":25}),
    )?;
    terminal_call(
        &mut observer,
        session,
        block,
        "com.superlogical.terminal.resize",
        json!({"columns":71,"rows":19}),
    )?;
    let size = terminal_call(
        owner,
        session,
        block,
        "com.superlogical.terminal.size",
        json!({}),
    )?;
    ensure!(
        size["columns"] == 81 && size["rows"] == 25,
        "non-owner resize changed terminal size"
    );
    // Owning a block arbitrates preferences; it does not gate terminal input.
    paste(
        &mut observer,
        session,
        block,
        "printf 'rex-native-observer-input\\n'\n",
    )?;
    observer.call(
        "session.own_block",
        json!({"session_id":session,"block_ids":[block],"mode":"take"}),
    )?;
    let size = terminal_call(
        &mut observer,
        session,
        block,
        "com.superlogical.terminal.size",
        json!({}),
    )?;
    ensure!(
        size["columns"] == 71 && size["rows"] == 19,
        "takeover did not apply requested size"
    );
    owner.call(
        "session.own_block",
        json!({"session_id":session,"block_ids":[block],"mode":"take"}),
    )?;
    Ok(())
}

fn open_terminal(
    control: &mut ControlConnection,
    endpoint: &str,
    session: &str,
    block: &str,
) -> Result<TerminalStream> {
    let result = control.call("data.connect", json!({"session_id":session,"block_id":block,"data_ref_id":"terminal","subprotocol":"rex.data.terminal.v1"}))?;
    TerminalStream::connect(endpoint, result["ticket"].as_str().context("data ticket")?)
}

fn terminal_call(
    control: &mut ControlConnection,
    session: &str,
    block: &str,
    method: &str,
    args: Value,
) -> Result<Value> {
    let mut params = json!({"session_id":session,"block_id":block});
    params["args"] = args;
    control.call(method, params)
}

fn paste(control: &mut ControlConnection, session: &str, block: &str, text: &str) -> Result<()> {
    // A bracketed paste inserts newlines into readline's editing buffer. Submit
    // fixture commands with a separate Enter, exactly as an interactive user does.
    let text = text.trim_end_matches(['\r', '\n']);
    control.call("block.event", json!({"session_id":session,"block_id":block,"event":{"type":"paste","payload":{"text":text}}}))?;
    key(control, session, block, "enter", "")
}

fn key(
    control: &mut ControlConnection,
    session: &str,
    block: &str,
    physical: &str,
    text: &str,
) -> Result<()> {
    control.call(
        "client.key",
        json!({"session_id":session,"block_id":block,"key":{
            "key":physical,"mods":"","consumed_mods":"","text":text,
            "unshifted_codepoint":0,"composing":false,"action":"press"
        }}),
    )?;
    Ok(())
}

fn wait_text(stream: &mut TerminalStream, needle: &str) -> Result<()> {
    wait_texts(stream, &[needle])
}

fn settle_stream(stream: &mut TerminalStream) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    let mut last_output = Instant::now();
    loop {
        ensure!(
            Instant::now() < deadline,
            "native terminal did not become idle before resume"
        );
        if stream.poll(Duration::from_millis(50))?.is_some() {
            last_output = Instant::now();
        } else if last_output.elapsed() >= Duration::from_millis(100) {
            return Ok(());
        }
    }
}

fn wait_texts(stream: &mut TerminalStream, needles: &[&str]) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        ensure!(
            Instant::now() < deadline,
            "native terminal never produced {needles:?}"
        );
        if let Some(frame) = stream.poll(Duration::from_millis(50))? {
            let text = frame.text_lines().join("\n");
            if needles.iter().all(|needle| text.contains(needle)) {
                return Ok(());
            }
        }
    }
}

fn check_cached_client(
    control: &mut ControlConnection,
    endpoint: &str,
    session: &str,
    block: &str,
    child_pid: u64,
) -> Result<()> {
    let mut client = WorkspaceClient::connect(endpoint.to_owned());
    wait_cache(&mut client, |client| {
        client.connected() && cached_text(client, block, "rex-native-resumed-世界")
    })?;
    let pane = client
        .state()
        .pane_ids()
        .into_iter()
        .find(|pane| {
            client
                .state()
                .pane(*pane)
                .is_some_and(|entry| entry.remote_id == block)
        })
        .context("cached native pane")?;
    control.call(
        "session.set_label",
        json!({"session_id":session,"label":"Cached 界"}),
    )?;
    wait_cache(&mut client, |client| {
        client
            .state()
            .sessions()
            .iter()
            .any(|entry| entry.remote_id() == session && entry.title() == "Cached 界")
    })?;

    client.write_input(pane, b"printf 'rex-native-cache-key\\n'")?;
    client.send_key(
        pane,
        &KeyInput {
            physical_key: None,
            key: "enter".into(),
            text: None,
            modifiers: Modifiers::default(),
            repeat: false,
        },
    )?;
    client.paste(
        pane,
        "printf 'rex-native-cache-\\344\\270\\226\\347\\225\\214\\n'",
    )?;
    client.send_key(
        pane,
        &KeyInput {
            physical_key: None,
            key: "enter".into(),
            text: None,
            modifiers: Modifiers::default(),
            repeat: false,
        },
    )?;
    wait_cache(&mut client, |client| {
        cached_text(client, block, "rex-native-cache-世界")
            && cached_text(client, block, "rex-native-cache-key")
    })?;
    client.set_size_ownership(pane, true)?;
    client.resize(pane, GridSize::new(87, 29)?, 8, 16)?;
    wait_cache(&mut client, |client| {
        client.owns_size(pane)
            && client
                .frame(pane)
                .is_some_and(|frame| frame.size.columns() == 87 && frame.size.rows() == 29)
    })?;
    drop(client);

    let mut reopened = WorkspaceClient::connect(endpoint.to_owned());
    wait_cache(&mut reopened, |client| {
        client.connected() && cached_text(client, block, "rex-native-cache-世界")
    })?;
    let process = terminal_call(
        control,
        session,
        block,
        "com.superlogical.terminal.process",
        json!({}),
    )?;
    ensure!(
        process["child"]["pid"].as_u64() == Some(child_pid),
        "cached desktop detach restarted shell"
    );
    ensure!(
        process["last_exit"].is_null(),
        "cached desktop detach terminated shell"
    );
    Ok(())
}

fn check_tab_broadcast(endpoint: &str, session: &str) -> Result<()> {
    let mut observers = Vec::new();
    for scope in ["session", "all"] {
        let mut observer = ControlConnection::connect(endpoint)?;
        observer.call(
            "session.bootstrap",
            json!({"event_scope":scope,"preferred_session_id":session}),
        )?;
        let attached = observer.call("session.attach", json!({"session_id":session}))?;
        let revision = attached["view"]["revision"]
            .as_u64()
            .context("observer revision")?;
        observers.push((observer, revision));
    }
    let mut desktop = WorkspaceClient::connect(endpoint.to_owned());
    wait_cache(&mut desktop, WorkspaceClient::connected)?;
    let session_alias = desktop
        .state()
        .sessions()
        .iter()
        .find(|entry| entry.remote_id() == session)
        .context("desktop broadcast session")?
        .id();
    desktop.apply(Command::CreateWindow {
        session: session_alias,
        title: "Desktop broadcast tab".into(),
    })?;
    let mut created = None;
    for (observer, revision) in &mut observers {
        // Do not request a new view until the independent observer receives
        // both creation and invalidation events. Polling must not trigger sync.
        let window = wait_window_broadcast(observer, session, "Desktop broadcast tab", *revision)?;
        if let Some(previous) = &created {
            ensure!(
                *previous == window,
                "observers received different desktop tabs"
            );
        }
        let view = observer.call("session.view", json!({"session_id":session}))?;
        ensure!(
            view["windows"]
                .as_array()
                .context("observer windows")?
                .iter()
                .any(|entry| entry["window_id"] == window),
            "broadcast tab missing from server view"
        );
        created = Some(window);
    }
    let created = created.context("broadcast window")?;
    wait_cache(&mut desktop, |client| {
        cached_window_title(client, &created) == Some("Desktop broadcast tab")
    })?;
    let remote = observers[0].0.call(
        "session.new_window",
        json!({
            "session_id":session,"window_label":"Remote broadcast tab",
            "layout":{"block":{"flavor":"com.superlogical.terminal.shell"}}
        }),
    )?;
    let remote = remote["window_id"]
        .as_str()
        .context("remote broadcast window")?;
    wait_cache(&mut desktop, |client| {
        cached_window_title(client, remote) == Some("Remote broadcast tab")
    })?;
    for window in [created.as_str(), remote] {
        observers[0].0.call(
            "session.close_window",
            json!({"session_id":session,"window_id":window}),
        )?;
        wait_cache(&mut desktop, |client| {
            cached_window_title(client, window).is_none()
        })?;
    }
    Ok(())
}

fn wait_window_broadcast(
    observer: &mut ControlConnection,
    session: &str,
    label: &str,
    baseline: u64,
) -> Result<String> {
    let deadline = Instant::now() + TIMEOUT;
    let mut window = None;
    let mut invalidated = false;
    loop {
        ensure!(
            Instant::now() < deadline,
            "observer did not receive tab creation and view invalidation"
        );
        if let Some(event) = observer.poll_event(Duration::from_millis(20))? {
            let payload = &event["payload"];
            if payload["type"] != "session_event" || payload["payload"]["session_id"] != session {
                continue;
            }
            let event = &payload["payload"]["event"];
            if event["type"] == "window_created" && event["payload"]["label"] == label {
                window = Some(
                    event["payload"]["window_id"]
                        .as_str()
                        .context("broadcast window ID")?
                        .to_owned(),
                );
            } else if event["type"] == "session_view_changed" {
                invalidated |= event["payload"]["revision"]
                    .as_u64()
                    .is_some_and(|revision| revision > baseline);
            }
        }
        if invalidated && let Some(window) = window.take() {
            return Ok(window);
        }
    }
}

fn cached_text(client: &WorkspaceClient, block: &str, needle: &str) -> bool {
    client.state().pane_ids().into_iter().any(|pane| {
        client
            .state()
            .pane(pane)
            .is_some_and(|entry| entry.remote_id == block)
            && client
                .frame(pane)
                .is_some_and(|frame| frame.text_lines().join("\n").contains(needle))
    })
}

struct ProxySockets {
    client: UnixStream,
    server: UnixStream,
}

impl ProxySockets {
    fn clone_sockets(&self) -> Result<Self> {
        Ok(Self {
            client: self.client.try_clone()?,
            server: self.server.try_clone()?,
        })
    }

    fn shutdown(&self) {
        let _ = self.client.shutdown(Shutdown::Both);
        let _ = self.server.shutdown(Shutdown::Both);
    }
}

#[derive(Default)]
struct ProxyState {
    stopped: AtomicBool,
    sockets: Mutex<Vec<ProxySockets>>,
    control: Mutex<Option<ProxySockets>>,
    data: Mutex<Vec<ProxySockets>>,
    data_connections: AtomicUsize,
}

/// Test-only byte proxy; it preserves WebSocket framing and Unix peer identity.
struct ControlProxy {
    endpoint: String,
    path: PathBuf,
    shared: Arc<ProxyState>,
    worker: Option<JoinHandle<()>>,
}

impl ControlProxy {
    fn start(endpoint: &str) -> Result<Self> {
        let upstream = PathBuf::from(
            endpoint
                .strip_prefix("unix://")
                .context("proxy upstream endpoint")?,
        );
        let path = upstream.with_file_name("reconnect-proxy.sock");
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        let shared = Arc::new(ProxyState::default());
        let accept_state = Arc::clone(&shared);
        let worker = thread::spawn(move || {
            let mut workers = Vec::new();
            while !accept_state.stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((client, _)) if workers.len() < 32 => {
                        let state = Arc::clone(&accept_state);
                        let upstream = upstream.clone();
                        workers.push(thread::spawn(move || {
                            let _ = forward_proxy(client, &upstream, &state);
                        }));
                    }
                    Ok((client, _)) => {
                        let _ = client.shutdown(Shutdown::Both);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            for worker in workers {
                let _ = worker.join();
            }
        });
        Ok(Self {
            endpoint: format!("unix://{}", path.display()),
            path,
            shared,
            worker: Some(worker),
        })
    }

    fn disconnect_control(&self) -> Result<()> {
        let control = self
            .shared
            .control
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .context("proxy has no active control connection")?;
        control.shutdown();
        Ok(())
    }

    fn disconnect_data(&self) -> Result<usize> {
        let mut data = self
            .shared
            .data
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        ensure!(!data.is_empty(), "proxy has no active data connections");
        let expected = self.shared.data_connections.load(Ordering::Acquire) + data.len();
        for socket in data.drain(..) {
            socket.shutdown();
        }
        Ok(expected)
    }
}

impl Drop for ControlProxy {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        for sockets in self
            .shared
            .sockets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            sockets.shutdown();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

fn forward_proxy(mut client: UnixStream, upstream: &Path, shared: &ProxyState) -> Result<()> {
    let server = UnixStream::connect(upstream)?;
    let mut sockets = ProxySockets {
        client: client.try_clone()?,
        server,
    };
    shared
        .sockets
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(sockets.clone_sockets()?);
    if shared.stopped.load(Ordering::Acquire) {
        sockets.shutdown();
        return Ok(());
    }
    client.set_read_timeout(Some(Duration::from_secs(3)))?;
    let mut initial = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !initial.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        let count = client.read(&mut buffer)?;
        ensure!(count > 0, "proxy client closed before upgrade");
        initial.extend_from_slice(&buffer[..count]);
        ensure!(initial.len() <= 16 * 1024, "proxy upgrade header too large");
    }
    client.set_read_timeout(None)?;
    if initial.starts_with(b"GET /api/control ") {
        *shared
            .control
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(sockets.clone_sockets()?);
    } else if initial.starts_with(b"GET /api/data ") {
        shared
            .data
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(sockets.clone_sockets()?);
        shared.data_connections.fetch_add(1, Ordering::Release);
    }
    sockets.server.write_all(&initial)?;
    let mut response_server = sockets.server.try_clone()?;
    let mut response_client = sockets.client.try_clone()?;
    let response = thread::spawn(move || {
        let _ = std::io::copy(&mut response_server, &mut response_client);
        let _ = response_client.shutdown(Shutdown::Both);
        let _ = response_server.shutdown(Shutdown::Both);
    });
    let _ = std::io::copy(&mut client, &mut sockets.server);
    sockets.shutdown();
    let _ = response.join();
    Ok(())
}

fn check_cached_reconnect(
    control: &mut ControlConnection,
    endpoint: &str,
    session: &str,
    block: &str,
    child_pid: u64,
) -> Result<()> {
    let proxy = ControlProxy::start(endpoint)?;
    let mut client = WorkspaceClient::connect(proxy.endpoint.clone());
    wait_cache(&mut client, |client| {
        client.connected() && cached_text(client, block, "rex-native-cache-世界")
    })?;
    let pane = client
        .state()
        .pane_ids()
        .into_iter()
        .find(|pane| {
            client
                .state()
                .pane(*pane)
                .is_some_and(|entry| entry.remote_id == block)
        })
        .context("proxy cached pane")?;
    let client_id = client.state().client_id().to_owned();
    let instance = client
        .server_id()
        .context("proxy server instance")?
        .to_owned();
    proxy.disconnect_control()?;
    wait_connection_state(&mut client, false)?;
    wait_connection_state(&mut client, true)?;
    ensure!(
        client.state().client_id() == client_id,
        "forced reconnect changed client identity"
    );
    ensure!(
        client.server_id() == Some(instance.as_str()),
        "forced reconnect changed server instance"
    );
    ensure!(
        client.state().remote_pane(pane)? == block,
        "forced reconnect changed pane alias"
    );
    control.call(
        "session.set_label",
        json!({"session_id":session,"label":"Reconnected 界"}),
    )?;
    paste(
        control,
        session,
        block,
        "printf 'rex-native-forced-\\344\\270\\226\\347\\225\\214\\n'\n",
    )?;
    wait_cache(&mut client, |client| {
        cached_text(client, block, "rex-native-forced-世界")
            && client
                .state()
                .sessions()
                .iter()
                .any(|entry| entry.remote_id() == session && entry.title() == "Reconnected 界")
    })?;
    let process = terminal_call(
        control,
        session,
        block,
        "com.superlogical.terminal.process",
        json!({}),
    )?;
    ensure!(
        process["child"]["pid"].as_u64() == Some(child_pid),
        "forced reconnect restarted shell"
    );
    drop(client);
    Ok(())
}

fn wait_connection_state(client: &mut WorkspaceClient, connected: bool) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    let mut last_error = None;
    loop {
        // This fixture intentionally breaks one socket. Its expected transport
        // error must not prevent checking the subsequent disconnected cache.
        if let Err(error) = client.poll() {
            last_error = Some(error.to_string());
        }
        if client.connected() == connected {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "client did not become connected={connected}: {last_error:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

const WHEEL_PROGRAM: &str = r"import os, re, sys, tty
tty.setraw(sys.stdin.fileno())
def emit(text):
    os.write(sys.stdout.fileno(), text.encode('utf-8'))
emit('\x1b]2;Rex native initial title\x07')
for row in range(160):
    emit('PRIMARY_HISTORY_%03d\r\n' % row)
emit('PRIMARY_READY\r\n')
pending = b''
count = 0
while True:
    data = os.read(sys.stdin.fileno(), 4096)
    if not data:
        break
    pending += data
    while pending:
        wheel = re.match(rb'\x1b\[<\d+;\d+;\d+[Mm]', pending)
        if wheel:
            packet = wheel.group(0)
            pending = pending[len(packet):]
            count += 1
            emit('WHEEL_%d_%s\r\n' % (count, packet.hex()))
            continue
        if b'\n' not in pending:
            break
        command, pending = pending.split(b'\n', 1)
        if command == b'TITLE':
            emit('\x1b]2;Rex native updated title\x07TITLE_READY\r\n')
        elif command == b'ALT':
            emit('\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[2J\x1b[HALT_READY\r\n')
        elif command == b'REPORT':
            emit('WHEEL_COUNT_%d\r\n' % count)
";

const POINTER_PROGRAM: &str = r"import os, re, tty
tty.setraw(0)
def emit(text):
    os.write(1, text.encode())
emit('\x1b[?1006hPOINTER_READY\r\n')
pending = b''
count = 0
while True:
    data = os.read(0, 4096)
    if not data:
        break
    pending += data
    while pending:
        event = re.match(rb'\x1b\[<\d+;\d+;\d+[Mm]|\x1b\[[IO]', pending)
        if event:
            packet = event.group(0)
            pending = pending[len(packet):]
            count += 1
            emit('EVENT_%d_%s\r\n' % (count, packet.hex()))
            continue
        if b'\n' not in pending:
            break
        command, pending = pending.split(b'\n', 1)
        if command == b'FOCUSON':
            emit('\x1b[?1004hACK_FOCUSON\r\n')
        elif command == b'FOCUSOFF':
            emit('\x1b[?1004lACK_FOCUSOFF\r\n')
        elif command == b'MODEONLYOFF':
            emit('\x1b[?1000l\x1b[?1002l\x1b[?1003l')
        elif command.startswith(b'MOUSE'):
            emit('\x1b[?1000l\x1b[?1002l\x1b[?1003l')
            if command != b'MOUSEOFF':
                emit('\x1b[?%sh' % command[5:].decode())
            emit('ACK_%s\r\n' % command.decode())
        elif command.startswith(b'REPORT_'):
            emit('%s_COUNT_%d\r\n' % (command.decode(), count))
";

struct WheelProgram {
    session: String,
    window: String,
    block: String,
    script: PathBuf,
}

impl WheelProgram {
    fn start(control: &mut ControlConnection, endpoint: &str, session: &str) -> Result<Self> {
        Self::start_source(
            control,
            endpoint,
            session,
            "wheel-program.py",
            WHEEL_PROGRAM,
        )
    }

    fn start_source(
        control: &mut ControlConnection,
        endpoint: &str,
        session: &str,
        filename: &str,
        source: &str,
    ) -> Result<Self> {
        let socket = Path::new(
            endpoint
                .strip_prefix("unix://")
                .context("wheel fixture endpoint")?,
        );
        let script = socket.with_file_name(filename);
        fs::write(&script, source)?;
        // Resolve version-manager shims while still in the project directory;
        // the server launches the program from its isolated fixture directory.
        let python = ProcessCommand::new("python3")
            .args(["-c", "import sys; print(sys.executable)"])
            .output()
            .context("resolve Python for native wheel fixture")?;
        ensure!(
            python.status.success(),
            "native wheel fixture requires Python 3"
        );
        let python = String::from_utf8(python.stdout)?;
        let python = python.trim();
        ensure!(
            Path::new(python).is_absolute(),
            "Python interpreter must be absolute"
        );
        let created = control.call("session.new_window", json!({
            "session_id":session,"layout":{"block":{"flavor":"com.superlogical.terminal.shell","options":{
                "shell":"none","command":[python,script],"size":{"columns":80,"rows":12}
            }}}
        }))?;
        Ok(Self {
            session: session.into(),
            window: created["window_id"]
                .as_str()
                .context("wheel window ID")?
                .into(),
            block: created["block_ids"][0]
                .as_str()
                .context("wheel block ID")?
                .into(),
            script,
        })
    }

    fn command(&self, control: &mut ControlConnection, command: &str) -> Result<()> {
        terminal_call(
            control,
            &self.session,
            &self.block,
            "com.superlogical.terminal.write",
            json!({"data":format!("{command}\n")}),
        )?;
        Ok(())
    }
}

fn pointer_command(
    client: &mut WorkspaceClient,
    program: &WheelProgram,
    pane: rex_rs::core::PaneId,
    command: &str,
) -> Result<()> {
    client.write_input(pane, format!("{command}\n").as_bytes())?;
    wait_cache(client, |client| {
        cached_text(client, &program.block, &format!("ACK_{command}"))
    })
}

fn pointer_barrier(
    client: &mut WorkspaceClient,
    program: &WheelProgram,
    pane: rex_rs::core::PaneId,
    tag: &str,
    expected: u32,
) -> Result<()> {
    let command = format!("REPORT_{tag}");
    client.write_input(pane, format!("{command}\n").as_bytes())?;
    wait_cache(client, |client| {
        cached_text(
            client,
            &program.block,
            &format!("{command}_COUNT_{expected}"),
        )
    })
}

fn pointer_event(
    client: &mut WorkspaceClient,
    program: &WheelProgram,
    pane: rex_rs::core::PaneId,
    input: MouseInput,
    event: u32,
    encoded: &str,
) -> Result<()> {
    client.mouse(pane, input)?;
    wait_cache(client, |client| {
        cached_text(client, &program.block, &format!("EVENT_{event}_{encoded}"))
    })
}

fn check_pointer_reconnect(
    control: &mut ControlConnection,
    endpoint: &str,
    session: &str,
) -> Result<()> {
    let program = WheelProgram::start_source(
        control,
        endpoint,
        session,
        "pointer-reconnect.py",
        POINTER_PROGRAM,
    )?;
    let proxy = ControlProxy::start(endpoint)?;
    let mut client = WorkspaceClient::connect(proxy.endpoint.clone());
    wait_cache(&mut client, |client| {
        cached_text(client, &program.block, "POINTER_READY")
    })?;
    let pane = client
        .state()
        .pane_ids()
        .into_iter()
        .find(|pane| {
            client
                .state()
                .pane(*pane)
                .is_some_and(|block| block.remote_id == program.block)
        })
        .context("pointer reconnect pane")?;
    pointer_command(&mut client, &program, pane, "MOUSE1000")?;
    wait_cache(&mut client, |client| client.mouse_reporting(pane))?;
    let mouse = MouseInput {
        action: MouseAction::Press,
        button: MouseButton::Left,
        column: 3,
        row: 2,
        modifiers: Modifiers::default(),
    };
    pointer_event(&mut client, &program, pane, mouse, 1, "1b5b3c303b343b334d")?;
    let expected = proxy.disconnect_data()?;
    wait_pointer_data_resume(&mut client, &proxy, pane, expected)?;
    // Shift changed during the gesture: only retained worker press state permits
    // this matching release, so a snapshot replacement cannot accidentally pass.
    let shifted_release = MouseInput {
        action: MouseAction::Release,
        modifiers: Modifiers {
            shift: true,
            ..Modifiers::default()
        },
        ..mouse
    };
    pointer_event(
        &mut client,
        &program,
        pane,
        shifted_release,
        2,
        "1b5b3c343b343b336d",
    )?;
    check_pointer_control_resume(&mut client, &proxy, &program, pane, mouse)?;
    control.call(
        "session.close_window",
        json!({"session_id":session,"window_id":program.window}),
    )?;
    wait_cache(&mut client, |client| client.state().pane(pane).is_none())?;
    drop(client);
    fs::remove_file(program.script)?;
    Ok(())
}

fn check_pointer_control_resume(
    client: &mut WorkspaceClient,
    proxy: &ControlProxy,
    program: &WheelProgram,
    pane: rex_rs::core::PaneId,
    mouse: MouseInput,
) -> Result<()> {
    pointer_event(client, program, pane, mouse, 3, "1b5b3c303b343b334d")?;
    let identity = client.state().client_id().to_owned();
    proxy.disconnect_control()?;
    wait_connection_state(client, false)?;
    wait_connection_state(client, true)?;
    wait_cache(client, |client| {
        client.input_available(pane)
            && cached_text(client, &program.block, "EVENT_4_1b5b3c303b343b336d")
    })?;
    ensure!(
        client.state().client_id() == identity,
        "pointer reconnect lost client identity"
    );
    // Reconnect cleanup already released it; the late physical release must not
    // produce a second release, and subsequent gestures must still work.
    client.mouse(
        pane,
        MouseInput {
            action: MouseAction::Release,
            modifiers: Modifiers {
                shift: true,
                ..Modifiers::default()
            },
            ..mouse
        },
    )?;
    pointer_barrier(client, program, pane, "RECONNECTED", 4)?;
    pointer_event(client, program, pane, mouse, 5, "1b5b3c303b343b334d")?;
    pointer_event(
        client,
        program,
        pane,
        MouseInput {
            action: MouseAction::Release,
            ..mouse
        },
        6,
        "1b5b3c303b343b336d",
    )?;
    Ok(())
}

fn wait_pointer_data_resume(
    client: &mut WorkspaceClient,
    proxy: &ControlProxy,
    pane: rex_rs::core::PaneId,
    expected: usize,
) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        client.poll()?;
        ensure!(
            client.input_available(pane),
            "exact data resume dropped input readiness"
        );
        ensure!(
            client.mouse_reporting(pane),
            "exact data resume dropped mouse mode"
        );
        if proxy.shared.data_connections.load(Ordering::Acquire) >= expected {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "proxy saw no resumed data connection"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

fn check_focus_and_mouse(
    control: &mut ControlConnection,
    endpoint: &str,
    session: &str,
) -> Result<()> {
    let program = WheelProgram::start_source(
        control,
        endpoint,
        session,
        "pointer-program.py",
        POINTER_PROGRAM,
    )?;
    let mut client = WorkspaceClient::connect(endpoint.to_owned());
    wait_cache(&mut client, |client| {
        cached_text(client, &program.block, "POINTER_READY")
    })?;
    let pane = client
        .state()
        .pane_ids()
        .into_iter()
        .find(|pane| {
            client
                .state()
                .pane(*pane)
                .is_some_and(|block| block.remote_id == program.block)
        })
        .context("pointer fixture pane")?;
    let mouse = MouseInput {
        action: MouseAction::Press,
        button: MouseButton::Left,
        column: 3,
        row: 2,
        modifiers: Modifiers::default(),
    };
    ensure!(!client.mouse_reporting(pane), "tracking starts disabled");
    client.focus(pane, true)?;
    client.focus(pane, false)?;
    client.mouse(pane, mouse)?;
    pointer_barrier(&mut client, &program, pane, "OFF", 0)?;

    pointer_command(&mut client, &program, pane, "FOCUSON")?;
    client.focus(pane, true)?;
    wait_cache(&mut client, |client| {
        cached_text(client, &program.block, "EVENT_1_1b5b49")
    })?;
    client.focus(pane, false)?;
    wait_cache(&mut client, |client| {
        cached_text(client, &program.block, "EVENT_2_1b5b4f")
    })?;
    pointer_command(&mut client, &program, pane, "FOCUSOFF")?;
    client.focus(pane, true)?;
    client.focus(pane, false)?;
    pointer_barrier(&mut client, &program, pane, "FOCUSOFF", 2)?;

    check_mouse_buttons(&mut client, &program, pane, mouse)?;
    check_mouse_motion(&mut client, &program, pane, mouse)?;
    control.call(
        "session.close_window",
        json!({"session_id":session,"window_id":program.window}),
    )?;
    wait_cache(&mut client, |client| client.state().pane(pane).is_none())?;
    ensure!(
        !client.mouse_reporting(pane),
        "removed pane retains mouse tracking"
    );
    fs::remove_file(program.script)?;
    Ok(())
}

fn check_mouse_buttons(
    client: &mut WorkspaceClient,
    program: &WheelProgram,
    pane: rex_rs::core::PaneId,
    mouse: MouseInput,
) -> Result<()> {
    pointer_command(client, program, pane, "MOUSE1000")?;
    wait_cache(client, |client| client.mouse_reporting(pane))?;
    // Coordinates are zero-based in the API and one-based in SGR reports.
    pointer_event(client, program, pane, mouse, 3, "1b5b3c303b343b334d")?;
    pointer_event(
        client,
        program,
        pane,
        MouseInput {
            action: MouseAction::Release,
            ..mouse
        },
        4,
        "1b5b3c303b343b336d",
    )?;
    let middle = MouseInput {
        button: MouseButton::Middle,
        modifiers: Modifiers {
            control: true,
            alt: true,
            ..Modifiers::default()
        },
        ..mouse
    };
    pointer_event(client, program, pane, middle, 5, "1b5b3c32353b343b334d")?;
    pointer_event(
        client,
        program,
        pane,
        MouseInput {
            action: MouseAction::Release,
            ..middle
        },
        6,
        "1b5b3c32353b343b336d",
    )?;
    let right = MouseInput {
        button: MouseButton::Right,
        ..mouse
    };
    pointer_event(client, program, pane, right, 7, "1b5b3c323b343b334d")?;
    pointer_event(
        client,
        program,
        pane,
        MouseInput {
            action: MouseAction::Release,
            ..right
        },
        8,
        "1b5b3c323b343b336d",
    )?;
    check_mouse_shift(client, program, pane, mouse)
}

fn check_mouse_shift(
    client: &mut WorkspaceClient,
    program: &WheelProgram,
    pane: rex_rs::core::PaneId,
    mouse: MouseInput,
) -> Result<()> {
    // Press-only tracking must not report hover or drag movement.
    client.mouse(
        pane,
        MouseInput {
            action: MouseAction::Move,
            button: MouseButton::None,
            ..mouse
        },
    )?;
    client.mouse(
        pane,
        MouseInput {
            action: MouseAction::Move,
            ..mouse
        },
    )?;
    let shifted = MouseInput {
        modifiers: Modifiers {
            shift: true,
            ..Modifiers::default()
        },
        ..mouse
    };
    client.mouse(pane, shifted)?;
    client.mouse(
        pane,
        MouseInput {
            action: MouseAction::Move,
            ..shifted
        },
    )?;
    client.mouse(
        pane,
        MouseInput {
            action: MouseAction::Release,
            ..shifted
        },
    )?;
    pointer_barrier(client, program, pane, "SHIFT", 8)?;
    // Changing Shift during an application gesture must still release its button.
    pointer_event(client, program, pane, mouse, 9, "1b5b3c303b343b334d")?;
    pointer_event(
        client,
        program,
        pane,
        MouseInput {
            action: MouseAction::Release,
            ..shifted
        },
        10,
        "1b5b3c343b343b336d",
    )?;
    pointer_barrier(client, program, pane, "BUTTONS", 10)
}

fn check_mouse_motion(
    client: &mut WorkspaceClient,
    program: &WheelProgram,
    pane: rex_rs::core::PaneId,
    mouse: MouseInput,
) -> Result<()> {
    check_mouse_drag(client, program, pane, mouse)?;
    check_mouse_hover(client, program, pane)?;
    check_mouse_blur_and_disable(client, program, pane, mouse)
}

fn check_mouse_drag(
    client: &mut WorkspaceClient,
    program: &WheelProgram,
    pane: rex_rs::core::PaneId,
    mouse: MouseInput,
) -> Result<()> {
    pointer_command(client, program, pane, "MOUSE1002")?;
    client.mouse(
        pane,
        MouseInput {
            action: MouseAction::Move,
            button: MouseButton::None,
            column: 4,
            row: 3,
            ..mouse
        },
    )?;
    pointer_barrier(client, program, pane, "HOVER", 10)?;
    pointer_event(client, program, pane, mouse, 11, "1b5b3c303b343b334d")?;
    pointer_event(
        client,
        program,
        pane,
        MouseInput {
            action: MouseAction::Move,
            column: 4,
            row: 3,
            ..mouse
        },
        12,
        "1b5b3c33323b353b344d",
    )?;
    pointer_event(
        client,
        program,
        pane,
        MouseInput {
            action: MouseAction::Release,
            column: 4,
            row: 3,
            ..mouse
        },
        13,
        "1b5b3c303b353b346d",
    )?;
    Ok(())
}

fn check_mouse_hover(
    client: &mut WorkspaceClient,
    program: &WheelProgram,
    pane: rex_rs::core::PaneId,
) -> Result<()> {
    pointer_command(client, program, pane, "MOUSE1003")?;
    pointer_event(
        client,
        program,
        pane,
        MouseInput {
            action: MouseAction::Move,
            button: MouseButton::None,
            column: 5,
            row: 4,
            modifiers: Modifiers {
                control: true,
                ..Modifiers::default()
            },
        },
        14,
        "1b5b3c35313b363b354d",
    )?;
    pointer_event(
        client,
        program,
        pane,
        MouseInput {
            action: MouseAction::Move,
            button: MouseButton::Right,
            column: 6,
            row: 5,
            modifiers: Modifiers {
                alt: true,
                ..Modifiers::default()
            },
        },
        15,
        "1b5b3c34323b373b364d",
    )?;
    Ok(())
}

fn check_mouse_blur_and_disable(
    client: &mut WorkspaceClient,
    program: &WheelProgram,
    pane: rex_rs::core::PaneId,
    mouse: MouseInput,
) -> Result<()> {
    // Blur releases all accepted buttons even if their UI release was lost.
    pointer_event(client, program, pane, mouse, 16, "1b5b3c303b343b334d")?;
    pointer_event(
        client,
        program,
        pane,
        MouseInput {
            button: MouseButton::Right,
            ..mouse
        },
        17,
        "1b5b3c323b343b334d",
    )?;
    client.focus(pane, false)?;
    wait_cache(client, |client| {
        cached_text(client, &program.block, "EVENT_18_1b5b3c303b343b336d")
            && cached_text(client, &program.block, "EVENT_19_1b5b3c323b343b336d")
    })?;
    // No text/cursor changes: mode publication must not depend on a new frame.
    client.write_input(pane, b"MODEONLYOFF\n")?;
    wait_cache(client, |client| !client.mouse_reporting(pane))?;
    client.mouse(pane, mouse)?;
    client.mouse(
        pane,
        MouseInput {
            action: MouseAction::Move,
            ..mouse
        },
    )?;
    client.mouse(
        pane,
        MouseInput {
            action: MouseAction::Release,
            ..mouse
        },
    )?;
    pointer_barrier(client, program, pane, "DISABLED", 19)
}

fn check_titles_and_wheel(
    control: &mut ControlConnection,
    endpoint: &str,
    session: &str,
) -> Result<()> {
    let program = WheelProgram::start(control, endpoint, session)?;
    let mut client = WorkspaceClient::connect(endpoint.to_owned());
    wait_cache(&mut client, |client| {
        client.connected() && cached_text(client, &program.block, "PRIMARY_READY")
    })?;
    let pane = client
        .state()
        .pane_ids()
        .into_iter()
        .find(|pane| {
            client
                .state()
                .pane(*pane)
                .is_some_and(|block| block.remote_id == program.block)
        })
        .context("cached wheel pane")?;
    wait_cache(&mut client, |client| {
        client.pane_label(pane) == "Rex native initial title"
            && cached_window_title(client, &program.window) == Some("Rex native initial title")
    })?;
    check_primary_wheel(control, &mut client, &program, pane)?;
    client.scroll(pane, -1000)?;
    control.call(
        "session.set_window_label",
        json!({"session_id":session,"window_id":program.window,"label":"Explicit native tab"}),
    )?;
    program.command(control, "TITLE")?;
    wait_cache(&mut client, |client| {
        client.pane_label(pane) == "Rex native updated title"
            && cached_window_title(client, &program.window) == Some("Explicit native tab")
    })?;
    check_application_wheel(control, &mut client, &program, pane)?;
    control.call(
        "session.close_window",
        json!({"session_id":session,"window_id":program.window}),
    )?;
    // The data close can race the ordered view event. It is pane removal,
    // not a failed terminal that should request a replacement snapshot.
    wait_cache(&mut client, |client| client.state().pane(pane).is_none())?;
    drop(client);
    fs::remove_file(program.script)?;
    Ok(())
}

fn cached_window_title<'a>(client: &'a WorkspaceClient, window: &str) -> Option<&'a str> {
    client
        .state()
        .sessions()
        .iter()
        .flat_map(rex_rs::rex::workspace::SessionView::windows)
        .find(|entry| entry.remote_id() == window)
        .map(rex_rs::rex::workspace::WindowView::title)
}

fn check_primary_wheel(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    program: &WheelProgram,
    pane: rex_rs::core::PaneId,
) -> Result<()> {
    let server_before = terminal_call(
        control,
        &program.session,
        &program.block,
        "com.superlogical.terminal.format",
        json!({"format":"text"}),
    )?;
    let before = client
        .frame(pane)
        .context("primary wheel baseline frame")?
        .text_lines();
    client.wheel(pane, 5, 3, 2, Modifiers::default())?;
    wait_cache(client, |client| {
        client
            .frame(pane)
            .is_some_and(|frame| frame.text_lines() != before)
    })?;
    let server_after = terminal_call(
        control,
        &program.session,
        &program.block,
        "com.superlogical.terminal.format",
        json!({"format":"text"}),
    )?;
    ensure!(
        server_before["content"] == server_after["content"],
        "local history wheel mutated authoritative terminal text"
    );
    Ok(())
}

fn check_application_wheel(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    program: &WheelProgram,
    pane: rex_rs::core::PaneId,
) -> Result<()> {
    program.command(control, "ALT")?;
    wait_cache(client, |client| {
        cached_text(client, &program.block, "ALT_READY")
    })?;
    client.wheel(pane, 1, 3, 2, Modifiers::default())?;
    // With SGR mouse reporting, zero-based cell (3,2) becomes (4,3).
    wait_cache(client, |client| {
        cached_text(client, &program.block, "WHEEL_1_1b5b3c36343b343b334d")
    })?;
    client.wheel(
        pane,
        1,
        3,
        2,
        Modifiers {
            shift: true,
            ..Modifiers::default()
        },
    )?;
    // A report on the same ordered worker queue is a barrier after the wheel.
    client.write_input(pane, b"REPORT\n")?;
    wait_cache(client, |client| {
        cached_text(client, &program.block, "WHEEL_COUNT_1")
    })?;
    ensure!(
        !cached_text(client, &program.block, "WHEEL_COUNT_2"),
        "Shift wheel reached the terminal application"
    );
    Ok(())
}

struct NativeActionFixture {
    session: String,
    window: String,
    block: String,
    foreign_session: String,
    foreign_window: String,
    foreign_block: String,
}

impl NativeActionFixture {
    fn start(control: &mut ControlConnection) -> Result<Self> {
        let create = |control: &mut ControlConnection, label| {
            control.call("session.create", json!({"label":label,"initial_windows":[{
                "window_label":"Original", "layout":{"block":{"flavor":"com.superlogical.terminal.shell"}}
            }]}))
        };
        let own = create(control, "Native action fixture")?;
        let foreign = create(control, "Foreign action context fixture")?;
        let id = |value: &Value, field| -> Result<String> {
            value[field]
                .as_str()
                .map(str::to_owned)
                .context("action fixture ID")
        };
        Ok(Self {
            session: id(&own, "session_id")?,
            window: id(&own["initial_windows"][0], "window_id")?,
            block: own["initial_windows"][0]["block_ids"][0]
                .as_str()
                .context("action block")?
                .into(),
            foreign_session: id(&foreign, "session_id")?,
            foreign_window: id(&foreign["initial_windows"][0], "window_id")?,
            foreign_block: foreign["initial_windows"][0]["block_ids"][0]
                .as_str()
                .context("foreign block")?
                .into(),
        })
    }

    fn context(&self, block: Option<&str>) -> ActionContext {
        ActionContext {
            session_id: Some(self.session.clone()),
            block_id: block.map(str::to_owned),
        }
    }
}

fn check_client_actions(control: &mut ControlConnection, endpoint: &str) -> Result<()> {
    let fixture = NativeActionFixture::start(control)?;
    let mut client = WorkspaceClient::connect(endpoint.to_owned());
    wait_cache(&mut client, |client| {
        client.connected()
            && client.keymap().is_object()
            && client
                .state()
                .sessions()
                .iter()
                .any(|s| s.remote_id() == fixture.foreign_session)
            && client.state().pane_ids().into_iter().any(|pane| {
                client
                    .state()
                    .pane(pane)
                    .is_some_and(|block| block.remote_id == fixture.foreign_block)
            })
    })?;
    check_action_registration(control, &client)?;
    native_action(
        &mut client,
        NativeAction::SelectSession {
            session_id: fixture.session.clone(),
            window_id: None,
        },
        ActionContext::default(),
    )?;
    check_action_tabs(control, &mut client, &fixture)?;
    check_action_panes(control, &mut client, &fixture)?;
    check_action_refusals(control, &mut client, &fixture)?;
    check_action_callbacks(control, &mut client, &fixture)?;
    check_action_cli(&mut client, endpoint, &fixture)?;
    check_client_scripts(control, &mut client, &fixture)?;
    check_server_action_result(control, &mut client, &fixture)?;
    check_self_target_action(endpoint)?;
    drop(client);
    for session in [&fixture.session, &fixture.foreign_session] {
        control.call("session.destroy", json!({"session_id":session}))?;
    }
    Ok(())
}

fn check_action_registration(
    control: &mut ControlConnection,
    client: &WorkspaceClient,
) -> Result<()> {
    let inspected = control.call(
        "client.inspect",
        json!({"client_id":client.state().client_id()}),
    )?;
    let offered = inspected["actions"]
        .as_array()
        .context("registered client actions")?;
    let expected = [
        "session.select",
        "session.new",
        "session.close",
        "client.tab.new",
        "client.tab.close",
        "client.tab.next",
        "client.tab.previous",
        "client.tab.goto",
        "pane.split",
        "pane.close",
        "pane.focus",
        "pane.zoom",
        "client.font.increase",
        "client.font.decrease",
        "client.font.reset",
        "client.sidebar.toggle",
        "client.copy",
        "client.paste",
        "client.config.reload",
        "client.mode.enter",
        "client.mode.exit",
        "client.mode.exit_all",
        "pane.next",
        "session.rename",
        "client.tab.rename",
        "pane.move",
        "pane.swap",
        "client.tab.move",
        "pane.arrange",
        "pane.detach",
        "pane.restore",
        "client.detached.toggle",
        "client.find.toggle",
        "client.find.next",
        "client.find.previous",
        "client.palette.toggle",
        "client.quit",
    ];
    ensure!(
        offered.len() == expected.len(),
        "unexpected native action count"
    );
    for name in expected {
        let action = offered
            .iter()
            .find(|action| action["name"] == name)
            .with_context(|| format!("missing registered action {name}"))?;
        ensure!(
            action["kind"] == "client"
                && action["args"]["type"] == "object"
                && action["args"]["additionalProperties"] == false,
            "native action lacks a strict argument schema: {name}"
        );
    }
    Ok(())
}

fn wait_action(
    client: &mut WorkspaceClient,
    future: impl Future<Output = Result<Value>>,
) -> Result<Value> {
    let mut future = Box::pin(future);
    let mut context = TaskContext::from_waker(Waker::noop());
    let deadline = Instant::now() + TIMEOUT;
    let mut last_error = None;
    loop {
        if let Err(error) = client.poll() {
            last_error = Some(error.to_string());
        }
        if let Poll::Ready(result) = future.as_mut().poll(&mut context) {
            // Completion is sent after authoritative cache publication.
            let _ = client.poll();
            return result;
        }
        ensure!(
            Instant::now() < deadline,
            "native action timed out: {last_error:?}"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn native_action(
    client: &mut WorkspaceClient,
    action: NativeAction,
    context: ActionContext,
) -> Result<Value> {
    let future = client.perform_native_action(action, context)?;
    wait_action(client, future)
}

fn action_view(control: &mut ControlConnection, fixture: &NativeActionFixture) -> Result<Value> {
    control.call("session.view", json!({"session_id":fixture.session}))
}

fn action_window<'a>(view: &'a Value, window: &str) -> Result<&'a Value> {
    view["windows"]
        .as_array()
        .context("action windows")?
        .iter()
        .find(|entry| entry["window_id"] == window)
        .context("action window missing")
}

fn check_action_tabs(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    fixture: &NativeActionFixture,
) -> Result<()> {
    let created = native_action(client, NativeAction::NewWindow, fixture.context(None))?;
    let second = created["window_id"]
        .as_str()
        .context("action new tab ID")?
        .to_owned();
    let view = action_view(control, fixture)?;
    ensure!(
        view["windows"].as_array().context("created tabs")?.len() == 2
            && view["active_window_id"] == second,
        "new-tab action did not focus its result"
    );
    native_action(
        client,
        NativeAction::GoToWindow { index: -2 },
        fixture.context(None),
    )?;
    ensure!(
        action_view(control, fixture)?["active_window_id"] == fixture.window,
        "negative tab index did not select from the end"
    );
    native_action(client, NativeAction::NextWindow, fixture.context(None))?;
    ensure!(
        action_view(control, fixture)?["active_window_id"] == second,
        "next tab action failed"
    );
    native_action(client, NativeAction::PreviousWindow, fixture.context(None))?;
    ensure!(
        action_view(control, fixture)?["active_window_id"] == fixture.window,
        "previous tab action failed"
    );
    native_action(
        client,
        NativeAction::GoToWindow { index: -1 },
        fixture.context(None),
    )?;
    let before = action_view(control, fixture)?;
    for index in [0, -3, i32::MIN, 3] {
        ensure!(
            native_action(
                client,
                NativeAction::GoToWindow { index },
                fixture.context(None)
            )
            .is_err(),
            "out-of-range tab index was accepted: {index}"
        );
        ensure!(
            action_view(control, fixture)? == before,
            "invalid index changed authoritative layout"
        );
    }
    let second_block = created["block_ids"][0]
        .as_str()
        .context("second tab block")?;
    native_action(
        client,
        NativeAction::CloseWindow,
        fixture.context(Some(second_block)),
    )?;
    let view = action_view(control, fixture)?;
    ensure!(
        view["windows"].as_array().context("remaining tabs")?.len() == 1
            && view["active_window_id"] == fixture.window,
        "close-tab action targeted the wrong tab"
    );
    Ok(())
}

fn check_action_panes(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    fixture: &NativeActionFixture,
) -> Result<()> {
    let process = terminal_call(
        control,
        &fixture.session,
        &fixture.block,
        "com.superlogical.terminal.process",
        json!({}),
    )?;
    let pid = process["child"]["pid"]
        .as_u64()
        .context("action original shell PID")?;
    native_action(
        client,
        NativeAction::Split {
            direction: rex_rs::core::Direction::Left,
        },
        fixture.context(Some(&fixture.block)),
    )?;
    let view = action_view(control, fixture)?;
    let layout = &action_window(&view, &fixture.window)?["layers"][0]["layout"];
    let left = layout["split"]["before"]["block_id"]
        .as_str()
        .context("new left block")?
        .to_owned();
    let split = layout["split"]["split_id"].clone();
    ensure!(
        layout["split"]["direction"] == "horizontal"
            && layout["split"]["after"]["block_id"] == fixture.block
            && left != fixture.block,
        "left-split action did not place the new block before its anchor"
    );
    native_action(
        client,
        NativeAction::Split {
            direction: rex_rs::core::Direction::Up,
        },
        fixture.context(Some(&fixture.block)),
    )?;
    let view = action_view(control, fixture)?;
    let layout = &action_window(&view, &fixture.window)?["layers"][0]["layout"];
    let upper = &layout["split"]["after"]["split"];
    let above = upper["before"]["block_id"]
        .as_str()
        .context("new upper block")?
        .to_owned();
    ensure!(
        layout["split"]["split_id"] == split
            && upper["direction"] == "vertical"
            && upper["after"]["block_id"] == fixture.block,
        "above-split action changed the old split or placed the anchor incorrectly"
    );
    check_action_pane_navigation(control, client, fixture, &left, &above)?;
    let process = terminal_call(
        control,
        &fixture.session,
        &fixture.block,
        "com.superlogical.terminal.process",
        json!({}),
    )?;
    ensure!(
        process["child"]["pid"].as_u64() == Some(pid),
        "native actions replaced the original shell"
    );
    Ok(())
}

fn check_action_pane_navigation(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    fixture: &NativeActionFixture,
    left: &str,
    above: &str,
) -> Result<()> {
    native_action(
        client,
        NativeAction::FocusPane {
            direction: rex_rs::core::Direction::Left,
        },
        fixture.context(Some(&fixture.block)),
    )?;
    ensure!(
        action_window(&action_view(control, fixture)?, &fixture.window)?["focused_block_id"]
            == left,
        "directional focus did not use its explicit anchor"
    );
    native_action(client, NativeAction::ZoomPane, fixture.context(Some(above)))?;
    ensure!(
        action_window(&action_view(control, fixture)?, &fixture.window)?["zoomed_block_id"]
            == above,
        "zoom action did not target the requested block"
    );
    native_action(client, NativeAction::ZoomPane, fixture.context(Some(above)))?;
    native_action(
        client,
        NativeAction::ClosePane,
        fixture.context(Some(above)),
    )?;
    let view = action_view(control, fixture)?;
    let window = action_window(&view, &fixture.window)?;
    ensure!(
        window["zoomed_block_id"].is_null()
            && window["layers"][0]["blocks"]
                .as_array()
                .context("surviving action blocks")?
                .len()
                == 2,
        "close action failed to remove exactly the requested split"
    );
    Ok(())
}

fn check_action_refusals(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    fixture: &NativeActionFixture,
) -> Result<()> {
    let before = action_view(control, fixture)?;
    let foreign_before = control.call(
        "session.view",
        json!({"session_id":fixture.foreign_session}),
    )?;
    let selection = client
        .state()
        .active_session()
        .context("selected action session")?
        .remote_id()
        .to_owned();
    let invalid = ActionContext {
        session_id: Some(fixture.session.clone()),
        block_id: Some(fixture.foreign_block.clone()),
    };
    ensure!(
        native_action(client, NativeAction::ClosePane, invalid).is_err(),
        "cross-session block was accepted"
    );
    ensure!(
        native_action(
            client,
            NativeAction::SelectSession {
                session_id: fixture.session.clone(),
                window_id: Some(fixture.foreign_window.clone()),
            },
            ActionContext::default()
        )
        .is_err(),
        "foreign tab was accepted by session.select"
    );
    let absent = ActionContext {
        session_id: Some("session:00000000-0000-7000-8000-000000000001".into()),
        block_id: None,
    };
    ensure!(
        native_action(client, NativeAction::NewSession, absent).is_err(),
        "missing context session created a new session"
    );
    ensure!(
        action_view(control, fixture)? == before
            && control.call(
                "session.view",
                json!({"session_id":fixture.foreign_session})
            )? == foreign_before,
        "invalid native action mutated a session"
    );
    ensure!(
        client
            .state()
            .active_session()
            .context("retained session")?
            .remote_id()
            == selection,
        "invalid native action changed local selection"
    );
    Ok(())
}

fn dispatch_headless_actions(
    client: &mut WorkspaceClient,
    seen: &mut Vec<(String, String)>,
) -> Result<()> {
    for request in client.take_client_actions() {
        dispatch_headless_request(client, request, seen)?;
    }
    Ok(())
}

fn dispatch_headless_request(
    client: &mut WorkspaceClient,
    request: ClientActionRequest,
    seen: &mut Vec<(String, String)>,
) -> Result<()> {
    ensure!(
        seen.iter().all(|(id, _)| id != &request.call_id),
        "client callback delivered twice"
    );
    seen.push((request.call_id.clone(), request.action.clone()));
    let outcome = if !client.action_request_current(&request) {
        Err(ActionError::refused("stale action connection"))
    } else if let Err(error) = automation::validate_context(client.state(), &request.context) {
        Err(ActionError::refused(error.to_string()))
    } else {
        match actions::parse(&request.action, &request.args) {
            Err(error) => Err(ActionError::unsupported(error.to_string())),
            Ok(action) => match client.perform_native_action(action, request.context.clone()) {
                Err(error) => Err(ActionError::internal(error.to_string())),
                Ok(future) => wait_action(client, future)
                    .map_err(|error| ActionError::internal(error.to_string())),
            },
        }
    };
    let reply = request.reply(outcome)?;
    wait_action(client, reply)?;
    Ok(())
}

fn wait_headless_request(client: &mut WorkspaceClient) -> Result<ClientActionRequest> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        client.poll()?;
        let mut requests = client.take_client_actions();
        if !requests.is_empty() {
            ensure!(
                requests.len() == 1,
                "unexpected callback before FIFO admission barrier"
            );
            return requests.pop().context("admitted callback disappeared");
        }
        ensure!(
            Instant::now() < deadline,
            "first remote callback was not admitted"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn pump_remote_action(
    client: &mut WorkspaceClient,
    pending: PendingCall,
    seen: &mut Vec<(String, String)>,
) -> Result<Value> {
    let mut pending = Box::pin(pending);
    let mut context = TaskContext::from_waker(Waker::noop());
    let deadline = Instant::now() + TIMEOUT;
    loop {
        client.poll()?;
        if let Poll::Ready(result) = pending.as_mut().poll(&mut context) {
            return result;
        }
        dispatch_headless_actions(client, seen)?;
        ensure!(
            Instant::now() < deadline,
            "remote client action did not complete"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn check_action_callbacks(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    fixture: &NativeActionFixture,
) -> Result<()> {
    let handle = control.handle();
    let target = client.state().client_id().to_owned();
    let first = handle.request(
        "client.perform",
        json!({"client_id":target,"action":"client.tab.new","session_id":fixture.session}),
    )?;
    // client.perform is not in the server's ordered RPC lane. Observe admission
    // before submitting the next call, then leave both calls outstanding while
    // testing the target client's FIFO callback dispatch.
    let admitted = wait_headless_request(client)?;
    ensure!(
        admitted.action == "client.tab.new",
        "wrong first admitted callback"
    );
    let second = handle.request("client.perform", json!({"client_id":target,"action":"client.tab.goto","args":{"index":1},"session_id":fixture.session}))?;
    let mut seen = Vec::new();
    dispatch_headless_request(client, admitted, &mut seen)?;
    let created = pump_remote_action(client, first, &mut seen)?;
    let selected = pump_remote_action(client, second, &mut seen)?;
    ensure!(
        created["performed_by"] == target && selected["performed_by"] == target,
        "callback response named another client"
    );
    ensure!(
        seen.len() == 2 && seen[0].1 == "client.tab.new" && seen[1].1 == "client.tab.goto",
        "native client callbacks did not execute FIFO exactly once"
    );
    let view = action_view(control, fixture)?;
    ensure!(
        view["active_window_id"] == fixture.window
            && view["windows"]
                .as_array()
                .context("callback-created windows")?
                .iter()
                .any(|window| window["window_id"] == created["result"]["window_id"]),
        "callback returned before its authoritative mutation completed"
    );
    let refused = handle.request(
        "client.perform",
        json!({"client_id":target,"action":"pane.close",
        "session_id":fixture.session,"block_id":fixture.foreign_block}),
    )?;
    let error = pump_remote_action(client, refused, &mut seen)
        .err()
        .context("invalid callback context was accepted")?;
    ensure!(
        error.to_string().contains("refused"),
        "invalid context returned the wrong error: {error:#}"
    );
    ensure!(
        action_view(control, fixture)? == view,
        "refused callback changed the session"
    );
    let unsupported = handle.request(
        "client.perform",
        json!({"client_id":target,"action":"fixture.undeclared"}),
    )?;
    let error = pump_remote_action(client, unsupported, &mut seen)
        .err()
        .context("undeclared action was accepted")?;
    ensure!(
        error.to_string().contains("not_supported") && seen.len() == 3,
        "unknown action reached the dispatcher or lost its error code"
    );
    Ok(())
}

fn check_action_cli(
    client: &mut WorkspaceClient,
    endpoint: &str,
    fixture: &NativeActionFixture,
) -> Result<()> {
    let mut process = ProcessCommand::new("rex")
        .args([
            "--autostart=false",
            "--server",
            endpoint,
            "-C",
            client.state().client_id(),
            "do",
            "session.select",
            "--args",
            &json!({"session_id":fixture.foreign_session}).to_string(),
            "--json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start real Rex action CLI")?;
    let mut seen = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        client.poll()?;
        dispatch_headless_actions(client, &mut seen)?;
        if process.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = process.kill();
            let _ = process.wait();
            anyhow::bail!("real Rex action CLI timed out");
        }
        thread::sleep(Duration::from_millis(2));
    }
    let output = process.wait_with_output()?;
    ensure!(
        output.status.success(),
        "real Rex action CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        seen.len() == 1
            && seen[0].1 == "session.select"
            && client
                .state()
                .active_session()
                .context("CLI-selected session")?
                .remote_id()
                == fixture.foreign_session,
        "real Rex CLI did not select the target client session exactly once"
    );
    Ok(())
}

fn check_self_target_action(endpoint: &str) -> Result<()> {
    let mut control = ControlConnection::connect(endpoint)?;
    control.call(
        "session.bootstrap",
        json!({"event_scope":"session","attach_session_ids":[]}),
    )?;
    control.call(
        "client.keymap",
        json!({"actions":[{"name":"fixture.status","kind":"client",
        "args":{"type":"object","additionalProperties":false}}]}),
    )?;
    let handle = control.handle();
    let (finished, completion) = mpsc::sync_channel(1);
    control.set_callback_handler(Arc::new(move |event| {
        let result = (|| -> Result<()> {
            let payload = &event["payload"]["payload"];
            let status = handle.request("server.status", json!({}))?.wait()?;
            handle
                .request(
                    "client.reply",
                    json!({"call_id":payload["call_id"],"result":{
                "call_id":payload["call_id"],"instance_id":status["instance_id"]}}),
                )?
                .wait()?;
            Ok(())
        })();
        let _ = finished.send(result);
    }));
    let result = control.call(
        "client.perform",
        json!({"client_id":control.client_id(),
        "action":"fixture.status","args":{},"timeout_ms":3000}),
    )?;
    completion.recv_timeout(TIMEOUT)??;
    ensure!(
        result["performed_by"] == control.client_id()
            && result["result"]["instance_id"].is_string(),
        "self-target action could not complete a nested backend request"
    );
    let duplicate = control.call(
        "client.reply",
        json!({"call_id":result["result"]["call_id"],"result":null}),
    );
    ensure!(
        duplicate
            .err()
            .is_some_and(|error| format!("{error:#}").contains("not_found")),
        "server accepted a second reply for a completed callback"
    );
    Ok(())
}

const CLIENT_SCRIPT_FIXTURE: &str = r#"local count = 0
rex.bind("ctrl+g", function(ctx, ev)
  count = count + 1
  rex.session.set_label{ label = "Client script " .. count }
  return true
end)
rex.bind("ctrl+b>s", function(ctx, ev)
  rex.session.set_label{ label = "Client sequence " .. table.concat(ev.prefix, ",") .. " " .. ev.keybinding }
  return true
end)
"#;

fn load_client_script(client: &mut WorkspaceClient, source: &str) -> Result<Value> {
    let future = client.set_scripts(vec![ClientScript {
        name: "native-client-fixture.lua".into(),
        source: source.into(),
    }])?;
    wait_action(client, future)
}

fn script_key(name: &str, control: bool) -> KeyInput {
    KeyInput {
        physical_key: None,
        key: name.into(),
        text: Some(name.into()),
        modifiers: Modifiers {
            control,
            ..Modifiers::default()
        },
        repeat: false,
    }
}

fn cached_action_label(
    client: &WorkspaceClient,
    fixture: &NativeActionFixture,
    label: &str,
) -> bool {
    client
        .state()
        .sessions()
        .iter()
        .any(|session| session.remote_id() == fixture.session && session.label() == label)
}

fn binding_present(client: &WorkspaceClient, keys: &[&str]) -> bool {
    client.keymap()["bindings"]
        .as_array()
        .is_some_and(|bindings| {
            bindings.iter().any(|binding| {
                binding["keys"] == json!(keys) && binding["action"] == "client.key.forward"
            })
        })
}

fn check_client_scripts(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    fixture: &NativeActionFixture,
) -> Result<()> {
    let pane = client
        .state()
        .pane_ids()
        .into_iter()
        .find(|pane| {
            client
                .state()
                .pane(*pane)
                .is_some_and(|block| block.remote_id == fixture.block)
        })
        .context("script fixture pane")?;
    wait_cache(client, |client| client.input_available(pane))?;
    let hash = client.keymap()["config"]["hash"].clone();
    let loaded = load_client_script(client, CLIENT_SCRIPT_FIXTURE)?;
    ensure!(
        loaded["bindings"] == json!(["ctrl+g"]),
        "script result incorrectly includes sequences"
    );
    ensure!(
        binding_present(client, &["ctrl+g"]) && binding_present(client, &["ctrl+b", "s"]),
        "successful script load did not immediately refresh the cached keymap"
    );
    ensure!(
        client.keymap()["config"]["hash"] == hash,
        "client script altered the account configuration hash"
    );
    let before = terminal_call(
        control,
        &fixture.session,
        &fixture.block,
        "com.superlogical.terminal.format",
        json!({"format":"text"}),
    )?;
    client.send_key(pane, &script_key("g", true))?;
    wait_cache(client, |client| {
        cached_action_label(client, fixture, "Client script 1")
    })?;
    check_client_script_failures(client, fixture, pane)?;
    client.send_key_sequence(pane, &script_key("s", false), vec!["ctrl+b".into()])?;
    wait_cache(client, |client| {
        cached_action_label(client, fixture, "Client sequence ctrl+b s")
    })?;
    let after = terminal_call(
        control,
        &fixture.session,
        &fixture.block,
        "com.superlogical.terminal.format",
        json!({"format":"text"}),
    )?;
    ensure!(
        before["content"] == after["content"],
        "handled client script keys or sequence prefixes leaked into the shell"
    );
    check_client_script_diagnostics(client, fixture, pane)?;
    let future = client.set_scripts(Vec::new())?;
    wait_action(client, future)?;
    ensure!(
        !binding_present(client, &["ctrl+g"])
            && !binding_present(client, &["ctrl+b", "s"])
            && !binding_present(client, &["ctrl+k"]),
        "empty script load did not unload client bindings"
    );
    Ok(())
}

fn check_client_script_failures(
    client: &mut WorkspaceClient,
    fixture: &NativeActionFixture,
    pane: rex_rs::core::PaneId,
) -> Result<()> {
    let bindings = client.keymap()["bindings"].clone();
    for (source, label) in [
        ("rex.bind(", "Client script 2"),
        (
            "error(\"intentional native fixture load failure\")",
            "Client script 3",
        ),
    ] {
        ensure!(
            load_client_script(client, source).is_err(),
            "invalid client script was accepted"
        );
        ensure!(
            client.keymap()["bindings"] == bindings,
            "failed script load replaced the working keymap"
        );
        client.send_key(pane, &script_key("g", true))?;
        wait_cache(client, |client| cached_action_label(client, fixture, label))?;
    }
    Ok(())
}

fn check_client_script_diagnostics(
    client: &mut WorkspaceClient,
    fixture: &NativeActionFixture,
    pane: rex_rs::core::PaneId,
) -> Result<()> {
    let loaded = load_client_script(
        client,
        r#"rex.bind("ctrl+hoem", "pane.zoom")
rex.bind("ctrl+k", function(ctx, ev)
  rex.session.set_label{ label = "Client diagnostic binding" }
  return true
end)
"#,
    )?;
    ensure!(
        loaded["diagnostics"]
            .as_array()
            .is_some_and(|diagnostics| diagnostics
                .iter()
                .any(|diagnostic| diagnostic["severity"] == "error")),
        "script fixture did not report its invalid key diagnostic"
    );
    ensure!(
        binding_present(client, &["ctrl+k"]) && !binding_present(client, &["ctrl+g"]),
        "successful script load with diagnostics did not replace the prior script"
    );
    client.send_key(pane, &script_key("k", true))?;
    wait_cache(client, |client| {
        cached_action_label(client, fixture, "Client diagnostic binding")
    })?;
    Ok(())
}

fn check_server_action_result(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    fixture: &NativeActionFixture,
) -> Result<()> {
    ensure!(
        client.keymap()["actions"]
            .as_array()
            .is_some_and(|actions| actions
                .iter()
                .any(|action| action["name"] == "smoke_queue_pair" && action["kind"] == "lua")),
        "configured Lua action is absent from the client keymap"
    );
    let future = client.perform_server_action(
        "smoke_queue_pair".into(),
        json!({}),
        fixture.context(None),
    )?;
    let result = wait_action(client, future)?;
    ensure!(
        result["result"]["applied"] == true
            && result["client_actions"]
                == json!([
                    {"action":"client.font.increase"}, {"action":"client.sidebar.toggle"}
                ]),
        "server action lost or reordered its queued client actions"
    );
    ensure!(
        action_view(control, fixture)?["label"] == "Server action applied"
            && cached_action_label(client, fixture, "Server action applied"),
        "server action completed before its authoritative state reached the cache"
    );
    ensure!(
        client.take_client_actions().is_empty(),
        "returned client_actions were also published as callbacks and would execute twice"
    );
    Ok(())
}

fn wait_cache(
    client: &mut WorkspaceClient,
    ready: impl Fn(&WorkspaceClient) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        client.poll()?;
        if ready(client) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "cached Rex client did not publish expected state"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
