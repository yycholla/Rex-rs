//! Modal and drawer surfaces. Callers attach dismissal and actions.
use crate::ui::theme::Theme;
use gpui::{
    AnyElement, Div, ElementId, FontWeight, SharedString, Stateful, div, prelude::*, px, rgb, rgba,
};

/// Width of the right-hand drawer for browsing and placement tasks.
const DRAWER_WIDTH: f32 = 380.;

/// Full-window layer that blocks pointer input to the workspace beneath.
fn scrim(alpha: u8) -> Div {
    div()
        .absolute()
        .inset_0()
        .occlude()
        .bg(rgba(u32::from(alpha)))
}

/// Title, optional supporting text and a trailing close control.
#[must_use]
pub fn overlay_header(
    title: impl Into<SharedString>,
    subtitle: Option<SharedString>,
    close: AnyElement,
    theme: Theme,
) -> Div {
    let title: SharedString = title.into();
    let colors = theme.colors;
    div()
        .flex()
        .flex_none()
        .items_start()
        .gap_3()
        .px_4()
        .pt_4()
        .pb_3()
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .text_size(px(14.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(colors.text))
                        .child(title),
                )
                .when_some(subtitle, |column, subtitle| {
                    column.child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(colors.muted))
                            .child(subtitle),
                    )
                }),
        )
        .child(close)
}

/// Action row closing a modal or drawer.
#[must_use]
pub fn overlay_footer(theme: Theme) -> Div {
    let colors = theme.colors;
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_end()
        .gap_2()
        .px_4()
        .py_3()
        .bg(rgb(colors.footer))
        .border_t_1()
        .border_color(rgb(colors.border))
}

/// Centers a filled dialog panel of the given width over a dimming scrim.
#[must_use]
pub fn modal(width: f32, panel: Div, theme: Theme) -> Div {
    let colors = theme.colors;
    scrim(0x66).flex().items_center().justify_center().child(
        panel
            .flex()
            .flex_col()
            .w(px(width))
            .max_w_full()
            .rounded(px(12.))
            .overflow_hidden()
            .bg(rgb(colors.surface))
            .border_1()
            .border_color(rgb(colors.border_strong))
            .shadow_lg(),
    )
}

/// A full-height panel docked to the right edge over a light scrim, so the
/// workspace stays visible while choosing a destination or block.
#[must_use]
pub fn drawer(panel: Div, theme: Theme) -> Div {
    let colors = theme.colors;
    scrim(0x55).child(
        panel
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .w(px(DRAWER_WIDTH))
            .max_w_full()
            .flex()
            .flex_col()
            .bg(rgb(colors.surface))
            .border_l_1()
            .border_color(rgb(colors.border_strong))
            .shadow_lg(),
    )
}

/// A bordered list item inside a modal or drawer.
#[must_use]
pub fn overlay_card(theme: Theme) -> Div {
    let colors = theme.colors;
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(10.))
        .p_3()
        .rounded(px(9.))
        .bg(rgb(colors.terminal))
        .border_1()
        .border_color(rgb(colors.border))
}

/// Small uppercase label grouping overlay cards.
#[must_use]
pub fn overlay_section(label: impl Into<SharedString>, theme: Theme) -> Div {
    let label: SharedString = label.into();
    div()
        .flex_none()
        .px_1()
        .pt_1()
        .truncate()
        .text_size(px(10.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(theme.colors.faint))
        .child(label.to_uppercase())
}

/// Container for a row of `segment` buttons.
#[must_use]
pub fn segment_group(theme: Theme) -> Div {
    let colors = theme.colors;
    div()
        .flex()
        .p(px(2.))
        .gap(px(2.))
        .rounded(px(7.))
        .bg(rgb(colors.background))
        .border_1()
        .border_color(rgb(colors.border))
}

/// One option inside a `segment_group`.
pub fn segment(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    theme: Theme,
) -> Stateful<Div> {
    let label: SharedString = label.into();
    let colors = theme.colors;
    div()
        .id(id)
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .px_2()
        .h(px(24.))
        .rounded(px(5.))
        .text_size(px(11.))
        .text_color(rgb(colors.muted))
        .cursor_pointer()
        .hover(move |style| style.bg(rgb(colors.selected)).text_color(rgb(colors.text)))
        .child(label)
}
