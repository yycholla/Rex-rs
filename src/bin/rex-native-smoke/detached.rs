//! Detached inventory and process restoration against the isolated official server.
use super::{
    ControlProxy, TIMEOUT, native_action, terminal_call, wait_cache, wait_connection_state,
};
use anyhow::{Context, Result, ensure};
use rex_rs::{
    core::{Direction, PaneId},
    rex::{
        WorkspaceClient, actions::NativeAction, automation::ActionContext,
        transport::ControlConnection,
    },
};
use serde_json::{Value, json};
use std::{
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    session: String,
    block: String,
    foreign_session: String,
    foreign_block: String,
}

fn shell(marker: &str) -> Value {
    json!({"block":{"flavor":"com.superlogical.terminal.shell","options":{
        "shell":"none","command":["/bin/sh","-c",r#"printf 'M4_MARK_%s\n' "$0"; exec /bin/sh"#,marker]
    }}})
}

fn id(value: &Value, key: &str) -> Result<String> {
    value[key]
        .as_str()
        .map(str::to_owned)
        .context("detached fixture ID")
}

impl Fixture {
    fn start(control: &mut ControlConnection) -> Result<Self> {
        let original = control.call(
            "session.create",
            json!({"label":"Detached fixture",
            "initial_windows":[{"layout":shell("original")}]}),
        )?;
        let foreign = control.call(
            "session.create",
            json!({"label":"Foreign detached fixture",
            "initial_windows":[{"layout":shell("foreign")}]}),
        )?;
        Ok(Self {
            session: id(&original, "session_id")?,
            block: original["initial_windows"][0]["block_ids"][0]
                .as_str()
                .context("detached block")?
                .into(),
            foreign_session: id(&foreign, "session_id")?,
            foreign_block: foreign["initial_windows"][0]["block_ids"][0]
                .as_str()
                .context("foreign detached block")?
                .into(),
        })
    }

    fn context(&self) -> ActionContext {
        ActionContext {
            session_id: Some(self.session.clone()),
            block_id: Some(self.block.clone()),
        }
    }

    fn view(&self, control: &mut ControlConnection) -> Result<Value> {
        control.call("session.view", json!({"session_id":self.session}))
    }

    fn inventory(&self, control: &mut ControlConnection) -> Result<Value> {
        control.call("session.list_blocks", json!({"session_id":self.session}))
    }

    fn pane(&self, client: &WorkspaceClient) -> Option<PaneId> {
        client.state().pane_ids().into_iter().find(|pane| {
            client
                .state()
                .pane(*pane)
                .is_some_and(|block| block.remote_id == self.block)
        })
    }

    fn detached(&self, client: &WorkspaceClient) -> bool {
        self.pane(client)
            .is_some_and(|pane| client.state().is_detached(pane))
    }

    fn sole_detached(&self, client: &WorkspaceClient) -> bool {
        client.connected()
            && self.detached(client)
            && client
                .state()
                .sessions()
                .iter()
                .find(|session| session.remote_id() == self.session)
                .is_some_and(|session| {
                    session.windows().is_empty()
                        && session.active_window().is_none()
                        && client.state().detached_blocks(session.id()).len() == 1
                })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Facts {
    pid: u64,
    owner: Option<String>,
}

fn facts(control: &mut ControlConnection, fixture: &Fixture) -> Result<Facts> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let content = terminal_call(
            control,
            &fixture.session,
            &fixture.block,
            "com.superlogical.terminal.format",
            json!({"format":"text"}),
        )?;
        if content["content"]
            .as_str()
            .is_some_and(|text| text.contains("M4_MARK_original"))
        {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "detached original output disappeared"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let process = terminal_call(
        control,
        &fixture.session,
        &fixture.block,
        "com.superlogical.terminal.process",
        json!({}),
    )?;
    let size = terminal_call(
        control,
        &fixture.session,
        &fixture.block,
        "com.superlogical.terminal.size",
        json!({}),
    )?;
    Ok(Facts {
        pid: process["child"]["pid"]
            .as_u64()
            .context("detached original PID")?,
        owner: size["owner"].as_str().map(str::to_owned),
    })
}

pub(super) fn check(control: &mut ControlConnection, endpoint: &str) -> Result<()> {
    let fixture = Fixture::start(control)?;
    let baseline = facts(control, &fixture)?;
    let proxy = ControlProxy::start(endpoint)?;
    let mut client = WorkspaceClient::connect(proxy.endpoint.clone());
    let mut replica = WorkspaceClient::connect(endpoint.to_owned());
    wait_cache(&mut client, |client| fixture.pane(client).is_some())?;
    wait_cache(&mut replica, |client| fixture.pane(client).is_some())?;
    native_action(
        &mut client,
        NativeAction::SelectSession {
            session_id: fixture.session.clone(),
            window_id: None,
        },
        ActionContext::default(),
    )?;
    let alias = fixture.pane(&client).context("initial detached alias")?;
    detach_last(control, &mut client, &mut replica, &fixture, alias)?;
    let mut fresh = WorkspaceClient::connect(endpoint.to_owned());
    wait_cache(&mut fresh, |client| fixture.sole_detached(client))?;
    let fresh_pane = fixture
        .pane(&fresh)
        .context("fresh detached bootstrap alias")?;
    ensure!(
        !fresh.input_available(fresh_pane) && fresh.frame(fresh_pane).is_none(),
        "fresh detached-only bootstrap opened a terminal stream"
    );
    drop(fresh);
    resume_detached(&mut client, &mut replica, &proxy, &fixture, alias)?;
    relabel(control, &mut client, &mut replica, &fixture)?;
    restore_with_anchor(control, &mut client, &fixture)?;
    restore_new_tab(control, &mut client, &mut replica, &fixture, alias)?;
    refuse_invalid(control, &mut client, &fixture)?;
    ensure!(
        facts(control, &fixture)? == baseline,
        "detach/restore changed PID, output or owner"
    );
    external_close(control, &mut client, &mut replica, &fixture)?;
    drop(client);
    drop(replica);
    drop(proxy);
    for session in [&fixture.session, &fixture.foreign_session] {
        control.call("session.destroy", json!({"session_id":session}))?;
    }
    Ok(())
}

fn detach_last(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    replica: &mut WorkspaceClient,
    fixture: &Fixture,
    alias: PaneId,
) -> Result<()> {
    wait_cache(client, |client| client.input_available(alias))?;
    native_action(client, NativeAction::DetachPane, fixture.context())?;
    wait_cache(client, |client| {
        fixture.sole_detached(client)
            && !client.input_available(alias)
            && client.frame(alias).is_none()
    })?;
    wait_cache(replica, |client| fixture.sole_detached(client))?;
    let before = fixture.view(control)?;
    ensure!(
        before["windows"].as_array().is_none_or(Vec::is_empty)
            && before["active_window_id"].is_null(),
        "last detach retained a window"
    );
    ensure!(
        fixture.inventory(control)?["blocks"][0]["placement"] == "detached",
        "last detach lost the authoritative inventory entry"
    );
    let repeated = native_action(client, NativeAction::DetachPane, fixture.context())?;
    ensure!(
        repeated["revision"] == before["revision"] && fixture.view(control)? == before,
        "repeated detach mutated the empty session"
    );
    Ok(())
}

fn resume_detached(
    client: &mut WorkspaceClient,
    replica: &mut WorkspaceClient,
    proxy: &ControlProxy,
    fixture: &Fixture,
    alias: PaneId,
) -> Result<()> {
    let identity = client.state().client_id().to_owned();
    proxy.disconnect_control()?;
    wait_connection_state(client, false)?;
    wait_connection_state(client, true)?;
    wait_cache(client, |client| fixture.sole_detached(client))?;
    wait_cache(replica, |client| fixture.sole_detached(client))?;
    ensure!(
        client.state().client_id() == identity
            && fixture.pane(client) == Some(alias)
            && !client.input_available(alias)
            && client.frame(alias).is_none(),
        "detached-only resume lost identity or reopened a terminal stream"
    );
    Ok(())
}

fn relabel(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    replica: &mut WorkspaceClient,
    fixture: &Fixture,
) -> Result<()> {
    let before = fixture.view(control)?;
    let changed = control.call(
        "session.set_block_label",
        json!({"session_id":fixture.session,
        "block_id":fixture.block,"label":"Detached external label"}),
    )?;
    ensure!(
        changed["revision"] == before["revision"] && fixture.view(control)? == before,
        "official server unexpectedly changed view for detached metadata"
    );
    // No session_view_changed (or label event) accompanies this mutation. Do not
    // call refresh_inventory here: this checks the periodic reconciliation path.
    let deadline = Instant::now() + Duration::from_millis(2500);
    loop {
        client.poll()?;
        replica.poll()?;
        let refreshed = [&*client, &*replica].into_iter().all(|target| {
            fixture
                .pane(target)
                .and_then(|pane| target.state().pane(pane))
                .is_some_and(|block| block.label == "Detached external label")
        });
        if refreshed {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "detached external metadata did not refresh within polling interval"
        );
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn restore_with_anchor(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    fixture: &Fixture,
) -> Result<()> {
    let created = control.call(
        "session.new_window",
        json!({"session_id":fixture.session,
        "layout":shell("anchor")}),
    )?;
    let anchor = created["block_ids"][0]
        .as_str()
        .context("detached restore anchor")?
        .to_owned();
    wait_cache(client, |client| {
        client.state().pane_ids().into_iter().any(|pane| {
            client
                .state()
                .pane(pane)
                .is_some_and(|block| block.remote_id == anchor)
        })
    })?;
    native_action(
        client,
        NativeAction::MovePane {
            anchor_block_id: anchor.clone(),
            direction: Direction::Left,
            ratio: 0.375,
        },
        fixture.context(),
    )?;
    let view = fixture.view(control)?;
    ensure!(
        view["windows"][0]["focused_block_id"] == anchor,
        "restoring detached pane stole existing anchor focus"
    );
    ensure!(
        view["windows"][0]["layers"][0]["layout"]["split"]["before"]["block_id"] == fixture.block,
        "move did not place the original detached block"
    );
    native_action(client, NativeAction::DetachPane, fixture.context())?;
    control.call(
        "block.close",
        json!({"session_id":fixture.session,"block_id":anchor}),
    )?;
    wait_cache(client, |client| fixture.sole_detached(client))
}

fn restore_new_tab(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    replica: &mut WorkspaceClient,
    fixture: &Fixture,
    alias: PaneId,
) -> Result<()> {
    let before = fixture.inventory(control)?;
    native_action(
        client,
        NativeAction::RestorePane {
            block_id: fixture.block.clone(),
        },
        fixture.context(),
    )?;
    let view = fixture.view(control)?;
    ensure!(
        view["windows"]
            .as_array()
            .is_some_and(|windows| windows.len() == 1)
            && view["windows"][0]["focused_block_id"] == fixture.block
            && view["windows"][0]["layers"][0]["layout"]["block_id"] == fixture.block,
        "new-tab restoration left a carrier or failed to focus the original block"
    );
    let after = fixture.inventory(control)?;
    ensure!(
        after["blocks"]
            .as_array()
            .is_some_and(|blocks| blocks.len() == 1)
            && after["blocks"][0]["block_id"] == before["blocks"][0]["block_id"],
        "restore leaked a carrier block or created a replacement shell"
    );
    for target in [&mut *client, &mut *replica] {
        wait_cache(target, |client| {
            fixture.pane(client).is_some_and(|pane| {
                !client.state().is_detached(pane) && client.input_available(pane)
            })
        })?;
    }
    ensure!(
        fixture.pane(client) == Some(alias),
        "restore changed original presentation alias"
    );
    Ok(())
}

fn refuse_invalid(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    fixture: &Fixture,
) -> Result<()> {
    let before = fixture.view(control)?;
    let inventory = fixture.inventory(control)?;
    let foreign = control.call(
        "session.view",
        json!({"session_id":fixture.foreign_session}),
    )?;
    for block in [
        fixture.block.as_str(),
        fixture.foreign_block.as_str(),
        "block:00000000-0000-0000-0000-000000000000",
    ] {
        ensure!(
            native_action(
                client,
                NativeAction::RestorePane {
                    block_id: block.to_string()
                },
                fixture.context()
            )
            .is_err(),
            "placed, foreign or stale restore was accepted"
        );
        ensure!(
            fixture.view(control)? == before
                && fixture.inventory(control)? == inventory
                && control.call(
                    "session.view",
                    json!({"session_id":fixture.foreign_session})
                )? == foreign,
            "invalid restore mutated a session or inventory"
        );
    }
    Ok(())
}

fn external_close(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    replica: &mut WorkspaceClient,
    fixture: &Fixture,
) -> Result<()> {
    native_action(client, NativeAction::DetachPane, fixture.context())?;
    wait_cache(replica, |client| fixture.sole_detached(client))?;
    let before = fixture.view(control)?;
    let closed = control.call(
        "block.close",
        json!({"session_id":fixture.session,
        "block_id":fixture.block}),
    )?;
    ensure!(
        closed["revision"] == before["revision"] && fixture.view(control)? == before,
        "detached external close unexpectedly changed view revision"
    );
    for target in [&mut *client, &mut *replica] {
        wait_cache(target, |client| fixture.pane(client).is_none())?;
        ensure!(
            client_session_present(target, &fixture.session),
            "closing final detached block destroyed its session"
        );
    }
    Ok(())
}

fn client_session_present(client: &WorkspaceClient, remote: &str) -> bool {
    client
        .state()
        .sessions()
        .iter()
        .any(|session| session.remote_id() == remote)
}
