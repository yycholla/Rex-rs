use crate::core::frame::Rgb;

/// Semantic colors keep components independent of a particular palette.
///
/// Names follow Rex's design tokens. Rex's translucent hover and selection
/// tokens are pre-composited over the sidebar so components use opaque colors.
#[derive(Clone, Copy, Debug)]
pub struct Colors {
    /// Window canvas behind every surface.
    pub background: u32,
    pub sidebar: u32,
    /// Pane and pane-header surface.
    pub surface: u32,
    /// Popovers, dialogs and the focused pane header.
    pub raised: u32,
    pub terminal: u32,
    pub hover: u32,
    pub selected: u32,
    pub border: u32,
    pub border_strong: u32,
    pub text: u32,
    pub muted: u32,
    pub faint: u32,
    pub accent: u32,
    pub accent_solid: u32,
    pub accent_on_solid: u32,
    /// Accent pre-composited over a surface: focus rings and soft accent fills.
    pub accent_muted: u32,
    /// Dialog and drawer footers, a step below the raised surface.
    pub footer: u32,
    pub danger: u32,
    pub success: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct TerminalMetrics {
    pub font_family: &'static str,
    pub font_size: f32,
    pub cell_width: f32,
    pub cell_height: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub colors: Colors,
    pub terminal: TerminalMetrics,
    pub sidebar_width: f32,
    pub pane_header_height: f32,
    pub row_height: f32,
    /// Space between the window edge, sidebar and pane cards.
    pub gutter: f32,
    pub pane_radius: f32,
    /// Opacity of unfocused pane content, as in Rex's "Fade inactive panes".
    pub inactive_pane_opacity: f32,
}

impl Default for Theme {
    /// Rex's default dark theme, Merino Dark.
    fn default() -> Self {
        Self {
            colors: Colors {
                background: 0x000a_0c0f,
                sidebar: 0x000a_0c0f,
                surface: 0x0011_1418,
                raised: 0x001a_1d22,
                terminal: 0x000d_1117,
                hover: 0x0016_191d,
                selected: 0x001d_2127,
                border: 0x0023_272d,
                border_strong: 0x0034_3941,
                text: 0x00e6_edf3,
                muted: 0x008b_9198,
                faint: 0x005d_636b,
                accent: 0x004c_8dff,
                accent_solid: 0x004c_8dff,
                accent_on_solid: 0x000a_0c0f,
                accent_muted: 0x002b_4f8a,
                footer: 0x000e_1114,
                danger: 0x00ff_7b72,
                success: 0x003f_b950,
            },
            // Starting metrics; the terminal view measures the font before resizing.
            terminal: TerminalMetrics {
                font_family: "JetBrains Mono",
                font_size: 13.,
                cell_width: 7.8,
                cell_height: 18.,
            },
            sidebar_width: 252.,
            pane_header_height: 32.,
            row_height: 28.,
            gutter: 8.,
            pane_radius: 10.,
            inactive_pane_opacity: 0.6,
        }
    }
}

impl Theme {
    #[must_use]
    pub fn terminal_config(self) -> crate::terminal::TerminalConfig {
        crate::terminal::TerminalConfig {
            foreground: Rgb::from_hex(self.colors.text),
            background: Rgb::from_hex(self.colors.terminal),
            ..Default::default()
        }
    }
}
