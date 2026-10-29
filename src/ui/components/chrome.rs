use super::{Icon, icon};
use crate::ui::theme::Theme;
use gpui::{
    AnyElement, Div, ElementId, FontWeight, SharedString, Stateful, div, prelude::*, px, rgb,
};

/// Vertical tabs show at most this many pane markers, as in Rex.
const MAX_PANE_MARKERS: usize = 3;

/// Host group heading ("LOCAL") with the connection state as a trailing dot.
pub fn sidebar_heading(label: impl Into<SharedString>, status_color: u32, theme: Theme) -> Div {
    let label: SharedString = label.into();
    let colors = theme.colors;
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .h(px(26.))
        .px_2()
        .text_size(px(10.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(colors.faint))
        .child(icon(Icon::Server, 11., colors.faint))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(label.to_uppercase()),
        )
        .child(status_dot(status_color))
}

fn sidebar_row(id: ElementId, selected: bool, theme: Theme) -> Stateful<Div> {
    let colors = theme.colors;
    div()
        .id(id)
        .flex()
        .flex_none()
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

/// A session group header. Its tabs follow as indented rows; callers may
/// append trailing indicators such as a detached-block badge. An `editor`
/// replaces the title while it is being renamed in place.
#[must_use]
pub fn session_row(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    editor: Option<AnyElement>,
    active: bool,
    theme: Theme,
) -> Stateful<Div> {
    let title: SharedString = title.into();
    let colors = theme.colors;
    sidebar_row(id.into(), false, theme)
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(if active { colors.text } else { colors.muted }))
        .child(icon(Icon::ChevronDown, 12., colors.faint))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .map(|slot| match editor {
                    Some(editor) => slot.font_weight(FontWeight::NORMAL).child(editor),
                    None => slot.child(title),
                }),
        )
}

/// A compact pill pairing an icon with a count.
#[must_use]
pub fn count_badge(
    id: impl Into<ElementId>,
    glyph: Icon,
    count: usize,
    theme: Theme,
) -> Stateful<Div> {
    let colors = theme.colors;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(4.))
        .px(px(6.))
        .h(px(18.))
        .rounded_full()
        .bg(rgb(colors.selected))
        .text_size(px(11.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(colors.muted))
        .cursor_pointer()
        .hover(move |style| {
            style
                .bg(rgb(colors.border_strong))
                .text_color(rgb(colors.text))
        })
        .child(icon(glyph, 11., colors.muted))
        .child(count.to_string())
}

/// A vertical tab with a miniature of the panes it contains. An `editor`
/// replaces the title while it is being renamed in place.
#[must_use]
pub fn tab_row(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    editor: Option<AnyElement>,
    detail: Option<SharedString>,
    selected: bool,
    panes: usize,
    theme: Theme,
) -> Stateful<Div> {
    let title: SharedString = title.into();
    let colors = theme.colors;
    let marker = if selected { colors.muted } else { colors.faint };
    let markers = (0..panes.min(MAX_PANE_MARKERS))
        .map(move |_| div().w(px(5.)).h(px(8.)).rounded(px(1.)).bg(rgb(marker)));
    sidebar_row(id.into(), selected, theme)
        .h(px(if detail.is_some() {
            theme.row_height + 16.
        } else {
            theme.row_height + 4.
        }))
        .gap_2()
        .pl(px(24.))
        .rounded(px(8.))
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(24.))
                .rounded(px(6.))
                .bg(rgb(if selected {
                    colors.accent_muted
                } else {
                    colors.hover
                }))
                .child(icon(
                    Icon::Terminal,
                    14.,
                    if selected { colors.text } else { colors.muted },
                )),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap(px(1.))
                .map(|column| match editor {
                    // Offset the field's inset so its text stays aligned with the detail.
                    Some(editor) => column.child(div().ml(px(-7.)).child(editor)),
                    None => column.child(
                        div()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(if selected { colors.text } else { colors.muted }))
                            .child(title),
                    ),
                })
                .when_some(detail, |column, detail| {
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
        .when(panes > 1, |row| {
            row.child(
                div()
                    .flex()
                    .flex_none()
                    .gap(px(2.))
                    .p(px(2.))
                    .rounded(px(3.))
                    .border_1()
                    .border_color(rgb(marker))
                    .children(markers),
            )
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
        .gap_2()
        .when(indent, |row| row.pl(px(28.)))
        .text_size(px(12.))
        .text_color(rgb(colors.faint))
        .child(icon(Icon::Plus, 13., colors.faint))
        .child(label)
}

#[must_use]
pub fn status_dot(color: u32) -> Div {
    div().flex_none().size(px(6.)).rounded_full().bg(rgb(color))
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
        .pr(px(4.))
        .h(px(theme.pane_header_height))
        .flex_shrink_0()
        .bg(rgb(if focused {
            colors.surface
        } else {
            colors.terminal
        }))
        .border_b_1()
        .border_color(rgb(colors.border))
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(if focused { colors.text } else { colors.muted }))
        .child(status_dot(status_color))
        .child(div().flex_1().min_w_0().truncate().child(title))
}

/// A thin vertical rule separating groups of header controls.
#[must_use]
pub fn control_separator(theme: Theme) -> Div {
    div()
        .flex_none()
        .w(px(1.))
        .h(px(14.))
        .mx(px(4.))
        .bg(rgb(theme.colors.border))
}

/// Server and error status pinned to the bottom of the sidebar.
pub fn sidebar_footer(message: impl Into<SharedString>, error: bool, theme: Theme) -> Div {
    let message: SharedString = message.into();
    let colors = theme.colors;
    div()
        .flex()
        .items_center()
        .gap_2()
        .min_h(px(36.))
        .mx_2()
        .mb_2()
        .px_3()
        .py(px(6.))
        .flex_shrink_0()
        .rounded(px(8.))
        .bg(rgb(colors.surface))
        .border_1()
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
pub fn empty_state(
    title: impl Into<SharedString>,
    message: impl Into<SharedString>,
    theme: Theme,
) -> Div {
    let title: SharedString = title.into();
    let message: SharedString = message.into();
    let colors = theme.colors;
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .size_full()
        .gap_3()
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(px(44.))
                .rounded(px(12.))
                .bg(rgb(colors.surface))
                .border_1()
                .border_color(rgb(colors.border))
                .child(icon(Icon::Terminal, 20., colors.muted)),
        )
        .child(
            div()
                .text_size(px(14.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(colors.text))
                .child(title),
        )
        .child(
            div()
                .max_w(px(360.))
                .text_size(px(12.))
                .text_color(rgb(colors.muted))
                .child(message),
        )
}
