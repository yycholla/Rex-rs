//! Existing-process layout management, independent of terminal rendering.
use super::WorkspaceView;
use crate::{
    core::{Command, PaneId, SessionId, SplitAxis, WindowId},
    ui::{
        components::{
            ButtonVariant, Icon, button, drawer, icon, icon_button, overlay_card, overlay_footer,
            overlay_header, overlay_section, segment, segment_group,
        },
        theme::Theme,
    },
};
use gpui::{Context, Div, KeyDownEvent, Render, SharedString, Window, div, prelude::*, px, rgb};

#[derive(Clone)]
pub(super) struct TabDrag {
    pub(super) session: SessionId,
    pub(super) window: WindowId,
    pub(super) label: String,
    theme: Theme,
    instance: String,
    generation: u64,
}

impl TabDrag {
    pub(super) fn new(
        session: SessionId,
        window: WindowId,
        label: String,
        theme: Theme,
        instance: String,
        generation: u64,
    ) -> Self {
        Self {
            session,
            window,
            label,
            theme,
            instance,
            generation,
        }
    }
}

impl Render for TabDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .p_2()
            .rounded_md()
            .bg(rgb(self.theme.colors.raised))
            .text_color(rgb(self.theme.colors.text))
            .child(self.label.clone())
    }
}

impl WorkspaceView {
    pub(super) fn request_arrangement(
        &mut self,
        pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.defer_arrangement(pane, cx) {
            return;
        }
        self.open_arrangement(pane, window, cx);
    }

    pub(super) fn open_arrangement(
        &mut self,
        pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.rename.is_some() || self.runtime.state().pane(pane).is_none() {
            return;
        }
        if let Ok(resolver) = self.shortcuts.resolver_mut() {
            resolver.cancel_sequence();
        }
        self.detached_browser = None;
        self.arranging = Some(pane);
        self.focus.focus(window);
        cx.notify();
    }

