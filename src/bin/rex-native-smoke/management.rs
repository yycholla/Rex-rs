//! Real-server placement checks; every resource belongs to the isolated smoke server.
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
    collections::BTreeMap,
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    session: String,
    windows: [String; 3],
    // Original A, its two split children, B, and the sole block of C.
    blocks: [String; 5],
    foreign_session: String,
    foreign_window: String,
    foreign_block: String,
}

fn shell(marker: &str) -> Value {
    json!({"block":{"flavor":"com.superlogical.terminal.shell","options":{
        "shell":"none","command":["/bin/sh","-c",r#"printf 'M3_MARK_%s\n' "$0"; exec /bin/sh"#,marker]
    }}})
}

fn id(value: &Value, key: &str) -> Result<String> {
    value[key]
        .as_str()
        .map(str::to_owned)
        .context("management fixture ID")
}

impl Fixture {
    fn start(control: &mut ControlConnection) -> Result<Self> {
        let created = control.call(
            "session.create",
            json!({"label":"Management fixture",
            "initial_windows":[{"window_label":"A","layout":shell("A")},
                {"window_label":"B","layout":shell("B")},
                {"window_label":"C","layout":shell("C")}]}),
        )?;
        let session = id(&created, "session_id")?;
        let initial = &created["initial_windows"];
        let original = initial[0]["block_ids"][0]
            .as_str()
            .context("original management block")?
            .to_owned();
        let first = control.call(
            "session.new_split",
            json!({"session_id":session,"anchor_id":original,
            "direction":"horizontal","side":"after","ratio":0.3,"layout":shell("A1")}),
        )?;
        let first_block = first["block_ids"][0]
            .as_str()
            .context("first split block")?
            .to_owned();
        let second = control.call(
            "session.new_split",
            json!({"session_id":session,"anchor_id":first_block,
            "direction":"vertical","side":"after","ratio":0.625,"layout":shell("A2")}),
        )?;
        let foreign = control.call(
            "session.create",
            json!({"label":"Foreign management fixture",
            "initial_windows":[{"layout":shell("FOREIGN")}]}),
        )?;
        Ok(Self {
            session,
            windows: [
                id(&initial[0], "window_id")?,
                id(&initial[1], "window_id")?,
                id(&initial[2], "window_id")?,
            ],
            blocks: [
                original,
                first_block,
                second["block_ids"][0]
                    .as_str()
                    .context("second split block")?
                    .into(),
                initial[1]["block_ids"][0]
                    .as_str()
                    .context("B block")?
                    .into(),
                initial[2]["block_ids"][0]
                    .as_str()
                    .context("C block")?
                    .into(),
            ],
            foreign_session: id(&foreign, "session_id")?,
            foreign_window: id(&foreign["initial_windows"][0], "window_id")?,
            foreign_block: foreign["initial_windows"][0]["block_ids"][0]
                .as_str()
                .context("foreign management block")?
                .into(),
        })
    }

    fn context(&self, block: usize) -> ActionContext {
        ActionContext {
            session_id: Some(self.session.clone()),
            block_id: Some(self.blocks[block].clone()),
        }
    }

    fn view(&self, control: &mut ControlConnection) -> Result<Value> {
        control.call("session.view", json!({"session_id":self.session}))
    }

    fn known(&self, client: &WorkspaceClient) -> bool {
        client.connected()
            && client.keymap().is_object()
            && self
                .blocks
                .iter()
                .chain(std::iter::once(&self.foreign_block))
                .all(|remote| {
                    client.state().pane_ids().into_iter().any(|pane| {
                        client
                            .state()
                            .pane(pane)
                            .is_some_and(|block| &block.remote_id == remote)
                    })
                })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct BlockFacts {
    pid: u64,
    owner: Option<String>,
}

fn facts(control: &mut ControlConnection, fixture: &Fixture) -> Result<Vec<BlockFacts>> {
    fixture
        .blocks
        .iter()
        .zip(["A", "A1", "A2", "B", "C"])
        .map(|(block, marker)| {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                let content = terminal_call(
                    control,
                    &fixture.session,
                    block,
                    "com.superlogical.terminal.format",
                    json!({"format":"text"}),
                )?;
                if content["content"]
                    .as_str()
                    .is_some_and(|text| text.contains(&format!("M3_MARK_{marker}")))
                {
                    break;
                }
                ensure!(
                    Instant::now() < deadline,
                    "management marker disappeared: {marker}"
                );
                thread::sleep(Duration::from_millis(5));
            }
            let process = terminal_call(
                control,
                &fixture.session,
                block,
                "com.superlogical.terminal.process",
                json!({}),
            )?;
            let size = terminal_call(
                control,
                &fixture.session,
                block,
                "com.superlogical.terminal.size",
                json!({}),
            )?;
            Ok(BlockFacts {
                pid: process["child"]["pid"]
                    .as_u64()
                    .context("management shell PID")?,
                owner: size["owner"].as_str().map(str::to_owned),
            })
        })
        .collect()
}

fn window<'a>(view: &'a Value, remote: &str) -> Result<&'a Value> {
    view["windows"]
        .as_array()
        .context("management windows")?
        .iter()
        .find(|window| window["window_id"] == remote)
        .context("management window absent")
}

