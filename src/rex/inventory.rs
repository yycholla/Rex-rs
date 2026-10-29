//! Pair layout and block inventory before publishing an authoritative cache.
use super::{transport::ControlConnection, workspace::WorkspaceState};
use anyhow::Result;
use serde_json::json;

pub(crate) fn refresh(
    control: &mut ControlConnection,
    state: &mut WorkspaceState,
    session: &str,
) -> Result<bool> {
    let view = control.call("session.view", json!({"session_id":session}))?;
    let inventory = control.call("session.list_blocks", json!({"session_id":session}))?;
    state.apply_snapshots(&[(view, inventory)])
}

pub(crate) struct RefreshQueue {
    pending: std::collections::VecDeque<String>,
    next: std::time::Instant,
}

impl Default for RefreshQueue {
    fn default() -> Self {
        Self {
            pending: std::collections::VecDeque::new(),
            next: std::time::Instant::now() + std::time::Duration::from_secs(2),
        }
    }
}

impl RefreshQueue {
    pub(crate) fn next_session(&mut self, state: &WorkspaceState) -> Option<String> {
        if self.pending.is_empty() && std::time::Instant::now() >= self.next {
            self.pending.extend(
                state
                    .sessions()
                    .iter()
                    .map(|session| session.remote_id().to_owned()),
            );
            self.next = std::time::Instant::now() + std::time::Duration::from_secs(2);
        }
        while let Some(id) = self.pending.pop_front() {
            if state
                .sessions()
                .iter()
                .any(|session| session.remote_id() == id)
            {
                return Some(id);
            }
        }
        None
    }
}
