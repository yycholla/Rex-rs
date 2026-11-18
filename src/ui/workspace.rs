use std::{collections::BTreeMap, sync::Arc, time::Duration};

use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, Div, Entity, FocusHandle, Focusable,
    FontWeight, MouseButton, Render, SharedString, Subscription, Task, Window, div, prelude::*, px,
    rgb,
};

use super::{
    actions::{
        ClosePane, CloseSession, CloseWindow, Copy, NewSession, NewWindow, NextPane, NextWindow,
        Paste, PreviousWindow, Quit, RenameSession, RenameWindow, SplitDown, SplitRight,
        ToggleZoom,
    },
    components::text_field::{TextField, TextFieldEvent},
    components::{
        ButtonVariant, Icon, add_row, button, empty_state, icon_button, pane_header, session_row,
        sidebar_footer, sidebar_heading, tab_row,
    },
    terminal_view::{TerminalView, TerminalViewEvent},
    theme::Theme,
};
use crate::{
    core::{Command, Layout, PaneId, Session, SessionId, SplitAxis, WindowId},
    terminal::{PaneStatus, ShellConfig, TerminalWorkspace},
};

struct PaneView {
    view: Entity<TerminalView>,
    _subscription: Subscription,
}

#[derive(Clone, Copy)]
enum RenameTarget {
    Session(SessionId),
    Window(WindowId),
}

struct RenameDialog {
    target: RenameTarget,
    input: Entity<TextField>,
    _subscription: Subscription,
}

pub struct WorkspaceView {
    runtime: TerminalWorkspace,
    panes: BTreeMap<PaneId, PaneView>,
    theme: Theme,
    focus: FocusHandle,
    last_error: Option<String>,
    next_session_number: u64,
    next_window_number: u64,
    zoomed: Option<PaneId>,
    rename: Option<RenameDialog>,
    _polling: Task<()>,
}