fn order(view: &Value) -> Result<Vec<String>> {
    view["windows"]
        .as_array()
        .context("management tab order")?
        .iter()
        .map(|window| id(window, "window_id"))
        .collect()
}

fn splits(view: &Value) -> Result<BTreeMap<String, Value>> {
    fn collect(node: &Value, found: &mut BTreeMap<String, Value>) -> Result<()> {
        if let Some(split) = node.get("split") {
            found.insert(
                id(split, "split_id")?,
                json!({"direction":split["direction"],"ratio":split["ratio"]}),
            );
            collect(&split["before"], found)?;
            collect(&split["after"], found)?;
        }
        Ok(())
    }
    let mut result = BTreeMap::new();
    for window in view["windows"].as_array().context("management windows")? {
        for layer in window["layers"].as_array().context("management layers")? {
            collect(&layer["layout"], &mut result)?;
        }
    }
    Ok(result)
}

fn view_matches(client: &WorkspaceClient, expected: &Value) -> bool {
    let Some(session) = client
        .state()
        .sessions()
        .iter()
        .find(|session| Some(session.remote_id()) == expected["session_id"].as_str())
    else {
        return false;
    };
    let Some(windows) = expected["windows"].as_array() else {
        return false;
    };
    session.revision() == expected["revision"].as_u64()
        && session.windows().len() == windows.len()
        && session
            .active_window()
            .map(rex_rs::rex::workspace::WindowView::remote_id)
            == expected["active_window_id"].as_str()
        && session
            .windows()
            .iter()
            .zip(windows)
            .all(|(local, remote)| {
                Some(local.remote_id()) == remote["window_id"].as_str()
                    && local.layers.len() == remote["layers"].as_array().map_or(0, Vec::len)
                    && local
                        .layers
                        .iter()
                        .zip(remote["layers"].as_array().into_iter().flatten())
                        .all(|(layer, remote)| layer.raw_layout == remote["layout"])
                    && local
                        .active_pane()
                        .and_then(|pane| client.state().remote_pane(pane).ok())
                        == remote["focused_block_id"].as_str()
                    && local
                        .zoomed_pane()
                        .and_then(|pane| client.state().remote_pane(pane).ok())
                        == remote["zoomed_block_id"].as_str()
            })
}

fn converge(
    primary: &mut WorkspaceClient,
    observer: &mut WorkspaceClient,
    expected: &Value,
) -> Result<()> {
    wait_cache(primary, |client| view_matches(client, expected))?;
    wait_cache(observer, |client| view_matches(client, expected))
}

pub(super) fn check(control: &mut ControlConnection, endpoint: &str) -> Result<()> {
    let fixture = Fixture::start(control)?;
    let before = facts(control, &fixture)?;
    let proxy = ControlProxy::start(endpoint)?;
    let mut client = WorkspaceClient::connect(proxy.endpoint.clone());
    let mut observer = WorkspaceClient::connect(endpoint.to_owned());
    wait_cache(&mut client, |client| fixture.known(client))?;
    wait_cache(&mut observer, |client| fixture.known(client))?;
    native_action(
        &mut client,
        NativeAction::SelectSession {
            session_id: fixture.session.clone(),
            window_id: None,
        },
        ActionContext::default(),
    )?;
    same_window_move(control, &mut client, &mut observer, &fixture)?;
    cross_window_move(control, &mut client, &mut observer, &fixture)?;
    swap(control, &mut client, &mut observer, &fixture)?;
    reorder_tabs(control, &mut client, &mut observer, &fixture)?;
    refuse_invalid(control, &mut client, &fixture)?;
    reconnect(control, &mut client, &mut observer, &proxy, &fixture)?;
    ensure!(
        facts(control, &fixture)? == before,
        "management changed a process PID, output marker or size owner"
    );
    drop(client);
    drop(observer);
    drop(proxy);
    for session in [&fixture.session, &fixture.foreign_session] {
        control.call("session.destroy", json!({"session_id":session}))?;
    }
    Ok(())
}

