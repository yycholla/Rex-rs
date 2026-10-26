use super::{GhosttyTerminal, TerminalConfig};
use crate::core::{Command, GridSize, PaneId, Workspace, frame::TerminalFrame};
use anyhow::{Context, Result};
use std::{collections::BTreeMap, sync::Arc};

struct PaneRuntime {
    terminal: GhosttyTerminal,
    frame: Arc<TerminalFrame>,
}

/// Couples pure layout state to terminal ownership. GPUI is not involved here.
pub struct TerminalWorkspace {
    state: Workspace,
    panes: BTreeMap<PaneId, PaneRuntime>,
    config: TerminalConfig,
}

impl TerminalWorkspace {
    #[must_use]
    pub fn new(config: TerminalConfig) -> Self {
        Self {
            state: Workspace::default(),
            panes: BTreeMap::new(),
            config,
        }
    }

    #[must_use]
    pub fn state(&self) -> &Workspace {
        &self.state
    }
    #[must_use]
    pub fn frame(&self, pane: PaneId) -> Option<&Arc<TerminalFrame>> {
        self.panes.get(&pane).map(|runtime| &runtime.frame)
    }

    /// Prepare resources before committing. A failed allocation leaves the
    /// existing workspace and its terminal contents intact.
    ///
    /// # Errors
    ///
    /// Returns an error if the command is invalid or a new pane's terminal or
    /// initial frame cannot be created. The current workspace is unchanged.
    pub fn apply(&mut self, command: Command) -> Result<()> {
        let mut candidate = self.state.clone();
        candidate.apply(command)?;
        let ids = candidate.pane_ids();
        let mut added = Vec::new();
        for id in &ids {
            if self.panes.contains_key(id) {
                continue;
            }
            let mut terminal = GhosttyTerminal::new(self.config)
                .with_context(|| format!("create terminal for pane {id}"))?;
            let frame = Arc::new(terminal.snapshot()?);
            added.push((*id, PaneRuntime { terminal, frame }));
        }
        self.panes.retain(|id, _| ids.contains(id));
        self.panes.extend(added);
        self.state = candidate;
        Ok(())
    }

    /// Feed PTY output, cache a fresh frame, and return responses for PTY write-back.
    ///
    /// # Errors
    ///
    /// Returns an error if the pane no longer exists or its updated terminal
    /// frame cannot be read. A snapshot error does not undo consumed output.
    pub fn feed(&mut self, pane: PaneId, bytes: &[u8]) -> Result<Vec<u8>> {
        let runtime = self
            .panes
            .get_mut(&pane)
            .with_context(|| format!("pane {pane} no longer exists"))?;
        runtime.terminal.feed(bytes);
        runtime.frame = Arc::new(runtime.terminal.snapshot()?);
        Ok(runtime.terminal.take_pending_output())
    }

    /// Resize a pane and return any responses for PTY write-back.
    ///
    /// # Errors
    ///
    /// Returns an error if the pane no longer exists, Ghostty cannot resize it,
    /// or the updated frame cannot be read. A snapshot error does not undo a
    /// completed resize.
    pub fn resize(
        &mut self,
        pane: PaneId,
        size: GridSize,
        cell_width: u32,
        cell_height: u32,
    ) -> Result<Vec<u8>> {
        let runtime = self
            .panes
            .get_mut(&pane)
            .with_context(|| format!("pane {pane} no longer exists"))?;
        runtime.terminal.resize(size, cell_width, cell_height)?;
        runtime.frame = Arc::new(runtime.terminal.snapshot()?);
        Ok(runtime.terminal.take_pending_output())
    }
}
