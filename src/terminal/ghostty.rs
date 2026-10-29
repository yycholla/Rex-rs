mod graphics;
mod interaction;
mod links;
use crate::core::{
    GridSize,
    frame::{Cell, CellStyle, CellWidth, Cursor, CursorStyle, Rgb, TerminalFrame, UnderlineKind},
    input::{KeyInput, MouseTracking},
};
use anyhow::{Context, Result, ensure};
use libghostty_vt::{
    RenderState, Terminal,
    key::{self, Action, Key, Mods},
    paste,
    render::{CellIterator, Colors, CursorVisualStyle, Dirty, RowId, RowIterator},
    screen::CellWide,
    snapshot::Decoder,
    style::{RgbColor, StyleColor, Underline},
    terminal::{Mode, ScrollViewport},
};
use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc};

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
    cached_frame: Option<Arc<TerminalFrame>>,
    cached_colors: Option<Colors>,
    cached_rows: HashMap<RowId, Arc<Vec<Cell>>>,
    interaction: interaction::Interaction,
    graphics: graphics::GraphicsState,
}

impl Drop for GhosttyTerminal {
    fn drop(&mut self) {
        self.interaction.before_drop(&self.terminal);
    }
}

fn snapshot_cell(
    cell: &libghostty_vt::render::CellIteration<'_, '_>,
    grapheme: &mut String,
    foreground: Rgb,
    background: Rgb,
    palette: &[RgbColor; 256],
) -> Result<Cell> {
    let style = cell.style()?;
    let width = match cell.raw_cell()?.wide()? {
        CellWide::Narrow => CellWidth::Narrow,
        CellWide::Wide => CellWidth::Wide,
        CellWide::SpacerTail => CellWidth::Continuation,
        CellWide::SpacerHead => CellWidth::WrapSpacer,
    };
    cell.graphemes_utf8(grapheme)?;
    Ok(Cell {
        text: grapheme.clone(),
        width,
        foreground: cell.fg_color()?.map_or(foreground, from_ghostty),
        background: cell.bg_color()?.map_or(background, from_ghostty),
        style: CellStyle {
            bold: style.bold,
            italic: style.italic,
            underline: style.underline != Underline::None,
            underline_kind: underline_kind(style.underline)?,
            underline_color: resolve_style_color(style.underline_color, palette),
            inverse: style.inverse,
            invisible: style.invisible,
            faint: style.faint,
            blink: style.blink,
            strikethrough: style.strikethrough,
            overline: style.overline,
        },
    })
}

fn snapshot_row<'alloc>(
    row: &libghostty_vt::render::RowIteration<'alloc, '_>,
    iterator: &mut CellIterator<'alloc>,
    grapheme: &mut String,
    colors: &Colors,
    previous: Option<&Arc<Vec<Cell>>>,
    columns: u16,
) -> Result<Arc<Vec<Cell>>> {
    let mut cells = Vec::with_capacity(usize::from(columns));
    let mut iterator = iterator.update(row)?;
    while let Some(cell) = iterator.next() {
        cells.push(snapshot_cell(
            cell,
            grapheme,
            from_ghostty(colors.foreground),
            from_ghostty(colors.background),
            &colors.palette,
        )?);
    }
    // Native cursor rows can be dirty without any cell changes. Reuse their
    // owned identity so cursor/image redraws preserve shaped glyph caches.
    Ok(previous
        .filter(|previous| previous.as_ref() == &cells)
        .map_or_else(|| Arc::new(cells), Arc::clone))
}

fn underline_kind(underline: Underline) -> Result<UnderlineKind> {
    Ok(match underline {
        Underline::None => UnderlineKind::None,
        Underline::Single => UnderlineKind::Single,
        Underline::Double => UnderlineKind::Double,
        Underline::Curly => UnderlineKind::Curly,
        Underline::Dotted => UnderlineKind::Dotted,
        Underline::Dashed => UnderlineKind::Dashed,
        _ => anyhow::bail!("unsupported native underline kind"),
    })
}

fn resolve_style_color(color: StyleColor, palette: &[RgbColor; 256]) -> Option<Rgb> {
    match color {
        StyleColor::None => None,
        StyleColor::Palette(index) => Some(from_ghostty(palette[usize::from(index.0)])),
        StyleColor::Rgb(color) => Some(from_ghostty(color)),
    }
}

