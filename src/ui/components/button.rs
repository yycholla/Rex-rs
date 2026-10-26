use crate::ui::theme::Theme;
use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, px, rgb};

#[derive(Clone, Copy, Debug)]
pub enum ButtonVariant {
    Quiet,
    Primary,
    Selected,
    Danger,
}

/// Returns a composable element: callers supply behavior with `on_click`.
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    variant: ButtonVariant,
    theme: Theme,
) -> Stateful<Div> {
    let label: SharedString = label.into();
    let colors = theme.colors;
    let (background, foreground) = match variant {
        ButtonVariant::Quiet => (colors.surface, colors.text),
        ButtonVariant::Primary => (colors.selected, colors.accent),
        ButtonVariant::Selected => (colors.selected, colors.text),
        ButtonVariant::Danger => (colors.surface, colors.danger),
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .px_3()
        .py_2()
        .gap_2()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .bg(rgb(background))
        .text_color(rgb(foreground))
        .border_1()
        .border_color(rgb(colors.border))
        .min_h(px(32.))
        .hover(move |style| style.bg(rgb(colors.hover)))
        .child(label)
}
