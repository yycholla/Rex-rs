//! Ordered native actions executed by the Rex control worker.
use std::{borrow::Cow, collections::BTreeSet};

use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};

use super::{
    actions::NativeAction,
    automation::ActionContext,
    diagnostics::WorkspaceCall,
    transport::ControlConnection,
    workspace::{ApiCall, BlockView, SessionView, WindowView, WorkspaceState},
};
use crate::core::{Command, Direction, SplitAxis};

struct Target<'a> {
    session: &'a SessionView,
    block: Option<&'a BlockView>,
}

struct ActionPlan {
    calls: Vec<ApiCall>,
    selection: Option<String>,
}

/// Execute once, refreshing authoritative views after each successful mutation.
/// Failed or uncertain mutations are never replayed here.
pub(crate) fn execute(
    control: &mut ControlConnection,
    state: &mut WorkspaceState,
    action: &NativeAction,
    context: &ActionContext,
) -> Result<Value> {
    if let NativeAction::RestorePane { block_id } = action {
        super::automation::validate_context(state, context)?;
        let block = state
            .pane_ids()
            .into_iter()
            .filter_map(|pane| state.pane(pane))
            .find(|block| &block.remote_id == block_id)
            .context("detached block no longer exists")?;
        let expected = context
            .session_id
            .as_deref()
            .or_else(|| state.active_session().map(SessionView::remote_id))
            .context("no session selected")?;
        ensure!(
            block.session_id == expected,
            "detached block belongs to another session"
        );
        let pane = block.id;
        let title = "Restored terminal";
        return super::restore::execute(control, state, pane, title);
    }
    let plan = plan(state, action, context)?;
    if let Some(remote) = &plan.selection {
        select(state, remote)?;
    }
    let mut result = Value::Null;
    if plan.calls.is_empty() {
        refresh(control, state, plan.selection.as_deref())?;
    }
    for call in plan.calls {
        let affected = call.params["session_id"].as_str().map(str::to_owned);
        let diagnostic = WorkspaceCall::new(call.method, &call.params);
        result = control.call(call.method, call.params)?;
        diagnostic.applied(&result);
        let created = if call.method == "session.create" {
            Some(
                result["session_id"]
                    .as_str()
                    .context("Rex create response lacks session ID")?
                    .to_owned(),
            )
        } else {
            None
        };
        refresh(control, state, created.as_deref().or(affected.as_deref()))?;
        if let Some(remote) = created {
            select(state, &remote)?;
        }
    }
    Ok(result)
}

fn select(state: &mut WorkspaceState, remote: &str) -> Result<()> {
    let id = state
        .sessions()
        .iter()
        .find(|entry| entry.remote_id() == remote)
        .context("requested Rex session is no longer present")?
        .id();
    state.select_session(id)
}

fn refresh(
    control: &mut ControlConnection,
    state: &mut WorkspaceState,
    affected: Option<&str>,
) -> Result<()> {
    state.update_sessions(&control.call("session.list", json!({}))?)?;
    let mut ids = BTreeSet::new();
    if let Some(id) = affected {
        ids.insert(id.to_owned());
    }
    if let Some(session) = state.active_session() {
        ids.insert(session.remote_id().to_owned());
    }
    let mut snapshots = Vec::new();
    for id in ids {
        if state
            .sessions()
            .iter()
            .any(|session| session.remote_id() == id)
        {
            let attached = control.call("session.attach", json!({"session_id":id}))?;
            let inventory = control.call("session.list_blocks", json!({"session_id":id}))?;
            snapshots.push((attached["view"].clone(), inventory));
        }
    }
    state.apply_snapshots(&snapshots)?;
    Ok(())
}

/// Resolve every explicit context field before an action can change selection.
fn target<'a>(state: &'a WorkspaceState, context: &ActionContext) -> Result<Option<Target<'a>>> {
    ensure!(
        context.block_id.is_none() || context.session_id.is_some(),
        "a block context requires a session context"
    );
    let session = if let Some(remote) = &context.session_id {
        Some(
            state
                .sessions()
                .iter()
                .find(|session| session.remote_id() == remote)
                .context("requested Rex session is no longer present")?,
        )
    } else {
        state.active_session()
    };
    let block = if let Some(remote) = &context.block_id {
        let session = session.context("no Rex session selected")?;
        let block = state
            .pane_ids()
            .into_iter()
            .filter_map(|pane| state.pane(pane))
            .find(|block| block.remote_id == *remote)
            .context("requested Rex block is no longer present")?;
        ensure!(
            block.session_id == session.remote_id(),
            "requested Rex block belongs to another session"
        );
        Some(block)
    } else {
        None
    };
    Ok(session.map(|session| Target { session, block }))
}

