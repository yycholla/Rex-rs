//! Detached resources stay on Rex; this browser only changes their placement.
use super::WorkspaceView;
use crate::{
    core::{Command, PaneId, SessionId},
    rex::automation::{ActionError, ActionFuture},
    ui::components::{
        ButtonVariant, Icon, button, drawer, icon, icon_button, overlay_card, overlay_footer,
        overlay_header, tooltip,
    },
};
use gpui::{
    Context, Div, FontWeight, KeyDownEvent, SharedString, Window, div, prelude::*, px, rgb,
};

impl WorkspaceView {
    pub(super) fn has_modal(&self) -> bool {
        self.rename.is_some() || self.arranging.is_some() || self.detached_browser.is_some()
    }

    pub(super) fn request_detached_browser(
        &mut self,
        session: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.defer_browser(session, cx) {
            return;
        }
        match self.open_detached_browser(session, window, cx) {
            Ok(future) => {
                cx.spawn_in(window, async move |this, cx| {
                    let result = future.await;
                    let _ = this.update_in(cx, |view, _, cx| {
                        view.poll(cx);
                        if let Err(error) = result {
                            view.report_error(format!("{error:#}"), cx);
                        }
                    });
                })
                .detach();
            }
            Err(error) => self.report_error(error.message, cx),
        }
    }

    pub(super) fn open_detached_browser(
        &mut self,
        session: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<ActionFuture, ActionError> {
        if self.rename.is_some() {
            return Err(ActionError::refused(
                "Finish renaming before browsing detached blocks",
            ));
        }
        let future = self
            .runtime
            .refresh_inventory(session)
            .map_err(|error| ActionError::refused(error.to_string()))?;
        self.arranging = None;
        if let Ok(resolver) = self.shortcuts.resolver_mut() {
            resolver.cancel_sequence();
        }
        self.detached_browser = Some(session);
        self.focus.focus(window);
        cx.notify();
        Ok(future)
    }

    pub(super) fn cancel_detached_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.detached_browser = None;
        self.focus_active(window, cx);
        cx.notify();
    }

    pub(super) fn detached_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.detached_browser.is_none() {
            return false;
        }
        if event.keystroke.key == "escape" {
            self.cancel_detached_browser(window, cx);
        }
        cx.stop_propagation();
        true
    }

    fn restore_detached(&mut self, pane: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        self.detached_browser = None;
        self.submit_command(
            Command::RestorePane {
                pane,
                title: "Restored block".into(),
            },
            window,
            cx,
        );
    }

    fn detached_row(
        &self,
        block: &crate::rex::workspace::BlockView,
        can_place: bool,
        cx: &Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let colors = theme.colors;
        let pane = block.id;
        let label = if block.title().is_empty() {
            "Unnamed block"
        } else {
            block.title()
        };
        let kind = if block.is_terminal() {
            "Terminal"
        } else {
            "Other block"
        };
        let summary = div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(30.))
                    .rounded(px(7.))
                    .bg(rgb(colors.selected))
                    .child(icon(Icon::Terminal, 15., colors.muted)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(13.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(label.to_owned()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(rgb(colors.faint))
                            .child(format!("{kind} · {}", block.remote_id)),
                    ),
            )
            .child(
                icon_button(
                    SharedString::from(format!("close-detached-{pane}")),
                    Icon::Close,
                    theme,
                )
                .tooltip(tooltip("Close block", theme))
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.submit_command(Command::ClosePane(pane), window, cx);
                })),
            );
        let actions = div()
            .flex()
            .gap_2()
            .child(
                button(
                    SharedString::from(format!("restore-{pane}")),
                    "Open in new tab",
                    ButtonVariant::Soft,
                    theme,
                )
                .flex_1()
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.restore_detached(pane, window, cx);
                })),
            )
            .when(can_place, |row| {
                row.child(
                    button(
                        SharedString::from(format!("place-{pane}")),
                        "Place beside…",
                        ButtonVariant::Quiet,
                        theme,
                    )
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.detached_browser = None;
                        view.request_arrangement(pane, window, cx);
                    })),
                )
            });
        overlay_card(theme).child(summary).child(actions)
    }

    pub(super) fn detached_dialog(&self, session: SessionId, cx: &Context<Self>) -> Div {
        let theme = self.theme;
        let state = self.runtime.state();
        let entries = state.detached_blocks(session);
        let entry = state.sessions().iter().find(|entry| entry.id() == session);
        let mut list = div()
            .id("detached-list")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(rgb(theme.colors.border))
            .overflow_y_scroll();
        if entries.is_empty() {
            list = list.child(
                div()
                    .p_4()
                    .text_size(px(12.))
                    .text_color(rgb(theme.colors.muted))
                    .child("No detached blocks in this session."),
            );
        }
        let can_place =
            entry.is_some_and(|entry| entry.windows().iter().any(|tab| tab.layout().is_some()));
        for block in entries {
            list = list.child(self.detached_row(block, can_place, cx));
        }
        let subtitle = entry.map_or_else(
            || "These keep running until you close them.".to_owned(),
            |entry| {
                format!(
                    "{} · These keep running until you close them.",
                    entry.title()
                )
            },
        );
        let close = icon_button("detached-close", Icon::Close, theme)
            .on_click(cx.listener(|view, _, window, cx| view.cancel_detached_browser(window, cx)))
            .into_any_element();
        let footer = overlay_footer(theme)
            .justify_between()
            .child(
                button("detached-refresh", "Refresh", ButtonVariant::Quiet, theme).on_click(
                    cx.listener(move |view, _, window, cx| {
                        view.request_detached_browser(session, window, cx);
                    }),
                ),
            )
            .child(
                button("detached-dismiss", "Done", ButtonVariant::Quiet, theme).on_click(
                    cx.listener(|view, _, window, cx| view.cancel_detached_browser(window, cx)),
                ),
            );
        drawer(
            div()
                .child(overlay_header(
                    "Detached blocks",
                    Some(subtitle.into()),
                    close,
                    theme,
                ))
                .child(list)
                .child(footer),
            theme,
        )
    }
}
