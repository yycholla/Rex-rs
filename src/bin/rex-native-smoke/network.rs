//! Exercise the installed server's HTTP listener, including its terminal channel.
use super::{open_terminal, paste, settle_stream, terminal_call, wait_text};
use anyhow::{Context, Result, ensure};
use rex_rs::rex::transport::ControlConnection;
use serde_json::json;

pub(super) fn check(endpoint: &str) -> Result<()> {
    let mut control = ControlConnection::connect(endpoint)?;
    control.call("session.bootstrap", json!({"event_scope":"all"}))?;
    let created = control.call("session.create", json!({
        "label":"HTTP transport fixture", "initial_windows":[{
            "layout":{"block":{"flavor":"com.superlogical.terminal.shell","options":{
                "shell":"none","command":["/bin/sh","-c",
                    "printf 'NETWORK_READY\\n'; while IFS= read -r line; do printf 'NETWORK_%s\\n' \"$line\"; done"]
            }}}
        }]
    }))?;
    let session = created["session_id"].as_str().context("network session")?;
    let block = created["initial_windows"][0]["block_ids"][0]
        .as_str()
        .context("network block")?;
    control.call("session.attach", json!({"session_id":session}))?;
    let result = exercise(control, endpoint, session, block);
    // Independent cleanup also runs when a transport check fails.
    let cleanup = (|| -> Result<()> {
        let mut cleanup = ControlConnection::connect(endpoint)?;
        cleanup.call("session.attach", json!({"session_id":session}))?;
        cleanup.call("session.destroy", json!({"session_id":session}))?;
        Ok(())
    })();
    result?;
    cleanup
}

fn exercise(
    mut control: ControlConnection,
    endpoint: &str,
    session: &str,
    block: &str,
) -> Result<()> {
    let identity = control.client_id().to_owned();
    let process = terminal_call(
        &mut control,
        session,
        block,
        "com.superlogical.terminal.process",
        json!({}),
    )?;
    let pid = process["child"]["pid"]
        .as_u64()
        .context("network shell PID")?;
    let mut stream = open_terminal(&mut control, endpoint, session, block)?;
    wait_text(&mut stream, "NETWORK_READY")?;
    paste(&mut control, session, block, "LIVE_世界")?;
    wait_text(&mut stream, "NETWORK_LIVE_世界")?;
    settle_stream(&mut stream)?;
    stream.resume().context("resume HTTP terminal channel")?;
    let token = control
        .reconnect_token()
        .context("network resume token")?
        .to_owned();
    control.disconnect_for_resume()?;
    drop(control);
    // ws:// and http:// select the same verified endpoint/path semantics.
    let websocket = endpoint.replacen("http://", "ws://", 1);
    let mut control = ControlConnection::connect_with_resume(&websocket, Some(&token))?;
    control.call("session.bootstrap", json!({"event_scope":"all"}))?;
    ensure!(
        control.client_id() == identity,
        "HTTP control resume changed identity"
    );
    paste(&mut control, session, block, "RESUMED")?;
    wait_text(&mut stream, "NETWORK_RESUMED")?;
    let process = terminal_call(
        &mut control,
        session,
        block,
        "com.superlogical.terminal.process",
        json!({}),
    )?;
    ensure!(
        process["child"]["pid"].as_u64() == Some(pid),
        "HTTP reconnect restarted shell"
    );
    Ok(())
}
