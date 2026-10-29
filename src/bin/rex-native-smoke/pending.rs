//! A callback is a deterministic barrier while the ordered Lua lane is pending.
use super::{cached_text, native_action, wait_action, wait_cache, wait_headless_request};
use anyhow::{Context, Result, ensure};
use rex_rs::core::interaction::SelectionCommand;
use rex_rs::{
    core::Command,
    rex::{
        WorkspaceClient, actions::NativeAction, automation::ActionContext,
        transport::ControlConnection,
    },
};
use serde_json::json;
use std::{
    future::Future,
    process::Command as ProcessCommand,
    task::{Context as TaskContext, Waker},
};

const PROGRAM: &str = "import os,tty; tty.setraw(0); os.write(1,b'COPY_PENDING'); os.write(1,b'\\r\\n'+os.read(0,1024)); os.read(0,1)";

pub(super) fn check(control: &mut ControlConnection, endpoint: &str) -> Result<()> {
    let python = ProcessCommand::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()?;
    ensure!(python.status.success(), "pending fixture requires Python");
    let python = String::from_utf8(python.stdout)?;
    let created = control.call(
        "session.create",
        json!({"label":"Pending worker fixture", "initial_windows":[{
            "layout":{"block":{"flavor":"com.superlogical.terminal.shell","options":{
                "shell":"none","command":[python.trim(),"-u","-c",PROGRAM],
                "size":{"columns":80,"rows":24},"exit":{"on_completion":false}
            }}}
        }]}),
    )?;
    let session = created["session_id"].as_str().context("pending session")?;
    let block = created["initial_windows"][0]["block_ids"][0]
        .as_str()
        .context("pending block")?;
    let result = exercise(control, endpoint, session, block);
    let cleanup = control.call("session.destroy", json!({"session_id":session}));
    result?;
    cleanup?;
    Ok(())
}

fn exercise(
    control: &mut ControlConnection,
    endpoint: &str,
    session: &str,
    block: &str,
) -> Result<()> {
    let mut client = WorkspaceClient::connect(endpoint.to_owned());
    wait_cache(&mut client, |client| {
        client.connected()
            && client
                .state()
                .sessions()
                .iter()
                .any(|s| s.remote_id() == session)
    })?;
    native_action(
        &mut client,
        NativeAction::SelectSession {
            session_id: session.into(),
            window_id: None,
        },
        ActionContext::default(),
    )?;
    wait_cache(&mut client, |client| {
        cached_text(client, block, "COPY_PENDING")
    })?;
    let pane = client
        .state()
        .pane_ids()
        .into_iter()
        .find(|pane| {
            client
                .state()
                .pane(*pane)
                .is_some_and(|p| p.remote_id == block)
        })
        .context("pending pane")?;
    select_copy(&mut client, pane)?;
    let context = ActionContext {
        session_id: Some(session.into()),
        block_id: Some(block.into()),
    };
    let pending = client.perform_server_action("smoke_nested_copy".into(), json!({}), context)?;
    let request = wait_headless_request(&mut client)?;
    ensure!(
        request.action == "client.copy",
        "nested copy was not admitted"
    );
    let local_session = client
        .state()
        .active_session()
        .context("pending active session")?
        .id();
    let mut queued = Box::pin(client.perform_command(Command::RenameSession {
        session: local_session,
        title: "After pending action".into(),
    })?);
    control.call("block.event",json!({"session_id":session,"block_id":block,"event":{"type":"paste","payload":{"text":"STREAM_DURING_CALLBACK"}}}))?;
    wait_cache(&mut client, |client| {
        cached_text(client, block, "STREAM_DURING_CALLBACK")
    })?;
    let copy = client.copy_callback_selection(pane)?;
    let copied = wait_action(&mut client, copy)?;
    ensure!(
        copied["text"] == "COPY_PENDING",
        "nested copy lost selection"
    );
    ensure!(
        queued
            .as_mut()
            .poll(&mut TaskContext::from_waker(Waker::noop()))
            .is_pending(),
        "ordered command overtook pending action"
    );
    ensure!(
        control.call("session.view", json!({"session_id":session}))?["label"]
            == "Pending worker fixture",
        "pending command mutated server early"
    );
    let reply = request.reply(Ok(copied))?;
    wait_action(&mut client, reply)?;
    let result = wait_action(&mut client, pending)?;
    ensure!(
        result["result"]["result"]["text"] == "COPY_PENDING",
        "nested callback result did not reach Lua: {result}"
    );
    wait_action(&mut client, queued)?;
    ensure!(
        control.call("session.view", json!({"session_id":session}))?["label"]
            == "After pending action",
        "queued command did not follow callback"
    );
    Ok(())
}

fn select_copy(client: &mut WorkspaceClient, pane: rex_rs::core::PaneId) -> Result<()> {
    let token = client
        .annotations(pane)
        .context("pending annotations")?
        .viewport_token;
    client.select(
        pane,
        SelectionCommand::Press {
            column: 0.1,
            row: 0.5,
            time_millis: 1000,
            viewport_token: token,
        },
    )?;
    client.select(
        pane,
        SelectionCommand::Drag {
            column: 12.0,
            row: 0.5,
            rectangle: false,
            viewport_token: token,
        },
    )?;
    client.select(pane, SelectionCommand::Release)?;
    let copied = client.copy_selection(pane)?;
    ensure!(
        wait_action(client, copied)?["text"] == "COPY_PENDING",
        "pending selection baseline"
    );
    Ok(())
}