fn same_window_move(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    observer: &mut WorkspaceClient,
    fixture: &Fixture,
) -> Result<()> {
    control.call(
        "session.focus_block",
        json!({"session_id":fixture.session,"block_id":fixture.blocks[1]}),
    )?;
    let before = fixture.view(control)?;
    converge(client, observer, &before)?;
    let layout = &window(&before, &fixture.windows[0])?["layers"][0]["layout"];
    let outer = id(&layout["split"], "split_id")?;
    let removed = id(&layout["split"]["after"]["split"], "split_id")?;
    let result = native_action(
        client,
        NativeAction::MovePane {
            anchor_block_id: fixture.blocks[0].clone(),
            direction: Direction::Left,
            ratio: 0.1875,
        },
        fixture.context(1),
    )?;
    let after = fixture.view(control)?;
    let layout = &window(&after, &fixture.windows[0])?["layers"][0]["layout"];
    ensure!(
        result["removed_split_id"] == removed
            && layout["split"]["split_id"] == outer
            && layout["split"]["ratio"] == json!(0.3),
        "same-window move changed the untouched fractional split"
    );
    let inserted = &layout["split"]["before"]["split"];
    ensure!(
        inserted["split_id"] == result["created_split_id"]
            && inserted["ratio"] == json!(0.1875)
            && inserted["before"]["block_id"] == fixture.blocks[1]
            && inserted["after"]["block_id"] == fixture.blocks[0],
        "same-window move lost direction, ratio or block identity"
    );
    ensure!(
        window(&after, &fixture.windows[0])?["focused_block_id"] == fixture.blocks[1],
        "focused moved block lost focus"
    );
    converge(client, observer, &after)
}

fn cross_window_move(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    observer: &mut WorkspaceClient,
    fixture: &Fixture,
) -> Result<()> {
    for block in [&fixture.blocks[3], &fixture.blocks[1]] {
        control.call(
            "session.zoom_block",
            json!({"session_id":fixture.session,"block_id":block}),
        )?;
    }
    converge(client, observer, &fixture.view(control)?)?;
    let before = fixture.view(control)?;
    let outer =
        window(&before, &fixture.windows[0])?["layers"][0]["layout"]["split"]["split_id"].clone();
    native_action(
        client,
        NativeAction::MovePane {
            anchor_block_id: fixture.blocks[3].clone(),
            direction: Direction::Left,
            ratio: 0.2,
        },
        fixture.context(1),
    )?;
    let after = fixture.view(control)?;
    ensure!(
        after["active_window_id"] == fixture.windows[1]
            && window(&after, &fixture.windows[1])?["focused_block_id"] == fixture.blocks[1],
        "cross-window move did not carry focus with its block"
    );
    for remote in [&fixture.windows[0], &fixture.windows[1]] {
        ensure!(
            window(&after, remote)?["zoomed_block_id"].is_null(),
            "cross-window move retained zoom"
        );
    }
    ensure!(
        window(&after, &fixture.windows[0])?["layers"][0]["layout"]["split"]["split_id"] == outer,
        "cross-window move replaced an unaffected split"
    );
    converge(client, observer, &after)?;
    native_action(
        client,
        NativeAction::MovePane {
            anchor_block_id: fixture.blocks[3].clone(),
            direction: Direction::Down,
            ratio: 0.7,
        },
        fixture.context(4),
    )?;
    let after = fixture.view(control)?;
    ensure!(
        order(&after)? == fixture.windows[..2]
            && after["active_window_id"] == fixture.windows[1]
            && window(&after, &fixture.windows[1])?["focused_block_id"] == fixture.blocks[4],
        "moving a window's sole block did not close that window and preserve focus"
    );
    converge(client, observer, &after)
}

fn swap(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    observer: &mut WorkspaceClient,
    fixture: &Fixture,
) -> Result<()> {
    control.call(
        "session.focus_block",
        json!({"session_id":fixture.session,"block_id":fixture.blocks[1]}),
    )?;
    let before = fixture.view(control)?;
    converge(client, observer, &before)?;
    native_action(
        client,
        NativeAction::SwapPanes {
            other_block_id: fixture.blocks[0].clone(),
        },
        fixture.context(1),
    )?;
    let after = fixture.view(control)?;
    ensure!(
        splits(&before)? == splits(&after)? && order(&before)? == order(&after)?,
        "swap changed split IDs, nondefault fractional ratios or tab order"
    );
    ensure!(
        after["active_window_id"] == fixture.windows[0]
            && window(&after, &fixture.windows[0])?["focused_block_id"] == fixture.blocks[1]
            && window(&after, &fixture.windows[1])?["focused_block_id"] == fixture.blocks[0],
        "cross-window swap did not carry the active focused block to its new window"
    );
    converge(client, observer, &after)
}