impl WorkspaceView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let theme = Theme::default();
        let mut runtime = TerminalWorkspace::live(theme.terminal_config(), ShellConfig::default());
        let last_error = runtime
            .apply(Command::CreateSession {
                title: "Development".into(),
            })
            .err()
            .map(|error| format!("{error:#}"));
        let polling = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                if this.update(cx, Self::poll).is_err() {
                    break;
                }
            }
        });
        let mut view = Self {
            runtime,
            panes: BTreeMap::new(),
            theme,
            focus: cx.focus_handle(),
            last_error,
            next_session_number: 2,
            next_window_number: 2,
            zoomed: None,
            rename: None,
            _polling: polling,
        };
        view.sync_views(cx);
        view
    }

    fn sync_views(&mut self, cx: &mut Context<Self>) {
        let ids = self.runtime.state().pane_ids();
        self.panes.retain(|id, _| ids.contains(id));
        for pane in ids {
            let Some(frame) = self.runtime.frame(pane).map(Arc::clone) else {
                continue;
            };
            if let Some(entry) = self.panes.get(&pane) {
                entry.view.update(cx, |view, cx| view.set_frame(frame, cx));
            } else {
                let view = cx.new(|cx| TerminalView::new(frame, self.theme, cx));
                let subscription = cx.subscribe(&view, move |host, _, event, cx| {
                    host.terminal_event(pane, event, cx);
                });
                self.panes.insert(
                    pane,
                    PaneView {
                        view,
                        _subscription: subscription,
                    },
                );
            }
        }
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        match self.runtime.poll() {
            Ok(true) => {
                self.sync_views(cx);
                cx.notify();
            }
            Ok(false) => {}
            Err(error) => self.report_error(format!("{error:#}"), cx),
        }
    }

    fn report_error(&mut self, message: String, cx: &mut Context<Self>) {
        if self.last_error.as_ref() != Some(&message) {
            self.last_error = Some(message);
            cx.notify();
        }
    }

    fn terminal_event(&mut self, pane: PaneId, event: &TerminalViewEvent, cx: &mut Context<Self>) {
        let result = match event {
            TerminalViewEvent::Focused => self.runtime.apply(Command::FocusPane(pane)),
            TerminalViewEvent::Key(input) => self.runtime.send_key(pane, input),
            TerminalViewEvent::Text(text) => self.runtime.write_input(pane, text.as_bytes()),
            TerminalViewEvent::Resized {
                size,
                cell_width,
                cell_height,
            } => self
                .runtime
                .resize(pane, *size, *cell_width, *cell_height)
                .map(|_| ()),
            TerminalViewEvent::Scroll(lines) => self.runtime.scroll(pane, *lines),
        };
        match result {
            Ok(()) => {
                self.sync_views(cx);
                cx.notify();
            }
            Err(error) => self.report_error(format!("{error:#}"), cx),
        }
    }

    fn dispatch(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        self.last_error = self
            .runtime
            .apply(command)
            .err()
            .map(|error| format!("{error:#}"));
        self.zoomed = None;
        self.sync_views(cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.active_pane().and_then(|pane| self.panes.get(&pane)) {
            pane.view.focus_handle(cx).focus(window);
        } else {
            self.focus.focus(window);
        }
    }

    fn active_pane(&self) -> Option<PaneId> {
        self.runtime
            .state()
            .active_session()
            .map(Session::active_pane)
    }

    fn new_session(&mut self, _: &NewSession, window: &mut Window, cx: &mut Context<Self>) {
        self.dispatch(
            Command::CreateSession {
                title: format!("Session {}", self.next_session_number),
            },
            window,
            cx,
        );
        if self.last_error.is_none() {
            self.next_session_number = self.next_session_number.saturating_add(1);
        }
    }
    fn new_window(&mut self, _: &NewWindow, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.runtime.state().active_session() {
            self.dispatch(
                Command::CreateWindow {
                    session: session.id(),
                    title: format!("Terminal {}", self.next_window_number),
                },
                window,
                cx,
            );
            if self.last_error.is_none() {
                self.next_window_number = self.next_window_number.saturating_add(1);
            }
        } else {
            self.new_session(&NewSession, window, cx);
        }
    }
    fn split(&mut self, axis: SplitAxis, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.active_pane() {
            self.dispatch(Command::SplitPane { pane, axis }, window, cx);
        }
    }
    fn split_right(&mut self, _: &SplitRight, window: &mut Window, cx: &mut Context<Self>) {
        self.split(SplitAxis::Horizontal, window, cx);
    }
    fn split_down(&mut self, _: &SplitDown, window: &mut Window, cx: &mut Context<Self>) {
        self.split(SplitAxis::Vertical, window, cx);
    }
    fn close_pane(&mut self, _: &ClosePane, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.active_pane() {
            self.dispatch(Command::ClosePane(pane), window, cx);
        }
    }
    fn close_session(&mut self, _: &CloseSession, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.runtime.state().active_session() {
            self.dispatch(Command::CloseSession(session.id()), window, cx);
        }
    }
    fn close_window(&mut self, _: &CloseWindow, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.runtime.state().active_session() {
            self.dispatch(
                Command::CloseWindow(session.active_window().id()),
                window,
                cx,
            );
        }
    }
    fn next_pane(&mut self, _: &NextPane, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_pane().is_some() {
            self.dispatch(Command::FocusNextPane, window, cx);
        }
    }
    fn next_window(&mut self, _: &NextWindow, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_pane().is_some() {
            self.dispatch(Command::NextWindow, window, cx);
        }
    }
    fn previous_window(&mut self, _: &PreviousWindow, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_pane().is_some() {
            self.dispatch(Command::PreviousWindow, window, cx);
        }
    }
    fn toggle_zoom(&mut self, _: &ToggleZoom, window: &mut Window, cx: &mut Context<Self>) {
        self.zoomed = if self.zoomed.is_some() {
            None
        } else {
            self.active_pane()
        };
        self.focus_active(window, cx);
        cx.notify();
    }
    fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.active_pane().and_then(|pane| self.panes.get(&pane))
            && let Some(text) = pane.view.read(cx).selected_text()
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }
    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.active_pane()
            && let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
            && let Err(error) = self.runtime.paste(pane, &text)
        {
            self.report_error(format!("{error:#}"), cx);
        }
        self.focus_active(window, cx);
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
        if let Some(session) = self.runtime.state().active_session() {
            let tab = session.active_window();
            self.open_rename(
                RenameTarget::Window(tab.id()),
                tab.title().to_owned(),
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
        let input = cx.new(|cx| TextField::new(value, "Name".into(), cx));
        let subscription =
            cx.subscribe_in(&input, window, |host, _, event, window, cx| match event {
                TextFieldEvent::Submitted => host.commit_rename(window, cx),
                TextFieldEvent::Cancelled => host.cancel_rename(window, cx),
                TextFieldEvent::Changed => {}
            });
        input.update(cx, TextField::select_all);
        input.focus_handle(cx).focus(window);
        self.last_error = None;
        self.rename = Some(RenameDialog {
            target,
            input,
            _subscription: subscription,
        });
        cx.notify();
    }
    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.rename else {
            return;
        };
        let title = dialog.input.read(cx).value().to_owned();
        let command = match dialog.target {
            RenameTarget::Session(session) => Command::RenameSession { session, title },
            RenameTarget::Window(window) => Command::RenameWindow { window, title },
        };
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
    fn rename_dialog(&self, dialog: &RenameDialog, cx: &Context<Self>) -> Div {
        let theme = self.theme;
        let colors = theme.colors;
        let title = match dialog.target {
            RenameTarget::Session(_) => "Rename Session",
            RenameTarget::Window(_) => "Rename Tab",
        };
        let mut panel = div()
            .flex()
            .flex_col()
            .w(px(360.))
            .p_4()
            .gap_3()
            .rounded(px(12.))
            .bg(rgb(colors.raised))
            .border_1()
            .border_color(rgb(colors.border_strong))
            .shadow_lg()
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(dialog.input.clone());
        if let Some(error) = &self.last_error {
            panel = panel.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(colors.danger))
                    .child(error.clone()),
            );
        }
        panel = panel.child(
            div()
                .flex()
                .justify_end()
                .gap_2()
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
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::rgba(0x0000_0066))
            .child(panel)
    }

    /// Sessions grouped under their host, each listing its tabs vertically.
    fn sidebar(&self, cx: &Context<Self>) -> Div {
        let theme = self.theme;
        let state = self.runtime.state();
        let active = state.active_session().map(Session::id);
        let mut list = div()
            .id("session-list")
            .flex()
            .flex_col()
            .gap(px(1.))
            .flex_1()
            .min_h_0()
            .px_2()
            .overflow_y_scroll()
            .child(sidebar_heading("Local", theme));
        for session in state.sessions() {
            let id = session.id();
            let is_active = active == Some(id);
            list = list.child(
                session_row(
                    SharedString::from(format!("session-{id}")),
                    session.title().to_owned(),
                    is_active,
                    theme,
                )
                .on_click(cx.listener(
                    move |view, event: &ClickEvent, window, cx| {
                        view.dispatch(Command::SelectSession(id), window, cx);
                        if event.click_count() > 1 {
                            view.rename_session(&RenameSession, window, cx);
                        }
                    },
                )),
            );
            let selected_window = session.active_window().id();
            for tab in session.windows() {
                let tab_id = tab.id();
                list = list.child(
                    tab_row(
                        SharedString::from(format!("window-{tab_id}")),
                        tab.title().to_owned(),
                        is_active && tab_id == selected_window,
                        tab.layout().panes().len(),
                        theme,
                    )
                    .on_click(cx.listener(
                        move |view, event: &ClickEvent, window, cx| {
                            view.dispatch(Command::SelectWindow(tab_id), window, cx);
                            if event.click_count() > 1 {
                                view.rename_window(&RenameWindow, window, cx);
                            }
                        },
                    )),
                );
            }
            if is_active {
                list = list.child(
                    add_row(
                        SharedString::from(format!("new-window-{id}")),
                        "New Tab",
                        true,
                        theme,
                    )
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.new_window(&NewWindow, window, cx);
                    })),
                );
            }
        }
        list = list.child(
            add_row("new-session-row", "New Session", false, theme).on_click(
                cx.listener(|view, _, window, cx| view.new_session(&NewSession, window, cx)),
            ),
        );
        let message = self
            .last_error
            .clone()
            .unwrap_or_else(|| "Local shells running".into());
        div()
            .flex()
            .flex_col()
            .w(px(theme.sidebar_width))
            .h_full()
            .flex_shrink_0()
            .bg(rgb(theme.colors.sidebar))
            .border_r_1()
            .border_color(rgb(theme.colors.border))
            .child(self.sidebar_titlebar(cx))
            .child(list)
            .child(sidebar_footer(
                SharedString::from(message),
                self.last_error.is_some(),
                theme,
            ))
    }

    /// Draggable strip above the session list, where Rex places window controls.
    fn sidebar_titlebar(&self, cx: &Context<Self>) -> Div {
        let theme = self.theme;
        div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(40.))
            .pl_3()
            .pr_2()
            .flex_shrink_0()
            .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(theme.colors.muted))
                    .child("Rex"),
            )
            .child(icon_button("new-session", Icon::Plus, theme).on_click(
                cx.listener(|view, _, window, cx| view.new_session(&NewSession, window, cx)),
            ))
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
        let zoomed = self.zoomed == Some(pane);
        let header = pane_header(
            SharedString::from(format!("Terminal {pane}")),
            status_color,
            focused,
            theme,
        )
        .when_some(status, |header, status| {
            header.child(
                div()
                    .flex_none()
                    .text_color(rgb(colors.faint))
                    .child(status),
            )
        });
        let header = self.pane_controls(header, pane, zoomed, multiple, cx);
        let mut content = div()
            .flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .p(px(4.))
            .bg(rgb(colors.terminal))
            .when(!focused, |content| {
                content.opacity(theme.inactive_pane_opacity)
            });
        if let Some(entry) = self.panes.get(&pane) {
            content = content.child(entry.view.clone());
        }
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .size_full()
            .bg(rgb(colors.surface))
            .child(header)
            .child(content)
            .when(focused && multiple, |body| {
                body.child(
                    div()
                        .absolute()
                        .inset_0()
                        .border_1()
                        .border_color(rgb(colors.accent)),
                )
            })
            .into_any_element()
    }

    /// Trailing pane-header controls. They act on their own pane, not the focused one.
    fn pane_controls(
        &self,
        header: Div,
        pane: PaneId,
        zoomed: bool,
        multiple: bool,
        cx: &Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let split = |axis: SplitAxis, glyph: Icon, name: &str| {
            icon_button(SharedString::from(format!("{name}-{pane}")), glyph, theme).on_click(
                cx.listener(move |view, _, window, cx| {
                    view.dispatch(Command::SplitPane { pane, axis }, window, cx);
                }),
            )
        };
        header
            .child(split(
                SplitAxis::Horizontal,
                Icon::SplitRight,
                "split-right",
            ))
            .child(split(SplitAxis::Vertical, Icon::SplitDown, "split-down"))
            .when(multiple || zoomed, |header| {
                header.child(
                    icon_button(
                        SharedString::from(format!("zoom-{pane}")),
                        if zoomed {
                            Icon::Minimize
                        } else {
                            Icon::Maximize
                        },
                        theme,
                    )
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.zoomed = if zoomed { None } else { Some(pane) };
                        view.focus_active(window, cx);
                        cx.notify();
                    })),
                )
            })
            .child(
                icon_button(
                    SharedString::from(format!("close-pane-{pane}")),
                    Icon::Close,
                    theme,
                )
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.dispatch(Command::ClosePane(pane), window, cx);
                })),
            )
    }

    /// Compact panes sit flush; the 1px gaps reveal the divider color.
    fn layout(
        &self,
        layout: &Layout,
        focused: PaneId,
        multiple: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        match layout {
            Layout::Pane(pane) => self.pane(*pane, *pane == focused, multiple, cx),
            Layout::Split {
                axis,
                first,
                second,
            } => div()
                .flex()
                .flex_1()
                .size_full()
                .min_w_0()
                .min_h_0()
                .gap(px(1.))
                .bg(rgb(self.theme.colors.border))
                .when(*axis == SplitAxis::Vertical, gpui::Styled::flex_col)
                .child(self.layout(first, focused, multiple, cx))
                .child(self.layout(second, focused, multiple, cx))
                .into_any_element(),
        }
    }
}

