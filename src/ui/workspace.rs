mod automation;
mod detached;
mod find;
mod floating;
mod links;
mod management;
mod overview;
mod palette;
mod shortcuts;

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, AnyWindowHandle, App, Bounds, ClickEvent, Context, CursorStyle, Div, Entity,
    FocusHandle, Focusable, FontWeight, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Render,
    SharedString, Subscription, Task, Window, canvas, div, prelude::*, px, rgb,
};

use super::{
    actions::{
        ClosePane, CloseSession, CloseWindow, Copy, FocusDown, FocusLeft, FocusRight, FocusUp,
        NewSession, NewWindow, NextPane, NextWindow, Paste, PreviousWindow, Quit, RenameSession,
        RenameWindow, SplitDown, SplitRight, ToggleZoom,
    },
    components::text_field::{TextField, TextFieldEvent},
    components::{
        ButtonVariant, Icon, add_row, button, control_separator, count_badge, empty_state,
        icon_button, modal, overlay_footer, overlay_header, pane_header, session_row,
        sidebar_footer, sidebar_heading, tab_row, tinted_icon_button, tooltip,
    },
    terminal_view::{TerminalView, TerminalViewEvent},
    theme::Theme,
};
use crate::{
    core::{
        Command, Direction, GridSize, PaneId, SessionId, SplitAxis, SplitId, WindowId,
        frame::TerminalFrame, input::MouseAction,
    },
    rex::{
        WorkspaceClient,
        workspace::{SessionView as Session, ViewLayout as Layout, WindowView},
    },
    terminal::PaneStatus,
};

struct SplitDrag {
    split: SplitId,
    axis: SplitAxis,
    bounds: Bounds<Pixels>,
    last_ratio: u16,
    last_sent: Instant,
}

struct PaneView {
    frame: Arc<TerminalFrame>,
    status: Option<PaneStatus>,
    input_enabled: bool,
    viewport_restore: Option<(GridSize, Instant)>,
    view: Entity<TerminalView>,
    _subscription: Subscription,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RenameTarget {
    Session(SessionId),
    Window(WindowId),
}

struct RenameDialog {
    target: RenameTarget,
    input: Entity<TextField>,
    _subscription: Subscription,
    /// Leaving the in-place field commits it, as in file managers.
    _blur: Subscription,
}

/// Presentation ownership is independent of the retained server connection.
struct PresentationState {
    visible: bool,
    generation: u64,
}

pub struct WorkspaceView {
    runtime: WorkspaceClient,
    presentation: PresentationState,
    panes: BTreeMap<PaneId, PaneView>,
    theme: Theme,
    focus: FocusHandle,
    last_error: Option<String>,
    split_bounds: BTreeMap<SplitId, Bounds<Pixels>>,
    split_drag: Option<SplitDrag>,
    layer_drag: Option<floating::LayerDrag>,
    floating_bounds: Option<Bounds<Pixels>>,
    focus_pending: bool,
    rename: Option<RenameDialog>,
    arranging: Option<PaneId>,
    detached_browser: Option<SessionId>,
    find: Option<find::FindState>,
    palette: Option<palette::PaletteState>,
    search_counter: u64,
    /// Session whose overview replaces the terminals while it stays active.
    overview: Option<SessionId>,
    /// Latest process and client activity for the overview's session.
    insights: Option<(SessionId, crate::rex::insights::SessionInsights)>,
    insights_task: Option<Task<()>>,
    _polling: Task<()>,
    window_handle: Option<AnyWindowHandle>,
    activation: Option<Subscription>,
    client_actions: VecDeque<crate::rex::automation::ClientActionRequest>,
    action_running: bool,
    action_task: Option<Task<()>>,
    sidebar_visible: bool,
    /// The host's label for this server while its sidebar renders the tree.
    embedded: Option<SharedString>,
    shortcuts: shortcuts::Shortcuts,
    initial_scripts: Option<Vec<crate::rex::ClientScript>>,
}

impl WorkspaceView {
    pub fn new(endpoint: Option<String>, cx: &mut Context<Self>) -> Self {
        Self::new_with_scripts(endpoint, Vec::new(), cx)
    }

    pub fn new_with_scripts(
        endpoint: Option<String>,
        scripts: Vec<crate::rex::ClientScript>,
        cx: &mut Context<Self>,
    ) -> Self {
        let runtime = endpoint.map_or_else(WorkspaceClient::new, WorkspaceClient::connect);
        Self::with_runtime(runtime, scripts, cx)
    }

    pub fn new_with_server(
        endpoint: String,
        scripts: Vec<crate::rex::ClientScript>,
        server: crate::rex::servers::NamedServer,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_runtime(
            WorkspaceClient::connect_named(endpoint, server),
            scripts,
            cx,
        )
    }

    fn with_runtime(
        runtime: WorkspaceClient,
        scripts: Vec<crate::rex::ClientScript>,
        cx: &mut Context<Self>,
    ) -> Self {
        let theme = Theme::default();
        let polling = cx.spawn(async move |this, cx| {
            loop {
                let Ok(update) = this.update(cx, |view, _| view.runtime.wait_for_update()) else {
                    break;
                };
                update.await;
                if this.update(cx, Self::poll).is_err() {
                    break;
                }
            }
        });
        let mut view = Self {
            runtime,
            presentation: PresentationState {
                visible: true,
                generation: 0,
            },
            panes: BTreeMap::new(),
            theme,
            focus: cx.focus_handle(),
            last_error: None,
            split_bounds: BTreeMap::new(),
            split_drag: None,
            layer_drag: None,
            floating_bounds: None,
            focus_pending: false,
            rename: None,
            arranging: None,
            detached_browser: None,
            find: None,
            palette: None,
            search_counter: 0,
            overview: None,
            insights: None,
            insights_task: None,
            _polling: polling,
            window_handle: None,
            activation: None,
            client_actions: VecDeque::new(),
            action_running: false,
            action_task: None,
            sidebar_visible: true,
            embedded: None,
            shortcuts: shortcuts::Shortcuts::default(),
            initial_scripts: (!scripts.is_empty()).then_some(scripts),
        };
        view.sync_views(true, cx);
        view
    }

    /// Bind every child, including those not yet rendered, so callbacks receive replies.
    pub fn bind_host_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.window_handle = Some(window.window_handle());
        self.observe_activation(window, cx);
        self.schedule_client_actions(cx);
    }

    #[must_use]
    pub fn connected(&self) -> bool {
        self.runtime.connected()
    }

    /// The latest error, which while disconnected explains the failed connection.
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// A transient notice the host's footer shows for the presented server:
    /// an error (`true`) or the active key mode.
    #[must_use]
    pub fn status_notice(&self) -> Option<(String, bool)> {
        if let Some(error) = &self.last_error {
            return Some((error.clone(), true));
        }
        self.shortcuts
            .active_mode()
            .map(|mode| (format!("Key mode: {mode} · Escape to leave"), false))
    }

    #[must_use]
    pub fn session_count(&self) -> usize {
        self.runtime.state().sessions().len()
    }

    /// Lets a host render this server's sessions in its own sidebar.
    pub fn set_embedded(&mut self, label: Option<SharedString>, cx: &mut Context<Self>) {
        if self.embedded != label {
            self.embedded = label;
            cx.notify();
        }
    }

    #[must_use]
    pub const fn sidebar_visible(&self) -> bool {
        self.sidebar_visible
    }