fn same_cell_colors(first: Option<&Colors>, second: &Colors) -> bool {
    first.is_some_and(|first| {
        first.foreground == second.foreground
            && first.background == second.background
            && first.palette == second.palette
    })
}

fn cursor_style(style: CursorVisualStyle) -> Result<CursorStyle> {
    Ok(match style {
        CursorVisualStyle::Block => CursorStyle::Block,
        CursorVisualStyle::Bar => CursorStyle::Bar,
        CursorVisualStyle::Underline => CursorStyle::Underline,
        CursorVisualStyle::BlockHollow => CursorStyle::BlockHollow,
        _ => anyhow::bail!("unsupported native cursor style"),
    })
}

fn snapshot_cursor(
    native: libghostty_vt::render::Cursor,
    color: Option<RgbColor>,
) -> Result<Option<Cursor>> {
    if !native.visible {
        return Ok(None);
    }
    let style = cursor_style(native.visual_style)?;
    Ok(native.viewport.map(|position| Cursor {
        column: position.x,
        row: position.y,
        style,
        color: color.map(from_ghostty),
        blinking: native.blinking,
        wide_tail: position.at_wide_tail,
    }))
}

const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;

/// Compatibility with the optional records in Ghostty's version-1 format.
/// Known records remain byte-for-byte unchanged and the native decoder still
/// validates their checksums, ordering and counts, including graphics payloads.
fn normalize_snapshot(bytes: &[u8]) -> Result<&[u8]> {
    ensure!(
        bytes.len() <= MAX_SNAPSHOT_BYTES,
        "Ghostty snapshot exceeds size limit"
    );
    ensure!(
        bytes.len() >= 10 && &bytes[..8] == b"GHOSTSNP",
        "invalid Ghostty snapshot envelope"
    );
    ensure!(
        u16::from_le_bytes([bytes[8], bytes[9]]) == 1,
        "unsupported Ghostty snapshot version"
    );
    let mut offset = 10;
    let mut finished = false;
    while offset < bytes.len() {
        ensure!(
            bytes.len() - offset >= 10,
            "truncated Ghostty snapshot record header"
        );
        let header = &bytes[offset..offset + 10];
        let tag = u16::from_le_bytes([header[0], header[1]]);
        let payload_len = usize::try_from(u32::from_le_bytes([
            header[2], header[3], header[4], header[5],
        ]))?;
        let end = offset
            .checked_add(10)
            .and_then(|start| start.checked_add(payload_len))
            .context("Ghostty snapshot record length overflow")?;
        ensure!(
            end <= bytes.len(),
            "truncated Ghostty snapshot record payload"
        );
        match tag {
            1..=7 => {}
            8..=10 => {
                let expected = u32::from_le_bytes([header[6], header[7], header[8], header[9]]);
                ensure!(
                    snapshot_crc32c(&header[..6], &bytes[offset + 10..end]) == expected,
                    "optional Ghostty snapshot record checksum mismatch"
                );
            }
            _ => anyhow::bail!("unsupported Ghostty snapshot record tag {tag}"),
        }
        offset = end;
        if tag == 6 {
            ensure!(payload_len == 0, "invalid Ghostty snapshot FINISH marker");
            ensure!(offset == bytes.len(), "trailing Ghostty snapshot data");
            finished = true;
            break;
        }
    }
    ensure!(finished, "Ghostty snapshot lacks FINISH marker");
    Ok(bytes)
}

