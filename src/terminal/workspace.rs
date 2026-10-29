use super::{
    GhosttyTerminal, TerminalConfig,
    pty::{PaneStatus, PtySession, ShellConfig},
};
use crate::core::{Command, GridSize, PaneId, Workspace, frame::TerminalFrame, input::KeyInput};
use anyhow::{Context, Result};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

const POLL_OUTPUT_BYTES: usize = 32 * 1024;
const PANE_OUTPUT_BYTES: usize = 8 * 1024;
const POLL_OUTPUT_TIME: Duration = Duration::from_millis(4);

struct OutputBudget {
    remaining: usize,
    background_served: bool,
}

impl OutputBudget {
    fn new() -> Self {
        Self {
            remaining: POLL_OUTPUT_BYTES,
            background_served: false,
        }
    }

    fn allowance(&self, elapsed: Duration) -> usize {
        if self.background_served && elapsed >= POLL_OUTPUT_TIME {
            0
        } else {
            self.remaining.min(PANE_OUTPUT_BYTES)
        }
    }

    fn record(&mut self, bytes: usize, focused: bool) {
        self.remaining -= bytes;
        self.background_served |= !focused;
    }
}

fn poll_order(
    mut panes: Vec<PaneId>,
    focused: Option<PaneId>,
    after: Option<PaneId>,
) -> Vec<PaneId> {
    let priority = focused.and_then(|focused| {
        panes
            .iter()
            .position(|pane| *pane == focused)
            .map(|index| panes.remove(index))
    });
    if let Some(after) = after {
        let start = panes.partition_point(|pane| *pane <= after);
        panes.rotate_left(start);
    }
    if let Some(priority) = priority {
        panes.insert(0, priority);
    }
    panes
}

struct PaneRuntime {
    terminal: GhosttyTerminal,
    frame: Arc<TerminalFrame>,
    process: Option<PtySession>,
    viewport: (GridSize, u32, u32),
    frame_dirty: bool,
}

/// Couples pure layout state to terminal ownership. GPUI is not involved here.
pub struct TerminalWorkspace {
    state: Workspace,
    panes: BTreeMap<PaneId, PaneRuntime>,
    config: TerminalConfig,
    shell: Option<ShellConfig>,
    /// Closed panes remain owned until their asynchronous reaper completes.
    retiring: Vec<PtySession>,
    /// Last background service slot, independent of pane insertion/removal.
    polled_background: Option<PaneId>,
    defer_hidden_frames: bool,
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
            retiring: Vec::new(),
            polled_background: None,
            defer_hidden_frames: false,
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

    /// Recover saved layout metadata with fresh terminals and fresh shell processes.
    /// No process ID, terminal output or shell job is restored.
    ///
    /// # Errors
    /// Returns an error for invalid state or a terminal/shell startup failure.
    pub fn restore(state: Workspace, config: TerminalConfig, shell: ShellConfig) -> Result<Self> {
        anyhow::ensure!(
            state.is_valid(),
            "cannot restore invalid workspace metadata"
        );
        let mut workspace = Self::live(config, shell);
        for pane in state.pane_ids() {
            match workspace.create_runtime() {
                Ok(runtime) => {
                    workspace.panes.insert(pane, runtime);
                }
                Err(error) => {
                    let _ = workspace.shutdown(Duration::from_secs(2));
                    return Err(error)
                        .with_context(|| format!("restore pane {pane} with a new shell"));
                }
            }
        }
        workspace.state = state;
        Ok(workspace)
    }

    fn create_runtime(&self) -> Result<PaneRuntime> {
        let mut terminal = GhosttyTerminal::new(self.config)?;
        let frame = Arc::new(terminal.snapshot()?);
        let process = self
            .shell
            .as_ref()
            .map(|shell| PtySession::spawn(shell, self.config.size))
            .transpose()?;
        Ok(PaneRuntime {
            terminal,
            frame,
            process,
            viewport: (self.config.size, 0, 0),
            frame_dirty: false,
        })
    }