    pub fn set_sidebar_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.sidebar_visible != visible {
            self.sidebar_visible = visible;
            cx.notify();
        }
    }

    pub fn request_new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_session(&NewSession, window, cx);
    }

    /// Retain the server connection while excluding this workspace from presentation.
    pub fn set_presented(&mut self, presented: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.bind_host_window(window, cx);
        if self.presentation.visible == presented {
            return;
        }
        self.presentation.visible = presented;
        self.presentation.generation = self.presentation.generation.wrapping_add(1);
        if !presented {
            self.split_drag = None;
            self.layer_drag = None;
            self.clear_queued_shortcuts();
            if self.focus.contains_focused(window, cx) {
                window.blur();
            }
        }
        self.focus_pending = presented;
        self.sync_presentation_visibility(cx);
        cx.notify();
    }

    /// Restore the current editor or terminal after selecting this server.
    pub fn focus_presented(&self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.presentation.visible {
            return;
        }
        if let Some(dialog) = &self.rename {
            dialog.input.focus_handle(cx).focus(window);
        } else if let Some(focus) = self.palette_focus(cx) {
            focus.focus(window);
        } else if self.arranging.is_some() || self.detached_browser.is_some() {
            self.focus.focus(window);
        } else {
            self.focus_active(window, cx);
        }
    }

    fn observe_activation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.activation.is_none() {
            self.activation = Some(cx.observe_window_activation(window, |view, window, cx| {
                if !window.is_window_active() {
                    view.layer_drag = None;
                    view.split_drag = None;
                    cx.notify();
                }
            }));
        }
    }

    fn sync_split_bounds(&mut self) {
        let mut splits = BTreeSet::new();
        for session in self.runtime.state().sessions() {
            for tab in session.windows() {
                for layer in &tab.layers {
                    if let Some(layout) = &layer.layout {
                        collect_split_ids(layout, &mut splits);
                    }
                }
            }
        }
        self.split_bounds.retain(|id, _| splits.contains(id));
        if self
            .split_drag
            .as_ref()
            .is_some_and(|drag| !splits.contains(&drag.split))
        {
            self.split_drag = None;
        }
    }

    /// Reconcile runtime frames without invalidating workspace chrome for output.
    /// Returns true only when pane membership or process status changed.
    fn sync_views(&mut self, layout_changed: bool, cx: &mut Context<Self>) -> bool {
        let ids = self.runtime.state().pane_ids();
        let active_pane = self.active_pane();
        let connected = self.runtime.connected();
        if layout_changed {
            self.sync_split_bounds();
        }
        let previous_count = self.panes.len();
        self.panes
            .retain(|id, _| ids.contains(id) && self.runtime.frame(*id).is_some());
        let mut chrome_changed = previous_count != self.panes.len();
        for pane in ids {
            let Some(frame) = self.runtime.frame(pane).map(Arc::clone) else {
                continue;
            };
            let status = self.runtime.status(pane).cloned();
            let mouse_tracking = self.runtime.mouse_tracking(pane);
            let annotations = self.runtime.annotations(pane).cloned().unwrap_or_default();
            let input_enabled = connected
                && self.runtime.input_available(pane)
                && !matches!(
                    status,
                    Some(PaneStatus::Failed { .. } | PaneStatus::Exited { .. })
                );
            if let Some(entry) = self.panes.get_mut(&pane) {
                entry.view.update(cx, |view, cx| {
                    view.set_mouse_tracking(mouse_tracking, cx);
                });
                if entry.input_enabled != input_enabled {
                    entry.input_enabled = input_enabled;
                    entry
                        .view
                        .update(cx, |view, cx| view.set_input_enabled(input_enabled, cx));
                }
                if Arc::ptr_eq(&entry.frame, &frame) {
                    entry
                        .view
                        .update(cx, |view, cx| view.set_annotations(annotations, cx));
                } else {
                    if entry
                        .viewport_restore
                        .is_some_and(|(size, _)| size == frame.size)
                    {
                        entry.viewport_restore = None;
                    }
                    entry.frame = Arc::clone(&frame);
                    entry.view.update(cx, |view, cx| {
                        view.set_frame(frame, cx);
                        view.set_annotations(annotations, cx);
                    });
                }
                if entry.status != status {
                    entry.status = status;
                    chrome_changed = true;
                }
            } else {
                let initial_frame = Arc::clone(&frame);
                let view = cx.new(|cx| {
                    let mut view = TerminalView::new(initial_frame, self.theme, cx);
                    view.set_annotations(annotations, cx);
                    view
                });
                view.update(cx, |view, cx| view.set_input_enabled(input_enabled, cx));
                view.update(cx, |view, cx| view.set_mouse_tracking(mouse_tracking, cx));
                let subscription = cx.subscribe(&view, move |host, _, event, cx| {
                    host.terminal_event(pane, event, cx);
                });
                self.panes.insert(
                    pane,
                    PaneView {
                        frame,
                        status,
                        input_enabled,
                        viewport_restore: None,
                        view,
                        _subscription: subscription,
                    },
                );
                if active_pane == Some(pane) {
                    self.focus_pending = true;
                }
                chrome_changed = true;
            }
        }
        chrome_changed
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        let previous_state = self.runtime.state_revision();
        let previous_pane = self.active_pane();
        let previous_server = self.runtime.server_id().map(str::to_owned);
        let was_connected = self.runtime.connected();
        let result = self.runtime.poll();
        let clipboard = self.runtime.take_program_clipboard();
        if self.presentation.visible
            && cx.active_window() == self.window_handle
            && self.window_handle.is_some()
            && !self.has_modal()
        {
            for text in clipboard {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            }
        }
        self.reconcile_find(cx);
        self.client_actions
            .extend(self.runtime.take_client_actions());
        self.schedule_client_actions(cx);
        self.refresh_shortcuts(cx);
        self.load_initial_scripts(cx);
        if matches!(result, Ok(false)) {
            return;
        }
        if self
            .arranging
            .is_some_and(|pane| self.runtime.state().pane(pane).is_none())
            || !self.runtime.connected()
        {
            self.arranging = None;
            self.focus_pending = true;
        }
        if self.detached_browser.is_some_and(|id| {
            !self
                .runtime
                .state()
                .sessions()
                .iter()
                .any(|session| session.id() == id)
        }) || !self.runtime.connected()
        {
            self.detached_browser = None;
        }
        let server_changed = previous_server.as_deref() != self.runtime.server_id();
        let state_changed = previous_state != self.runtime.state_revision();
        let connection_changed = was_connected != self.runtime.connected();
        if self.rename.as_ref().is_some_and(|dialog| {
            let sessions = self.runtime.state().sessions();
            match dialog.target {
                RenameTarget::Session(id) => !sessions.iter().any(|session| session.id() == id),
                RenameTarget::Window(id) => !sessions
                    .iter()
                    .flat_map(Session::windows)
                    .any(|tab| tab.id() == id),
            }
        }) {
            // Inline editors disappear with their rows. Retire the modal state
            // as well, so an externally closed target cannot trap all input.
            self.rename = None;
            self.focus_pending = true;
        }
        if !was_connected && self.runtime.connected() {
            for pane in self.panes.values_mut() {
                pane.view.update(cx, TerminalView::reset_viewport);
            }
        }
        if !was_connected && self.runtime.connected() {
            self.last_error = None;
        }
        if server_changed {
            // IDs may be allocated again by a new server. Do not carry local
            // selection, composition or zoom into a different terminal lifetime.
            self.panes.clear();
            self.split_bounds.clear();
            self.layer_drag = None;
            self.floating_bounds = None;
            self.split_drag = None;
            self.rename = None;
            self.arranging = None;
            self.detached_browser = None;
            self.overview = None;
            self.insights = None;
            self.insights_task = None;
        }
        if previous_pane != self.active_pane() || server_changed {
            self.focus_pending = true;
        }
        match result {
            Ok(_) => {
                let chrome_changed = self.sync_views(state_changed || server_changed, cx);
                if chrome_changed || state_changed || connection_changed || server_changed {
                    // A snapshot can change labels/focus without changing pane
                    // membership, so chrome must also follow authoritative state.
                    cx.notify();
                }
            }
            Err(error) => {
                self.sync_views(state_changed || server_changed, cx);
                // Worker errors have already been logged before cache publication.
                self.set_error(format!("{error:#}"), cx);
            }
        }
        self.sync_presentation_visibility(cx);
    }

    fn report_error(&mut self, message: String, cx: &mut Context<Self>) {
        if self.last_error.as_ref() != Some(&message) {
            crate::rex::diagnostics::write("ERROR", "ui", &message);
        }
        self.set_error(message, cx);
    }

    fn set_error(&mut self, message: String, cx: &mut Context<Self>) {
        if self.last_error.as_ref() != Some(&message) {
            self.last_error = Some(message);
            cx.notify();
        }
    }

    fn terminal_event(&mut self, pane: PaneId, event: &TerminalViewEvent, cx: &mut Context<Self>) {
        if !self.presentation.visible && !inactive_cleanup_event(event) {
            return;
        }
        if !self.runtime.connected()
            || self.runtime.state().pane(pane).is_none()
            || self.runtime.state().is_detached(pane)
        {
            return;
        }
        if self.presentation.visible
            && (self.arranging.is_some()
                || self.detached_browser.is_some()
                || self.palette.is_some())
            && matches!(
                event,
                TerminalViewEvent::OpenLink { .. }
                    | TerminalViewEvent::Key(_)
                    | TerminalViewEvent::Text(_)
                    | TerminalViewEvent::Mouse(_)
                    | TerminalViewEvent::Wheel { .. }
                    | TerminalViewEvent::Selection(
                        crate::core::interaction::SelectionCommand::Press { .. }
                            | crate::core::interaction::SelectionCommand::Drag { .. }
                    )
            )
        {
            return;
        }
        if self.presentation.visible && self.defer_terminal_input(pane, event, cx) {
            return;
        }
        let restore_size = matches!(
            event,
            TerminalViewEvent::Interacted
                | TerminalViewEvent::Key(_)
                | TerminalViewEvent::Text(_)
                | TerminalViewEvent::Wheel { .. }
        ) || matches!(event, TerminalViewEvent::Mouse(input) if input.action == MouseAction::Press);
        if restore_size && let Err(error) = self.restore_viewport(pane, cx) {
            self.report_error(format!("{error:#}"), cx);
            return;
        }
        let focus_changed =
            matches!(event, TerminalViewEvent::Focused) && self.active_pane() != Some(pane);
        if matches!(event, TerminalViewEvent::Focused) && !focus_changed {
            return;
        }
        let result = match event {
            TerminalViewEvent::OpenLink {
                column,
                row,
                viewport_token,
            } => {
                self.open_terminal_link(pane, *column, *row, *viewport_token, cx);
                Ok(())
            }
            TerminalViewEvent::Focused => self.runtime.apply(Command::FocusPane(pane)),
            TerminalViewEvent::Interacted => Ok(()),
            TerminalViewEvent::FocusChanged(focused) => self.runtime.focus(pane, *focused),
            TerminalViewEvent::Mouse(input) => self.runtime.mouse(pane, *input),
            TerminalViewEvent::Key(input) => self.runtime.send_key(pane, input),
            TerminalViewEvent::Text(text) => self.runtime.write_input(pane, text.as_bytes()),
            TerminalViewEvent::Resized {
                size,
                cell_width,
                cell_height,
            } => self.runtime.resize(pane, *size, *cell_width, *cell_height),
            TerminalViewEvent::Scroll(lines) => self.runtime.scroll(pane, *lines),
            TerminalViewEvent::Selection(command) => self.runtime.select(pane, *command),
            TerminalViewEvent::Wheel {
                lines,
                column,
                row,
                modifiers,
            } => self.runtime.wheel(pane, *lines, *column, *row, *modifiers),
        };
        // Queueing input does not change displayed state. Reconcile only when
        // an authoritative update arrives, rather than walking every pane on
        // each key, text commit, wheel event and resize request.
        if let Err(error) = result {
            self.report_error(format!("{error:#}"), cx);
        }
    }

    fn restore_viewport(&mut self, pane: PaneId, cx: &App) -> anyhow::Result<()> {
        let Some(entry) = self.panes.get_mut(&pane) else {
            return Ok(());
        };
        let Some((size, width, height)) = entry.view.read(cx).measured_viewport() else {
            return Ok(());
        };
        if entry.frame.size == size {
            entry.viewport_restore = None;
            return Ok(());
        }
        let now = Instant::now();
        if !entry.input_enabled || !viewport_restore_due(entry.viewport_restore, size, now) {
            return Ok(());
        }
        if !self.runtime.owns_size(pane) {
            self.runtime.set_size_ownership(pane, true)?;
        }
        self.runtime.resize(pane, size, width, height)?;
        // Coalesce input arriving before the server acknowledges this size.
        entry.viewport_restore = Some((size, now));
        Ok(())
    }

    fn dispatch(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        if !self.presentation.visible {
            return;
        }
        self.overview = None;
        if self.defer_command(&command, cx) {
            return;
        }
        match self.runtime.apply(command) {
            Ok(()) => self.last_error = None,
            Err(error) => self.report_error(format!("{error:#}"), cx),
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.presentation.visible || self.has_modal() {
            return;
        }
        if let Some(focus) = self.find_focus(cx) {
            focus.focus(window);
        } else if self.overview_session().is_some() {
            // Terminals are not rendered beneath the overview; keep workspace
            // shortcuts reachable through the root focus handle.
            self.focus.focus(window);
        } else if let Some(pane) = self.active_pane().and_then(|pane| self.panes.get(&pane)) {
            pane.view.focus_handle(cx).focus(window);
        } else {
            self.focus.focus(window);
        }
    }

    fn active_pane(&self) -> Option<PaneId> {
        let tab = self.runtime.state().active_session()?.active_window()?;
        if let Some(zoomed) = tab.zoomed_pane() {
            return Some(zoomed);
        }
        tab.active_pane()
            .filter(|pane| tab.visible_panes().contains(pane))
    }

    fn new_session(&mut self, _: &NewSession, window: &mut Window, cx: &mut Context<Self>) {
        self.invoke_local_action(crate::rex::actions::NativeAction::NewSession, window, cx);
    }
    fn new_window(&mut self, _: &NewWindow, window: &mut Window, cx: &mut Context<Self>) {
        self.invoke_local_action(crate::rex::actions::NativeAction::NewWindow, window, cx);
    }
    fn split_right(&mut self, _: &SplitRight, window: &mut Window, cx: &mut Context<Self>) {
        self.invoke_local_action(
            crate::rex::actions::NativeAction::Split {
                direction: Direction::Right,
            },
            window,
            cx,
        );
    }
    fn split_down(&mut self, _: &SplitDown, window: &mut Window, cx: &mut Context<Self>) {
        self.invoke_local_action(
            crate::rex::actions::NativeAction::Split {
                direction: Direction::Down,
            },
            window,
            cx,
        );
    }
    fn close_pane(&mut self, _: &ClosePane, window: &mut Window, cx: &mut Context<Self>) {
        self.invoke_local_action(crate::rex::actions::NativeAction::ClosePane, window, cx);
    }
    fn close_session(&mut self, _: &CloseSession, window: &mut Window, cx: &mut Context<Self>) {
        self.invoke_local_action(crate::rex::actions::NativeAction::CloseSession, window, cx);
    }
    fn close_window(&mut self, _: &CloseWindow, window: &mut Window, cx: &mut Context<Self>) {
        self.invoke_local_action(crate::rex::actions::NativeAction::CloseWindow, window, cx);
    }
    fn next_pane(&mut self, _: &NextPane, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_pane().is_some() {
            self.dispatch(Command::FocusNextPane, window, cx);
        }
    }
    fn next_window(&mut self, _: &NextWindow, window: &mut Window, cx: &mut Context<Self>) {
        self.invoke_local_action(crate::rex::actions::NativeAction::NextWindow, window, cx);
    }
    fn previous_window(&mut self, _: &PreviousWindow, window: &mut Window, cx: &mut Context<Self>) {
        self.invoke_local_action(
            crate::rex::actions::NativeAction::PreviousWindow,
            window,
            cx,
        );
    }
    fn toggle_zoom(&mut self, _: &ToggleZoom, window: &mut Window, cx: &mut Context<Self>) {
        self.invoke_local_action(crate::rex::actions::NativeAction::ZoomPane, window, cx);
    }
    fn focus_left(&mut self, _: &FocusLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.invoke_local_action(
            crate::rex::actions::NativeAction::FocusPane {
                direction: Direction::Left,
            },
            window,
            cx,
        );
    }
    fn focus_right(&mut self, _: &FocusRight, window: &mut Window, cx: &mut Context<Self>) {
        self.invoke_local_action(
            crate::rex::actions::NativeAction::FocusPane {
                direction: Direction::Right,
            },
            window,
            cx,
        );
    }
    fn focus_up(&mut self, _: &FocusUp, window: &mut Window, cx: &mut Context<Self>) {
        self.invoke_local_action(
            crate::rex::actions::NativeAction::FocusPane {
                direction: Direction::Up,
            },
            window,
            cx,
        );
    }
    fn focus_down(&mut self, _: &FocusDown, window: &mut Window, cx: &mut Context<Self>) {
        self.invoke_local_action(
            crate::rex::actions::NativeAction::FocusPane {
                direction: Direction::Down,
            },
            window,
            cx,
        );
    }
    fn copy(&mut self, _: &Copy, window: &mut Window, cx: &mut Context<Self>) {
        self.invoke_local_action(crate::rex::actions::NativeAction::Copy, window, cx);
    }
    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        self.invoke_local_action(crate::rex::actions::NativeAction::Paste, window, cx);
    }
    #[expect(
        clippy::unused_self,
        reason = "GPUI listeners require a view receiver."
    )]
    fn quit(&mut self, _: &Quit, _window: &mut Window, cx: &mut Context<Self>) {
        cx.quit();
    }

    fn rename_session(&mut self, _: &RenameSession, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.runtime.state().active_session() {
            self.open_rename(
                RenameTarget::Session(session.id()),
                session.title().to_owned(),
                window,
                cx,
            );
        }
    }
    fn rename_window(&mut self, _: &RenameWindow, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self
            .runtime
            .state()
            .active_session()
            .and_then(Session::active_window)
        {
            self.open_rename(
                RenameTarget::Window(tab.id()),
                tab.label().to_owned(),
                window,
                cx,
            );
        }
    }
    fn open_rename(
        &mut self,
        target: RenameTarget,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.arranging = None;
        self.detached_browser = None;
        let input = cx.new(|cx| {
            let field = TextField::new(value, "Name".into(), cx);
            if self.sidebar_visible {
                field.compact()
            } else {
                field
            }
        });
        let subscription =
            cx.subscribe_in(&input, window, |host, _, event, window, cx| match event {
                TextFieldEvent::Submitted => host.commit_rename(window, cx),
                TextFieldEvent::Cancelled => host.cancel_rename(window, cx),
                TextFieldEvent::Changed => {}
            });
        let focus = input.focus_handle(cx);
        let blur = cx.on_blur(&focus, window, |host, window, cx| {
            host.commit_rename(window, cx);
        });
        input.update(cx, TextField::select_all);
        focus.focus(window);
        self.last_error = None;
        self.rename = Some(RenameDialog {
            target,
            input,
            _subscription: subscription,
            _blur: blur,
        });
        cx.notify();
    }
    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Switching servers hides the editor without accepting unfinished text.
        if !self.presentation.visible {
            return;
        }
        let Some(dialog) = &self.rename else {
            return;
        };
        let title = dialog.input.read(cx).value().to_owned();
        let command = match dialog.target {
            RenameTarget::Session(session) => Command::RenameSession { session, title },
            RenameTarget::Window(window) => Command::RenameWindow { window, title },
        };
        if let Some(admitted) = self.defer_rename_command(&command, cx) {
            if admitted {
                self.rename = None;
                self.last_error = None;
                self.focus_active(window, cx);
                cx.notify();
            }
            return;
        }
        match self.runtime.apply(command) {
            Ok(()) => {
                self.rename = None;
                self.last_error = None;
                self.focus_active(window, cx);
                cx.notify();
            }
            Err(error) => self.report_error(format!("{error:#}"), cx),
        }
    }
    fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rename = None;
        self.last_error = None;
        self.focus_active(window, cx);
        cx.notify();
    }
    /// The in-place field for `target` while it is being renamed in the sidebar.
    fn rename_editor(&self, target: RenameTarget) -> Option<AnyElement> {
        self.rename
            .as_ref()
            .filter(|dialog| self.sidebar_visible && dialog.target == target)
            .map(|dialog| dialog.input.clone().into_any_element())
    }

    /// Fallback when the sidebar is hidden and rows cannot host the field.
    fn rename_dialog(&self, dialog: &RenameDialog, cx: &Context<Self>) -> Div {
        let theme = self.theme;
        let colors = theme.colors;
        let title = match dialog.target {
            RenameTarget::Session(_) => "Rename session",
            RenameTarget::Window(_) => "Rename tab",
        };
        let close = icon_button("dismiss-rename", Icon::Close, theme)
            .on_click(cx.listener(|view, _, window, cx| view.cancel_rename(window, cx)))
            .into_any_element();
        let mut body = div()
            .flex()
            .flex_col()
            .gap_2()
            .px_4()
            .pb_4()
            .child(dialog.input.clone());
        if let Some(error) = &self.last_error {
            body = body.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(colors.danger))
                    .child(error.clone()),
            );
        }
        let panel = div()
            .child(overlay_header(title, None, close, theme))
            .child(body)
            .child(
                overlay_footer(theme)
                    .child(
                        button("cancel-rename", "Cancel", ButtonVariant::Quiet, theme).on_click(
                            cx.listener(|view, _, window, cx| view.cancel_rename(window, cx)),
                        ),
                    )
                    .child(
                        button("save-rename", "Rename", ButtonVariant::Primary, theme).on_click(
                            cx.listener(|view, _, window, cx| view.commit_rename(window, cx)),
                        ),
                    ),
            );
        modal(380., panel, theme)
    }

    /// Sessions grouped under their host, each listing its tabs vertically.
    fn tab_detail(&self, tab: &WindowView) -> Option<SharedString> {
        tab.active_pane()
            .and_then(|pane| self.runtime.state().pane(pane))
            .map(|block| block.terminal_title.as_str())
            .filter(|title| !title.is_empty() && *title != tab.title())
            .map(|title| SharedString::from(title.to_owned()))
    }

    fn sidebar_tab(
        &self,
        session: &Session,
        tab: &WindowView,
        selected: bool,
        cx: &Context<Self>,
    ) -> gpui::Stateful<Div> {
        let id = session.id();
        let theme = self.theme;
        let tab_id = tab.id();
        let title = tab.label().to_owned();
        tab_row(
            SharedString::from(format!("window-{tab_id}")),
            tab.title().to_owned(),
            self.rename_editor(RenameTarget::Window(tab_id)),
            self.tab_detail(tab),
            selected,
            tab.layers.iter().map(|layer| layer.blocks.len()).sum(),
            theme,
        )
        .on_drag(
            management::TabDrag::new(
                id,
                tab_id,
                tab.title().to_owned(),
                theme,
                self.runtime.server_id().unwrap_or_default().to_owned(),
                self.runtime.connection_generation(),
            ),
            |drag, _, _, cx| cx.new(|_| drag.clone()),
        )
        .on_drop(
            cx.listener(move |view, drag: &management::TabDrag, window, cx| {
                view.drop_tab(drag, id, Some(tab_id), window, cx);
            }),
        )
        .on_click(cx.listener(move |view, event: &ClickEvent, window, cx| {
            view.dispatch(Command::SelectWindow(tab_id), window, cx);
            if event.click_count() > 1 {
                view.open_rename(RenameTarget::Window(tab_id), title.clone(), window, cx);
            }
        }))
    }

    /// A session header followed by its tabs and, when active, a new-tab row.
    fn sidebar_session(
        &self,
        session: &Session,
        is_active: bool,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = self.theme;
        let id = session.id();
        let title = session.title().to_owned();
        let detached_count = self.runtime.state().detached_blocks(id).len();
        let mut rows = vec![
            session_row(
                SharedString::from(format!("session-{id}")),
                session.title().to_owned(),
                self.rename_editor(RenameTarget::Session(id)),
                is_active,
                theme,
            )
            .when(is_active && self.overview == Some(id), |row| {
                row.bg(rgb(theme.colors.selected))
            })
            .group("session-row")
            .when(is_active || detached_count > 0, |row| {
                row.child(
                    count_badge(
                        SharedString::from(format!("detached-browser-{id}")),
                        Icon::Layers,
                        detached_count,
                        theme,
                    )
                    // An empty browser stays reachable without adding a resting "0".
                    .when(detached_count == 0, |badge| {
                        badge
                            .opacity(0.)
                            .group_hover("session-row", |style| style.opacity(1.))
                    })
                    .tooltip(tooltip("Detached blocks", theme))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        cx.stop_propagation();
                        view.request_detached_browser(id, window, cx);
                    })),
                )
            })
            .on_click(cx.listener(move |view, event: &ClickEvent, window, cx| {
                view.request_overview(id, window, cx);
                if event.click_count() > 1 {
                    view.open_rename(RenameTarget::Session(id), title.clone(), window, cx);
                }
            }))
            .into_any_element(),
        ];
        let selected_window = session.active_window().map(WindowView::id);
        for tab in session.windows() {
            rows.push(
                self.sidebar_tab(
                    session,
                    tab,
                    is_active
                        && self.presentation.visible
                        && self.overview != Some(id)
                        && Some(tab.id()) == selected_window,
                    cx,
                )
                .into_any_element(),
            );
        }
        if is_active && self.presentation.visible {
            rows.push(
                add_row(
                    SharedString::from(format!("new-window-{id}")),
                    "New tab",
                    true,
                    theme,
                )
                .on_drop(
                    cx.listener(move |view, drag: &management::TabDrag, window, cx| {
                        view.drop_tab(drag, id, None, window, cx);
                    }),
                )
                .on_click(cx.listener(|view, _, window, cx| {
                    view.new_window(&NewWindow, window, cx);
                }))
                .into_any_element(),
            );
        }
        rows
    }

    /// Every session with its tabs, separated by small gaps.
    fn session_rows(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let state = self.runtime.state();
        let active = state.active_session().map(Session::id);
        let mut rows = Vec::new();
        for (index, session) in state.sessions().iter().enumerate() {
            if index > 0 {
                rows.push(div().flex_none().h(px(6.)).into_any_element());
            }
            rows.extend(self.sidebar_session(session, active == Some(session.id()), cx));
        }
        rows
    }

    fn new_session_row(&self, cx: &Context<Self>) -> gpui::Stateful<Div> {
        add_row("new-session-row", "New session", false, self.theme)
            .on_click(cx.listener(|view, _, window, cx| view.new_session(&NewSession, window, cx)))
    }

    /// This server's sessions for a host sidebar. A server without sessions
    /// offers to create one; otherwise the host's header provides that action.
    #[must_use]
    pub fn session_tree(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let mut rows = self.session_rows(cx);
        if rows.is_empty() {
            rows.push(self.new_session_row(cx).pl(px(28.)).into_any_element());
        }
        rows
    }

    fn sidebar(&self, cx: &Context<Self>) -> Div {
        let theme = self.theme;
        let mut list = div()
            .id("session-list")
            .flex()
            .flex_col()
            .gap(px(1.))
            .flex_1()
            .min_h_0()
            .px_2()
            .pt_1()
            .overflow_y_scroll()
            .child(sidebar_heading(
                "Local",
                if self.last_error.is_some() {
                    theme.colors.danger
                } else if self.runtime.connected() {
                    theme.colors.success
                } else {
                    theme.colors.faint
                },
                theme,
            ));
        list = list
            .children(self.session_rows(cx))
            .child(div().flex_none().h(px(6.)))
            .child(self.new_session_row(cx));
        let message = self.connection_message();
        div()
            .flex()
            .flex_col()
            .w(px(theme.sidebar_width))
            .h_full()
            .flex_shrink_0()
            .bg(rgb(theme.colors.sidebar))
            .child(self.sidebar_titlebar(cx))
            .child(list)
            .child(sidebar_footer(
                SharedString::from(message),
                self.last_error.is_some(),
                theme,
            ))
    }

    fn sync_presentation_visibility(&self, cx: &mut Context<Self>) {
        let visible: BTreeSet<_> =
            if !self.presentation.visible || self.overview_session().is_some() {
                BTreeSet::new()
            } else {
                self.runtime
                    .state()
                    .active_session()
                    .and_then(Session::active_window)
                    .map(WindowView::visible_panes)
                    .unwrap_or_default()
                    .into_iter()
                    .collect()
            };
        for (pane, entry) in &self.panes {
            entry.view.update(cx, |view, cx| {
                view.set_animation_enabled(
                    self.runtime.connected()
                        && !matches!(
                            entry.status,
                            Some(PaneStatus::Exited { .. } | PaneStatus::Failed { .. })
                        ),
                    cx,
                );
                view.set_visible(visible.contains(pane), cx);
            });
        }
    }

    fn workspace_content(&self, cx: &Context<Self>) -> AnyElement {
        let theme = self.theme;
        if let Some(session) = self.overview_session() {
            return self.overview(session, cx).into_any_element();
        }
        if let Some(session) = self.runtime.state().active_session() {
            if let Some(tab) = session.active_window() {
                let warnings: Vec<_> = tab
                    .layers
                    .iter()
                    .filter_map(|layer| layer.unsupported.as_deref())
                    .collect();
                let body = self.layered_window(tab, cx);
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .gap(px(theme.gutter / 2.))
                    .min_w_0()
                    .min_h_0()
                    .when(!warnings.is_empty(), |body| {
                        body.child(notice(warnings.join(" · "), theme))
                    })
                    .child(body)
                    .into_any_element()
            } else {
                empty_state(
                    "No tab open",
                    "This session has no active tab. Create one to begin.",
                    theme,
                )
                .child(
                    button("empty-new-tab", "New tab", ButtonVariant::Primary, theme).on_click(
                        cx.listener(|view, _, window, cx| view.new_window(&NewWindow, window, cx)),
                    ),
                )
                .into_any_element()
            }
        } else {
            empty_state(
                "No session open",
                "Rex keeps sessions running while you are away.",
                theme,
            )
            .child(
                button(
                    "empty-new-session",
                    "New session",
                    ButtonVariant::Primary,
                    theme,
                )
                .on_click(cx.listener(|view, _, window, cx| {
                    view.new_session(&NewSession, window, cx);
                })),
            )
            .into_any_element()
        }
    }

    fn connection_message(&self) -> String {
        self.last_error.clone().unwrap_or_else(|| {
            if self.runtime.connected() {
                self.shortcuts.active_mode().map_or_else(
                    || "Connected to Rex".into(),
                    |mode| format!("Key mode: {mode} · Escape to leave"),
                )
            } else {
                "Connecting to Rex…".into()
            }
        })
    }

    /// Draggable strip above the session list, where Rex places window controls.
    fn sidebar_titlebar(&self, cx: &Context<Self>) -> Div {
        let theme = self.theme;
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .h(px(44.))
            .pl(px(14.))
            .pr_2()
            .flex_shrink_0()
            .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(20.))
                    .rounded(px(5.))
                    .bg(rgb(theme.colors.accent_solid))
                    .text_size(px(11.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(theme.colors.accent_on_solid))
                    .child("R"),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(theme.colors.text))
                    .child("Rex"),
            )
            .child(
                icon_button("new-session", Icon::Plus, theme)
                    // Keep the titlebar's window move from swallowing the click.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .tooltip(tooltip("New session", theme))
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.new_session(&NewSession, window, cx);
                    })),
            )
    }

    /// "Server / session" for the focused pane while a host lists several servers.
    fn breadcrumb(&self) -> Option<String> {
        let server = self.embedded.as_ref()?;
        let session = self.runtime.state().active_session()?;
        Some(format!("{server} / {}", session.title()))
    }

    fn pane(&self, pane: PaneId, focused: bool, multiple: bool, cx: &Context<Self>) -> AnyElement {
        let theme = self.theme;
        let colors = theme.colors;
        let (status_color, status) = match self.runtime.status(pane) {
            Some(PaneStatus::Running) => (colors.success, None),
            Some(PaneStatus::Exited { code }) => (colors.faint, Some(format!("exited {code}"))),
            Some(PaneStatus::Failed { message }) => (colors.danger, Some(message.clone())),
            None => (colors.faint, None),
        };
        let zoomed = self
            .runtime
            .state()
            .active_session()
            .and_then(Session::active_window)
            .and_then(WindowView::zoomed_pane)
            == Some(pane);
        let header = pane_header(
            SharedString::from(self.runtime.pane_label(pane)),
            status_color,
            focused,
            theme,
        )
        .rounded_t(px(theme.pane_radius - 1.))
        .when_some(self.breadcrumb().filter(|_| focused), |header, crumb| {
            header.child(
                div()
                    .flex_none()
                    .max_w(px(220.))
                    .truncate()
                    .font_weight(FontWeight::NORMAL)
                    .text_color(rgb(colors.faint))
                    .child(crumb),
            )
        })
        .when_some(status, |header, status| {
            header.child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::NORMAL)
                    .text_color(rgb(colors.faint))
                    .child(status),
            )
        });
        let header = self.pane_controls(header, pane, focused, zoomed, multiple, cx);
        // Rex supplies the terminal palette; the inset around the grid matches it.
        let backdrop = self
            .panes
            .get(&pane)
            .map_or(colors.terminal, |entry| entry.frame.background.to_hex());
        let mut content = div()
            .flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .px(px(8.))
            .py(px(6.))
            .rounded_b(px(theme.pane_radius - 1.))
            .bg(rgb(backdrop))
            .when(!focused, |content| {
                content.opacity(theme.inactive_pane_opacity)
            });
        if let Some(entry) = self.panes.get(&pane) {
            content = content.child(entry.view.clone());
        } else {
            content = content.child(self.runtime.pane_message(pane));
        }
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .size_full()
            .group("pane")
            .rounded(px(theme.pane_radius))
            .overflow_hidden()
            .border_1()
            .border_color(rgb(if focused && multiple {
                colors.accent_muted
            } else {
                colors.border
            }))
            .bg(rgb(backdrop))
            .child(header)
            .child(content)
            .into_any_element()
    }

    fn can_float(&self, pane: PaneId) -> bool {
        self.runtime.connected()
            && self
                .runtime
                .state()
                .command(&Command::FloatPane {
                    pane,
                    bounds: [0.1, 0.1, 0.8, 0.8],
                })
                .is_ok()
    }

    fn ownership_button(&self, pane: PaneId, cx: &Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let colors = theme.colors;
        let own = self.runtime.owns_size(pane);
        tinted_icon_button(
            SharedString::from(format!("terminal-control-{pane}")),
            Icon::Lock,
            if own { colors.accent } else { colors.faint },
            theme,
        )
        .tooltip(tooltip(
            if own {
                "Release size ownership"
            } else {
                "Take size ownership"
            },
            theme,
        ))
        .on_click(cx.listener(move |view, _, _, cx| {
            if let Err(error) = view.runtime.set_size_ownership(pane, !own) {
                view.report_error(format!("{error:#}"), cx);
            }
        }))
    }

    /// Trailing pane-header controls. They act on their own pane, not the focused
    /// one, and stay hidden on unfocused panes until the pointer is over them.
    fn pane_controls(
        &self,
        header: Div,
        pane: PaneId,
        focused: bool,
        zoomed: bool,
        multiple: bool,
        cx: &Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let control = |name: &str, glyph: Icon, label: &'static str| {
            icon_button(SharedString::from(format!("{name}-{pane}")), glyph, theme)
                .tooltip(tooltip(label, theme))
        };
        let split = |axis: SplitAxis, glyph: Icon, name: &str, label: &'static str| {
            control(name, glyph, label).on_click(cx.listener(move |view, _, window, cx| {
                view.dispatch(Command::SplitPane { pane, axis }, window, cx);
            }))
        };
        let controls = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(1.))
            .when(!focused, |controls| {
                controls
                    .opacity(0.)
                    .group_hover("pane", |style| style.opacity(1.))
            })
            .when(self.runtime.connected(), |controls| {
                controls
                    .child(self.ownership_button(pane, cx))
                    .child(control_separator(theme))
            })
            .child(split(
                SplitAxis::Horizontal,
                Icon::SplitRight,
                "split-right",
                "Split right",
            ))
            .child(split(
                SplitAxis::Vertical,
                Icon::SplitDown,
                "split-down",
                "Split down",
            ))
            .child(
                control("arrange", Icon::Arrange, "Move or swap…").on_click(cx.listener(
                    move |view, _, window, cx| {
                        view.request_arrangement(pane, window, cx);
                    },
                )),
            )
            .when(self.can_float(pane), |controls| {
                controls.child(control("float", Icon::Maximize, "Float pane").on_click(
                    cx.listener(move |view, _, window, cx| {
                        view.submit_command(
                            Command::FloatPane {
                                pane,
                                bounds: [0.1, 0.1, 0.8, 0.8],
                            },
                            window,
                            cx,
                        );
                    }),
                ))
            })
            .child(
                control("detach", Icon::Detach, "Detach").on_click(cx.listener(
                    move |view, _, window, cx| {
                        view.submit_command(Command::DetachPane(pane), window, cx);
                    },
                )),
            )
            .when(multiple || zoomed, |controls| {
                controls.child(
                    control(
                        "zoom",
                        if zoomed {
                            Icon::Minimize
                        } else {
                            Icon::Maximize
                        },
                        if zoomed { "Restore" } else { "Zoom" },
                    )
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.dispatch(Command::ToggleZoom(pane), window, cx);
                    })),
                )
            })
            .child(control_separator(theme))
            .child(
                control("close-pane", Icon::Close, "Close pane").on_click(cx.listener(
                    move |view, _, window, cx| {
                        view.dispatch(Command::ClosePane(pane), window, cx);
                    },
                )),
            );
        header.child(controls)
    }

    fn resize_drag(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.move_layer_drag(event, window, cx);
        if event.dragging() {
            self.resize_to(event.position, false, cx);
        }
    }

    fn finish_resize(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_layer_drag(event, window, cx);
        self.resize_to(event.position, true, cx);
        self.split_drag = None;
    }

    fn resize_to(
        &mut self,
        position: gpui::Point<Pixels>,
        final_position: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = &self.split_drag else {
            return;
        };
        if !final_position && drag.last_sent.elapsed() < Duration::from_millis(24) {
            return;
        }
        let Some(ratio) = ratio_at(drag.bounds, drag.axis, position) else {
            return;
        };
        if ratio == drag.last_ratio {
            return;
        }
        let split = drag.split;
        match self.runtime.apply(Command::ResizeSplit { split, ratio }) {
            Ok(()) => {
                if let Some(drag) = &mut self.split_drag {
                    drag.last_ratio = ratio;
                    drag.last_sent = Instant::now();
                }
            }
            Err(error) => self.report_error(format!("{error:#}"), cx),
        }
    }

    fn divider(
        &self,
        split: SplitId,
        axis: SplitAxis,
        ratio: u16,
        cx: &Context<Self>,
    ) -> AnyElement {
        let colors = self.theme.colors;
        div()
            .id(SharedString::from(format!("split-divider-{split}")))
            .flex_none()
            .rounded_full()
            .hover(|divider| divider.bg(rgb(colors.accent_muted)))
            .when(axis == SplitAxis::Horizontal, |divider| {
                divider
                    .w(px(4.))
                    .h_full()
                    .cursor(CursorStyle::ResizeLeftRight)
            })
            .when(axis == SplitAxis::Vertical, |divider| {
                divider.h(px(4.)).w_full().cursor(CursorStyle::ResizeUpDown)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, _, _, cx| {
                    if let Some(bounds) = view.split_bounds.get(&split).copied() {
                        let now = Instant::now();
                        view.split_drag = Some(SplitDrag {
                            split,
                            axis,
                            bounds,
                            last_ratio: ratio,
                            last_sent: now.checked_sub(Duration::from_millis(24)).unwrap_or(now),
                        });
                    }
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "GPUI layout uses f32 fractions; the validated server ratio is finite and between zero and one."
    )]
    fn weighted_layout(
        &self,
        layout: &Layout,
        share: f64,
        focused: Option<PaneId>,
        multiple: bool,
        cx: &Context<Self>,
    ) -> Div {
        let mut child = div()
            .flex()
            .flex_basis(px(0.))
            .size_full()
            .min_w_0()
            .min_h_0();
        child.style().flex_grow = Some(share as f32);
        child.child(self.layout(layout, focused, multiple, cx))
    }

    /// Server-owned ratios determine child shares; dividers emit core commands.
    fn layout(
        &self,
        layout: &Layout,
        focused: Option<PaneId>,
        multiple: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        match layout {
            Layout::Pane(pane) => self.pane(*pane, Some(*pane) == focused, multiple, cx),
            Layout::Split {
                id,
                axis,
                ratio,
                first,
                second,
            } => {
                let split = *id;
                let bounds_view = cx.entity().downgrade();
                div()
                    .relative()
                    .flex()
                    .flex_1()
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .when(*axis == SplitAxis::Vertical, gpui::Styled::flex_col)
                    .child(self.weighted_layout(first, *ratio, focused, multiple, cx))
                    .child(self.divider(split, *axis, display_ratio(*ratio), cx))
                    .child(self.weighted_layout(second, 1.0 - *ratio, focused, multiple, cx))
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                cx.defer(move |cx| {
                                    let _ = bounds_view.update(cx, |view, _| {
                                        view.split_bounds.insert(split, bounds);
                                    });
                                });
                            },
                            |_, (), _, _| {},
                        )
                        .absolute()
                        .inset_0()
                        .size_full(),
                    )
                    .into_any_element()
            }
        }
    }
}

