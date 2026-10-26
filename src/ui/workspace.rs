use std::{collections::BTreeMap, sync::Arc, time::Duration};

use gpui::{
    AnyElement, App, ClipboardItem, Context, Div, Entity, FocusHandle, Focusable, Render,
    SharedString, Stateful, Subscription, Task, Window, div, prelude::*, px, rgb,
};

use super::{
    actions::{
        ClosePane, CloseSession, CloseWindow, Copy, NewSession, NewWindow, NextPane, NextWindow,
        Paste, PreviousWindow, Quit, RenameSession, RenameWindow, SplitDown, SplitRight,
        ToggleZoom,
    },
    components::text_field::{TextField, TextFieldEvent},
    components::{
        ButtonVariant, button, empty_state, pane_header, section_label, session_item, status_bar,
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
        let title = match dialog.target {
            RenameTarget::Session(_) => "Rename session",
            RenameTarget::Window(_) => "Rename tab",
        };
        let mut panel = div()
            .flex()
            .flex_col()
            .w(px(420.))
            .p_4()
            .gap_4()
            .rounded_lg()
            .bg(rgb(theme.colors.surface))
            .border_1()
            .border_color(rgb(theme.colors.border))
            .child(div().text_lg().child(title))
            .child(dialog.input.clone());
        if let Some(error) = &self.last_error {
            panel = panel.child(
                div()
                    .text_sm()
                    .text_color(rgb(theme.colors.danger))
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
                    button("save-rename", "Save", ButtonVariant::Primary, theme).on_click(
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
            .bg(gpui::rgba(0x0000_0080))
            .child(panel)
    }

    fn sidebar(&self, cx: &Context<Self>) -> Div {
        let theme = self.theme;
        let active = self.runtime.state().active_session().map(Session::id);
        let sessions = self.runtime.state().sessions().iter().map(|session| {
            let id = session.id();
            session_item(session, active == Some(id), theme).on_click(cx.listener(
                move |view, _, window, cx| {
                    view.dispatch(Command::SelectSession(id), window, cx);
                },
            ))
        });
        div()
            .flex()
            .flex_col()
            .w(px(theme.sidebar_width))
            .h_full()
            .flex_shrink_0()
            .p_4()
            .gap_4()
            .bg(rgb(theme.colors.sidebar))
            .border_r_1()
            .border_color(rgb(theme.colors.border))
            .child(div().text_xl().child("Rex-rs"))
            .child(section_label("LOCAL SESSIONS", theme))
            .child(
                div()
                    .id("session-list")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(sessions),
            )
            .child(
                button(
                    "new-session",
                    "+ New session",
                    ButtonVariant::Primary,
                    theme,
                )
                .on_click(
                    cx.listener(|view, _, window, cx| view.new_session(&NewSession, window, cx)),
                ),
            )
            .child(section_label("Ctrl+Shift+N · New session", theme))
    }

    fn toolbar(&self, cx: &Context<Self>) -> Div {
        let theme = self.theme;
        let active = self.runtime.state().active_session();
        let mut toolbar = div()
            .flex()
            .items_center()
            .justify_between()
            .p_3()
            .gap_3()
            .flex_shrink_0()
            .border_b_1()
            .border_color(rgb(theme.colors.border))
            .child(
                div()
                    .text_sm()
                    .child(active.map_or("Workspace", Session::title).to_owned()),
            );
        if active.is_some() {
            toolbar =
                toolbar.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            button(
                                "rename-session",
                                "Rename session",
                                ButtonVariant::Quiet,
                                theme,
                            )
                            .on_click(cx.listener(
                                |view, _, window, cx| {
                                    view.rename_session(&RenameSession, window, cx);
                                },
                            )),
                        )
                        .child(
                            button("rename-tab", "Rename tab", ButtonVariant::Quiet, theme)
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.rename_window(&RenameWindow, window, cx);
                                })),
                        )
                        .child(
                            button("split-right", "Split right", ButtonVariant::Quiet, theme)
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.split_right(&SplitRight, window, cx);
                                })),
                        )
                        .child(
                            button("split-down", "Split down", ButtonVariant::Quiet, theme)
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.split_down(&SplitDown, window, cx);
                                })),
                        )
                        .child(
                            button(
                                "zoom",
                                if self.zoomed.is_some() {
                                    "Unzoom"
                                } else {
                                    "Zoom"
                                },
                                ButtonVariant::Quiet,
                                theme,
                            )
                            .on_click(cx.listener(
                                |view, _, window, cx| view.toggle_zoom(&ToggleZoom, window, cx),
                            )),
                        )
                        .child(
                            button("close-pane", "Close pane", ButtonVariant::Quiet, theme)
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.close_pane(&ClosePane, window, cx);
                                })),
                        ),
                );
        }
        toolbar
    }

    fn tabs(&self, cx: &Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        let mut tabs = div()
            .id("window-tabs")
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .flex_shrink_0()
            .overflow_x_scroll()
            .border_b_1()
            .border_color(rgb(theme.colors.border));
        if let Some(session) = self.runtime.state().active_session() {
            for window in session.windows() {
                let id = window.id();
                let selected = id == session.active_window().id();
                tabs = tabs.child(
                    div()
                        .flex()
                        .gap_1()
                        .child(
                            button(
                                SharedString::from(format!("window-{id}")),
                                SharedString::from(window.title().to_owned()),
                                if selected {
                                    ButtonVariant::Selected
                                } else {
                                    ButtonVariant::Quiet
                                },
                                theme,
                            )
                            .on_click(cx.listener(
                                move |view, _, window, cx| {
                                    view.dispatch(Command::SelectWindow(id), window, cx);
                                },
                            )),
                        )
                        .child(
                            button(
                                SharedString::from(format!("close-window-{id}")),
                                "×",
                                ButtonVariant::Quiet,
                                theme,
                            )
                            .on_click(cx.listener(
                                move |view, _, window, cx| {
                                    view.dispatch(Command::CloseWindow(id), window, cx);
                                },
                            )),
                        ),
                );
            }
            tabs = tabs.child(
                button("new-window", "+", ButtonVariant::Primary, theme).on_click(
                    cx.listener(|view, _, window, cx| view.new_window(&NewWindow, window, cx)),
                ),
            );
        }
        tabs
    }

    fn pane(&self, pane: PaneId, focused: bool) -> AnyElement {
        let theme = self.theme;
        let status = match self.runtime.status(pane) {
            Some(PaneStatus::Running) => "RUNNING".to_owned(),
            Some(PaneStatus::Exited { code }) => format!("EXITED {code}"),
            Some(PaneStatus::Failed { message }) => format!("ERROR: {message}"),
            None => "TERMINAL".to_owned(),
        };
        let mut body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .size_full()
            .border_1()
            .border_color(rgb(if focused {
                theme.colors.accent
            } else {
                theme.colors.border
            }))
            .child(pane_header(
                SharedString::from(format!("Terminal {pane}")),
                SharedString::from(status),
                focused,
                theme,
            ));
        if let Some(entry) = self.panes.get(&pane) {
            body = body.child(entry.view.clone());
        }
        body.into_any_element()
    }

    fn layout(&self, layout: &Layout, focused: PaneId) -> AnyElement {
        match layout {
            Layout::Pane(pane) => self.pane(*pane, *pane == focused),
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
                .gap_2()
                .when(*axis == SplitAxis::Vertical, gpui::Styled::flex_col)
                .child(self.layout(first, focused))
                .child(self.layout(second, focused))
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
            Some(session) => self.zoomed.map_or_else(
                || self.layout(session.layout(), session.active_pane()),
                |pane| self.pane(pane, true),
            ),
            None => empty_state(theme).into_any_element(),
        };
        let message = self.last_error.clone().unwrap_or_else(|| {
            "Local shells · Ctrl+Shift+T: new tab · Ctrl+Shift+C/V: copy/paste · Ctrl+Shift+Z: zoom".into()
        });
        let mut root = div()
            .id("workspace")
            .relative()
            .flex()
            .size_full()
            .bg(rgb(theme.colors.background))
            .text_color(rgb(theme.colors.text))
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
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.toolbar(cx))
                    .child(self.tabs(cx))
                    .child(div().flex().flex_1().min_h_0().p_3().child(content))
                    .child(status_bar(
                        SharedString::from(message),
                        self.last_error.is_some(),
                        theme,
                    )),
            );
        if let Some(dialog) = &self.rename {
            root = root.child(self.rename_dialog(dialog, cx));
        }
        root
    }
}
