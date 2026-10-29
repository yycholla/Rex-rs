//! Session overview: tabs as layout miniatures with live process activity,
//! detached blocks and the clients attached to the session.
use std::time::Duration;

use super::{Layout, RenameTarget, Session, WorkspaceView};
use crate::{
    core::{Command, PaneId, SessionId, SplitAxis},
    rex::{
        insights::{BlockActivity, SessionInsights},
        workspace::{BlockView, WindowView},
    },
    terminal::PaneStatus,
    ui::{
        actions::NewWindow,
        components::{ButtonVariant, Icon, button, icon, overlay_section, status_dot},
        theme::Theme,
    },
};
use gpui::{
    AnyElement, Context, Div, FontWeight, SharedString, Stateful, div, prelude::*, px, rgb,
};

/// Width of a tab card; cards wrap to fill the available space.
const CARD_WIDTH: f32 = 300.;
/// Height of the layout miniature at the top of a tab card.
const PREVIEW_HEIGHT: f32 = 150.;
/// Delay between activity refreshes while the overview stays open.
const INSIGHTS_INTERVAL: Duration = Duration::from_secs(3);

/// How a pane presents in the overview, from its process and stream status.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Activity {
    Busy,
    Idle,
    Failed,
    Exited,
    Unknown,
}

impl Activity {
    fn of(block: Option<&BlockActivity>, status: Option<&PaneStatus>) -> Self {
        match (block, status) {
            (_, Some(PaneStatus::Failed { .. })) => Self::Failed,
            (Some(block), _) if block.failed() => Self::Failed,
            (Some(block), _) if block.busy => Self::Busy,
            (Some(block), _) if block.program.is_some() => Self::Idle,
            (Some(_), _) | (_, Some(PaneStatus::Exited { .. })) => Self::Exited,
            _ => Self::Unknown,
        }
    }

    const fn color(self, theme: Theme) -> u32 {
        match self {
            Self::Busy => theme.colors.accent,
            Self::Idle => theme.colors.success,
            Self::Failed => theme.colors.danger,
            Self::Exited | Self::Unknown => theme.colors.faint,
        }
    }
}

impl WorkspaceView {
    /// The active session when its overview has been requested.
    pub(super) fn overview_session(&self) -> Option<&Session> {
        let session = self.runtime.state().active_session()?;
        (self.overview == Some(session.id())).then_some(session)
    }

    /// Keeps one activity refresh in flight while the overview is visible and
    /// drops it, cancelling the timer, once the overview closes.
    pub(super) fn schedule_insights(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.overview_session().map(Session::id) else {
            self.insights_task = None;
            return;
        };
        if self.insights_task.is_some() || !self.runtime.connected() {
            return;
        }
        let Ok(future) = self.runtime.refresh_insights(session) else {
            return;
        };
        self.insights_task = Some(cx.spawn(async move |this, cx| {
            let result = future.await;
            let _ = this.update(cx, |view, cx| {
                if let Ok(value) = &result {
                    view.insights = Some((session, SessionInsights::from_value(value)));
                    cx.notify();
                }
            });
            cx.background_executor().timer(INSIGHTS_INTERVAL).await;
            let _ = this.update(cx, |view, cx| {
                view.insights_task = None;
                view.schedule_insights(cx);
            });
        }));
    }

    fn session_insights(&self, session: SessionId) -> Option<&SessionInsights> {
        self.insights
            .as_ref()
            .filter(|(owner, _)| *owner == session)
            .map(|(_, insights)| insights)
    }

    fn pane_activity(&self, pane: PaneId, insights: Option<&SessionInsights>) -> Activity {
        let block = self
            .runtime
            .state()
            .pane(pane)
            .and_then(|block| insights?.activity(&block.remote_id));
        Activity::of(block, self.runtime.status(pane))
    }