    pub(super) fn arrangement_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.arranging.is_none() {
            return false;
        }
        if event.keystroke.key == "escape" {
            self.cancel_arrangement(window, cx);
        }
        cx.stop_propagation();
        true
    }

    pub(super) fn cancel_arrangement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.arranging = None;
        self.focus_active(window, cx);
        cx.notify();
    }

    fn arrange_command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        self.arranging = None;
        self.dispatch(command, window, cx);
    }

    pub(super) fn arrangement_dialog(&self, source: PaneId, cx: &Context<Self>) -> Div {
        let theme = self.theme;
        let block = self.runtime.state().pane(source);
        let session = block.and_then(|block| {
            self.runtime
                .state()
                .sessions()
                .iter()
                .find(|session| session.remote_id() == block.session_id)
        });
        let mut targets = div()
            .id("arrangement-targets")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(rgb(theme.colors.border))
            .overflow_y_scroll();
        let mut count = 0;
        if let Some(session) = session {
            for tab in session.windows() {
                let anchors: Vec<_> = tab
                    .layout()
                    .into_iter()
                    .flat_map(super::Layout::panes)
                    .filter(|pane| *pane != source)
                    .collect();
                if anchors.is_empty() {
                    continue;
                }
                targets = targets.child(overlay_section(tab.title().to_owned(), theme));
                for anchor in anchors {
                    count += 1;
                    targets = targets.child(self.arrangement_target(
                        source,
                        anchor,
                        self.runtime.pane_label(anchor),
                        cx,
                    ));
                }
            }
        }
        if count == 0 {
            targets = targets.child(
                div()
                    .p_4()
                    .text_size(px(12.))
                    .text_color(rgb(theme.colors.muted))
                    .child("Create another pane or tab to choose a destination."),
            );
        }
        let title = if self.runtime.state().is_detached(source) {
            "Place detached block"
        } else {
            "Move or swap pane"
        };
        let close = icon_button("arrange-close", Icon::Close, theme)
            .on_click(cx.listener(|view, _, window, cx| view.cancel_arrangement(window, cx)))
            .into_any_element();
        drawer(
            div()
                .child(overlay_header(
                    title,
                    Some(
                        format!(
                            "Choose a destination for {}.",
                            self.runtime.pane_label(source)
                        )
                        .into(),
                    ),
                    close,
                    theme,
                ))
                .child(targets)
                .child(overlay_footer(theme).child(
                    button("arrange-cancel", "Cancel", ButtonVariant::Quiet, theme).on_click(
                        cx.listener(|view, _, window, cx| view.cancel_arrangement(window, cx)),
                    ),
                )),
            theme,
        )
    }

    fn arrangement_target(
        &self,
        source: PaneId,
        anchor: PaneId,
        label: String,
        cx: &Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let colors = theme.colors;
        let mut controls = div().flex().gap_2();
        {
            let mut directions = segment_group(theme).flex_1();
            for (name, axis, before) in [
                ("Left", SplitAxis::Horizontal, true),
                ("Right", SplitAxis::Horizontal, false),
                ("Above", SplitAxis::Vertical, true),
                ("Below", SplitAxis::Vertical, false),
            ] {
                directions = directions.child(
                    segment(
                        SharedString::from(format!("move-{source}-{anchor}-{name}")),
                        name,
                        theme,
                    )
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.arrange_command(
                            Command::MovePane {
                                pane: source,
                                anchor,
                                axis,
                                before,
                                ratio: 0.5,
                            },
                            window,
                            cx,
                        );
                    })),
                );
            }
            controls = controls.child(directions);
        }
        if !self.runtime.state().is_detached(source) {
            controls = controls.child(
                segment_group(theme).child(
                    segment(
                        SharedString::from(format!("swap-{source}-{anchor}")),
                        "Swap",
                        theme,
                    )
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.arrange_command(
                            Command::SwapPanes {
                                pane: source,
                                other: anchor,
                            },
                            window,
                            cx,
                        );
                    })),
                ),
            );
        }
        overlay_card(theme)
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(icon(Icon::Terminal, 13., colors.faint))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .child(label),
                    ),
            )
            .child(controls)
    }

    pub(super) fn drop_tab(
        &mut self,
        drag: &TabDrag,
        session: SessionId,
        before: Option<WindowId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let connection = self
            .runtime
            .server_id()
            .filter(|_| self.runtime.connected())
            .map(|instance| (instance, self.runtime.connection_generation()));
        match tab_drop_command(drag, connection, session, before) {
            Ok(Some(command)) => self.dispatch(command, window, cx),
            Ok(None) => {}
            Err(message) => self.report_error(message.into(), cx),
        }
    }
}

fn tab_drop_command(
    drag: &TabDrag,
    connection: Option<(&str, u64)>,
    session: SessionId,
    before: Option<WindowId>,
) -> Result<Option<Command>, &'static str> {
    if connection != Some((drag.instance.as_str(), drag.generation)) {
        return Err("This drag belongs to an earlier connection. Drag the tab again.");
    }
    if drag.session != session {
        return Err("Tabs can be reordered within their session.");
    }
    if before == Some(drag.window) {
        return Ok(None);
    }
    Ok(Some(Command::MoveWindow {
        window: drag.window,
        before,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;

    #[test]
    fn old_drag_cannot_target_reused_aliases_after_server_replacement_or_resume()
    -> anyhow::Result<()> {
        let session = SessionId::from_value(1).context("session alias")?;
        let window = WindowId::from_value(2).context("window alias")?;
        let before = WindowId::from_value(3).context("target alias")?;
        let drag = TabDrag::new(
            session,
            window,
            "Old tab".into(),
            Theme::default(),
            "old-server".into(),
            7,
        );
        // The new server can have the identical numeric aliases. They identify
        // different resources, so no command may escape this UI boundary.
        assert!(tab_drop_command(&drag, Some(("new-server", 8)), session, Some(before)).is_err());
        assert!(tab_drop_command(&drag, Some(("old-server", 8)), session, Some(before)).is_err());
        assert!(tab_drop_command(&drag, None, session, Some(before)).is_err());
        assert!(
            matches!(tab_drop_command(&drag,Some(("old-server",7)),session,Some(before)).map_err(anyhow::Error::msg)?,Some(Command::MoveWindow { window:source,before:Some(target) }) if source==window && target==before)
        );
        Ok(())
    }
}