fn reorder_tabs(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    observer: &mut WorkspaceClient,
    fixture: &Fixture,
) -> Result<()> {
    let before = fixture.view(control)?;
    native_action(
        client,
        NativeAction::MoveWindow {
            before_window_id: Some(fixture.windows[0].clone()),
        },
        fixture.context(0),
    )?;
    let after = fixture.view(control)?;
    ensure!(
        order(&after)? == [fixture.windows[1].clone(), fixture.windows[0].clone()]
            && after["active_window_id"] == before["active_window_id"]
            && splits(&after)? == splits(&before)?,
        "tab reorder changed the active tab or its split layout"
    );
    for remote in &fixture.windows[..2] {
        ensure!(
            window(&after, remote)?["focused_block_id"]
                == window(&before, remote)?["focused_block_id"],
            "tab reorder changed pane focus"
        );
    }
    converge(client, observer, &after)?;
    let no_op = native_action(
        client,
        NativeAction::MoveWindow {
            before_window_id: Some(fixture.windows[0].clone()),
        },
        fixture.context(0),
    )?;
    ensure!(
        no_op["revision"] == after["revision"],
        "already-placed tab move changed revision"
    );
    native_action(
        client,
        NativeAction::MoveWindow {
            before_window_id: None,
        },
        fixture.context(0),
    )?;
    let end = fixture.view(control)?;
    ensure!(
        order(&end)? == fixture.windows[..2]
            && end["active_window_id"] == before["active_window_id"],
        "omitting the destination did not move the tab to the end"
    );
    converge(client, observer, &end)
}

fn refuse_invalid(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    fixture: &Fixture,
) -> Result<()> {
    let before = fixture.view(control)?;
    let foreign = control.call(
        "session.view",
        json!({"session_id":fixture.foreign_session}),
    )?;
    let selected = client
        .state()
        .active_session()
        .context("management selection")?
        .remote_id()
        .to_owned();
    let mut invalid = vec![
        NativeAction::MovePane {
            anchor_block_id: fixture.blocks[1].clone(),
            direction: Direction::Left,
            ratio: 0.5,
        },
        NativeAction::SwapPanes {
            other_block_id: fixture.blocks[1].clone(),
        },
        NativeAction::SwapPanes {
            other_block_id: fixture.foreign_block.clone(),
        },
        NativeAction::MovePane {
            anchor_block_id: fixture.foreign_block.clone(),
            direction: Direction::Right,
            ratio: 0.5,
        },
        NativeAction::MoveWindow {
            before_window_id: Some(fixture.foreign_window.clone()),
        },
        NativeAction::MoveWindow {
            before_window_id: Some(fixture.windows[0].clone()),
        },
    ];
    invalid.extend(
        [
            0.0,
            1.0,
            -0.1,
            1.1,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ]
        .into_iter()
        .map(|ratio| NativeAction::MovePane {
            anchor_block_id: fixture.blocks[3].clone(),
            direction: Direction::Right,
            ratio,
        }),
    );
    for action in invalid {
        ensure!(
            native_action(client, action, fixture.context(1)).is_err(),
            "invalid management action was accepted"
        );
        ensure!(
            fixture.view(control)? == before
                && control.call(
                    "session.view",
                    json!({"session_id":fixture.foreign_session})
                )? == foreign,
            "invalid management action mutated a session"
        );
        ensure!(
            client
                .state()
                .active_session()
                .context("retained management selection")?
                .remote_id()
                == selected,
            "invalid management action changed local selection"
        );
    }
    Ok(())
}

fn aliases(client: &WorkspaceClient, fixture: &Fixture) -> Result<BTreeMap<String, PaneId>> {
    fixture
        .blocks
        .iter()
        .map(|remote| {
            let pane = client
                .state()
                .pane_ids()
                .into_iter()
                .find(|pane| {
                    client
                        .state()
                        .pane(*pane)
                        .is_some_and(|block| &block.remote_id == remote)
                })
                .context("management block alias")?;
            Ok((remote.clone(), pane))
        })
        .collect()
}

fn reconnect(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    observer: &mut WorkspaceClient,
    proxy: &ControlProxy,
    fixture: &Fixture,
) -> Result<()> {
    let before = fixture.view(control)?;
    let ids = aliases(client, fixture)?;
    let identity = client.state().client_id().to_owned();
    proxy.disconnect_control()?;
    wait_connection_state(client, false)?;
    wait_connection_state(client, true)?;
    converge(client, observer, &before)?;
    ensure!(
        client.state().client_id() == identity && aliases(client, fixture)? == ids,
        "management reconnect changed client or block presentation identities"
    );
    Ok(())
}