/// Inline banner for layout warnings and keymap diagnostics.
fn notice(content: impl IntoElement, theme: Theme) -> Div {
    let colors = theme.colors;
    div()
        .flex()
        .flex_none()
        .items_start()
        .gap_2()
        .px_3()
        .py_2()
        .rounded(px(8.))
        .bg(rgb(colors.surface))
        .border_1()
        .border_color(rgb(colors.border))
        .text_size(px(12.))
        .text_color(rgb(colors.muted))
        .child(
            div()
                .mt(px(5.))
                .child(super::components::status_dot(colors.danger)),
        )
        .child(div().flex().flex_col().flex_1().min_w_0().child(content))
}

#[expect(
    clippy::cast_sign_loss,
    reason = "Validated server fractions are clamped to positive presentation permille before conversion."
)]
fn display_ratio(ratio: f64) -> u16 {
    (ratio * 1000.0).round().clamp(1.0, 999.0) as u16
}

fn collect_split_ids(layout: &Layout, ids: &mut BTreeSet<SplitId>) {
    if let Layout::Split {
        id, first, second, ..
    } = layout
    {
        ids.insert(*id);
        collect_split_ids(first, ids);
        collect_split_ids(second, ids);
    }
}

fn viewport_restore_due(
    pending: Option<(GridSize, Instant)>,
    size: GridSize,
    now: Instant,
) -> bool {
    pending.is_none_or(|(previous, sent)| {
        previous != size || now.duration_since(sent) >= Duration::from_millis(100)
    })
}