fn plan(
    state: &WorkspaceState,
    action: &NativeAction,
    context: &ActionContext,
) -> Result<ActionPlan> {
    let resolved = target(state, context)?;
    let mut plan = ActionPlan {
        calls: Vec::new(),
        selection: context.session_id.clone(),
    };
    match action {
        NativeAction::Find
        | NativeAction::FindNext
        | NativeAction::FindPrevious
        | NativeAction::ToggleDetached
        | NativeAction::ArrangePane
        | NativeAction::FontIncrease
        | NativeAction::FontDecrease
        | NativeAction::FontReset
        | NativeAction::ToggleSidebar
        | NativeAction::Copy
        | NativeAction::Paste
        | NativeAction::EnterMode { .. }
        | NativeAction::ExitMode
        | NativeAction::ExitAllModes
        | NativeAction::RenameSession
        | NativeAction::RenameWindow => bail!("action requires the desktop UI"),
        NativeAction::SelectSession {
            session_id,
            window_id,
        } => {
            let session = state
                .sessions()
                .iter()
                .find(|session| session.remote_id() == session_id)
                .context("requested Rex session is no longer present")?;
            if let Some(window_id) = window_id {
                let window = session
                    .windows()
                    .iter()
                    .find(|window| window.remote_id() == window_id)
                    .context("requested Rex window is not in the session")?;
                plan.calls
                    .push(translate(state, &Command::SelectWindow(window.id()))?);
            }
            plan.selection = Some(session_id.clone());
        }
        NativeAction::NewSession => plan.calls.push(translate(
            state,
            &Command::CreateSession {
                title: format!("Session {}", state.sessions().len() + 1),
            },
        )?),
        NativeAction::ReloadConfig => plan.calls.push(ApiCall {
            method: "server.config_reload",
            params: json!({}),
        }),
        _ => {
            let target = resolved.context("no Rex session selected")?;
            plan.calls = session_action(state, action, &target)?;
        }
    }
    Ok(plan)
}

fn session_action(
    state: &WorkspaceState,
    action: &NativeAction,
    target: &Target<'_>,
) -> Result<Vec<ApiCall>> {
    if let Some(command) = relocation_action(state, action, target)? {
        return Ok(vec![translate(state, &command)?]);
    }
    let session = target.session;
    // Only active-context commands need a selected candidate. Explicit-ID core
    // commands already resolve their own session, avoiding an unnecessary clone.
    let active_context = matches!(
        action,
        NativeAction::NextWindow
            | NativeAction::PreviousWindow
            | NativeAction::NextPane
            | NativeAction::FocusPane { .. }
    );
    let candidate =
        if active_context && state.active_session().map(SessionView::id) != Some(session.id()) {
            let mut candidate = state.clone();
            candidate.select_session(session.id())?;
            Cow::Owned(candidate)
        } else {
            Cow::Borrowed(state)
        };
    let mut calls = Vec::new();
    let command = match action {
        NativeAction::CloseSession => Command::CloseSession(session.id()),
        NativeAction::NextPane => {
            if let Some(block) = target.block {
                calls.push(translate(&candidate, &Command::FocusPane(block.id))?);
            }
            Command::FocusNextPane
        }
        NativeAction::NewWindow => Command::CreateWindow {
            session: session.id(),
            title: format!("Terminal {}", session.windows().len() + 1),
        },
        NativeAction::CloseWindow => Command::CloseWindow(window_for_target(target)?.id()),
        NativeAction::NextWindow | NativeAction::PreviousWindow => {
            if let Some(block) = target.block {
                calls.push(translate(&candidate, &Command::FocusPane(block.id))?);
            }
            if matches!(action, NativeAction::NextWindow) {
                Command::NextWindow
            } else {
                Command::PreviousWindow
            }
        }
        NativeAction::GoToWindow { index } => {
            let position = tab_position(*index, session.windows().len())?;
            Command::SelectWindow(session.windows()[position].id())
        }
        NativeAction::Split { direction } => Command::SplitPane {
            pane: block_for_target(state, target)?.id,
            axis: match direction {
                Direction::Left | Direction::Right => SplitAxis::Horizontal,
                Direction::Up | Direction::Down => SplitAxis::Vertical,
            },
        },
        NativeAction::DetachPane => Command::DetachPane(block_for_target(state, target)?.id),
        NativeAction::ClosePane => Command::ClosePane(block_for_target(state, target)?.id),
        NativeAction::ZoomPane => Command::ToggleZoom(block_for_target(state, target)?.id),
        NativeAction::FocusPane { direction } => {
            if let Some(block) = target.block {
                calls.push(translate(&candidate, &Command::FocusPane(block.id))?);
            }
            ensure!(
                session.active_window().is_some() || target.block.is_some(),
                "Rex session has no active window"
            );
            Command::FocusDirection {
                direction: *direction,
                wrap: false,
            }
        }
        _ => bail!("action is not a worker session action"),
    };
    let mut call = translate(&candidate, &command)?;
    if let NativeAction::Split { direction } = action {
        // The legacy core split command places new panes after their anchor.
        // Rex additionally supports insertion on its left/top side.
        call.params["side"] = json!(split_placement(*direction).1);
    }
    calls.push(call);
    Ok(calls)
}

