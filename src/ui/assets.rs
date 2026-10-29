//! Embedded interface assets. GPUI renders SVGs as masks tinted by text color.
use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

/// Wraps 16×16 stroked outline paths in an SVG document.
macro_rules! icon_svg {
    ($($body:literal),+ $(,)?) => {
        concat!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="none" "#,
            r#"stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round">"#,
            $($body,)+
            "</svg>"
        )
    };
}

const ICONS: &[(&str, &str)] = &[
    (
        "icons/plus.svg",
        icon_svg!(r#"<path d="M8 3.5v9M3.5 8h9"/>"#),
    ),
    (
        "icons/close.svg",
        icon_svg!(r#"<path d="M4.5 4.5l7 7M11.5 4.5l-7 7"/>"#),
    ),
    (
        "icons/chevron-down.svg",
        icon_svg!(r#"<path d="M4.5 6.5L8 10l3.5-3.5"/>"#),
    ),
    (
        "icons/terminal.svg",
        icon_svg!(
            r#"<rect x="2" y="3" width="12" height="10" rx="2"/>"#,
            r#"<path d="M5 6.5L6.75 8 5 9.5M8 10h3"/>"#
        ),
    ),
    (
        "icons/split-right.svg",
        icon_svg!(r#"<rect x="2" y="3" width="12" height="10" rx="2"/><path d="M8 3v10"/>"#),
    ),
    (
        "icons/split-down.svg",
        icon_svg!(r#"<rect x="2" y="3" width="12" height="10" rx="2"/><path d="M2 8h12"/>"#),
    ),
    (
        "icons/maximize.svg",
        icon_svg!(r#"<path d="M9.5 2.5h4v4M6.5 13.5h-4v-4M13.5 2.5L9 7M2.5 13.5L7 9"/>"#),
    ),
    (
        "icons/minimize.svg",
        icon_svg!(r#"<path d="M13 7H9V3M3 9h4v4M9 7l4.5-4.5M7 9l-4.5 4.5"/>"#),
    ),
    (
        "icons/sessions.svg",
        icon_svg!(
            r#"<rect x="2.5" y="5.5" width="11" height="8" rx="1.8"/>"#,
            r#"<path d="M4.5 3.5h7"/>"#
        ),
    ),
    (
        "icons/detach.svg",
        icon_svg!(
            r#"<rect x="2" y="5" width="9" height="9" rx="1.8"/><path d="M8 2h6v6M14 2L8.5 7.5"/>"#
        ),
    ),
    (
        "icons/arrange.svg",
        icon_svg!(
            r#"<rect x="2" y="2" width="5" height="5" rx="1.2"/>"#,
            r#"<rect x="9" y="9" width="5" height="5" rx="1.2"/>"#,
            r#"<path d="M11.5 2.5v4M9.5 4.5l2-2 2 2M4.5 13.5v-4M2.5 11.5l2 2 2-2"/>"#
        ),
    ),
    (
        "icons/layers.svg",
        icon_svg!(
            r#"<path d="M8 2.5L14 5.5 8 8.5 2 5.5z"/><path d="M2 8.5l6 3 6-3M2 11l6 3 6-3"/>"#
        ),
    ),
    (
        "icons/server.svg",
        icon_svg!(
            r#"<rect x="2.5" y="2.5" width="11" height="4.5" rx="1.4"/>"#,
            r#"<rect x="2.5" y="9" width="11" height="4.5" rx="1.4"/>"#,
            r#"<path d="M5 4.75h.01M5 11.25h.01"/>"#
        ),
    ),
    (
        "icons/lock.svg",
        icon_svg!(
            r#"<rect x="3" y="7" width="10" height="7" rx="1.6"/><path d="M5.5 7V5a2.5 2.5 0 015 0v2"/>"#
        ),
    ),
    (
        "icons/refresh.svg",
        icon_svg!(r#"<path d="M13 8a5 5 0 11-1.5-3.55M13 2.5v3h-3"/>"#),
    ),
    (
        "icons/chevron-right.svg",
        icon_svg!(r#"<path d="M6.5 4.5L10 8l-3.5 3.5"/>"#),
    ),
    (
        "icons/more.svg",
        icon_svg!(r#"<path d="M4 8h.01M8 8h.01M12 8h.01" stroke-width="2"/>"#),
    ),
    (
        "icons/power.svg",
        icon_svg!(r#"<path d="M8 2.5v5M4.8 4.6a4.5 4.5 0 106.4 0"/>"#),
    ),
    (
        "icons/globe.svg",
        icon_svg!(
            r#"<circle cx="8" cy="8" r="5.5"/><path d="M2.5 8h11"/>"#,
            r#"<path d="M8 2.5c1.6 1.8 1.6 9.2 0 11M8 2.5c-1.6 1.8-1.6 9.2 0 11"/>"#
        ),
    ),
    (
        "icons/sidebar.svg",
        icon_svg!(r#"<rect x="2" y="3" width="12" height="10" rx="2"/><path d="M6 3v10"/>"#),
    ),
];

/// Serves the bundled icon set to GPUI.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, svg)| Cow::Borrowed(svg.as_bytes())))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_is_a_complete_svg_document() {
        for (name, svg) in ICONS {
            assert!(name.starts_with("icons/"), "{name}");
            assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"), "{name}");
        }
    }

    #[test]
    fn unknown_assets_are_absent_rather_than_errors() {
        assert!(matches!(Assets.load("icons/missing.svg"), Ok(None)));
    }
}