#[expect(
    clippy::cast_sign_loss,
    reason = "Divider ratios are clamped to the positive core ratio range before casting."
)]
fn ratio_at(bounds: Bounds<Pixels>, axis: SplitAxis, position: gpui::Point<Pixels>) -> Option<u16> {
    let local = position - bounds.origin;
    let (offset, length) = match axis {
        SplitAxis::Horizontal => (f32::from(local.x), f32::from(bounds.size.width)),
        SplitAxis::Vertical => (f32::from(local.y), f32::from(bounds.size.height)),
    };
    // Shares apply to the available space beside the four-pixel divider, and
    // the pointer tracks its center rather than either child edge.
    let offset = offset - 2.;
    let length = length - 4.;
    (length > 0. && offset.is_finite() && length.is_finite())
        .then(|| (offset / length * 1000.).round().clamp(100., 900.) as u16)
}

impl Focusable for WorkspaceView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.rename
            .as_ref()
            .map(|dialog| dialog.input.focus_handle(cx))
            .or_else(|| self.palette_focus(cx))
            .or_else(|| self.find_focus(cx))
            .unwrap_or_else(|| {
                self.active_pane()
                    .and_then(|pane| self.panes.get(&pane))
                    .map_or_else(|| self.focus.clone(), |entry| entry.view.focus_handle(cx))
            })
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.bind_host_window(window, cx);
        self.schedule_insights(cx);
        if self.presentation.visible && self.focus_pending {
            self.focus_pending = false;
            cx.defer_in(window, |view, window, cx| {
                view.focus_presented(window, cx);
            });
        }
        self.sync_presentation_visibility(cx);
        let theme = self.theme;
        let content = self.workspace_content(cx);
        let mut root = div()
            .id("workspace")
            .relative()
            .flex()
            .size_full()
            .bg(rgb(theme.colors.background))
            .text_color(rgb(theme.colors.text))
            .text_size(px(13.))
            .track_focus(&self.focus)
            .key_context(if self.has_modal() {
                "WorkspaceModal"
            } else {
                "Workspace"
            })
            .capture_key_down(cx.listener(Self::shortcut_key))
            .on_action(cx.listener(Self::rename_session))
            .on_action(cx.listener(Self::rename_window))
            .on_action(cx.listener(Self::new_session))
            .on_action(cx.listener(Self::new_window))
            .on_action(cx.listener(Self::split_right))
            .on_action(cx.listener(Self::split_down))
            .on_action(cx.listener(Self::close_pane))
            .on_action(cx.listener(Self::close_session))
            .on_action(cx.listener(Self::close_window))
            .on_action(cx.listener(Self::next_pane))
            .on_action(cx.listener(Self::focus_left))
            .on_action(cx.listener(Self::focus_right))
            .on_action(cx.listener(Self::focus_up))
            .on_action(cx.listener(Self::focus_down))
            .on_action(cx.listener(Self::next_window))
            .on_action(cx.listener(Self::previous_window))
            .on_action(cx.listener(Self::toggle_zoom))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::quit))
            .on_mouse_move(cx.listener(Self::resize_drag))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::finish_resize))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::finish_resize))
            .when(self.sidebar_visible && self.embedded.is_none(), |root| {
                root.child(self.sidebar(cx))
            })
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex_col()
                    .gap(px(theme.gutter / 2.))
                    .py(px(theme.gutter))
                    .pr(px(theme.gutter))
                    .when(!self.sidebar_visible, |body| body.pl(px(theme.gutter)))
                    .when(!self.shortcuts.diagnostics.is_empty(), |body| {
                        body.child(
                            notice(
                                div().children(
                                    self.shortcuts
                                        .diagnostics
                                        .iter()
                                        .map(|message| div().child(message.clone())),
                                ),
                                theme,
                            )
                            .id("keymap-diagnostics")
                            .max_h(px(120.))
                            .overflow_y_scroll(),
                        )
                    })
                    .child(content),
            );
        if self.find.is_some() {
            root = root.child(self.find_bar(cx));
        }
        if let Some(dialog) = self.rename.as_ref().filter(|_| !self.sidebar_visible) {
            root = root.child(self.rename_dialog(dialog, cx));
        }
        if let Some(source) = self.arranging {
            root = root.child(self.arrangement_dialog(source, cx));
        }
        if let Some(session) = self.detached_browser {
            root = root.child(self.detached_dialog(session, cx));
        }
        if self.palette.is_some() {
            root = root.child(self.palette_dialog(cx));
        }
        root
    }
}

