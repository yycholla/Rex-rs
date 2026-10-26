use crate::core::frame::Rgb;

/// Semantic colors keep components independent of a particular palette.
#[derive(Clone, Copy, Debug)]
pub struct Colors {
    pub background: u32,
    pub sidebar: u32,
    pub surface: u32,
    pub hover: u32,
    pub selected: u32,
    pub border: u32,
    pub text: u32,
    pub muted: u32,
    pub accent: u32,
    pub danger: u32,
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
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            colors: Colors {
                background: 0x0010_1216,
                sidebar: 0x0019_1b20,
                surface: 0x0020_232a,
                hover: 0x002a_2f38,
                selected: 0x0030_384a,
                border: 0x0030_333b,
                text: 0x00d8_dce5,
                muted: 0x0092_9bad,
                accent: 0x009c_baff,
                danger: 0x00ff_a3a3,
            },
            // Preview metrics; measure the chosen font before enabling PTY resizing.
            terminal: TerminalMetrics {
                font_family: "monospace",
                font_size: 14.,
                cell_width: 8.4,
                cell_height: 22.,
            },
            sidebar_width: 232.,
            pane_header_height: 36.,
        }
    }
}

impl Theme {
    #[must_use]
    pub fn terminal_config(self) -> crate::terminal::TerminalConfig {
        crate::terminal::TerminalConfig {
            foreground: Rgb::from_hex(self.colors.text),
            background: Rgb::from_hex(self.colors.background),
            ..Default::default()
        }
    }
}
