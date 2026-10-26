use super::{ButtonVariant, button};
use crate::{core::Session, ui::theme::Theme};
use gpui::{Div, SharedString, Stateful, div, prelude::*, px, rgb};

pub fn section_label(label: impl Into<SharedString>, theme: Theme) -> Div {
    let label: SharedString = label.into();
    div()
        .text_xs()
        .text_color(rgb(theme.colors.muted))
        .child(label)
}

#[must_use]
pub fn session_item(session: &Session, selected: bool, theme: Theme) -> Stateful<Div> {
    button(
        SharedString::from(format!("session-{}", session.id())),
        SharedString::from(session.title().to_owned()),
        if selected {
            ButtonVariant::Selected
        } else {
            ButtonVariant::Quiet
        },
        theme,
    )
    .justify_between()
    .w_full()
    .child(
        div()
            .text_xs()
            .text_color(rgb(theme.colors.muted))
            .child(session.layout().panes().len().to_string()),
    )
}

pub fn pane_header(title: impl Into<SharedString>, focused: bool, theme: Theme) -> Div {
    let title: SharedString = title.into();
    div()
        .flex()
        .items_center()
        .justify_between()
        .px_3()
        .h(px(theme.pane_header_height))
        .flex_shrink_0()
        .bg(rgb(theme.colors.surface))
        .border_b_1()
        .border_color(rgb(theme.colors.border))
        .text_sm()
        .text_color(rgb(if focused {
            theme.colors.accent
        } else {
            theme.colors.muted
        }))
        .child(title)
        .child(section_label("PREVIEW", theme))
}

#[must_use]
pub fn empty_state(theme: Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .size_full()
        .gap_3()
        .child(div().text_xl().child("Your workspace is clear"))
        .child(
            div()
                .text_sm()
                .text_color(rgb(theme.colors.muted))
                .child("Create a session to open your first pane."),
        )
}

pub fn status_bar(message: impl Into<SharedString>, error: bool, theme: Theme) -> Div {
    let message: SharedString = message.into();
    div()
        .flex()
        .items_center()
        .px_3()
        .py_2()
        .flex_shrink_0()
        .border_t_1()
        .border_color(rgb(theme.colors.border))
        .text_xs()
        .text_color(rgb(if error {
            theme.colors.danger
        } else {
            theme.colors.muted
        }))
        .child(message)
}
