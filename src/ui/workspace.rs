use gpui::{
    AnyElement, App, Context, Div, FocusHandle, Focusable, Render, SharedString, Window, div,
    prelude::*, px, rgb,
};

use super::{
    actions::{ClosePane, CloseSession, NewSession, NextPane, Quit, SplitDown, SplitRight},
    components::{
        ButtonVariant, button, empty_state, pane_header, section_label, session_item, status_bar,
        terminal_surface,
    },
    theme::Theme,
};
use crate::{
    core::{Command, Layout, PaneId, Session, SplitAxis},
    terminal::TerminalWorkspace,
};

pub struct WorkspaceView {
    runtime: TerminalWorkspace,
    theme: Theme,
    focus: FocusHandle,
    last_error: Option<String>,
    next_session_number: u64,
}

impl WorkspaceView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let theme = Theme::default();
        let mut runtime = TerminalWorkspace::new(theme.terminal_config());
        let last_error = apply_preview_command(
            &mut runtime,
            Command::CreateSession {
                title: "Development".into(),
            },
        )
        .err()
        .map(|error| format!("{error:#}"));
        Self {
            runtime,
            theme,
            focus: cx.focus_handle(),
            last_error,
            next_session_number: 2,
        }
    }

    fn dispatch(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        self.last_error = apply_preview_command(&mut self.runtime, command)
            .err()
            .map(|error| format!("{error:#}"));
        self.focus.focus(window);
        cx.notify();
    }

    fn new_session(&mut self, _: &NewSession, window: &mut Window, cx: &mut Context<Self>) {
        let title = format!("Session {}", self.next_session_number);
        self.dispatch(Command::CreateSession { title }, window, cx);
        if self.last_error.is_none() {
            self.next_session_number = self.next_session_number.saturating_add(1);
        }
    }

    fn split(&mut self, axis: SplitAxis, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.runtime.state().active_session() {
            self.dispatch(
                Command::SplitPane {
                    pane: session.active_pane(),
                    axis,
                },
                window,
                cx,
            );
        }
    }

    fn split_right(&mut self, _: &SplitRight, window: &mut Window, cx: &mut Context<Self>) {
        self.split(SplitAxis::Horizontal, window, cx);
    }
    fn split_down(&mut self, _: &SplitDown, window: &mut Window, cx: &mut Context<Self>) {
        self.split(SplitAxis::Vertical, window, cx);
    }
    fn close_pane(&mut self, _: &ClosePane, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.runtime.state().active_session() {
            self.dispatch(Command::ClosePane(session.active_pane()), window, cx);
        }
    }
    fn close_session(&mut self, _: &CloseSession, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.runtime.state().active_session() {
            self.dispatch(Command::CloseSession(session.id()), window, cx);
        }
    }
    fn next_pane(&mut self, _: &NextPane, window: &mut Window, cx: &mut Context<Self>) {
        if self.runtime.state().active_session().is_some() {
            self.dispatch(Command::FocusNextPane, window, cx);
        }
    }
    #[expect(
        clippy::unused_self,
        reason = "GPUI listeners require a mutable view receiver."
    )]
    fn quit(&mut self, _: &Quit, _window: &mut Window, cx: &mut Context<Self>) {
        cx.quit();
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
            .child(section_label("Ctrl+Shift+N to create a session", theme))
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
                            button("close-pane", "Close pane", ButtonVariant::Quiet, theme)
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.close_pane(&ClosePane, window, cx);
                                })),
                        )
                        .child(
                            button(
                                "close-session",
                                "Close session",
                                ButtonVariant::Danger,
                                theme,
                            )
                            .on_click(cx.listener(
                                |view, _, window, cx| view.close_session(&CloseSession, window, cx),
                            )),
                        ),
                );
        }
        toolbar
    }

    fn pane(&self, pane: PaneId, focused: bool, cx: &Context<Self>) -> AnyElement {
        let theme = self.theme;
        let mut content = div()
            .id(SharedString::from(format!("pane-scroll-{pane}")))
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_scroll()
            .p_3();
        if let Some(frame) = self.runtime.frame(pane) {
            content = content.child(terminal_surface(frame, focused, theme));
        } else {
            content = content.child("Terminal frame unavailable");
        }
        div()
            .id(SharedString::from(format!("pane-{pane}")))
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
                focused,
                theme,
            ))
            .child(content)
            .on_click(cx.listener(move |view, _, window, cx| {
                view.dispatch(Command::FocusPane(pane), window, cx);
            }))
            .into_any_element()
    }

    fn layout(&self, layout: &Layout, focused: PaneId, cx: &Context<Self>) -> AnyElement {
        match layout {
            Layout::Pane(pane) => self.pane(*pane, *pane == focused, cx),
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
                .child(self.layout(first, focused, cx))
                .child(self.layout(second, focused, cx))
                .into_any_element(),
        }
    }
}

/// Sample content is a presentation concern, never a terminal-engine default.
fn apply_preview_command(runtime: &mut TerminalWorkspace, command: Command) -> anyhow::Result<()> {
    let previous = runtime.state().pane_ids();
    runtime.apply(command)?;
    for pane in runtime.state().pane_ids() {
        if !previous.contains(&pane) {
            runtime.feed(pane, b"\x1b[1mRex-rs\x1b[0m\r\n\r\n\x1b[36mWorkspace ready.\x1b[0m\r\nThis pane is a terminal preview.\r\nLive shell transport is the next milestone.\r\n")?;
        }
    }
    Ok(())
}

impl Focusable for WorkspaceView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let content = match self.runtime.state().active_session() {
            Some(session) => self.layout(session.layout(), session.active_pane(), cx),
            None => empty_state(theme).into_any_element(),
        };
        let message = self.last_error.clone().unwrap_or_else(|| {
            "Terminal preview · Ctrl+Shift+D / E: split · Ctrl+Tab: next pane".into()
        });
        div()
            .id("workspace")
            .flex()
            .size_full()
            .bg(rgb(theme.colors.background))
            .text_color(rgb(theme.colors.text))
            .track_focus(&self.focus)
            .key_context("Workspace")
            .on_action(cx.listener(Self::new_session))
            .on_action(cx.listener(Self::split_right))
            .on_action(cx.listener(Self::split_down))
            .on_action(cx.listener(Self::close_pane))
            .on_action(cx.listener(Self::close_session))
            .on_action(cx.listener(Self::next_pane))
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
                    .child(div().flex().flex_1().min_h_0().p_3().child(content))
                    .child(status_bar(
                        SharedString::from(message),
                        self.last_error.is_some(),
                        theme,
                    )),
            )
    }
}