    pub(super) fn overview(&self, session: &Session, cx: &Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        let colors = theme.colors;
        let insights = self.session_insights(session.id());
        div()
            .id("session-overview")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .gap_6()
            .p_6()
            .rounded(px(theme.pane_radius))
            .bg(rgb(colors.terminal))
            .border_1()
            .border_color(rgb(colors.border))
            .overflow_y_scroll()
            .child(self.overview_header(session, insights, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(section_heading(
                        "Tabs",
                        session.windows().len(),
                        None,
                        theme,
                    ))
                    .child(self.overview_cards(session, insights, cx)),
            )
            .child(self.overview_detached(session, insights, cx))
            .when_some(insights, |overview, insights| {
                overview.child(self.overview_clients(insights))
            })
    }

    fn overview_header(
        &self,
        session: &Session,
        insights: Option<&SessionInsights>,
        cx: &Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let colors = theme.colors;
        let id = session.id();
        let title = session.title().to_owned();
        let panes: Vec<PaneId> = session
            .windows()
            .iter()
            .filter_map(WindowView::layout)
            .flat_map(Layout::panes)
            .collect();
        let mut summary = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_3()
            .text_size(px(12.))
            .text_color(rgb(colors.muted));
        if insights.is_some() {
            for (activity, label) in [
                (Activity::Busy, "running"),
                (Activity::Idle, "idle"),
                (Activity::Failed, "failed"),
            ] {
                let count = panes
                    .iter()
                    .filter(|pane| self.pane_activity(**pane, insights) == activity)
                    .count();
                if count > 0 {
                    summary = summary.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(status_dot(activity.color(theme)))
                            .child(format!("{count} {label}")),
                    );
                }
            }
        }
        div()
            .flex()
            .items_end()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(11.))
                            .text_color(rgb(colors.faint))
                            .child(icon(Icon::Server, 11., colors.faint))
                            .child("Local · session"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_3()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(22.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title.clone()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(12.))
                                    .text_color(rgb(colors.faint))
                                    .child(format!(
                                        "{} · {}",
                                        plural(session.windows().len(), "tab"),
                                        plural(panes.len(), "pane")
                                    )),
                            ),
                    )
                    .child(summary),
            )
            .child(
                button("overview-rename", "Rename", ButtonVariant::Quiet, theme).on_click(
                    cx.listener(move |view, _, window, cx| {
                        view.open_rename(RenameTarget::Session(id), title.clone(), window, cx);
                    }),
                ),
            )
            .child(
                button("overview-new-tab", "New tab", ButtonVariant::Primary, theme).on_click(
                    cx.listener(|view, _, window, cx| view.new_window(&NewWindow, window, cx)),
                ),
            )
    }

    fn overview_cards(
        &self,
        session: &Session,
        insights: Option<&SessionInsights>,
        cx: &Context<Self>,
    ) -> Div {
        let colors = self.theme.colors;
        let selected = session.active_window().map(WindowView::id);
        div()
            .flex()
            .flex_wrap()
            .gap_3()
            .children(
                session
                    .windows()
                    .iter()
                    .map(|tab| self.tab_card(tab, Some(tab.id()) == selected, insights, cx)),
            )
            .child(
                div()
                    .id("overview-new-tab-card")
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .w(px(CARD_WIDTH))
                    .min_h(px(PREVIEW_HEIGHT + 56.))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(rgb(colors.border))
                    .text_size(px(12.))
                    .text_color(rgb(colors.faint))
                    .cursor_pointer()
                    .hover(move |style| style.bg(rgb(colors.hover)))
                    .child(icon(Icon::Plus, 18., colors.faint))
                    .child("New tab")
                    .on_click(
                        cx.listener(|view, _, window, cx| view.new_window(&NewWindow, window, cx)),
                    ),
            )
    }

    fn overview_detached(
        &self,
        session: &Session,
        insights: Option<&SessionInsights>,
        cx: &Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let colors = theme.colors;
        let id = session.id();
        let detached = self.runtime.state().detached_blocks(id);
        let manage = div()
            .id("overview-manage-detached")
            .text_size(px(12.))
            .text_color(rgb(colors.accent))
            .cursor_pointer()
            .hover(gpui::Styled::underline)
            .child("Manage…")
            .on_click(cx.listener(move |view, _, window, cx| {
                view.request_detached_browser(id, window, cx);
            }))
            .into_any_element();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_heading(
                "Detached blocks",
                detached.len(),
                Some(manage),
                theme,
            ))
            .child(if detached.is_empty() {
                div()
                    .text_size(px(12.))
                    .text_color(rgb(colors.faint))
                    .child("Nothing is detached. Detached blocks keep running here.")
            } else {
                div().flex().flex_wrap().gap_2().children(
                    detached
                        .into_iter()
                        .map(|block| self.detached_chip(block, insights)),
                )
            })
    }

    fn detached_chip(&self, block: &BlockView, insights: Option<&SessionInsights>) -> Div {
        let theme = self.theme;
        let colors = theme.colors;
        let process = insights.and_then(|insights| insights.activity(&block.remote_id));
        let activity = Activity::of(process, self.runtime.status(block.id));
        let label = if block.title().is_empty() {
            "Unnamed block".to_owned()
        } else {
            block.title().to_owned()
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(34.))
            .max_w(px(CARD_WIDTH))
            .rounded(px(8.))
            .bg(rgb(colors.surface))
            .border_1()
            .border_color(rgb(colors.border))
            .child(status_dot(activity.color(theme)))
            .child(div().truncate().child(label))
            .when_some(process.and_then(activity_label), |chip, detail| {
                chip.child(
                    div()
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(rgb(colors.faint))
                        .child(detail),
                )
            })
    }

    fn overview_clients(&self, insights: &SessionInsights) -> Div {
        let theme = self.theme;
        let colors = theme.colors;
        let own = self.runtime.state().client_id();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_heading(
                "Attached clients",
                insights.clients.len(),
                None,
                theme,
            ))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children(insights.clients.iter().map(|client| {
                        let detail = [client.kind.as_str(), client.principal.as_str()]
                            .into_iter()
                            .filter(|part| !part.is_empty())
                            .collect::<Vec<_>>()
                            .join(" · ");
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .h(px(34.))
                            .rounded(px(8.))
                            .bg(rgb(colors.surface))
                            .border_1()
                            .border_color(rgb(colors.border))
                            .child(icon(
                                if client.kind == "cli" {
                                    Icon::Terminal
                                } else {
                                    Icon::Sessions
                                },
                                13.,
                                colors.muted,
                            ))
                            .child(client.name.clone())
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(colors.faint))
                                    .child(detail),
                            )
                            .when(client.id == own, |chip| {
                                chip.child(
                                    div()
                                        .px(px(6.))
                                        .rounded_full()
                                        .bg(rgb(colors.accent_muted))
                                        .text_size(px(10.))
                                        .child("This app"),
                                )
                            })
                    })),
            )
    }

    fn tab_card(
        &self,
        tab: &WindowView,
        current: bool,
        insights: Option<&SessionInsights>,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let theme = self.theme;
        let colors = theme.colors;
        let tab_id = tab.id();
        let panes = tab.layout().map(Layout::panes).unwrap_or_default();
        let busy = panes
            .iter()
            .filter(|pane| self.pane_activity(**pane, insights) == Activity::Busy)
            .count();
        let preview = tab.layout().map_or_else(
            || {
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size_full()
                    .text_size(px(11.))
                    .text_color(rgb(colors.faint))
                    .child("No tiled layout")
                    .into_any_element()
            },
            |layout| self.miniature(layout, tab.active_pane(), insights),
        );
        div()
            .id(SharedString::from(format!("overview-tab-{tab_id}")))
            .flex()
            .flex_col()
            .w(px(CARD_WIDTH))
            .rounded(px(10.))
            .overflow_hidden()
            .bg(rgb(colors.surface))
            .border_1()
            .border_color(rgb(if current {
                colors.accent_muted
            } else {
                colors.border
            }))
            .cursor_pointer()
            .hover(move |style| style.border_color(rgb(colors.border_strong)))
            .child(
                div()
                    .flex()
                    .h(px(PREVIEW_HEIGHT))
                    .p(px(10.))
                    .bg(rgb(colors.terminal))
                    .border_b_1()
                    .border_color(rgb(colors.border))
                    .child(preview),
            )
            .child(self.tab_card_footer(tab, current, busy, insights.is_some()))
            .on_click(cx.listener(move |view, _, window, cx| {
                view.dispatch(Command::SelectWindow(tab_id), window, cx);
            }))
    }

    fn tab_card_footer(&self, tab: &WindowView, current: bool, busy: usize, live: bool) -> Div {
        let theme = self.theme;
        let colors = theme.colors;
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py(px(10.))
            .child(icon(
                Icon::Terminal,
                14.,
                if current { colors.accent } else { colors.muted },
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(tab.title().to_owned()),
                    )
                    .when_some(self.tab_detail(tab), |column, detail| {
                        column.child(
                            div()
                                .truncate()
                                .font_family(theme.terminal.font_family)
                                .text_size(px(11.))
                                .text_color(rgb(colors.faint))
                                .child(detail),
                        )
                    }),
            )
            .when(live, |footer| {
                footer.child(
                    div()
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(rgb(if busy > 0 {
                            colors.accent
                        } else {
                            colors.faint
                        }))
                        .child(if busy > 0 {
                            format!("{busy} running")
                        } else {
                            "Idle".to_owned()
                        }),
                )
            })
    }

    /// Scaled-down split tree using the same server-owned ratios as the panes.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "GPUI layout uses f32 fractions; the validated server ratio is finite and between zero and one."
    )]
    fn miniature(
        &self,
        layout: &Layout,
        focused: Option<PaneId>,
        insights: Option<&SessionInsights>,
    ) -> AnyElement {
        match layout {
            Layout::Pane(pane) => self.miniature_pane(*pane, Some(*pane) == focused, insights),
            Layout::Split {
                axis,
                ratio,
                first,
                second,
                ..
            } => {
                let weighted = |child: &Layout, share: f64| {
                    let mut slot = div()
                        .flex()
                        .flex_basis(px(0.))
                        .size_full()
                        .min_w_0()
                        .min_h_0();
                    slot.style().flex_grow = Some(share as f32);
                    slot.child(self.miniature(child, focused, insights))
                };
                div()
                    .flex()
                    .flex_1()
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .gap(px(4.))
                    .when(*axis == SplitAxis::Vertical, gpui::Styled::flex_col)
                    .child(weighted(first, *ratio))
                    .child(weighted(second, 1.0 - *ratio))
                    .into_any_element()
            }
        }
    }

    /// One pane in a miniature: its foreground program and working directory
    /// once activity has loaded, otherwise its title.
    fn miniature_pane(
        &self,
        pane: PaneId,
        focused: bool,
        insights: Option<&SessionInsights>,
    ) -> AnyElement {
        let theme = self.theme;
        let colors = theme.colors;
        let process = self
            .runtime
            .state()
            .pane(pane)
            .and_then(|block| insights?.activity(&block.remote_id));
        let activity = Activity::of(process, self.runtime.status(pane));
        let heading = process
            .and_then(activity_label)
            .unwrap_or_else(|| self.runtime.pane_label(pane));
        div()
            .flex()
            .flex_col()
            .flex_1()
            .size_full()
            .min_w_0()
            .min_h_0()
            .gap(px(2.))
            .p(px(6.))
            .overflow_hidden()
            .rounded(px(4.))
            .bg(rgb(colors.background))
            .border_1()
            .border_color(rgb(if focused {
                colors.accent_muted
            } else {
                colors.border
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(status_dot(activity.color(theme)).size(px(5.)))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(10.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(colors.text))
                            .child(heading),
                    ),
            )
            .when_some(process.and_then(|block| block.cwd.clone()), |pane, cwd| {
                pane.child(
                    div()
                        .truncate()
                        .font_family(theme.terminal.font_family)
                        .text_size(px(9.))
                        .text_color(rgb(colors.faint))
                        .child(cwd),
                )
            })
            .into_any_element()
    }
}

/// Program name, or the exit code once the process has ended.
fn activity_label(block: &BlockActivity) -> Option<String> {
    block
        .program
        .clone()
        .or_else(|| block.last_exit.map(|code| format!("exited {code}")))
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Uppercase section label with its count beside it and an optional action.
fn section_heading(
    label: &'static str,
    count: usize,
    action: Option<AnyElement>,
    theme: Theme,
) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(overlay_section(label, theme).flex_none())
        .child(
            div()
                .pt_1()
                .flex_1()
                .text_size(px(11.))
                .text_color(rgb(theme.colors.muted))
                .child(count.to_string()),
        )
        .children(action)
}
