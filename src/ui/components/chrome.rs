use super::{Icon, icon};
use crate::ui::theme::Theme;
use gpui::{Div, ElementId, FontWeight, SharedString, Stateful, div, prelude::*, px, rgb};

/// Vertical tabs show at most this many pane markers, as in Rex.
const MAX_PANE_MARKERS: usize = 3;

/// Small uppercase-free group heading, like Rex's host header ("Local").
pub fn sidebar_heading(label: impl Into<SharedString>, theme: Theme) -> Div {
    let label: SharedString = label.into();
    div()
        .flex()
        .items_center()
        .justify_between()
        .h(px(24.))
        .px_2()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(theme.colors.faint))
        .child(label)
}

fn sidebar_row(id: ElementId, selected: bool, theme: Theme) -> Stateful<Div> {
    let colors = theme.colors;
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(6.))
        .h(px(theme.row_height))
        .px_2()
        .rounded(px(6.))
        .text_size(px(13.))
        .cursor_pointer()
        .when(selected, |row| row.bg(rgb(colors.selected)))
        .when(!selected, |row| {
            row.hover(move |style| style.bg(rgb(colors.hover)))
        })
}

/// A session group header. Its tabs follow as indented rows.
#[must_use]
pub fn session_row(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    active: bool,
    theme: Theme,
) -> Stateful<Div> {
    let title: SharedString = title.into();
    let colors = theme.colors;
    sidebar_row(id.into(), false, theme)
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(if active { colors.text } else { colors.muted }))
        .child(icon(Icon::ChevronDown, 12., colors.faint))
        .child(div().flex_1().min_w_0().truncate().child(title))
}

/// A vertical tab with up to three markers for the panes it contains.
#[must_use]
pub fn tab_row(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    selected: bool,
    panes: usize,
    theme: Theme,
) -> Stateful<Div> {
    let title: SharedString = title.into();
    let colors = theme.colors;
    let markers = (0..panes.min(MAX_PANE_MARKERS)).map(move |_| {
        div()
            .w(px(7.))
            .h(px(9.))
            .rounded(px(2.))
            .border_1()
            .border_color(rgb(if selected { colors.muted } else { colors.faint }))
    });
    sidebar_row(id.into(), selected, theme)
        .pl(px(26.))
        .text_color(rgb(if selected { colors.text } else { colors.muted }))
        .child(icon(
            Icon::Terminal,
            14.,
            if selected {
                colors.accent
            } else {
                colors.faint
            },
        ))
        .child(div().flex_1().min_w_0().truncate().child(title))
        .when(panes > 1, |row| {
            row.child(div().flex().flex_none().gap(px(3.)).children(markers))
        })
}

/// A quiet "+ label" row for creating sessions and tabs.
#[must_use]
pub fn add_row(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    indent: bool,
    theme: Theme,
) -> Stateful<Div> {
    let label: SharedString = label.into();
    let colors = theme.colors;
    sidebar_row(id.into(), false, theme)
        .when(indent, |row| row.pl(px(26.)))
        .text_color(rgb(colors.faint))
        .child(icon(Icon::Plus, 14., colors.faint))
        .child(label)
}

#[must_use]
pub fn status_dot(color: u32) -> Div {
    div().flex_none().size(px(7.)).rounded_full().bg(rgb(color))
}

/// Title strip above a pane. Callers append trailing controls.
pub fn pane_header(
    title: impl Into<SharedString>,
    status_color: u32,
    focused: bool,
    theme: Theme,
) -> Div {
    let title: SharedString = title.into();
    let colors = theme.colors;
    div()
        .flex()
        .items_center()
        .gap_2()
        .pl_3()
        .pr_1()
        .h(px(theme.pane_header_height))
        .flex_shrink_0()
        .bg(rgb(if focused {
            colors.raised
        } else {
            colors.surface
        }))
        .border_b_1()
        .border_color(rgb(colors.border))
        .text_size(px(12.))
        .text_color(rgb(if focused { colors.text } else { colors.muted }))
        .child(status_dot(status_color))
        .child(div().flex_1().min_w_0().truncate().child(title))
}

/// Server and error status pinned to the bottom of the sidebar.
pub fn sidebar_footer(message: impl Into<SharedString>, error: bool, theme: Theme) -> Div {
    let message: SharedString = message.into();
    let colors = theme.colors;
    div()
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_2()
        .flex_shrink_0()
        .border_t_1()
        .border_color(rgb(colors.border))
        .text_size(px(11.))
        .text_color(rgb(if error { colors.danger } else { colors.muted }))
        .child(status_dot(if error {
            colors.danger
        } else {
            colors.success
        }))
        .child(div().flex_1().min_w_0().child(message))
}

/// Callers append the primary action so they can attach its behavior.
#[must_use]
pub fn empty_state(theme: Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .size_full()
        .gap_3()
        .child(icon(Icon::Sessions, 28., theme.colors.faint))
        .child(
            div()
                .text_size(px(13.))
                .text_color(rgb(theme.colors.muted))
                .child("Open a new session to start a terminal."),
        )
}