impl Focusable for WorkspaceView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.active_pane()
            .and_then(|pane| self.panes.get(&pane))
            .map_or_else(|| self.focus.clone(), |entry| entry.view.focus_handle(cx))
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let content = match self.runtime.state().active_session() {
            Some(session) => {
                let multiple = session.layout().panes().len() > 1;
                self.zoomed.map_or_else(
                    || self.layout(session.layout(), session.active_pane(), multiple, cx),
                    |pane| self.pane(pane, true, false, cx),
                )
            }
            None => empty_state(theme)
                .child(
                    button(
                        "empty-new-session",
                        "New Session",
                        ButtonVariant::Primary,
                        theme,
                    )
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.new_session(&NewSession, window, cx);
                    })),
                )
                .into_any_element(),
        };
        let mut root = div()
            .id("workspace")
            .relative()
            .flex()
            .size_full()
            .bg(rgb(theme.colors.background))
            .text_color(rgb(theme.colors.text))
            .text_size(px(13.))
            .track_focus(&self.focus)
            .key_context(if self.rename.is_some() {
                "WorkspaceModal"
            } else {
                "Workspace"
            })
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
            .on_action(cx.listener(Self::next_window))
            .on_action(cx.listener(Self::previous_window))
            .on_action(cx.listener(Self::toggle_zoom))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::quit))
            .child(self.sidebar(cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .bg(rgb(theme.colors.terminal))
                    .child(content),
            );
        if let Some(dialog) = &self.rename {
            root = root.child(self.rename_dialog(dialog, cx));
        }
        root
    }
}