    #[must_use]
    pub fn state(&self) -> &Workspace {
        &self.state
    }
    /// Latest materialized owned frame. With hidden-frame deferral enabled,
    /// call [`Self::refresh_all_frames`] before an authoritative all-pane read.
    #[must_use]
    pub fn frame(&self, pane: PaneId) -> Option<&Arc<TerminalFrame>> {
        self.panes.get(&pane).map(|runtime| &runtime.frame)
    }

    /// Keep parsing hidden terminals while deferring their owned frame copies.
    /// Their published caches refresh when visible or explicitly requested with
    /// [`Self::refresh_all_frames`]. The default preserves eager frame updates.
    pub fn defer_hidden_frame_updates(&mut self) {
        self.defer_hidden_frames = true;
    }

    /// Materialize every pending frame, including hidden tabs and zoomed panes.
    ///
    /// # Errors
    /// Returns an error if Ghostty cannot snapshot a dirty terminal.
    pub fn refresh_all_frames(&mut self) -> Result<bool> {
        let mut changed = false;
        for runtime in self
            .panes
            .values_mut()
            .filter(|runtime| runtime.frame_dirty)
        {
            changed |= refresh_runtime_frame(runtime)?;
        }
        Ok(changed)
    }

    fn visible_panes(&self) -> Vec<PaneId> {
        if !self.defer_hidden_frames {
            return self.panes.keys().copied().collect();
        }
        self.state
            .active_session()
            .and_then(crate::core::Session::active_window)
            .map_or_else(Vec::new, |window| {
                window
                    .zoomed_pane()
                    .map_or_else(|| window.layout().panes(), |pane| vec![pane])
            })
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

    /// Terminate all owned PTYs and wait within one shared deadline for reaping.
    /// Call only when the standalone server is exiting, never from the UI.
    /// Ordinary pane close and client disconnect do not invoke this method.
    ///
    /// # Errors
    /// Returns an error if any child fails to reap before the deadline.
    pub fn shutdown(mut self, timeout: Duration) -> Result<()> {
        let start = Instant::now();
        for runtime in self.panes.values() {
            if let Some(process) = &runtime.process {
                process.begin_shutdown();
            }
        }
        for process in &self.retiring {
            process.begin_shutdown();
        }
        let mut errors = Vec::new();
        for process in &mut self.retiring {
            if let Err(error) = process.wait_for_shutdown(timeout.saturating_sub(start.elapsed())) {
                errors.push(format!("closed pane: {error:#}"));
            }
        }
        for (pane, runtime) in &mut self.panes {
            if let Some(process) = &mut runtime.process
                && let Err(error) =
                    process.wait_for_shutdown(timeout.saturating_sub(start.elapsed()))
            {
                errors.push(format!("pane {pane}: {error:#}"));
            }
        }
        self.panes.clear();
        anyhow::ensure!(errors.is_empty(), "server shutdown: {}", errors.join("; "));
        Ok(())
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
        self.retiring.retain_mut(|process| !process.is_reaped());
        let mut candidate = self.state.clone();
        candidate.apply(command)?;
        let ids = candidate.pane_ids();
        let removed = self
            .panes
            .iter()
            .filter(|(id, runtime)| !ids.contains(id) && runtime.process.is_some())
            .count();
        anyhow::ensure!(
            self.retiring.len() + removed <= 64,
            "process cleanup queue is full; try again"
        );
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
                    frame_dirty: false,
                },
            ));
        }
        for (id, runtime) in &mut self.panes {
            if !ids.contains(id)
                && let Some(process) = runtime.process.take()
            {
                process.begin_shutdown();
                self.retiring.push(process);
            }
        }
        self.panes.retain(|id, _| ids.contains(id));
        self.panes.extend(added);
        self.state = candidate;
        Ok(())
    }

    /// Drain a globally bounded amount of PTY output without blocking. Focused
    /// output gets the first slice, followed by rotating background service.
    /// Status checks still run on every pane when output service is exhausted.
    ///
    /// Returns true when a frame or process status changed.
    ///
    /// # Errors
    ///
    /// Returns an error if an updated terminal frame cannot be read or a device
    /// response cannot be queued back to a running process. Already consumed
    /// output is not rolled back.
    pub fn poll(&mut self) -> Result<bool> {
        self.retiring.retain_mut(|process| !process.is_reaped());
        let mut changed = false;
        let visible = self.visible_panes();
        let focused = self
            .state
            .active_session()
            .and_then(crate::core::Session::active_pane);
        let panes = poll_order(
            self.panes
                .iter()
                .filter_map(|(pane, runtime)| runtime.process.is_some().then_some(*pane))
                .collect(),
            focused,
            self.polled_background,
        );
        let started = Instant::now();
        let mut budget = OutputBudget::new();
        for pane in panes {
            let Some(runtime) = self.panes.get_mut(&pane) else {
                continue;
            };
            let Some(process) = &mut runtime.process else {
                continue;
            };
            let previous_status = process.status().clone();
            let allowance = budget.allowance(started.elapsed());
            let update = process.poll(allowance);
            if allowance != 0 {
                let bytes = update.output.iter().map(Vec::len).sum();
                budget.record(bytes, Some(pane) == focused);
                if Some(pane) != focused {
                    self.polled_background = Some(pane);
                }
            }
            if !update.changed {
                continue;
            }
            changed |= process.status() != &previous_status;
            if update.output.is_empty() {
                continue;
            }
            for bytes in update.output {
                runtime.frame_dirty = true;
                runtime.terminal.feed(&bytes);
            }
            let replies = runtime.terminal.take_pending_output();
            if matches!(process.status(), PaneStatus::Running) && !replies.is_empty() {
                process.write(&replies)?;
            }
            if visible.contains(&pane) {
                changed |= refresh_runtime_frame(runtime)?;
            }
        }
        // A tab/zoom/focus command may expose a dirty cache without another
        // byte arriving. Publish it on this same poll, including headless tests.
        for pane in visible {
            if let Some(runtime) = self.panes.get_mut(&pane)
                && runtime.frame_dirty
            {
                changed |= refresh_runtime_frame(runtime)?;
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
        if !bytes.is_empty() && !runtime.terminal.is_at_bottom()? {
            runtime.frame_dirty = true;
            runtime.terminal.scroll_to_bottom();
            refresh_runtime_frame(runtime)?;
        }
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
        runtime.frame_dirty = true;
        runtime.terminal.scroll_lines(lines)?;
        refresh_runtime_frame(runtime)?;
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
        runtime.frame_dirty = true;
        runtime.terminal.feed(bytes);
        refresh_runtime_frame(runtime)?;
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
        runtime.frame_dirty = true;
        runtime.terminal.resize(size, cell_width, cell_height)?;
        refresh_runtime_frame(runtime)?;
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

fn refresh_runtime_frame(runtime: &mut PaneRuntime) -> Result<bool> {
    let changed = refresh_frame(&mut runtime.terminal, &mut runtime.frame)?;
    runtime.frame_dirty = false;
    Ok(changed)
}

/// Preserve outer frame identity when an update changed no visible cells or metadata.
fn refresh_frame(terminal: &mut GhosttyTerminal, cached: &mut Arc<TerminalFrame>) -> Result<bool> {
    let frame = terminal.snapshot()?;
    let unchanged = frame.size == cached.size
        && frame.cursor == cached.cursor
        && frame.foreground == cached.foreground
        && frame.background == cached.background
        && frame.rows.len() == cached.rows.len()
        && frame
            .rows
            .iter()
            .zip(&cached.rows)
            .all(|(next, previous)| Arc::ptr_eq(next, previous));
    if !unchanged {
        *cached = Arc::new(frame);
    }
    Ok(!unchanged)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Scheduling fixtures use known nonzero pane IDs."
)]
mod scheduling_tests {
    use super::*;

    fn ids(values: &[u64]) -> Vec<PaneId> {
        values
            .iter()
            .map(|value| PaneId::from_value(*value).unwrap())
            .collect()
    }

    #[test]
    fn focused_service_preserves_rotating_background_progress() {
        let panes = ids(&[1, 2, 3, 4, 5]);
        let focused = panes[2];
        let mut after = None;
        let mut serviced = Vec::new();
        // Model the slowest allowed output turn: focused slice plus one
        // background slice. Every background still receives a turn.
        for _ in 0..4 {
            let order = poll_order(panes.clone(), Some(focused), after);
            assert_eq!(order[0], focused);
            serviced.push(order[1]);
            after = Some(order[1]);
        }
        assert_eq!(serviced, ids(&[1, 2, 4, 5]));
        let order = poll_order(panes, Some(focused), after);
        assert_eq!(order[1], ids(&[1])[0]);
    }

    #[test]
    fn rotation_survives_removal_and_focus_changes() {
        let panes = ids(&[1, 3, 4]);
        let after = ids(&[2])[0];
        assert_eq!(
            poll_order(panes.clone(), Some(panes[2]), Some(after)),
            ids(&[4, 3, 1])
        );
        assert_eq!(
            poll_order(panes, Some(ids(&[1])[0]), Some(after)),
            ids(&[1, 3, 4])
        );
    }

    #[test]
    fn global_output_budget_and_soft_deadline_preserve_one_background_slice() {
        let mut budget = OutputBudget::new();
        assert_eq!(budget.allowance(Duration::ZERO), PANE_OUTPUT_BYTES);
        budget.record(PANE_OUTPUT_BYTES, true);
        assert_eq!(budget.allowance(POLL_OUTPUT_TIME), PANE_OUTPUT_BYTES);
        budget.record(PANE_OUTPUT_BYTES, false);
        assert_eq!(budget.allowance(POLL_OUTPUT_TIME), 0);

        let mut budget = OutputBudget::new();
        let mut consumed = 0;
        for _ in 0..16 {
            let allowance = budget.allowance(Duration::ZERO);
            consumed += allowance;
            budget.record(allowance, false);
        }
        assert_eq!(consumed, POLL_OUTPUT_BYTES);
        assert_eq!(budget.allowance(Duration::ZERO), 0);
    }
}

#[cfg(test)]
mod deferred_frame_tests {
    use super::*;
    use crate::core::SplitAxis;

    fn fixture() -> Result<(TerminalWorkspace, PaneId, PaneId)> {
        let mut workspace = TerminalWorkspace::new(TerminalConfig {
            size: GridSize::new(16, 3)?,
            ..TerminalConfig::default()
        });
        workspace.apply(Command::CreateSession {
            title: "deferred".into(),
        })?;
        let first = workspace.state.pane_ids()[0];
        workspace.apply(Command::SplitPane {
            pane: first,
            axis: SplitAxis::Horizontal,
        })?;
        let second = workspace
            .state
            .pane_ids()
            .into_iter()
            .find(|pane| *pane != first)
            .context("fixture second pane")?;
        workspace.apply(Command::ToggleZoom(second))?;
        Ok((workspace, first, second))
    }

    fn parsed_output(workspace: &mut TerminalWorkspace, pane: PaneId, bytes: &[u8]) -> Result<()> {
        let runtime = workspace.panes.get_mut(&pane).context("fixture pane")?;
        // The same boundary as PTY poll: bytes have reached Ghostty, but have
        // not yet been copied to an owned frame. No process/IO is required.
        runtime.terminal.feed(bytes);
        runtime.frame_dirty = true;
        Ok(())
    }

    #[test]
    fn detached_only_session_keeps_its_replica_and_restores_pending_output() -> Result<()> {
        let mut workspace = TerminalWorkspace::new(TerminalConfig {
            size: GridSize::new(24, 3)?,
            ..TerminalConfig::default()
        });
        workspace.apply(Command::CreateSession {
            title: "detached".into(),
        })?;
        let pane = workspace.state.pane_ids()[0];
        workspace.feed(pane, b"before")?;
        let frame = Arc::clone(workspace.frame(pane).context("fixture frame")?);
        workspace.defer_hidden_frame_updates();
        workspace.apply(Command::DetachPane(pane))?;
        assert!(
            workspace
                .state
                .active_session()
                .and_then(crate::core::Session::active_window)
                .is_none()
        );
        assert_eq!(workspace.visible_panes(), []);
        assert!(Arc::ptr_eq(
            &frame,
            workspace.frame(pane).context("retained detached frame")?
        ));
        parsed_output(&mut workspace, pane, b" while detached")?;
        assert!(!workspace.poll()?);
        assert!(Arc::ptr_eq(
            &frame,
            workspace.frame(pane).context("deferred detached frame")?
        ));
        workspace.apply(Command::RestorePane {
            pane,
            title: "restored".into(),
        })?;
        assert!(workspace.poll()?);
        assert_eq!(
            workspace
                .frame(pane)
                .context("restored frame")?
                .text_lines()[0],
            "before while detached"
        );
        assert_eq!(workspace.panes.len(), 1);
        assert!(workspace.state.is_valid());
        Ok(())
    }

    #[test]
    fn zoomed_hidden_output_is_parsed_and_materializes_when_exposed() -> Result<()> {
        let (mut workspace, hidden, visible) = fixture()?;
        workspace.defer_hidden_frame_updates();
        let cached = Arc::clone(workspace.frame(hidden).context("hidden cache")?);
        parsed_output(&mut workspace, hidden, b"hidden marker")?;
        assert!(!workspace.poll()?);
        assert!(Arc::ptr_eq(
            workspace.frame(hidden).context("hidden cache")?,
            &cached
        ));
        assert!(workspace.panes[&hidden].frame_dirty);
        workspace.apply(Command::ToggleZoom(visible))?;
        assert!(workspace.poll()?);
        assert_eq!(
            workspace.panes[&hidden].frame.text_lines()[0],
            "hidden marker"
        );
        assert!(!workspace.panes[&hidden].frame_dirty);
        assert!(!workspace.poll()?);
        Ok(())
    }

    #[test]
    fn explicit_full_refresh_includes_hidden_frames_without_changing_visibility() -> Result<()> {
        let (mut workspace, hidden, visible) = fixture()?;
        workspace.defer_hidden_frame_updates();
        parsed_output(&mut workspace, hidden, b"authoritative")?;
        assert!(!workspace.poll()?);
        assert!(workspace.refresh_all_frames()?);
        assert_eq!(workspace.visible_panes(), [visible]);
        assert_eq!(
            workspace.panes[&hidden].frame.text_lines()[0],
            "authoritative"
        );
        assert!(!workspace.refresh_all_frames()?);
        Ok(())
    }

    #[test]
    fn eager_default_materializes_hidden_frames_and_tab_selection_flushes_deferred_frames()
    -> Result<()> {
        let (mut workspace, hidden, _) = fixture()?;
        parsed_output(&mut workspace, hidden, b"eager")?;
        assert!(workspace.poll()?);
        assert_eq!(workspace.panes[&hidden].frame.text_lines()[0], "eager");
        let session = workspace
            .state
            .active_session()
            .context("fixture session")?;
        let session_id = session.id();
        let old_window = session.active_window().context("fixture window")?.id();
        workspace.apply(Command::CreateWindow {
            session: session_id,
            title: "other tab".into(),
        })?;
        workspace.defer_hidden_frame_updates();
        parsed_output(&mut workspace, hidden, b" latest")?;
        assert!(!workspace.poll()?);
        workspace.apply(Command::SelectWindow(old_window))?;
        // The old tab is still zoomed to its other pane; unzooming reveals it.
        let zoomed = workspace
            .state
            .active_session()
            .context("fixture session")?
            .active_window()
            .context("fixture window")?
            .zoomed_pane()
            .context("fixture zoom")?;
        workspace.apply(Command::ToggleZoom(zoomed))?;
        assert!(workspace.poll()?);
        assert_eq!(
            workspace.panes[&hidden].frame.text_lines()[0],
            "eager latest"
        );
        Ok(())
    }
}