fn relocation_action(
    state: &WorkspaceState,
    action: &NativeAction,
    target: &Target<'_>,
) -> Result<Option<Command>> {
    let command = match action {
        NativeAction::MovePane {
            anchor_block_id,
            direction,
            ratio,
        } => {
            let pane = block_for_target(state, target)?.id;
            let anchor = remote_block(state, anchor_block_id)?.id;
            Command::MovePane {
                pane,
                anchor,
                axis: match direction {
                    Direction::Left | Direction::Right => SplitAxis::Horizontal,
                    Direction::Up | Direction::Down => SplitAxis::Vertical,
                },
                before: matches!(direction, Direction::Left | Direction::Up),
                ratio: *ratio,
            }
        }
        NativeAction::SwapPanes { other_block_id } => Command::SwapPanes {
            pane: block_for_target(state, target)?.id,
            other: remote_block(state, other_block_id)?.id,
        },
        NativeAction::MoveWindow { before_window_id } => Command::MoveWindow {
            window: window_for_target(target)?.id(),
            before: before_window_id
                .as_deref()
                .map(|remote| {
                    state
                        .sessions()
                        .iter()
                        .flat_map(SessionView::windows)
                        .find(|window| window.remote_id() == remote)
                        .map(WindowView::id)
                        .context("requested Rex destination window is no longer present")
                })
                .transpose()?,
        },
        _ => return Ok(None),
    };
    Ok(Some(command))
}

fn remote_block<'a>(state: &'a WorkspaceState, remote: &str) -> Result<&'a BlockView> {
    state
        .pane_ids()
        .into_iter()
        .filter_map(|pane| state.pane(pane))
        .find(|block| block.remote_id == remote)
        .context("requested Rex destination block is no longer present")
}

fn translate(state: &WorkspaceState, command: &Command) -> Result<ApiCall> {
    state
        .command(command)?
        .context("workspace command has no remote mutation")
}

fn block_for_target<'a>(state: &'a WorkspaceState, target: &Target<'a>) -> Result<&'a BlockView> {
    target
        .block
        .or_else(|| {
            target
                .session
                .active_pane()
                .and_then(|pane| state.pane(pane))
        })
        .context("Rex session has no focused block")
}

fn window_for_target<'a>(target: &Target<'a>) -> Result<&'a WindowView> {
    if let Some(block) = target.block {
        target
            .session
            .windows()
            .iter()
            .find(|window| {
                window
                    .layers
                    .iter()
                    .any(|layer| layer.blocks.contains(&block.id))
            })
            .context("requested Rex block is not in a window")
    } else {
        target
            .session
            .active_window()
            .context("Rex session has no active window")
    }
}

