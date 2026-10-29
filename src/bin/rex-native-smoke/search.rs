//! Local history interaction against real server snapshots, with no process input.
use super::{ControlProxy, TIMEOUT, terminal_call, wait_cache, wait_connection_state};
use anyhow::{Context, Result, ensure};
use rex_rs::{
    core::{
        PaneId,
        interaction::{SearchCommand, SelectionCommand},
    },
    rex::{
        WorkspaceClient, actions::NativeAction, automation::ActionContext,
        transport::ControlConnection,
    },
    terminal::PaneStatus,
};
use serde_json::{Value, json};
use std::{
    fs,
    future::Future,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context as TaskContext, Poll, Waker},
    thread,
    time::{Duration, Instant},
};

static GESTURE_TIME: AtomicU64 = AtomicU64::new(1000);

const WRAPPED: &str = "123456789012345678世界e\u{301}TAIL";
const PROGRAM: &str = r"import os, sys, tty
tty.setraw(0)
with open(sys.argv[1], 'wb', buffering=0) as received:
    for i in range(100):
        os.write(1, ('HISTORY_NEEDLE_%03d\r\n' % i).encode())
    os.write(1, '\x1b[2J\x1b[H123456789012345678世界e\u0301TAIL\r\nREADY_SEARCH'.encode())
    while True:
        data = os.read(0, 1024)
        received.write(data)
        if b'!' in data:
            os.write(1, b'\r\nEXIT_SEARCH\r\n')
            break
";

struct Fixture {
    session: String,
    block: String,
    script: PathBuf,
    received: PathBuf,
}

impl Fixture {
    fn start(control: &mut ControlConnection, endpoint: &str) -> Result<Self> {
        let socket = Path::new(
            endpoint
                .strip_prefix("unix://")
                .context("search fixture endpoint")?,
        );
        let script = socket.with_file_name("search-program.py");
        let received = socket.with_file_name("search-input.bin");
        fs::write(&script, PROGRAM)?;
        let python = Command::new("python3")
            .args(["-c", "import sys; print(sys.executable)"])
            .output()?;
        ensure!(python.status.success(), "search fixture requires Python 3");
        let python = String::from_utf8(python.stdout)?;
        let created = control.call(
            "session.create",
            json!({"label":"Local history fixture", "initial_windows":[{
                "layout":{"block":{"flavor":"com.superlogical.terminal.shell","options":{
                    "shell":"none","command":[python.trim(),script,received],
                    "size":{"columns":20,"rows":6},"exit":{"on_completion":false}
                }}}
            }]}),
        )?;
        Ok(Self {
            session: created["session_id"]
                .as_str()
                .context("search session")?
                .into(),
            block: created["initial_windows"][0]["block_ids"][0]
                .as_str()
                .context("search block")?
                .into(),
            script,
            received,
        })
    }

    fn pane(&self, client: &WorkspaceClient) -> Option<PaneId> {
        client.state().pane_ids().into_iter().find(|pane| {
            client
                .state()
                .pane(*pane)
                .is_some_and(|block| block.remote_id == self.block)
        })
    }

    fn text(&self, client: &WorkspaceClient, marker: &str) -> bool {
        self.pane(client)
            .and_then(|pane| client.frame(pane))
            .is_some_and(|frame| frame.text_lines().join("\n").contains(marker))
    }

    fn facts(&self, control: &mut ControlConnection) -> Result<Value> {
        let process = terminal_call(
            control,
            &self.session,
            &self.block,
            "com.superlogical.terminal.process",
            json!({}),
        )?;
        let size = terminal_call(
            control,
            &self.session,
            &self.block,
            "com.superlogical.terminal.size",
            json!({}),
        )?;
        Ok(json!({"pid":process["child"]["pid"],"size":size}))
    }
}