const fn crc32c_table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut index = 0;
    while index < table.len() {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value.trailing_zeros() > 0 {
                value >> 1
            } else {
                (value >> 1) ^ 0x82f6_3b78
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

fn snapshot_crc32c(prefix: &[u8], payload: &[u8]) -> u32 {
    const TABLE: [u32; 256] = crc32c_table();
    let mut checksum = u32::MAX;
    for byte in prefix.iter().chain(payload) {
        checksum = (checksum >> 8) ^ TABLE[usize::from(checksum.to_le_bytes()[0] ^ *byte)];
    }
    !checksum
}

impl GhosttyTerminal {
    /// Create a terminal and its reusable render handles.
    ///
    /// # Errors
    ///
    /// Returns an error if Ghostty cannot allocate or configure the terminal,
    /// render/input handles, or PTY response callback.
    pub fn new(config: TerminalConfig) -> Result<Self> {
        let mut terminal = Terminal::new(config.size.columns(), config.size.rows())
            .context("initialize Ghostty terminal")?;
        terminal.set_scrollback_max_lines(Some(config.scrollback_limit))?;
        terminal.set_default_fg_color(Some(to_ghostty(config.foreground)))?;
        terminal.set_default_bg_color(Some(to_ghostty(config.background)))?;
        Self::from_terminal(terminal, config.size, true)
    }

    /// Import the complete server-owned terminal state, including history and
    /// unfinished VT parser input. Post-snapshot bytes continue that parser.
    ///
    /// Imported display terminals do not answer device queries: the Rex server
    /// already owns emulation and sends those replies to the child process.
    ///
    /// # Errors
    /// Rejects oversized, malformed or incompatible snapshots, invalid grids,
    /// and failures allocating the reusable render/input handles.
    pub fn from_snapshot(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_SNAPSHOT_BYTES,
            "Ghostty snapshot exceeds 64 MiB limit"
        );
        // The recorded native compatibility patch decodes Kitty graphics records
        // without changing the snapshot version or replaying process side effects.
        let bytes = normalize_snapshot(bytes)?;
        let decoder = Decoder::new_buf(bytes).context("initialize Ghostty snapshot decoder")?;
        let terminal = decoder.decode().context("decode Rex Ghostty snapshot")?;
        let size = GridSize::new(terminal.cols()?, terminal.rows()?)?;
        ensure!(
            usize::from(size.columns()) * usize::from(size.rows()) <= 65_536,
            "Ghostty snapshot viewport exceeds cell limit"
        );
        Self::from_terminal(terminal, size, false)
    }

    fn from_terminal(
        mut terminal: Terminal<'static, 'static>,
        size: GridSize,
        respond: bool,
    ) -> Result<Self> {
        terminal.set_kitty_image_storage_limit(u64::try_from(graphics::IMAGE_BYTES)?)?;
        // Remote terminal output may carry only direct pixel data, never paths or
        // shared-memory references resolved on this desktop's machine.
        terminal.set_kitty_image_from_file_allowed(false)?;
        terminal.set_kitty_image_from_shared_mem_allowed(false)?;
        terminal.set_kitty_image_temp_file_dir(None)?;
        let pending_output = Rc::new(RefCell::new(Vec::new()));
        if respond {
            let output = Rc::clone(&pending_output);
            terminal.on_pty_write(move |_terminal, bytes| {
                output.borrow_mut().extend_from_slice(bytes);
            })?;
        }
        Ok(Self {
            terminal,
            render_state: RenderState::new()?,
            row_iterator: RowIterator::new()?,
            cell_iterator: CellIterator::new()?,
            key_encoder: key::Encoder::new()?,
            key_event: key::Event::new()?,
            size,
            pending_output,
            cached_frame: None,
            cached_colors: None,
            cached_rows: HashMap::new(),
            interaction: interaction::Interaction::new()?,
            graphics: graphics::GraphicsState::new()?,
        })
    }

    #[cfg(test)]
    pub(crate) fn snapshot_bytes(&mut self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.terminal.encode_snapshot(&mut bytes)?;
        Ok(bytes)
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.terminal.vt_write(bytes);
        self.graphics.needs_tick = true;
        self.interaction.mutated();
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
        self.interaction.interacted();
        Ok(())
    }

    /// Return the viewport to the active screen after user input.
    pub fn scroll_to_bottom(&mut self) {
        self.terminal.scroll_viewport(ScrollViewport::Bottom);
        self.interaction.interacted();
    }

    /// Whether the viewport already follows the active screen.
    ///
    /// # Errors
    /// Returns an error if Ghostty cannot read the viewport's scrollbar state.
    pub fn is_at_bottom(&self) -> Result<bool> {
        let scrollbar = self.terminal.scrollbar()?;
        Ok(scrollbar.offset.saturating_add(scrollbar.len) >= scrollbar.total)
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
        self.interaction.resized(&self.terminal);
        Ok(())
    }

    /// Whether terminal applications requested mouse events instead of local history.
    ///
    /// # Errors
    /// Reports failures reading terminal modes.
    pub fn uses_mouse_reporting(&self) -> Result<bool> {
        Ok(self.terminal.is_mouse_tracking()?)
    }

    /// Classify requested pointer traffic before it enters the control queue.
    ///
    /// # Errors
    /// Reports failures reading terminal modes.
    pub fn mouse_tracking(&self) -> Result<MouseTracking> {
        if self.terminal.mode(Mode::ANY_MOUSE)? {
            Ok(MouseTracking::Any)
        } else if self.terminal.mode(Mode::BUTTON_MOUSE)? {
            Ok(MouseTracking::Drag)
        } else if self.terminal.is_mouse_tracking()? {
            Ok(MouseTracking::Press)
        } else {
            Ok(MouseTracking::None)
        }
    }

    /// Whether a full-screen terminal application is using the alternate screen.
    ///
    /// # Errors
    /// Reports failures reading terminal screen state.
    pub fn alternate_screen(&self) -> Result<bool> {
        Ok(self.terminal.active_screen()? == libghostty_vt::screen::Screen::Alternate)
    }

    /// Copy visible terminal cells and cursor state into an owned frame.
    ///
    /// # Errors
    ///
    /// Returns an error if Ghostty cannot update the render state or read its
    /// rows, cells, colors, styles, or cursor data.
    pub fn snapshot(&mut self) -> Result<TerminalFrame> {
        Ok(self.snapshot_shared()?.as_ref().clone())
    }

    /// Materialize a frame while retaining its identity when visible state is unchanged.
    ///
    /// # Errors
    /// Reports failures reading Ghostty render state and cells.
    pub fn snapshot_shared(&mut self) -> Result<Arc<TerminalFrame>> {
        // A failed read invalidates the cache, so clearing native dirty flags
        // partway through a snapshot can never make a retry reuse stale cells.
        let previous = self.cached_frame.take();
        let snapshot = self.render_state.update(&self.terminal)?;
        let dirty = snapshot.dirty()?;
        self.graphics.refresh(&self.terminal, &snapshot, self.size);
        let images = Arc::clone(&self.graphics.images);
        let graphics_warning = self.graphics.warning.clone();
        let colors = snapshot.colors()?;
        let foreground = from_ghostty(colors.foreground);
        let background = from_ghostty(colors.background);
        let cursor = snapshot_cursor(snapshot.cursor()?, colors.cursor)?;
        // Cursor color changes do not change cell paint or glyph geometry.
        let cell_colors_unchanged = same_cell_colors(self.cached_colors.as_ref(), &colors);
        // Palette/default-color changes affect otherwise clean cells too.
        let full = dirty == Dirty::Full
            || previous
                .as_ref()
                .is_none_or(|frame| frame.size != self.size)
            || !cell_colors_unchanged;
        if let Some(frame) = previous.as_ref().filter(|frame| {
            !full
                && dirty == Dirty::Clean
                && frame.cursor == cursor
                && Arc::ptr_eq(&frame.images, &images)
                && frame.graphics_warning == graphics_warning
        }) {
            snapshot.set_dirty(Dirty::Clean)?;
            let frame = Arc::clone(frame);
            self.cached_colors = Some(colors);
            self.cached_frame = Some(Arc::clone(&frame));
            return Ok(frame);
        }
        let rows_compatible = previous
            .as_ref()
            .is_some_and(|frame| frame.size.columns() == self.size.columns())
            && cell_colors_unchanged;
        let rows = if let Some(frame) = previous.as_ref().filter(|_| !full && dirty == Dirty::Clean)
        {
            frame.rows.clone()
        } else {
            let previous_rows = std::mem::take(&mut self.cached_rows);
            let mut next_rows = HashMap::with_capacity(usize::from(self.size.rows()));
            let mut rows = Vec::with_capacity(usize::from(self.size.rows()));
            // Encode directly into one reusable scratch buffer. The previous
            // Vec<char> path allocated twice and queried each cluster length twice.
            let mut grapheme = String::with_capacity(16);
            let mut row_iter = self.row_iterator.update(&snapshot)?;
            while let Some(row) = row_iter.next() {
                let row_id = row.id()?;
                let same_row = rows_compatible
                    .then(|| previous_rows.get(&row_id))
                    .flatten();
                let cached = same_row.or_else(|| {
                    previous
                        .as_ref()
                        .and_then(|frame| frame.rows.get(rows.len()))
                });
                let row_dirty = full || row.dirty()? || same_row.is_none();
                let owned = if let Some(cached) = same_row.filter(|_| !row_dirty) {
                    Arc::clone(cached)
                } else {
                    snapshot_row(
                        row,
                        &mut self.cell_iterator,
                        &mut grapheme,
                        &colors,
                        cached,
                        self.size.columns(),
                    )?
                };
                next_rows.insert(row_id, Arc::clone(&owned));
                rows.push(owned);
                // Global and row dirty flags are independent in Ghostty.
                row.set_dirty(false)?;
            }
            self.cached_rows = next_rows;
            rows
        };
        snapshot.set_dirty(Dirty::Clean)?;
        let frame = previous
            .filter(|previous| {
                previous.size == self.size
                    && previous.cursor == cursor
                    && previous.foreground == foreground
                    && previous.background == background
                    && Arc::ptr_eq(&previous.images, &images)
                    && previous.graphics_warning == graphics_warning
                    && same_row_storage(&previous.rows, &rows)
            })
            .unwrap_or_else(|| {
                Arc::new(TerminalFrame {
                    size: self.size,
                    rows,
                    cursor,
                    foreground,
                    background,
                    images,
                    graphics_warning,
                })
            });
        self.cached_colors = Some(colors);
        self.cached_frame = Some(Arc::clone(&frame));
        Ok(frame)
    }
}

fn same_row_storage(before: &[Arc<Vec<Cell>>], after: &[Arc<Vec<Cell>>]) -> bool {
    before.len() == after.len()
        && before
            .iter()
            .zip(after)
            .all(|(before, after)| Arc::ptr_eq(before, after))
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

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    fn terminal() -> Result<GhosttyTerminal> {
        GhosttyTerminal::new(TerminalConfig {
            size: GridSize::new(8, 3)?,
            ..TerminalConfig::default()
        })
    }

    fn snapshot_record(tag: u16, payload: &[u8]) -> Result<Vec<u8>> {
        let mut prefix = Vec::new();
        prefix.extend_from_slice(&tag.to_le_bytes());
        prefix.extend_from_slice(&u32::try_from(payload.len())?.to_le_bytes());
        let checksum = snapshot_crc32c(&prefix, payload);
        prefix.extend_from_slice(&checksum.to_le_bytes());
        prefix.extend_from_slice(payload);
        Ok(prefix)
    }

    fn optional_graphics_snapshot(bytes: &[u8]) -> Result<Vec<u8>> {
        let finish_offset = bytes.len() - 10;
        let mut extended = bytes[..10].to_vec();
        extended.extend_from_slice(&snapshot_record(8, b"synthetic storage metadata")?);
        extended.extend_from_slice(&bytes[10..finish_offset]);
        extended.extend_from_slice(&snapshot_record(9, b"synthetic image bytes")?);
        extended.extend_from_slice(&snapshot_record(10, b"synthetic page placements")?);
        extended.extend_from_slice(&bytes[finish_offset..]);
        Ok(extended)
    }

    #[test]
    fn graphics_records_are_preserved_and_checked_before_native_decode() -> Result<()> {
        assert_eq!(snapshot_crc32c(b"123456789", &[]), 0xe306_9283);
        let mut source = terminal()?;
        source.feed(b"safe text");
        let mut bytes = Vec::new();
        source.terminal.encode_snapshot(&mut bytes)?;
        assert_eq!(normalize_snapshot(&bytes)?, bytes.as_slice());
        let extended = optional_graphics_snapshot(&bytes)?;
        assert_eq!(normalize_snapshot(&extended)?, extended.as_slice());
        // Correct framing does not make fabricated image metadata valid.
        assert!(GhosttyTerminal::from_snapshot(&extended).is_err());
        let mut corrupt = extended.clone();
        corrupt[20] ^= 1;
        assert!(normalize_snapshot(&corrupt).is_err());
        assert!(normalize_snapshot(&extended[..15]).is_err());
        assert!(normalize_snapshot(&extended[..25]).is_err());
        assert!(normalize_snapshot(&extended[..extended.len() - 10]).is_err());
        let mut trailing = extended.clone();
        trailing.push(0);
        assert!(normalize_snapshot(&trailing).is_err());
        let mut unsupported = bytes[..10].to_vec();
        unsupported.extend_from_slice(&snapshot_record(11, b"unrecognized extension")?);
        unsupported.extend_from_slice(&bytes[10..]);
        assert!(normalize_snapshot(&unsupported).is_err());
        Ok(())
    }

    #[test]
    fn imported_snapshot_preserves_state_and_utf8_continuation_without_query_replies() -> Result<()>
    {
        let mut source = terminal()?;
        source.terminal.set_continuation_max_bytes(4096)?;
        source.feed(b"one\r\ntwo\r\nthree\r\nfour\x1b[31m\xc3");
        let mut bytes = Vec::new();
        source.terminal.encode_snapshot(&mut bytes)?;
        let mut restored = GhosttyTerminal::from_snapshot(&bytes)?;
        source.feed(b"\xa9");
        restored.feed(b"\xa9");
        assert_eq!(restored.snapshot()?, source.snapshot()?);
        restored.scroll_lines(1)?;
        source.scroll_lines(1)?;
        assert_eq!(restored.snapshot()?, source.snapshot()?);
        restored.feed(b"\x1b[6n");
        assert_eq!(restored.take_pending_output(), [] as [u8; 0]);
        source.feed(b"\x1b[6n");
        assert_ne!(source.take_pending_output(), [] as [u8; 0]);
        Ok(())
    }

    #[test]
    fn snapshot_import_rejects_truncation_and_invalid_records() -> Result<()> {
        let mut source = terminal()?;
        source.feed(b"valid");
        let mut bytes = Vec::new();
        source.terminal.encode_snapshot(&mut bytes)?;
        assert!(GhosttyTerminal::from_snapshot(&bytes[..bytes.len() - 1]).is_err());
        bytes[0] ^= 1;
        assert!(GhosttyTerminal::from_snapshot(&bytes).is_err());
        assert!(GhosttyTerminal::from_snapshot(&[]).is_err());
        Ok(())
    }

    #[test]
    fn unchanged_and_cursor_only_updates_share_cell_rows() -> Result<()> {
        let mut terminal = terminal()?;
        terminal.feed(b"one\r\ntwo\r\nthree");
        let first = terminal.snapshot()?;
        let second = terminal.snapshot()?;
        assert!(
            first
                .rows
                .iter()
                .zip(&second.rows)
                .all(|(a, b)| Arc::ptr_eq(a, b))
        );
        terminal.feed(b"\x1b[1;1H");
        let moved = terminal.snapshot()?;
        assert_ne!(first.cursor, moved.cursor);
        assert!(
            first
                .rows
                .iter()
                .zip(&moved.rows)
                .all(|(a, b)| Arc::ptr_eq(a, b))
        );
        Ok(())
    }

    #[test]
    fn changing_one_row_preserves_other_rows() -> Result<()> {
        let mut terminal = terminal()?;
        terminal.feed(b"one\r\ntwo\r\nthree");
        let first = terminal.snapshot()?;
        terminal.feed(b"\x1b[1;1HX");
        let changed = terminal.snapshot()?;
        assert!(!Arc::ptr_eq(&first.rows[0], &changed.rows[0]));
        assert!(Arc::ptr_eq(&first.rows[1], &changed.rows[1]));
        assert!(Arc::ptr_eq(&first.rows[2], &changed.rows[2]));
        assert_eq!(changed.text_lines(), ["Xne", "two", "three"]);
        Ok(())
    }

    #[test]
    fn color_and_resize_invalidation_do_not_reuse_stale_cells() -> Result<()> {
        let mut terminal = terminal()?;
        terminal.feed(b"one\r\ntwo");
        let first = terminal.snapshot()?;
        terminal.feed(b"\x1b]10;#ff0000\x07");
        let recolored = terminal.snapshot()?;
        assert_eq!(recolored.foreground, Rgb::from_hex(0x00ff_0000));
        assert_eq!(recolored.rows[1][0].foreground, recolored.foreground);
        assert!(!Arc::ptr_eq(&first.rows[1], &recolored.rows[1]));
        terminal.resize(GridSize::new(10, 4)?, 8, 22)?;
        let resized = terminal.snapshot()?;
        assert_eq!(resized.rows.len(), 4);
        assert!(resized.rows.iter().all(|row| row.len() == 10));
        Ok(())
    }
    #[test]
    fn shared_frames_preserve_identity_for_nonvisual_updates() -> Result<()> {
        let mut terminal = terminal()?;
        terminal.feed(b"text");
        let first = terminal.snapshot_shared()?;
        assert!(Arc::ptr_eq(&first, &terminal.snapshot_shared()?));
        terminal.feed(b"\x1b]2;updated title\x1b\\");
        assert!(Arc::ptr_eq(&first, &terminal.snapshot_shared()?));
        terminal.feed(b"\x1b[1;1H");
        let cursor = terminal.snapshot_shared()?;
        assert!(!Arc::ptr_eq(&first, &cursor));
        assert!(
            first
                .rows
                .iter()
                .zip(&cursor.rows)
                .all(|(before, after)| Arc::ptr_eq(before, after))
        );
        Ok(())
    }

    #[test]
    fn local_history_scroll_changes_viewport_and_application_modes_are_observable() -> Result<()> {
        let mut terminal = terminal()?;
        for line in 0..100 {
            terminal.feed(format!("line{line:03}\r\n").as_bytes());
        }
        let before = terminal.snapshot_shared()?;
        assert!(before.text_lines().iter().any(|line| line == "line099"));
        terminal.scroll_lines(3)?;
        let history = terminal.snapshot_shared()?;
        assert!(!terminal.is_at_bottom()?);
        assert!(!history.text_lines().iter().any(|line| line == "line099"));
        terminal.scroll_to_bottom();
        assert!(terminal.is_at_bottom()?);
        assert_eq!(
            terminal.snapshot_shared()?.text_lines(),
            before.text_lines()
        );
        terminal.feed(b"\x1b[?1049h\x1b[?1000h");
        assert!(terminal.alternate_screen()?);
        assert!(terminal.uses_mouse_reporting()?);
        terminal.feed(b"\x1b[?1000l\x1b[?1049l");
        assert!(!terminal.alternate_screen()?);
        assert!(!terminal.uses_mouse_reporting()?);
        Ok(())
    }

    #[test]
    fn pointer_tracking_distinguishes_click_drag_and_any_motion() -> Result<()> {
        let mut terminal = terminal()?;
        assert_eq!(terminal.mouse_tracking()?, MouseTracking::None);
        for (enable, disable, expected) in [
            (
                b"\x1b[?9h".as_slice(),
                b"\x1b[?9l".as_slice(),
                MouseTracking::Press,
            ),
            (
                b"\x1b[?1000h".as_slice(),
                b"\x1b[?1000l".as_slice(),
                MouseTracking::Press,
            ),
            (
                b"\x1b[?1002h".as_slice(),
                b"\x1b[?1002l".as_slice(),
                MouseTracking::Drag,
            ),
            (
                b"\x1b[?1003h".as_slice(),
                b"\x1b[?1003l".as_slice(),
                MouseTracking::Any,
            ),
        ] {
            terminal.feed(enable);
            assert_eq!(terminal.mouse_tracking()?, expected);
            terminal.feed(disable);
            assert_eq!(terminal.mouse_tracking()?, MouseTracking::None);
        }
        Ok(())
    }
    #[test]
    fn scrolling_preserves_owned_rows_when_their_viewport_position_changes() -> Result<()> {
        let mut terminal = terminal()?;
        for line in 0..10 {
            terminal.feed(format!("line{line:03}\r\n").as_bytes());
        }
        let before = terminal.snapshot_shared()?;
        assert_eq!(before.text_lines(), ["line008", "line009", ""]);
        terminal.feed(b"line010\r\n");
        let after = terminal.snapshot_shared()?;
        assert_eq!(after.text_lines(), ["line009", "line010", ""]);
        assert!(Arc::ptr_eq(&before.rows[1], &after.rows[0]));
        assert_eq!(terminal.cached_rows.len(), 3);
        terminal.scroll_lines(1)?;
        let history = terminal.snapshot_shared()?;
        assert!(Arc::ptr_eq(&after.rows[0], &history.rows[1]));
        assert_eq!(terminal.cached_rows.len(), 3);
        Ok(())
    }
}

#[cfg(test)]
mod fidelity_tests;
