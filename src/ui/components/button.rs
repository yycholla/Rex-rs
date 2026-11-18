use crate::ui::theme::Theme;
use gpui::{Div, ElementId, SharedString, Stateful, Svg, div, prelude::*, px, rgb, svg};

/// Height of compact controls such as dialog buttons.
const CONTROL_HEIGHT: f32 = 26.;

#[derive(Clone, Copy, Debug)]
pub enum ButtonVariant {
    Quiet,
    Primary,
    Selected,
    Danger,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Plus,
    Close,
    ChevronDown,
    Terminal,
    SplitRight,
    SplitDown,
    Maximize,
    Minimize,
    Sessions,
    Sidebar,
}

impl Icon {
    const fn path(self) -> &'static str {
        match self {
            Self::Plus => "icons/plus.svg",
            Self::Close => "icons/close.svg",
            Self::ChevronDown => "icons/chevron-down.svg",
            Self::Terminal => "icons/terminal.svg",
            Self::SplitRight => "icons/split-right.svg",
            Self::SplitDown => "icons/split-down.svg",
            Self::Maximize => "icons/maximize.svg",
            Self::Minimize => "icons/minimize.svg",
            Self::Sessions => "icons/sessions.svg",
            Self::Sidebar => "icons/sidebar.svg",
        }
    }
}

/// A tinted outline glyph sized for 13px interface text.
#[must_use]
pub fn icon(icon: Icon, size: f32, color: u32) -> Svg {
    svg()
        .path(icon.path())
        .flex_none()
        .size(px(size))
        .text_color(rgb(color))
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
    let (background, foreground, border, hover) = match variant {
        ButtonVariant::Quiet => (colors.raised, colors.text, colors.border, colors.selected),
        ButtonVariant::Primary => (
            colors.accent_solid,
            colors.accent_on_solid,
            colors.accent_solid,
            colors.accent,
        ),
        ButtonVariant::Selected => (colors.selected, colors.text, colors.border, colors.selected),
        ButtonVariant::Danger => (colors.raised, colors.danger, colors.border, colors.selected),
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .px_3()
        .gap_2()
        .h(px(CONTROL_HEIGHT))
        .rounded(px(6.))
        .text_size(px(13.))
        .cursor_pointer()
        .bg(rgb(background))
        .text_color(rgb(foreground))
        .border_1()
        .border_color(rgb(border))
        .hover(move |style| style.bg(rgb(hover)))
        .child(label)
}

/// A borderless square control showing only an icon, as in Rex pane headers.
pub fn icon_button(id: impl Into<ElementId>, glyph: Icon, theme: Theme) -> Stateful<Div> {
    let colors = theme.colors;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(22.))
        .rounded(px(5.))
        .cursor_pointer()
        .group("icon-button")
        .hover(move |style| style.bg(rgb(colors.selected)))
        .child(
            icon(glyph, 14., colors.muted).group_hover("icon-button", move |style| {
                style.text_color(rgb(colors.text))
            }),
        )
}