fn tab_position(index: i32, count: usize) -> Result<usize> {
    ensure!(index != 0, "tab index must not be zero");
    let count = i64::try_from(count).context("too many Rex tabs")?;
    let position = if index > 0 {
        i64::from(index) - 1
    } else {
        count + i64::from(index)
    };
    ensure!(
        (0..count).contains(&position),
        "requested tab position does not exist"
    );
    usize::try_from(position).context("invalid Rex tab position")
}
const fn split_placement(direction: Direction) -> (&'static str, &'static str) {
    match direction {
        Direction::Left => ("horizontal", "before"),
        Direction::Right => ("horizontal", "after"),
        Direction::Up => ("vertical", "before"),
        Direction::Down => ("vertical", "after"),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Result<WorkspaceState> {
        let mut state = WorkspaceState::default();
        for suffix in ["a", "b"] {
            state.apply_view(&json!({"session_id":format!("session:{suffix}"),"revision":1,"active_window_id":format!("window:{suffix}"),"windows":[{
                "window_id":format!("window:{suffix}"),"focused_block_id":format!("block:{suffix}"),"layers":[{
                    "layer_id":format!("layer:{suffix}"),"kind":"tiled","bounds":[0,0,1,1],"layout":{"block_id":format!("block:{suffix}"),"rect":[0,0,1,1]},
                    "blocks":[{"block_id":format!("block:{suffix}"),"creator_name":"com.superlogical.terminal"}]
                }]
            }]}))?;
        }
        Ok(state)
    }

    fn relocation_fixture() -> Result<WorkspaceState> {
        let mut state = fixture()?;
        let windows: Vec<_> = ["a", "a2"].into_iter().map(|suffix| json!({
            "window_id":format!("window:{suffix}"),"focused_block_id":format!("block:{suffix}"),"layers":[{
                "layer_id":format!("layer:{suffix}"),"kind":"tiled","bounds":[0,0,1,1],"layout":{"block_id":format!("block:{suffix}"),"rect":[0,0,1,1]},
                "blocks":[{"block_id":format!("block:{suffix}"),"creator_name":"com.superlogical.terminal"}]
            }]
        })).collect();
        state.apply_view(&json!({"session_id":"session:a","revision":2,"active_window_id":"window:a","windows":windows}))?;
        Ok(state)
    }

    #[test]
    fn relocation_actions_share_the_core_translation_and_explicit_source_context() -> Result<()> {
        let state = relocation_fixture()?;
        let context = ActionContext {
            session_id: Some("session:a".into()),
            block_id: Some("block:a2".into()),
        };
        let source = remote_block(&state, "block:a2")?.id;
        let anchor = remote_block(&state, "block:a")?.id;
        let moved = plan(
            &state,
            &NativeAction::MovePane {
                anchor_block_id: "block:a".into(),
                direction: Direction::Left,
                ratio: 0.38125,
            },
            &context,
        )?;
        assert_eq!(
            moved.calls,
            vec![translate(
                &state,
                &Command::MovePane {
                    pane: source,
                    anchor,
                    axis: SplitAxis::Horizontal,
                    before: true,
                    ratio: 0.38125
                }
            )?]
        );
        let swapped = plan(
            &state,
            &NativeAction::SwapPanes {
                other_block_id: "block:a".into(),
            },
            &context,
        )?;
        assert_eq!(swapped.calls[0].params["block_id"], "block:a2");
        let tab = plan(
            &state,
            &NativeAction::MoveWindow {
                before_window_id: Some("window:a".into()),
            },
            &context,
        )?;
        assert_eq!(tab.calls[0].params["window_id"], "window:a2");
        Ok(())
    }

    #[test]
    fn invalid_relocation_targets_fail_during_planning() -> Result<()> {
        let state = relocation_fixture()?;
        let context = ActionContext::default();
        for other in ["block:a", "block:b", "block:missing"] {
            assert!(
                plan(
                    &state,
                    &NativeAction::SwapPanes {
                        other_block_id: other.into()
                    },
                    &context
                )
                .is_err()
            );
            assert!(
                plan(
                    &state,
                    &NativeAction::MovePane {
                        anchor_block_id: other.into(),
                        direction: Direction::Right,
                        ratio: 0.5
                    },
                    &context
                )
                .is_err()
            );
        }
        for before in ["window:a", "window:b", "window:missing"] {
            assert!(
                plan(
                    &state,
                    &NativeAction::MoveWindow {
                        before_window_id: Some(before.into())
                    },
                    &context
                )
                .is_err()
            );
        }
        assert_eq!(
            state.active_session().context("session")?.remote_id(),
            "session:a"
        );
        Ok(())
    }

    #[test]
    fn native_actions_use_the_same_requests_as_core_commands() -> Result<()> {
        let state = fixture()?;
        let session = state.active_session().context("session")?;
        let pane = session.active_pane().context("pane")?;
        let window = session.active_window().context("window")?.id();
        let cases = [
            (
                NativeAction::NewSession,
                Command::CreateSession {
                    title: "Session 3".into(),
                },
            ),
            (
                NativeAction::NewWindow,
                Command::CreateWindow {
                    session: session.id(),
                    title: "Terminal 2".into(),
                },
            ),
            (
                NativeAction::CloseSession,
                Command::CloseSession(session.id()),
            ),
            (NativeAction::CloseWindow, Command::CloseWindow(window)),
            (NativeAction::NextWindow, Command::NextWindow),
            (NativeAction::PreviousWindow, Command::PreviousWindow),
            (
                NativeAction::GoToWindow { index: 1 },
                Command::SelectWindow(window),
            ),
            (
                NativeAction::Split {
                    direction: Direction::Right,
                },
                Command::SplitPane {
                    pane,
                    axis: SplitAxis::Horizontal,
                },
            ),
            (
                NativeAction::Split {
                    direction: Direction::Down,
                },
                Command::SplitPane {
                    pane,
                    axis: SplitAxis::Vertical,
                },
            ),
            (NativeAction::ClosePane, Command::ClosePane(pane)),
            (NativeAction::ZoomPane, Command::ToggleZoom(pane)),
            (
                NativeAction::FocusPane {
                    direction: Direction::Left,
                },
                Command::FocusDirection {
                    direction: Direction::Left,
                    wrap: false,
                },
            ),
        ];
        for (action, command) in cases {
            let planned = plan(&state, &action, &ActionContext::default())?;
            assert_eq!(planned.calls, vec![translate(&state, &command)?]);
        }
        Ok(())
    }

    #[test]
    fn context_is_validated_even_for_actions_without_a_target() -> Result<()> {
        let state = fixture()?;
        for context in [
            ActionContext {
                session_id: Some("session:missing".into()),
                block_id: None,
            },
            ActionContext {
                session_id: None,
                block_id: Some("block:a".into()),
            },
            ActionContext {
                session_id: Some("session:a".into()),
                block_id: Some("block:b".into()),
            },
        ] {
            assert!(plan(&state, &NativeAction::NewSession, &context).is_err());
            assert!(plan(&state, &NativeAction::ReloadConfig, &context).is_err());
        }
        assert_eq!(
            state.active_session().context("session")?.remote_id(),
            "session:a"
        );
        Ok(())
    }

    #[test]
    fn explicit_context_targets_another_session_without_changing_local_selection() -> Result<()> {
        let state = fixture()?;
        let context = ActionContext {
            session_id: Some("session:b".into()),
            block_id: Some("block:b".into()),
        };
        let action = plan(&state, &NativeAction::ClosePane, &context)?;
        assert_eq!(action.selection.as_deref(), Some("session:b"));
        assert_eq!(action.calls[0].method, "block.close");
        assert_eq!(action.calls[0].params["block_id"], "block:b");
        assert_eq!(
            state.active_session().context("session")?.remote_id(),
            "session:a"
        );
        Ok(())
    }

    #[test]
    fn selection_rejects_cross_session_window_before_mutating() -> Result<()> {
        let state = fixture()?;
        assert!(
            plan(
                &state,
                &NativeAction::SelectSession {
                    session_id: "session:a".into(),
                    window_id: Some("window:b".into())
                },
                &ActionContext::default()
            )
            .is_err()
        );
        assert_eq!(
            state.active_session().context("session")?.remote_id(),
            "session:a"
        );
        Ok(())
    }

    #[test]
    fn split_directions_preserve_before_and_after_placement() {
        assert_eq!(split_placement(Direction::Left), ("horizontal", "before"));
        assert_eq!(split_placement(Direction::Right), ("horizontal", "after"));
        assert_eq!(split_placement(Direction::Up), ("vertical", "before"));
        assert_eq!(split_placement(Direction::Down), ("vertical", "after"));
    }

    #[test]
    fn tab_positions_are_checked_without_signed_overflow() -> Result<()> {
        assert_eq!(tab_position(1, 3)?, 0);
        assert_eq!(tab_position(3, 3)?, 2);
        assert_eq!(tab_position(-1, 3)?, 2);
        assert_eq!(tab_position(-3, 3)?, 0);
        for (index, count) in [(0, 3), (4, 3), (-4, 3), (i32::MIN, 3), (1, 0)] {
            assert!(tab_position(index, count).is_err());
        }
        Ok(())
    }

    #[test]
    fn directional_focus_anchors_the_explicit_block_first() -> Result<()> {
        let state = fixture()?;
        let context = ActionContext {
            session_id: Some("session:b".into()),
            block_id: Some("block:b".into()),
        };
        let action = plan(
            &state,
            &NativeAction::FocusPane {
                direction: Direction::Left,
            },
            &context,
        )?;
        assert_eq!(action.calls.len(), 2);
        assert_eq!(action.calls[0].method, "session.focus_block");
        assert_eq!(action.calls[1].method, "session.focus_direction");
        assert_eq!(action.calls[1].params["wrap"], false);
        assert_eq!(action.calls[1].params["session_id"], "session:b");
        Ok(())
    }
}
