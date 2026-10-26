use crate::core::{
    GridSize,
    frame::{Cell, CellStyle, CellWidth, Cursor, Rgb, TerminalFrame},
    input::KeyInput,
};
use anyhow::{Context, Result, ensure};
use libghostty_vt::{
    RenderState, Terminal, TerminalOptions,
    key::{self, Action, Key, Mods},
    paste,
    render::{CellIterator, RowIterator},
    screen::CellWide,
    style::{RgbColor, Underline},
    terminal::{Mode, ScrollViewport},
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
    key_encoder: key::Encoder<'static>,
    key_event: key::Event<'static>,
    size: GridSize,
    pending_output: Rc<RefCell<Vec<u8>>>,
}

impl GhosttyTerminal {
    /// Create a terminal and its reusable render handles.
    ///
    /// # Errors
    ///
    /// Returns an error if Ghostty cannot allocate or configure the terminal,
    /// render/input handles, or PTY response callback.
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
            key_encoder: key::Encoder::new()?,
            key_event: key::Event::new()?,
            size: config.size,
            pending_output,
        })
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.terminal.vt_write(bytes);
    }

    /// Encode a normalized UI key using the terminal's current keyboard modes.
    ///
    /// # Errors
    ///
    /// Returns an error for an unrecognized key without printable text, or when
    /// Ghostty cannot encode the event. Modifier-only events can produce no bytes.
    pub fn encode_key(&mut self, input: &KeyInput) -> Result<Vec<u8>> {
        let character = single_character(&input.key);
        let text = input
            .text
            .as_deref()
            .filter(|text| printable_text(text))
            .or_else(|| {
                character
                    .filter(|_| printable_text(&input.key))
                    .map(|_| input.key.as_str())
            })
            .or_else(|| (input.key == "space").then_some(" "));
        let (key, unshifted) = character.map_or_else(
            || (named_key(&input.key).unwrap_or(Key::Unidentified), '\0'),
            character_key,
        );
        ensure!(
            key != Key::Unidentified || text.is_some(),
            "unsupported terminal key {:?}",
            input.key
        );
        let unshifted = if key == Key::Space {
            ' '
        } else if unshifted == '\0' {
            text.and_then(single_character).unwrap_or('\0')
        } else {
            unshifted
        };
        let mut modifiers = Mods::empty();
        modifiers.set(Mods::SHIFT, input.modifiers.shift);
        modifiers.set(Mods::CTRL, input.modifiers.control);
        modifiers.set(Mods::ALT, input.modifiers.alt);
        modifiers.set(Mods::SUPER, input.modifiers.super_key);
        self.key_event
            .set_action(if input.repeat {
                Action::Repeat
            } else {
                Action::Press
            })
            .set_key(key)
            .set_mods(modifiers)
            .set_consumed_mods(Mods::empty())
            .set_composing(false)
            .set_utf8(text)
            .set_unshifted_codepoint(unshifted);
        self.key_encoder.set_options_from_terminal(&self.terminal);
        let mut bytes = Vec::new();
        self.key_encoder
            .encode_to_vec(&self.key_event, &mut bytes)
            .with_context(|| format!("encode terminal key {:?}", input.key))?;
        Ok(bytes)
    }

    /// Prepare clipboard text using Ghostty's sanitization and bracketed-paste mode.
    ///
    /// # Errors
    ///
    /// Returns an error if the terminal mode cannot be read, the output size
    /// overflows, or Ghostty cannot encode the paste.
    pub fn encode_paste(&self, text: &str) -> Result<Vec<u8>> {
        let bracketed = self.terminal.mode(Mode::BRACKETED_PASTE)?;
        let capacity = text
            .len()
            .checked_add(12)
            .context("paste output size overflow")?;
        let mut input = text.as_bytes().to_vec();
        let mut output = vec![0; capacity];
        let written = paste::encode(&mut input, bracketed, &mut output)?;
        output.truncate(written);
        Ok(output)
    }

    /// Move the viewport by logical lines; positive values move toward history.
    ///
    /// # Errors
    ///
    /// Returns an error if the reversed delta cannot fit the platform's pointer size.
    pub fn scroll_lines(&mut self, lines: i32) -> Result<()> {
        let delta = isize::try_from(-i64::from(lines)).context("scroll delta is out of range")?;
        self.terminal.scroll_viewport(ScrollViewport::Delta(delta));
        Ok(())
    }

    /// Return the viewport to the active screen after user input.
    pub fn scroll_to_bottom(&mut self) {
        self.terminal.scroll_viewport(ScrollViewport::Bottom);
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

fn single_character(text: &str) -> Option<char> {
    let mut characters = text.chars();
    let first = characters.next()?;
    characters.next().is_none().then_some(first)
}

fn printable_text(text: &str) -> bool {
    !text.is_empty()
        && text.chars().all(|character| {
            !character.is_control() && !(0xf700..=0xf8ff).contains(&u32::from(character))
        })
}

fn named_key(name: &str) -> Option<Key> {
    Some(match name {
        "enter" | "return" => Key::Enter,
        "escape" | "esc" => Key::Escape,
        "backspace" => Key::Backspace,
        "tab" => Key::Tab,
        "space" => Key::Space,
        "up" => Key::ArrowUp,
        "down" => Key::ArrowDown,
        "left" => Key::ArrowLeft,
        "right" => Key::ArrowRight,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "page_up" => Key::PageUp,
        "pagedown" | "page_down" => Key::PageDown,
        "insert" => Key::Insert,
        "delete" => Key::Delete,
        "shift" => Key::ShiftLeft,
        "control" | "ctrl" => Key::ControlLeft,
        "alt" => Key::AltLeft,
        "super" | "meta" | "command" => Key::MetaLeft,
        "capslock" => Key::CapsLock,
        "numlock" => Key::NumLock,
        "scrolllock" => Key::ScrollLock,
        "pause" => Key::Pause,
        "printscreen" => Key::PrintScreen,
        "menu" => Key::ContextMenu,
        _ => return function_key(name),
    })
}

fn function_key(name: &str) -> Option<Key> {
    const KEYS: [Key; 25] = [
        Key::F1,
        Key::F2,
        Key::F3,
        Key::F4,
        Key::F5,
        Key::F6,
        Key::F7,
        Key::F8,
        Key::F9,
        Key::F10,
        Key::F11,
        Key::F12,
        Key::F13,
        Key::F14,
        Key::F15,
        Key::F16,
        Key::F17,
        Key::F18,
        Key::F19,
        Key::F20,
        Key::F21,
        Key::F22,
        Key::F23,
        Key::F24,
        Key::F25,
    ];
    let number = name.strip_prefix('f')?.parse::<usize>().ok()?;
    KEYS.get(number.checked_sub(1)?).copied()
}

fn character_key(character: char) -> (Key, char) {
    match character {
        'a' | 'A' => (Key::A, 'a'),
        'b' | 'B' => (Key::B, 'b'),
        'c' | 'C' => (Key::C, 'c'),
        'd' | 'D' => (Key::D, 'd'),
        'e' | 'E' => (Key::E, 'e'),
        'f' | 'F' => (Key::F, 'f'),
        'g' | 'G' => (Key::G, 'g'),
        'h' | 'H' => (Key::H, 'h'),
        'i' | 'I' => (Key::I, 'i'),
        'j' | 'J' => (Key::J, 'j'),
        'k' | 'K' => (Key::K, 'k'),
        'l' | 'L' => (Key::L, 'l'),
        'm' | 'M' => (Key::M, 'm'),
        'n' | 'N' => (Key::N, 'n'),
        'o' | 'O' => (Key::O, 'o'),
        'p' | 'P' => (Key::P, 'p'),
        'q' | 'Q' => (Key::Q, 'q'),
        'r' | 'R' => (Key::R, 'r'),
        's' | 'S' => (Key::S, 's'),
        't' | 'T' => (Key::T, 't'),
        'u' | 'U' => (Key::U, 'u'),
        'v' | 'V' => (Key::V, 'v'),
        'w' | 'W' => (Key::W, 'w'),
        'x' | 'X' => (Key::X, 'x'),
        'y' | 'Y' => (Key::Y, 'y'),
        'z' | 'Z' => (Key::Z, 'z'),
        '0' | ')' => (Key::Digit0, '0'),
        '1' | '!' => (Key::Digit1, '1'),
        '2' | '@' => (Key::Digit2, '2'),
        '3' | '#' => (Key::Digit3, '3'),
        '4' | '$' => (Key::Digit4, '4'),
        '5' | '%' => (Key::Digit5, '5'),
        '6' | '^' => (Key::Digit6, '6'),
        '7' | '&' => (Key::Digit7, '7'),
        '8' | '*' => (Key::Digit8, '8'),
        '9' | '(' => (Key::Digit9, '9'),
        '`' | '~' => (Key::Backquote, '`'),
        '-' | '_' => (Key::Minus, '-'),
        '=' | '+' => (Key::Equal, '='),
        '[' | '{' => (Key::BracketLeft, '['),
        ']' | '}' => (Key::BracketRight, ']'),
        '\\' | '|' => (Key::Backslash, '\\'),
        ';' | ':' => (Key::Semicolon, ';'),
        '\'' | '"' => (Key::Quote, '\''),
        ',' | '<' => (Key::Comma, ','),
        '.' | '>' => (Key::Period, '.'),
        '/' | '?' => (Key::Slash, '/'),
        ' ' => (Key::Space, ' '),
        _ => (Key::Unidentified, character),
    }
}
