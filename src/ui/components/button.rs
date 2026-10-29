use crate::ui::theme::Theme;
use gpui::{
    AnyView, App, Context, Div, ElementId, FontWeight, SharedString, Stateful, Svg, Window, div,
    prelude::*, px, rgb, svg,
};

/// Height of compact controls such as dialog buttons.
const CONTROL_HEIGHT: f32 = 28.;

#[derive(Clone, Copy, Debug)]
pub enum ButtonVariant {
    Quiet,
    Primary,
    /// Accent-tinted secondary action, for repeated primary actions in lists.
    Soft,
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
    Detach,
    Arrange,
    Layers,
    Server,
    Lock,
    Refresh,
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
            Self::Detach => "icons/detach.svg",
            Self::Arrange => "icons/arrange.svg",
            Self::Layers => "icons/layers.svg",
            Self::Server => "icons/server.svg",
            Self::Lock => "icons/lock.svg",
            Self::Refresh => "icons/refresh.svg",
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
        ButtonVariant::Quiet => (
            colors.raised,
            colors.text,
            colors.border_strong,
            colors.selected,
        ),
        ButtonVariant::Primary => (
            colors.accent_solid,
            colors.accent_on_solid,
            colors.accent_solid,
            colors.accent,
        ),
        ButtonVariant::Soft => (
            colors.accent_muted,
            colors.text,
            colors.accent_muted,
            colors.accent_solid,
        ),
        ButtonVariant::Selected => (colors.selected, colors.text, colors.border, colors.selected),
        ButtonVariant::Danger => (
            colors.raised,
            colors.danger,
            colors.border_strong,
            colors.selected,
        ),
    };
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .px_3()
        .gap(px(6.))
        .h(px(CONTROL_HEIGHT))
        .rounded(px(7.))
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
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
    tinted_icon_button(id, glyph, theme.colors.muted, theme)
}

/// An icon button whose resting tint conveys state, such as an active toggle.
pub fn tinted_icon_button(
    id: impl Into<ElementId>,
    glyph: Icon,
    tint: u32,
    theme: Theme,
) -> Stateful<Div> {
    let colors = theme.colors;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(24.))
        .rounded(px(6.))
        .cursor_pointer()
        .group("icon-button")
        .hover(move |style| style.bg(rgb(colors.selected)))
        .child(
            icon(glyph, 14., tint).group_hover("icon-button", move |style| {
                style.text_color(rgb(colors.text))
            }),
        )
}

/// Builds a tooltip view for `Stateful::tooltip`.
pub fn tooltip(
    text: impl Into<SharedString>,
    theme: Theme,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text: SharedString = text.into();
    move |_, cx| {
        cx.new(|_| Tooltip {
            text: text.clone(),
            theme,
        })
        .into()
    }
}

struct Tooltip {
    text: SharedString,
    theme: Theme,
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let colors = self.theme.colors;
        div()
            .px_2()
            .py_1()
            .rounded(px(6.))
            .bg(rgb(colors.raised))
            .border_1()
            .border_color(rgb(colors.border_strong))
            .shadow_md()
            .text_size(px(11.))
            .text_color(rgb(colors.text))
            .child(self.text.clone())
    }
}
