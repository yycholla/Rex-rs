//! Real named-peer Lua callbacks, with stale and busy route rejection.
use super::{
    ControlProxy, ServerFixture, terminal_call, wait_action, wait_cache, wait_connection_state,
    wait_headless_request,
};
use anyhow::{Context, Result, ensure};
use rex_rs::rex::{
    WorkspaceClient, automation::ActionContext, servers::ServerRoutes, transport::ControlConnection,
};
use serde_json::{Value, json};

fn remote(client: &mut WorkspaceClient, method: &str, payload: Value) -> Result<Value> {
    let mut args = json!({"peer":"target","method":method});
    args["payload"] = payload;
    let action = client.perform_server_action(
        "smoke_cross_server".into(),
        args,
        ActionContext::default(),
    )?;
    Ok(wait_action(client, action)?["result"].clone())
}

pub(super) fn check(endpoint: &str) -> Result<()> {
    let mut target_server = ServerFixture::start()?;
    let proxy = ControlProxy::start(&target_server.endpoint)?;
    let routes = ServerRoutes::default();
    let mut origin = WorkspaceClient::connect_named(endpoint.into(), routes.named("origin")?);
    let mut target =
        WorkspaceClient::connect_named(proxy.endpoint.clone(), routes.named("target")?);
    for client in [&mut origin, &mut target] {
        wait_cache(client, |client| {
            client.connected() && client.keymap().is_object()
        })?;
    }
    let status = remote(&mut origin, "server.status", json!({}))?;
    ensure!(
        status["instance_id"].as_str() == target.server_id(),
        "cross-server read used wrong peer"
    );
    let made = remote(
        &mut origin,
        "session.create",
        json!({
            "label":"Named peer fixture", "initial_windows":[{"layout":{"block":{
                "flavor":"com.superlogical.terminal.shell", "options":{"shell":"none","command":["/bin/sleep","60"]}
            }}}]
        }),
    )?;
    let session = made["session_id"].as_str().context("routed session")?;
    let block = made["initial_windows"][0]["block_ids"][0]
        .as_str()
        .context("routed block")?;
    wait_cache(&mut target, |client| {
        client
            .state()
            .sessions()
            .iter()
            .any(|s| s.remote_id() == session)
    })?;
    let mut control = ControlConnection::connect(&target_server.endpoint)?;
    control.call("session.attach", json!({"session_id":session}))?;
    let before = process(&mut control, session, block)?;
    busy_target(&mut origin, &mut target, &mut control, session, block)?;
    nested_mutation(&mut origin, &mut control, session, block)?;
    stale_target(&mut origin, &mut target, &proxy, &mut control, session)?;
    remote(
        &mut origin,
        "session.set_label",
        json!({"session_id":session,"label":"Fresh route applied"}),
    )?;
    ensure!(
        control.call("session.view", json!({"session_id":session}))?["label"]
            == "Fresh route applied",
        "fresh route did not replace stale route"
    );
    ensure!(
        process(&mut control, session, block)? == before,
        "named routing/reconnect restarted target process"
    );
    remote(
        &mut origin,
        "session.destroy",
        json!({"session_id":session}),
    )?;
    drop(origin);
    drop(target);
    drop(proxy);
    target_server.stop()?;
    Ok(())
}

fn process(control: &mut ControlConnection, session: &str, block: &str) -> Result<u64> {
    terminal_call(
        control,
        session,
        block,
        "com.superlogical.terminal.process",
        json!({}),
    )?["child"]["pid"]
        .as_u64()
        .context("routed process PID")
}

fn busy_target(
    origin: &mut WorkspaceClient,
    target: &mut WorkspaceClient,
    control: &mut ControlConnection,
    session: &str,
    block: &str,
) -> Result<()> {
    let held = target.perform_server_action(
        "smoke_nested_copy".into(),
        json!({}),
        ActionContext {
            session_id: Some(session.into()),
            block_id: Some(block.into()),
        },
    )?;
    let callback = wait_headless_request(target)?;
    let error = remote(
        origin,
        "session.set_label",
        json!({"session_id":session,"label":"Busy route mutated"}),
    )
    .err()
    .context("busy route unexpectedly succeeded")?;
    ensure!(
        format!("{error:#}").contains("busy"),
        "busy route lacked explicit refusal: {error:#}"
    );
    ensure!(
        control.call("session.view", json!({"session_id":session}))?["label"]
            == "Named peer fixture",
        "busy route mutated before refusal"
    );
    let reply = callback.reply(Ok(Value::Null))?;
    wait_action(target, reply)?;
    wait_action(target, held)?;
    ensure!(
        control.call("session.view", json!({"session_id":session}))?["label"]
            == "Named peer fixture",
        "busy route mutated after refusal"
    );
    Ok(())
}

fn stale_target(
    origin: &mut WorkspaceClient,
    target: &mut WorkspaceClient,
    proxy: &ControlProxy,
    control: &mut ControlConnection,
    session: &str,
) -> Result<()> {
    let held = origin.perform_server_action(
        "smoke_cross_after_copy".into(),
        json!({"peer":"target","session_id":session}),
        ActionContext::default(),
    )?;
    let callback = wait_headless_request(origin)?;
    proxy.disconnect_control()?;
    wait_connection_state(target, false)?;
    wait_connection_state(target, true)?;
    wait_cache(target, |client| client.keymap().is_object())?;
    let reply = callback.reply(Ok(Value::Null))?;
    wait_action(origin, reply)?;
    let error = wait_action(origin, held)
        .err()
        .context("stale route unexpectedly succeeded")?;
    ensure!(
        format!("{error:#}").contains("disconnected"),
        "stale route lacked explicit refusal: {error:#}"
    );
    ensure!(
        control.call("session.view", json!({"session_id":session}))?["label"]
            == "Named peer fixture",
        "old route mutated reconnected peer"
    );
    Ok(())
}

fn nested_mutation(
    origin: &mut WorkspaceClient,
    control: &mut ControlConnection,
    session: &str,
    block: &str,
) -> Result<()> {
    let before = control.call("session.view", json!({"session_id":session}))?;
    let error = remote(
        origin,
        "client.action",
        json!({
            "action":"smoke_nested_split", "args":{}, "session_id":session, "block_id":block
        }),
    )
    .err()
    .context("nested routed mutation unexpectedly succeeded")?;
    ensure!(
        format!("{error:#}").contains("nested worker actions"),
        "nested routed mutation was not explicitly refused: {error:#}"
    );
    let after = control.call("session.view", json!({"session_id":session}))?;
    ensure!(
        after == before,
        "nested routed mutation changed the layout after refusal"
    );
    Ok(())
}