fn strict_result(
    client: &mut WorkspaceClient,
    future: impl Future<Output = Result<Value>>,
) -> Result<Value> {
    let mut future = Box::pin(future);
    let mut context = TaskContext::from_waker(Waker::noop());
    let deadline = Instant::now() + TIMEOUT;
    loop {
        client.poll().context("local history worker error")?;
        if let Poll::Ready(result) = future.as_mut().poll(&mut context) {
            client.poll()?;
            return result;
        }
        ensure!(
            Instant::now() < deadline,
            "local history completion timed out"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn query(
    client: &mut WorkspaceClient,
    pane: PaneId,
    text: &str,
    generation: u64,
    total: u64,
) -> Result<()> {
    client.search(
        pane,
        SearchCommand::Set {
            query: text.into(),
            generation,
        },
    )?;
    wait_cache(client, |client| {
        client
            .annotations(pane)
            .and_then(|annotations| annotations.search.as_ref())
            .is_some_and(|search| {
                search.generation == generation && search.complete && search.total == total
            })
    })
}

fn select_wrapped(client: &mut WorkspaceClient, pane: PaneId) -> Result<()> {
    let viewport_token = client
        .annotations(pane)
        .context("selection viewport")?
        .viewport_token;
    client.select(
        pane,
        SelectionCommand::Press {
            column: 0.1,
            row: 0.5,
            time_millis: GESTURE_TIME.fetch_add(1000, Ordering::Relaxed),
            viewport_token,
        },
    )?;
    client.select(
        pane,
        SelectionCommand::Drag {
            column: 7.9,
            row: 1.5,
            rectangle: false,
            viewport_token,
        },
    )?;
    client.select(pane, SelectionCommand::Release)?;
    let future = client.copy_selection(pane)?;
    let copied = strict_result(client, future)?;
    ensure!(
        copied["text"]
            .as_str()
            .is_some_and(|text| text.trim_end() == WRAPPED),
        "native logical softwrap/Unicode copy differed: {copied}"
    );
    Ok(())
}

pub(super) fn check(control: &mut ControlConnection, endpoint: &str) -> Result<()> {
    let fixture = Fixture::start(control, endpoint)?;
    let proxy = ControlProxy::start(endpoint)?;
    let mut client = WorkspaceClient::connect(proxy.endpoint.clone());
    let mut replica = WorkspaceClient::connect(endpoint.to_owned());
    for target in [&mut client, &mut replica] {
        wait_cache(target, |client| fixture.pane(client).is_some())?;
        let future = target.perform_native_action(
            NativeAction::SelectSession {
                session_id: fixture.session.clone(),
                window_id: None,
            },
            ActionContext::default(),
        )?;
        strict_result(target, future)?;
        wait_cache(target, |client| fixture.text(client, "READY_SEARCH"))?;
    }
    let pane = fixture.pane(&client).context("local search pane")?;
    let replica_pane = fixture.pane(&replica).context("replica search pane")?;
    let facts = fixture.facts(control)?;
    ensure!(!facts["pid"].is_null(), "search fixture has no live PID");
    select_wrapped(&mut client, pane)?;
    let future = replica.copy_selection(replica_pane)?;
    ensure!(
        strict_result(&mut replica, future)?["text"].is_null(),
        "selection leaked to another client"
    );
    check_search(&mut client, &mut replica, &fixture, pane, replica_pane)?;
    ensure!(
        fs::read(&fixture.received)?.is_empty(),
        "local search/selection injected raw input"
    );
    ensure!(
        fixture.facts(control)? == facts,
        "local history changed server size, owner or PID"
    );
    check_hidden_reveal(control, &mut client, &fixture, pane)?;
    check_reconnect(&mut client, &proxy, &fixture, pane)?;
    ensure!(
        fs::read(&fixture.received)?.is_empty(),
        "local reconnect interaction injected raw input"
    );
    ensure!(
        fixture.facts(control)? == facts,
        "reconnect history changed server size, owner or PID"
    );
    check_exit_and_retirement(control, &mut client, &fixture, pane)?;
    drop(client);
    drop(replica);
    drop(proxy);
    control.call("session.destroy", json!({"session_id":fixture.session}))?;
    fs::remove_file(fixture.script)?;
    fs::remove_file(fixture.received)?;
    Ok(())
}

fn check_search(
    client: &mut WorkspaceClient,
    replica: &mut WorkspaceClient,
    fixture: &Fixture,
    pane: PaneId,
    replica_pane: PaneId,
) -> Result<()> {
    let frame = Arc::clone(client.frame(pane).context("pre-search frame")?);
    let revision = client
        .annotations(pane)
        .context("pre-search annotations")?
        .revision;
    query(client, pane, "history_needle_000", 1, 1)?;
    ensure!(
        Arc::ptr_eq(&frame, client.frame(pane).context("metadata-only frame")?),
        "setting a search changed the cell frame"
    );
    ensure!(
        client
            .annotations(pane)
            .context("metadata wakeup")?
            .revision
            > revision,
        "search completion did not publish annotations"
    );
    client.search(pane, SearchCommand::Next { generation: 1 })?;
    wait_cache(client, |client| fixture.text(client, "HISTORY_NEEDLE_000"))?;
    replica.poll()?;
    ensure!(
        fixture.text(replica, "READY_SEARCH")
            && replica
                .annotations(replica_pane)
                .is_some_and(|annotations| annotations.search.is_none()),
        "local history viewport or query leaked to replica"
    );
    query(client, pane, "世界e\u{301}", 2, 1)?;
    query(client, pane, "NO_SUCH_HISTORY", 3, 0)?;
    client.search(pane, SearchCommand::Cancel { generation: 4 })?;
    wait_cache(client, |client| {
        client
            .annotations(pane)
            .is_some_and(|annotations| annotations.search.is_none())
            && client.search_query(pane).is_none()
    })?;
    client.scroll(pane, -10_000)?;
    wait_cache(client, |client| fixture.text(client, "READY_SEARCH"))?;
    select_wrapped(client, pane)?;
    query(client, pane, "HISTORY_NEEDLE_000", 5, 1)?;
    Ok(())
}

fn check_hidden_reveal(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    fixture: &Fixture,
    pane: PaneId,
) -> Result<()> {
    let original = client
        .state()
        .active_session()
        .and_then(rex_rs::rex::workspace::SessionView::active_window)
        .context("original search window")?
        .remote_id()
        .to_owned();
    let created = control.call(
        "session.new_window",
        json!({
            "session_id":fixture.session,
            "layout":{"block":{"flavor":"com.superlogical.terminal.shell","options":{
                "shell":"none","command":["/bin/sleep","30"],
                "size":{"columns":20,"rows":6}
            }}}
        }),
    )?;
    let hidden_window = created["window_id"]
        .as_str()
        .context("hidden search window")?;
    // This window was created through another control connection. Wait for its
    // authoritative view before invoking a client action with that target.
    wait_cache(client, |client| {
        client.state().sessions().iter().any(|session| {
            session.remote_id() == fixture.session
                && session
                    .windows()
                    .iter()
                    .any(|window| window.remote_id() == hidden_window)
        })
    })?;
    let future = client.perform_native_action(
        NativeAction::SelectSession {
            session_id: fixture.session.clone(),
            window_id: Some(hidden_window.into()),
        },
        ActionContext::default(),
    )?;
    strict_result(client, future)?;
    wait_cache(client, |client| {
        client
            .annotations(pane)
            .is_some_and(|annotations| annotations.selection.is_empty())
    })?;
    let frame = Arc::clone(client.frame(pane).context("hidden search frame")?);
    client.search(pane, SearchCommand::Next { generation: 5 })?;
    wait_cache(client, |client| {
        client
            .annotations(pane)
            .and_then(|annotations| annotations.search.as_ref())
            .is_some_and(|search| {
                search.complete
                    && search.selected.is_some()
                    && search.matches.is_empty()
                    && search.selected_match.is_empty()
            })
    })?;
    ensure!(
        Arc::ptr_eq(&frame, client.frame(pane).context("retained hidden frame")?),
        "hidden navigation materialized a terminal frame"
    );
    let future = client.perform_native_action(
        NativeAction::SelectSession {
            session_id: fixture.session.clone(),
            window_id: Some(original),
        },
        ActionContext::default(),
    )?;
    strict_result(client, future)?;
    wait_cache(client, |client| {
        fixture.text(client, "HISTORY_NEEDLE_000")
            && client
                .annotations(pane)
                .and_then(|annotations| annotations.search.as_ref())
                .is_some_and(|search| {
                    !search.matches.is_empty() && !search.selected_match.is_empty()
                })
    })?;
    ensure!(
        fs::read(&fixture.received)?.is_empty(),
        "hidden history navigation injected raw input"
    );
    let future = client.copy_selection(pane)?;
    ensure!(
        strict_result(client, future)?["text"]
            .as_str()
            .is_some_and(|text| text.trim_end() == WRAPPED),
        "visibility transition changed native logical selection"
    );
    Ok(())
}

fn check_reconnect(
    client: &mut WorkspaceClient,
    proxy: &ControlProxy,
    fixture: &Fixture,
    pane: PaneId,
) -> Result<()> {
    proxy.disconnect_control()?;
    // Only this deliberate transport break permits the expected connection error.
    wait_connection_state(client, false)?;
    wait_connection_state(client, true)?;
    wait_cache(client, |client| {
        fixture.pane(client) == Some(pane)
            && client
                .annotations(pane)
                .and_then(|annotations| annotations.search.as_ref())
                .is_some_and(|search| {
                    search.generation == 5 && search.complete && search.total == 1
                })
    })?;
    ensure!(
        client.search_query(pane) == Some(("HISTORY_NEEDLE_000", 5)),
        "control reconnect lost owned query intent"
    );
    let future = client.copy_selection(pane)?;
    ensure!(
        strict_result(client, future)?["text"].is_null(),
        "fresh control snapshot retained old native selection"
    );
    Ok(())
}

fn check_exit_and_retirement(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    fixture: &Fixture,
    pane: PaneId,
) -> Result<()> {
    terminal_call(
        control,
        &fixture.session,
        &fixture.block,
        "com.superlogical.terminal.write",
        json!({"data":"!"}),
    )?;
    wait_cache(client, |client| {
        client.status(pane) == Some(&PaneStatus::Exited { code: 0 })
    })?;
    ensure!(
        fs::read(&fixture.received)? == b"!",
        "process received unrequested input before exit"
    );
    query(client, pane, "HISTORY_NEEDLE_000", 6, 1)?;
    client.search(pane, SearchCommand::Next { generation: 6 })?;
    wait_cache(client, |client| fixture.text(client, "HISTORY_NEEDLE_000"))?;
    let future = client.perform_native_action(
        NativeAction::DetachPane,
        ActionContext {
            session_id: Some(fixture.session.clone()),
            block_id: Some(fixture.block.clone()),
        },
    )?;
    strict_result(client, future)?;
    wait_cache(client, |client| {
        client.state().is_detached(pane)
            && client.annotations(pane).is_none()
            && client.search_query(pane).is_none()
            && client.frame(pane).is_none()
    })?;
    control.call(
        "block.close",
        json!({"session_id":fixture.session,"block_id":fixture.block}),
    )?;
    wait_cache(client, |client| {
        client.state().pane(pane).is_none()
            && client.annotations(pane).is_none()
            && client.search_query(pane).is_none()
    })?;
    Ok(())
}
