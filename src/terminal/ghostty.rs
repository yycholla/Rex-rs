use crate::core::{
    GridSize,
    frame::{Cell, CellStyle, CellWidth, Cursor, Rgb, TerminalFrame},
};
use anyhow::{Context, Result};
use libghostty_vt::{
    RenderState, Terminal, TerminalOptions,
    render::{CellIterator, RowIterator},
    screen::CellWide,
    style::{RgbColor, Underline},
};
use std::{cell::RefCell, rc::Rc};

#[derive(Clone, Copy, Debug)]
pub struct TerminalConfig {
    pub size: GridSize,
    pub scrollback_limit: usize,
    pub foreground: Rgb,
    pub background: Rgb,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            size: GridSize::default(),
            scrollback_limit: 10_000,
            foreground: Rgb::from_hex(0x00d8_dce5),
            background: Rgb::from_hex(0x0010_1216),
        }
    }
}

/// Construct, feed and snapshot on one owning thread. Never mark this Send/Sync.
pub struct GhosttyTerminal {
    terminal: Terminal<'static, 'static>,
    render_state: RenderState<'static>,
    row_iterator: RowIterator<'static>,
    cell_iterator: CellIterator<'static>,
    size: GridSize,
    pending_output: Rc<RefCell<Vec<u8>>>,
}

impl GhosttyTerminal {
    /// Create a terminal and its reusable render handles.
    ///
    /// # Errors
    ///
    /// Returns an error if Ghostty cannot allocate or configure the terminal,
    /// render handles, or PTY response callback.
    pub fn new(config: TerminalConfig) -> Result<Self> {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: config.size.columns(),
            rows: config.size.rows(),
            max_scrollback: config.scrollback_limit,
        })
        .context("initialize Ghostty terminal")?;
        terminal.set_default_fg_color(Some(to_ghostty(config.foreground)))?;
        terminal.set_default_bg_color(Some(to_ghostty(config.background)))?;
        let pending_output = Rc::new(RefCell::new(Vec::new()));
        let output = Rc::clone(&pending_output);
        terminal.on_pty_write(move |_terminal, bytes| {
            output.borrow_mut().extend_from_slice(bytes);
        })?;
        Ok(Self {
            terminal,
            render_state: RenderState::new()?,
            row_iterator: RowIterator::new()?,
            cell_iterator: CellIterator::new()?,
            size: config.size,
            pending_output,
        })
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.terminal.vt_write(bytes);
    }

    /// The PTY owner must write these response bytes back to the process.
    #[must_use]
    pub fn take_pending_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.pending_output.borrow_mut())
    }

    /// Resize and reflow the terminal to the supplied logical and pixel dimensions.
    ///
    /// # Errors
    ///
    /// Returns an error if Ghostty cannot resize the terminal.
    pub fn resize(&mut self, size: GridSize, cell_width: u32, cell_height: u32) -> Result<()> {
        self.terminal
            .resize(size.columns(), size.rows(), cell_width, cell_height)?;
        self.size = size;
        Ok(())
    }

    /// Copy visible terminal cells and cursor state into an owned frame.
    ///
    /// # Errors
    ///
    /// Returns an error if Ghostty cannot update the render state or read its
    /// rows, cells, colors, styles, or cursor data.
    pub fn snapshot(&mut self) -> Result<TerminalFrame> {
        let snapshot = self.render_state.update(&self.terminal)?;
        let colors = snapshot.colors()?;
        let foreground = from_ghostty(colors.foreground);
        let background = from_ghostty(colors.background);
        let cursor = if snapshot.cursor_visible()? {
            snapshot.cursor_viewport()?.map(|position| Cursor {
                column: position.x,
                row: position.y,
            })
        } else {
            None
        };
        let mut rows = Vec::with_capacity(usize::from(self.size.rows()));
        let mut row_iter = self.row_iterator.update(&snapshot)?;
        while let Some(row) = row_iter.next() {
            let mut cells = Vec::with_capacity(usize::from(self.size.columns()));
            let mut cell_iter = self.cell_iterator.update(row)?;
            while let Some(cell) = cell_iter.next() {
                let style = cell.style()?;
                let width = match cell.raw_cell()?.wide()? {
                    CellWide::Narrow => CellWidth::Narrow,
                    CellWide::Wide => CellWidth::Wide,
                    CellWide::SpacerTail => CellWidth::Continuation,
                    CellWide::SpacerHead => CellWidth::WrapSpacer,
                };
                cells.push(Cell {
                    text: cell.graphemes()?.into_iter().collect(),
                    width,
                    foreground: cell.fg_color()?.map_or(foreground, from_ghostty),
                    background: cell.bg_color()?.map_or(background, from_ghostty),
                    style: CellStyle {
                        bold: style.bold,
                        italic: style.italic,
                        underline: style.underline != Underline::None,
                        inverse: style.inverse,
                        invisible: style.invisible,
                        faint: style.faint,
                        blink: style.blink,
                        strikethrough: style.strikethrough,
                        overline: style.overline,
                    },
                });
            }
            rows.push(cells);
        }
        Ok(TerminalFrame {
            size: self.size,
            rows,
            cursor,
            foreground,
            background,
        })
    }
}

fn from_ghostty(color: RgbColor) -> Rgb {
    Rgb {
        r: color.r,
        g: color.g,
        b: color.b,
    }
}
fn to_ghostty(color: Rgb) -> RgbColor {
    RgbColor {
        r: color.r,
        g: color.g,
        b: color.b,
    }
}
