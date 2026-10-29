//! Restore an existing block without ever closing it during carrier cleanup.
use super::{
    transport::ControlConnection,
    workspace::{ApiCall, WorkspaceState},
};
use crate::core::{Command, PaneId};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};

pub(crate) fn execute(
    control: &mut ControlConnection,
    state: &mut WorkspaceState,
    pane: PaneId,
    title: &str,
) -> Result<Value> {
    let command = Command::RestorePane {
        pane,
        title: title.into(),
    };
    let create = state
        .command(&command)?
        .context("restore request lacks a carrier")?;
    let block = state
        .pane(pane)
        .context("detached block disappeared")?
        .remote_id
        .clone();
    let session = create.params["session_id"]
        .as_str()
        .context("restore session missing")?
        .to_owned();
    let outcome = run(
        |method, params| control.call(method, params),
        create,
        &block,
    );
    let refreshed = super::inventory::refresh(control, state, &session);
    match outcome {
        Ok(mut value) => {
            refreshed.context("Restore completed but its view could not be refreshed")?;
            let selected = state
                .sessions()
                .iter()
                .find(|entry| entry.remote_id() == session)
                .context("Restored session no longer exists")?
                .id();
            let placed = state
                .sessions()
                .iter()
                .find(|entry| entry.id() == selected)
                .and_then(|entry| {
                    entry.windows().iter().find(|window| {
                        window
                            .layers
                            .iter()
                            .any(|layer| layer.blocks.contains(&pane))
                    })
                })
                .context("Restored block is no longer placed in its session")?
                .remote_id()
                .to_owned();
            value["window_id"] = json!(placed);
            state.select_session(selected)?;
            Ok(value)
        }
        Err(error) => Err(error.context(match refreshed {
            Ok(_) => "Restore failed; authoritative placement was refreshed".to_owned(),
            Err(refresh) => {
                format!("Restore failed and placement could not be refreshed: {refresh:#}")
            }
        })),
    }
}

fn run(
    mut call: impl FnMut(&str, Value) -> Result<Value>,
    create: ApiCall,
    original: &str,
) -> Result<Value> {
    let session = create.params["session_id"]
        .as_str()
        .context("missing carrier session")?
        .to_owned();
    let created = call(create.method, create.params)?;
    let window = created["window_id"]
        .as_str()
        .filter(|id| id.starts_with("window:"))
        .context("carrier response lacks a window ID")?
        .to_owned();
    let blocks = created["block_ids"]
        .as_array()
        .context("carrier response lacks block IDs")?;
    ensure!(
        blocks.len() == 1,
        "carrier response must identify exactly one new block"
    );
    let anchor = blocks[0]
        .as_str()
        .filter(|id| id.starts_with("block:"))
        .context("carrier response lacks a block ID")?;
    ensure!(
        anchor != original,
        "carrier response reused the block being restored"
    );
    let moved = call(
        "session.move_block",
        json!({"session_id":session,"block_id":original,"anchor_block_id":anchor,"direction":"horizontal","side":"after","ratio":0.5}),
    );
    // Even a lost move response can mean the original is already in the new
    // window. Close ONLY the newly created anchor, never the whole window.
    let cleanup = call(
        "block.close",
        json!({"session_id":session,"block_id":anchor}),
    );
    match (moved, cleanup) {
        (Ok(_), Ok(value)) => {
            Ok(json!({"window_id":window,"block_id":original,"revision":value["revision"]}))
        }
        (Err(error), Ok(_)) => {
            Err(error.context("Restore placement failed or is uncertain; temporary anchor removed"))
        }
        (Ok(_), Err(error)) => {
            Err(error.context("Block restored, but the temporary anchor could not be removed"))
        }
        (Err(error), Err(cleanup)) => Err(error.context(format!(
            "Restore placement is uncertain; anchor cleanup also failed: {cleanup:#}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    fn create() -> ApiCall {
        ApiCall {
            method: "session.new_window",
            params: json!({"session_id":"session:test","layout":{"block":{"flavor":"com.superlogical.terminal.shell","options":{"shell":"none","command":["/bin/true"],"exit":{"on_completion":false}}}}}),
        }
    }

    #[test]
    fn uncertain_move_cleans_only_its_anchor_and_never_replays_original() {
        let mut calls = Vec::new();
        let result = run(
            |method, params| {
                calls.push((method.to_owned(), params));
                match calls.len() {
                    1 => Ok(json!({"window_id":"window:new","block_ids":["block:anchor"]})),
                    2 => bail!("reply lost after possible placement"),
                    _ => Ok(json!({"revision":4})),
                }
            },
            create(),
            "block:original",
        );
        assert!(result.is_err());
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[1].0, "session.move_block");
        assert_eq!(calls[1].1["block_id"], "block:original");
        assert_eq!(calls[2].0, "block.close");
        assert_eq!(calls[2].1["block_id"], "block:anchor");
        assert!(
            calls
                .iter()
                .all(|(method, _)| method != "session.close_window")
        );
    }

    #[test]
    fn malformed_carrier_cannot_close_the_original() {
        let mut count = 0;
        let result = run(
            |_, _| {
                count += 1;
                Ok(json!({"window_id":"window:new","block_ids":["block:original"]}))
            },
            create(),
            "block:original",
        );
        assert!(result.is_err());
        assert_eq!(count, 1);
    }
}
