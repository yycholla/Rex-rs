use super::{
    GhosttyTerminal, TerminalConfig,
    pty::{PaneStatus, PtySession, ShellConfig},
};
use crate::core::{Command, GridSize, PaneId, Workspace, frame::TerminalFrame, input::KeyInput};
use anyhow::{Context, Result};
use std::{collections::BTreeMap, sync::Arc};

struct PaneRuntime {
    terminal: GhosttyTerminal,
    frame: Arc<TerminalFrame>,
    process: Option<PtySession>,
    viewport: (GridSize, u32, u32),
}

/// Couples pure layout state to terminal ownership. GPUI is not involved here.
pub struct TerminalWorkspace {
    state: Workspace,
    panes: BTreeMap<PaneId, PaneRuntime>,
    config: TerminalConfig,
    shell: Option<ShellConfig>,
}

impl TerminalWorkspace {
    /// Create a headless workspace with no shell processes.
    #[must_use]
    pub fn new(config: TerminalConfig) -> Self {
        Self {
            state: Workspace::default(),
            panes: BTreeMap::new(),
            config,
            shell: None,
        }
    }

    /// Create a workspace that starts a PTY process for each newly added pane.
    #[must_use]
    pub fn live(config: TerminalConfig, shell: ShellConfig) -> Self {
        Self {
            shell: Some(shell),
            ..Self::new(config)
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
    #[must_use]
    pub fn status(&self, pane: PaneId) -> Option<&PaneStatus> {
        self.panes
            .get(&pane)?
            .process
            .as_ref()
            .map(PtySession::status)
    }
    #[must_use]
    pub fn process_id(&self, pane: PaneId) -> Option<u32> {
        self.panes.get(&pane)?.process.as_ref()?.process_id()
    }

    /// Prepare resources before committing. A failed allocation leaves the
    /// existing workspace and its terminal contents intact.
    ///
    /// # Errors
    ///
    /// Returns an error if the command is invalid, a terminal or frame cannot
    /// be created, or a configured shell cannot be spawned. New processes are
    /// cleaned up asynchronously if the candidate workspace cannot be committed.
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
            let process = self
                .shell
                .as_ref()
                .map(|shell| PtySession::spawn(shell, self.config.size))
                .transpose()
                .with_context(|| format!("start shell for pane {id}"))?;
            added.push((
                *id,
                PaneRuntime {
                    terminal,
                    frame,
                    process,
                    viewport: (self.config.size, 0, 0),
                },
            ));
        }
        self.panes.retain(|id, _| ids.contains(id));
        self.panes.extend(added);
        self.state = candidate;
        Ok(())
    }

    /// Drain a bounded amount of pending output from each PTY without blocking.
    ///
    /// Returns true when a frame or process status changed.
    ///
    /// # Errors
    ///
    /// Returns an error if an updated terminal frame cannot be read or a device
    /// response cannot be queued back to a running process. Already consumed
    /// output is not rolled back.
    pub fn poll(&mut self) -> Result<bool> {
        let mut changed = false;
        for runtime in self.panes.values_mut() {
            let Some(process) = &mut runtime.process else {
                continue;
            };
            let update = process.poll();
            changed |= update.changed;
            if update.output.is_empty() {
                continue;
            }
            for bytes in update.output {
                runtime.terminal.feed(&bytes);
            }
            runtime.frame = Arc::new(runtime.terminal.snapshot()?);
            let replies = runtime.terminal.take_pending_output();
            if matches!(process.status(), PaneStatus::Running) && !replies.is_empty() {
                process.write(&replies)?;
            }
        }
        Ok(changed)
    }

    /// Queue user input without blocking the application thread.
    ///
    /// # Errors
    ///
    /// Returns an error if the pane or live process is missing, its input worker
    /// has stopped, the bounded queue is full, or the message exceeds 64 KiB.
    pub fn write_input(&mut self, pane: PaneId, bytes: &[u8]) -> Result<()> {
        let runtime = self
            .panes
            .get_mut(&pane)
            .with_context(|| format!("pane {pane} no longer exists"))?;
        let process = runtime.process.as_ref().context("pane has no live shell")?;
        process.write(bytes)?;
        runtime.terminal.scroll_to_bottom();
        runtime.frame = Arc::new(runtime.terminal.snapshot()?);
        Ok(())
    }

    /// Encode a key according to the terminal's active keyboard protocol.
    ///
    /// # Errors
    ///
    /// Returns an error if key encoding fails or input cannot be queued.
    pub fn send_key(&mut self, pane: PaneId, key: &KeyInput) -> Result<()> {
        let bytes = self
            .panes
            .get_mut(&pane)
            .with_context(|| format!("pane {pane} no longer exists"))?
            .terminal
            .encode_key(key)?;
        self.write_input(pane, &bytes)
    }

    /// Encode a paste, including bracketed-paste markers when requested by the terminal.
    ///
    /// # Errors
    ///
    /// Returns an error if paste encoding fails or input cannot be queued.
    pub fn paste(&mut self, pane: PaneId, text: &str) -> Result<()> {
        let bytes = self
            .panes
            .get(&pane)
            .with_context(|| format!("pane {pane} no longer exists"))?
            .terminal
            .encode_paste(text)?;
        self.write_input(pane, &bytes)
    }

    /// Move the viewport by history lines; positive values scroll into history.
    ///
    /// # Errors
    ///
    /// Returns an error if the pane is missing or its viewport cannot be updated.
    pub fn scroll(&mut self, pane: PaneId, lines: i32) -> Result<()> {
        let runtime = self
            .panes
            .get_mut(&pane)
            .with_context(|| format!("pane {pane} no longer exists"))?;
        runtime.terminal.scroll_lines(lines)?;
        runtime.frame = Arc::new(runtime.terminal.snapshot()?);
        Ok(())
    }

    /// Feed output and cache a fresh frame. Headless workspaces return response
    /// bytes; live workspaces queue responses for PTY write-back and return none.
    ///
    /// # Errors
    ///
    /// Returns an error if the pane no longer exists, its frame cannot be read,
    /// or a response cannot be queued. A snapshot error does not undo consumed output.
    pub fn feed(&mut self, pane: PaneId, bytes: &[u8]) -> Result<Vec<u8>> {
        let runtime = self
            .panes
            .get_mut(&pane)
            .with_context(|| format!("pane {pane} no longer exists"))?;
        runtime.terminal.feed(bytes);
        runtime.frame = Arc::new(runtime.terminal.snapshot()?);
        write_back(runtime)
    }

    /// Resize a pane and its PTY. Headless workspaces return response bytes;
    /// live workspaces queue responses for PTY write-back and return none.
    ///
    /// # Errors
    ///
    /// Returns an error if the pane is missing, resize fails, the updated frame
    /// cannot be read, or a response cannot be queued. Completed IO is not undone.
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
        if runtime.viewport == (size, cell_width, cell_height) {
            return Ok(Vec::new());
        }
        if let Some(process) = &runtime.process {
            process.resize(size, cell_width, cell_height)?;
        }
        runtime.terminal.resize(size, cell_width, cell_height)?;
        runtime.frame = Arc::new(runtime.terminal.snapshot()?);
        runtime.viewport = (size, cell_width, cell_height);
        write_back(runtime)
    }
}

fn write_back(runtime: &mut PaneRuntime) -> Result<Vec<u8>> {
    let replies = runtime.terminal.take_pending_output();
    if let Some(process) = &runtime.process {
        if matches!(process.status(), PaneStatus::Running) {
            process.write(&replies)?;
        }
        Ok(Vec::new())
    } else {
        Ok(replies)
    }
}
