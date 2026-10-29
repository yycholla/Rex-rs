//! Owned rendering data. These values can cross threads; Ghostty handles cannot.
use super::GridSize;
use std::sync::Arc;

#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    #[must_use]
    pub const fn from_hex(hex: u32) -> Self {
        Self {
            r: (hex >> 16) as u8,
            g: (hex >> 8) as u8,
            b: hex as u8,
        }
    }
    #[must_use]
    pub const fn to_hex(self) -> u32 {
        ((self.r as u32) << 16) | ((self.g as u32) << 8) | self.b as u32
    }
}

#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CellWidth {
    #[default]
    Narrow,
    Wide,
    Continuation,
    WrapSpacer,
}

/// Native underline forms, retained independently from the legacy enabled flag.
#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnderlineKind {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Terminal text decorations are independent flags, not mutually exclusive states."
)]
pub struct CellStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    #[cfg_attr(feature = "ghostty", serde(default))]
    pub underline_kind: UnderlineKind,
    #[cfg_attr(feature = "ghostty", serde(default))]
    pub underline_color: Option<Rgb>,
    pub inverse: bool,
    pub invisible: bool,
    pub faint: bool,
    pub blink: bool,
    pub strikethrough: bool,
    pub overline: bool,
}

impl CellStyle {
    /// Old frames with only the enabled flag retain a single underline.
    #[must_use]
    pub const fn resolved_underline(self) -> UnderlineKind {
        if !self.underline {
            return UnderlineKind::None;
        }
        match self.underline_kind {
            UnderlineKind::None => UnderlineKind::Single,
            kind => kind,
        }
    }
}

#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    /// Full grapheme cluster, not an assumed one-byte character.
    pub text: String,
    pub width: CellWidth,
    pub foreground: Rgb,
    pub background: Rgb,
    pub style: CellStyle,
}

#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorStyle {
    #[default]
    Block,
    Bar,
    Underline,
    BlockHollow,
}

#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub column: u16,
    pub row: u16,
    #[cfg_attr(feature = "ghostty", serde(default))]
    pub style: CursorStyle,
    #[cfg_attr(feature = "ghostty", serde(default))]
    pub color: Option<Rgb>,
    #[cfg_attr(feature = "ghostty", serde(default))]
    pub blinking: bool,
    #[cfg_attr(feature = "ghostty", serde(default))]
    pub wide_tail: bool,
}

/// Consecutive cells sharing a style, suitable for batched text shaping.
#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellRun {
    pub column: usize,
    pub columns: usize,
    pub text: String,
    pub foreground: Rgb,
    pub background: Rgb,
    pub style: CellStyle,
}

/// Immutable terminal row shared across frames when its cells have not changed.
pub type TerminalRow = Arc<Vec<Cell>>;

#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalFrame {
    pub size: GridSize,
    /// One vector entry per physical column, including wide-character spacers.
    pub rows: Vec<TerminalRow>,
    pub cursor: Option<Cursor>,
    pub foreground: Rgb,
    pub background: Rgb,
}

impl TerminalFrame {
    #[must_use]
    pub fn row_runs(&self, row: usize) -> Vec<CellRun> {
        let Some(cells) = self.rows.get(row) else {
            return Vec::new();
        };
        let mut runs: Vec<CellRun> = Vec::new();
        for (column, cell) in cells.iter().enumerate() {
            if cell.width == CellWidth::Continuation {
                continue;
            }
            let columns = if cell.width == CellWidth::Wide { 2 } else { 1 };
            if let Some(last) = runs.last_mut()
                && last.column + last.columns == column
                && last.foreground == cell.foreground
                && last.background == cell.background
                && last.style == cell.style
            {
                last.columns += columns;
                last.text.push_str(if cell.text.is_empty() {
                    " "
                } else {
                    &cell.text
                });
                continue;
            }
            runs.push(CellRun {
                column,
                columns,
                text: if cell.text.is_empty() {
                    " ".into()
                } else {
                    cell.text.clone()
                },
                foreground: cell.foreground,
                background: cell.background,
                style: cell.style,
            });
        }
        runs
    }

    /// Diagnostic representation only. Renderers should use cells and positions.
    #[must_use]
    pub fn text_lines(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|row| {
                let mut line = String::new();
                for cell in row.iter() {
                    if cell.width == CellWidth::Continuation {
                        continue;
                    }
                    if cell.text.is_empty() {
                        line.push(' ');
                    } else {
                        line.push_str(&cell.text);
                    }
                }
                line.trim_end().to_owned()
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests assert fixture preconditions and should fail immediately when those preconditions break."
)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_text_preserves_graphemes_without_duplicating_wide_cells() {
        let frame = TerminalFrame {
            size: GridSize::new(5, 1).unwrap(),
            rows: vec![Arc::new(vec![
                Cell {
                    text: "界".into(),
                    width: CellWidth::Wide,
                    ..Cell::default()
                },
                Cell {
                    width: CellWidth::Continuation,
                    ..Cell::default()
                },
                Cell {
                    text: "e\u{301}".into(),
                    ..Cell::default()
                },
                Cell::default(),
                Cell::default(),
            ])],
            cursor: None,
            foreground: Rgb::default(),
            background: Rgb::default(),
        };
        assert_eq!(frame.text_lines(), ["界e\u{301}"]);
        let runs = frame.row_runs(0);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].columns, 5);
        assert_eq!(runs[0].text, "界e\u{301}  ");
    }
    #[test]
    fn legacy_enabled_underline_defaults_single_but_disabled_stays_off() {
        let mut style = CellStyle {
            underline: true,
            ..CellStyle::default()
        };
        assert_eq!(style.resolved_underline(), UnderlineKind::Single);
        style.underline_kind = UnderlineKind::Curly;
        assert_eq!(style.resolved_underline(), UnderlineKind::Curly);
        style.underline = false;
        assert_eq!(style.resolved_underline(), UnderlineKind::None);
    }

    #[cfg(feature = "ghostty")]
    #[test]
    fn legacy_cursor_and_styles_decode_with_presentation_defaults() -> Result<(), serde_json::Error>
    {
        let cursor: Cursor = serde_json::from_str(r#"{"column":1,"row":2}"#)?;
        assert_eq!(
            cursor,
            Cursor {
                column: 1,
                row: 2,
                ..Cursor::default()
            }
        );
        let style: CellStyle = serde_json::from_str(
            r#"{"bold":false,"italic":false,"underline":true,"inverse":false,"invisible":false,"faint":false,"blink":false,"strikethrough":false,"overline":false}"#,
        )?;
        assert_eq!(style.resolved_underline(), UnderlineKind::Single);
        assert_eq!(style.underline_color, None);
        Ok(())
    }
}