/// Cleanup must reach its original server after the user switches away.
fn inactive_cleanup_event(event: &TerminalViewEvent) -> bool {
    matches!(event, TerminalViewEvent::FocusChanged(false))
        || matches!(event, TerminalViewEvent::Mouse(input) if input.action == MouseAction::Release)
        || matches!(
            event,
            TerminalViewEvent::Selection(
                crate::core::interaction::SelectionCommand::Release
                    | crate::core::interaction::SelectionCommand::Cancel
            )
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_workspaces_allow_cleanup_but_reject_new_input() {
        use crate::core::{
            input::{Modifiers, MouseButton, MouseInput},
            interaction::SelectionCommand,
        };
        assert!(inactive_cleanup_event(&TerminalViewEvent::FocusChanged(
            false
        )));
        assert!(!inactive_cleanup_event(&TerminalViewEvent::FocusChanged(
            true
        )));
        assert!(inactive_cleanup_event(&TerminalViewEvent::Selection(
            SelectionCommand::Release
        )));
        assert!(inactive_cleanup_event(&TerminalViewEvent::Selection(
            SelectionCommand::Cancel
        )));
        for action in [MouseAction::Press, MouseAction::Move, MouseAction::Release] {
            assert_eq!(
                inactive_cleanup_event(&TerminalViewEvent::Mouse(MouseInput {
                    action,
                    button: MouseButton::Left,
                    column: 2,
                    row: 3,
                    modifiers: Modifiers::default(),
                })),
                action == MouseAction::Release
            );
        }
        for event in [
            TerminalViewEvent::Focused,
            TerminalViewEvent::Interacted,
            TerminalViewEvent::Text("wrong server".into()),
            TerminalViewEvent::Scroll(1),
        ] {
            assert!(!inactive_cleanup_event(&event));
        }
    }

    #[test]
    fn viewport_handoff_coalesces_input_but_retries_and_accepts_new_local_size()
    -> Result<(), crate::core::InvalidGridSize> {
        let desktop = GridSize::new(220, 120)?;
        let resized = GridSize::new(221, 120)?;
        let sent = Instant::now();
        let pending = Some((desktop, sent));
        assert!(viewport_restore_due(None, desktop, sent));
        assert!(!viewport_restore_due(
            pending,
            desktop,
            sent + Duration::from_millis(99)
        ));
        assert!(viewport_restore_due(
            pending,
            desktop,
            sent + Duration::from_millis(100)
        ));
        assert!(viewport_restore_due(pending, resized, sent));
        Ok(())
    }

    #[test]
    fn divider_drag_uses_parent_coordinates_and_keeps_children_nonempty() {
        let bounds = Bounds::new(
            gpui::point(px(10.), px(20.)),
            gpui::size(px(104.), px(204.)),
        );
        assert_eq!(
            ratio_at(bounds, SplitAxis::Horizontal, gpui::point(px(62.), px(20.))),
            Some(500)
        );
        assert_eq!(
            ratio_at(bounds, SplitAxis::Vertical, gpui::point(px(10.), px(172.))),
            Some(750)
        );
        assert_eq!(
            ratio_at(
                bounds,
                SplitAxis::Horizontal,
                gpui::point(px(-500.), px(20.))
            ),
            Some(100)
        );
        assert_eq!(
            ratio_at(bounds, SplitAxis::Vertical, gpui::point(px(10.), px(500.))),
            Some(900)
        );
        assert_eq!(
            ratio_at(
                Bounds::default(),
                SplitAxis::Horizontal,
                gpui::point(px(0.), px(0.))
            ),
            None
        );
    }
}
