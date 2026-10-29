//! Closing a real pane must quietly retire UI events already in flight.
use super::{TIMEOUT, terminal_call};
use anyhow::{Context, Result, ensure};
use rex_rs::{
    core::{
        Command, GridSize, PaneId,
        input::{Modifiers, MouseAction, MouseButton, MouseInput},
    },
    rex::{
        WorkspaceClient, actions::NativeAction, automation::ActionContext,
        transport::ControlConnection,
    },
};
use serde_json::{Value, json};
use std::{
    future::Future,
    task::{Context as TaskContext, Poll, Waker},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    session: String,
    blocks: [String; 3],
}

fn shell(marker: &str) -> Value {
    json!({"block":{"flavor":"com.superlogical.terminal.shell","options":{
        "shell":"none","command":["/bin/sh","-c",r#"printf 'CLOSE_MARK_%s\n' "$0"; exec /bin/sh"#,marker]
    }}})
}

impl Fixture {
    fn start(control: &mut ControlConnection) -> Result<Self> {
        let created = control.call(
            "session.create",
            json!({"label":"Close event race fixture",
            "initial_windows":[{"layout":{"split":{"direction":"horizontal","ratio":0.3,
                "before":shell("local"),"after":{"split":{"direction":"vertical","ratio":0.6,
                "before":shell("external"),"after":shell("survivor")}}}}}]}),
        )?;
        let ids = created["initial_windows"][0]["block_ids"]
            .as_array()
            .context("close fixture blocks")?;
        ensure!(ids.len() == 3, "close fixture did not create three blocks");
        Ok(Self {
            session: created["session_id"]
                .as_str()
                .context("close fixture session")?
                .into(),
            blocks: [
                ids[0].as_str().context("local close block")?.into(),
                ids[1].as_str().context("external close block")?.into(),
                ids[2].as_str().context("surviving close block")?.into(),
            ],
        })
    }

    fn pane(&self, client: &WorkspaceClient, index: usize) -> Option<PaneId> {
        client.state().pane_ids().into_iter().find(|pane| {
            client
                .state()
                .pane(*pane)
                .is_some_and(|block| block.remote_id == self.blocks[index])
        })
    }

    fn context(&self, remote: &str) -> ActionContext {
        ActionContext {
            session_id: Some(self.session.clone()),
            block_id: Some(remote.into()),
        }
    }
}

// Unlike the general action pump, this intentionally propagates every worker
// error: absorbing a transient "block no longer exists" would hide the bug.
fn strict_action(
    client: &mut WorkspaceClient,
    future: impl Future<Output = Result<Value>>,
) -> Result<Value> {
    let mut future = Box::pin(future);
    let mut context = TaskContext::from_waker(Waker::noop());
    let deadline = Instant::now() + TIMEOUT;
    loop {
        client
            .poll()
            .context("closed-pane queue produced a user-visible worker error")?;
        if let Poll::Ready(result) = future.as_mut().poll(&mut context) {
            client.poll()?;
            return result;
        }
        ensure!(Instant::now() < deadline, "close race action timed out");
        thread::sleep(Duration::from_millis(2));
    }
}

fn quiet_until(
    client: &mut WorkspaceClient,
    ready: impl Fn(&WorkspaceClient) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        client
            .poll()
            .context("closed-pane event produced a user-visible worker error")?;
        if ready(client) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "close race membership did not converge"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn queue_tail(client: &mut WorkspaceClient, pane: PaneId) -> Result<()> {
    // The UI cache deliberately has not polled the close yet, so input/focus
    // availability still reflects the previously displayed terminal.
    client.resize(pane, GridSize::new(82, 25)?, 9, 18)?;
    client.scroll(pane, 2)?;
    client.wheel(pane, -1, 0, 0, Modifiers::default())?;
    client.focus(pane, false)?;
    client.focus(pane, true)?;
    client.mouse(
        pane,
        MouseInput {
            action: MouseAction::Release,
            button: MouseButton::Left,
            column: 0,
            row: 0,
            modifiers: Modifiers::default(),
        },
    )?;
    client.write_input(pane, b"printf 'RETIRED_INPUT_MUST_NOT_REACH_SURVIVOR\\n'\n")?;
    client.paste(pane, "retired paste")?;
    client.set_size_ownership(pane, true)?;
    Ok(())
}

fn barrier(client: &mut WorkspaceClient, survivor: PaneId) -> Result<()> {
    let future = client.perform_command(Command::FocusPane(survivor))?;
    strict_action(client, future)?;
    // Drain errors and updates emitted by the subsequent stream/event service,
    // rather than stopping as soon as the command future resolves.
    let deadline = Instant::now() + Duration::from_millis(150);
    while Instant::now() < deadline {
        client.poll()?;
        thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

pub(super) fn check(control: &mut ControlConnection, endpoint: &str) -> Result<()> {
    let fixture = Fixture::start(control)?;
    let mut client = WorkspaceClient::connect(endpoint.to_owned());
    quiet_until(&mut client, |client| {
        (0..3).all(|index| {
            fixture
                .pane(client, index)
                .is_some_and(|pane| client.input_available(pane))
        })
    })?;
    let selected = client.perform_native_action(
        NativeAction::SelectSession {
            session_id: fixture.session.clone(),
            window_id: None,
        },
        ActionContext::default(),
    )?;
    strict_action(&mut client, selected)?;
    let local = fixture.pane(&client, 0).context("local close alias")?;
    let external = fixture.pane(&client, 1).context("external close alias")?;
    let survivor = fixture.pane(&client, 2).context("survivor alias")?;
    client.apply(Command::ClosePane(local))?;
    queue_tail(&mut client, local)?;
    barrier(&mut client, survivor)?;
    quiet_until(&mut client, |client| fixture.pane(client, 0).is_none())?;
    control.call(
        "block.close",
        json!({"session_id":fixture.session,"block_id":fixture.blocks[1]}),
    )?;
    queue_tail(&mut client, external)?;
    barrier(&mut client, survivor)?;
    quiet_until(&mut client, |client| fixture.pane(client, 1).is_none())?;
    refuse_explicit_stale(control, &mut client, &fixture)?;
    survivor_usable(control, &mut client, &fixture, survivor)?;
    drop(client);
    control.call("session.destroy", json!({"session_id":fixture.session}))?;
    Ok(())
}

fn refuse_explicit_stale(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    fixture: &Fixture,
) -> Result<()> {
    let before = control.call("session.view", json!({"session_id":fixture.session}))?;
    for remote in [
        fixture.blocks[0].as_str(),
        fixture.blocks[1].as_str(),
        "block:00000000-0000-0000-0000-000000000000",
    ] {
        for action in [NativeAction::ClosePane, NativeAction::ZoomPane] {
            let result = match client.perform_native_action(action, fixture.context(remote)) {
                Ok(future) => strict_action(client, future),
                Err(error) => Err(error),
            };
            ensure!(
                result.is_err(),
                "explicit retired/unknown action context was silently accepted"
            );
            ensure!(
                control.call("session.view", json!({"session_id":fixture.session}))? == before,
                "invalid explicit context mutated surviving layout"
            );
        }
    }
    Ok(())
}

fn survivor_usable(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    fixture: &Fixture,
    survivor: PaneId,
) -> Result<()> {
    ensure!(
        client.input_available(survivor),
        "close race disabled surviving pane input"
    );
    client.write_input(survivor, b"printf 'CLOSE_SURVIVOR_%s\\n' STILL_USABLE\n")?;
    barrier(client, survivor)?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        client.poll()?;
        let content = terminal_call(
            control,
            &fixture.session,
            &fixture.blocks[2],
            "com.superlogical.terminal.format",
            json!({"format":"text"}),
        )?;
        let text = content["content"]
            .as_str()
            .context("close survivor output")?;
        ensure!(
            !text.contains("RETIRED_INPUT_MUST_NOT_REACH_SURVIVOR"),
            "retired pane input was redirected to a survivor"
        );
        if text.contains("CLOSE_SURVIVOR_STILL_USABLE") {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "surviving pane input stopped after close"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
