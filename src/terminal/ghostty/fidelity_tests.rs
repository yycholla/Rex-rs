//! Direct VT and snapshot coverage for the owned terminal presentation contract.
use super::*;

fn terminal() -> Result<GhosttyTerminal> {
    GhosttyTerminal::new(TerminalConfig {
        size: GridSize::new(20, 3)?,
        ..TerminalConfig::default()
    })
}

fn current_cursor(terminal: &mut GhosttyTerminal) -> Result<Cursor> {
    terminal
        .snapshot_shared()?
        .cursor
        .context("visible native cursor")
}

#[test]
fn extended_underline_vt_preserves_each_kind_color_and_independent_resets() -> Result<()> {
    let mut terminal = terminal()?;
    terminal.feed(b"\x1b[58:2::12:34:56m");
    for (code, text) in [(0, "A"), (1, "B"), (2, "C"), (3, "D"), (4, "E"), (5, "F")] {
        terminal.feed(format!("\x1b[4:{code}m{text}").as_bytes());
    }
    terminal.feed(b"\x1b[24mG\x1b[4:2m\x1b[59mH\x1b[0mI");
    let frame = terminal.snapshot_shared()?;
    let expected = [
        UnderlineKind::None,
        UnderlineKind::Single,
        UnderlineKind::Double,
        UnderlineKind::Curly,
        UnderlineKind::Dotted,
        UnderlineKind::Dashed,
        UnderlineKind::None,
    ];
    for (cell, kind) in frame.rows[0].iter().zip(expected) {
        assert_eq!(cell.style.underline_kind, kind);
        assert_eq!(cell.style.underline, kind != UnderlineKind::None);
        assert_eq!(
            cell.style.underline_color,
            Some(Rgb {
                r: 12,
                g: 34,
                b: 56
            })
        );
    }
    assert_eq!(frame.rows[0][7].style.underline_kind, UnderlineKind::Double);
    assert_eq!(frame.rows[0][7].style.underline_color, None);
    assert_eq!(frame.rows[0][8].style, CellStyle::default());
    Ok(())
}

#[test]
fn palette_underline_updates_resolve_against_current_snapshot_and_preserve_clean_rows() -> Result<()>
{
    let mut terminal = terminal()?;
    terminal.feed(b"\x1b]4;42;rgb:12/34/56\x1b\\\x1b[4:3m\x1b[58:5:42mA");
    let before = terminal.snapshot_shared()?;
    assert_eq!(
        before.rows[0][0].style.underline_color,
        Some(Rgb::from_hex(0x0012_3456))
    );
    terminal.feed(b"\x1b]4;42;rgb:ab/cd/ef\x1b\\");
    let after = terminal.snapshot_shared()?;
    assert_eq!(
        after.rows[0][0].style.underline_color,
        Some(Rgb::from_hex(0x00ab_cdef))
    );
    assert_eq!(after.rows[0][0].style.underline_kind, UnderlineKind::Curly);
    assert!(!Arc::ptr_eq(&before.rows[0], &after.rows[0]));
    assert!(Arc::ptr_eq(&before.rows[1], &after.rows[1]));
    assert!(Arc::ptr_eq(&after, &terminal.snapshot_shared()?));
    Ok(())
}

#[test]
fn faint_strike_overline_blink_inverse_and_invisible_are_native_flags_not_prepainted_colors()
-> Result<()> {
    let mut terminal = terminal()?;
    terminal.feed(b"\x1b[38;2;200;100;50m\x1b[2;9;53;5;7;8mA\x1b[22;29;55;25;27;28mB\x1b[0mC");
    let frame = terminal.snapshot_shared()?;
    let style = frame.rows[0][0].style;
    assert!(
        style.faint
            && style.strikethrough
            && style.overline
            && style.blink
            && style.inverse
            && style.invisible
    );
    assert_eq!(
        frame.rows[0][0].foreground,
        Rgb {
            r: 200,
            g: 100,
            b: 50
        }
    );
    assert_eq!(frame.rows[0][1].style, CellStyle::default());
    assert_eq!(
        frame.rows[0][1].foreground,
        Rgb {
            r: 200,
            g: 100,
            b: 50
        }
    );
    assert_eq!(
        frame.rows[0][2].foreground,
        TerminalConfig::default().foreground
    );
    Ok(())
}

#[test]
fn cursor_vt_shapes_and_blink_changes_at_fixed_position_preserve_row_arcs() -> Result<()> {
    let mut terminal = terminal()?;
    // DECSCUSR0 restores configured defaults; it does not force blinking.
    terminal.terminal.set_default_cursor_blink(Some(false))?;
    terminal.feed(b"cached rows\x1b[2 q");
    let before = terminal.snapshot_shared()?;
    let position = before.cursor.context("cursor")?;
    for (code, style, blinking) in [
        (0, CursorStyle::Block, false),
        (1, CursorStyle::Block, true),
        (2, CursorStyle::Block, false),
        (3, CursorStyle::Underline, true),
        (4, CursorStyle::Underline, false),
        (5, CursorStyle::Bar, true),
        (6, CursorStyle::Bar, false),
    ] {
        terminal.feed(format!("\x1b[{code} q").as_bytes());
        let frame = terminal.snapshot_shared()?;
        let cursor = frame.cursor.context("cursor")?;
        assert_eq!((cursor.column, cursor.row), (position.column, position.row));
        assert_eq!((cursor.style, cursor.blinking), (style, blinking));
        assert!(
            before
                .rows
                .iter()
                .zip(&frame.rows)
                .all(|(a, b)| Arc::ptr_eq(a, b))
        );
        assert!(Arc::ptr_eq(&frame, &terminal.snapshot_shared()?));
    }
    let after = terminal.snapshot_shared()?;
    assert!(!Arc::ptr_eq(&before, &after));
    terminal.terminal.set_default_cursor_blink(Some(true))?;
    assert!(!current_cursor(&mut terminal)?.blinking);
    terminal.feed(b"\x1b[0 q");
    let defaults = current_cursor(&mut terminal)?;
    assert_eq!(
        (defaults.style, defaults.blinking),
        (CursorStyle::Block, true)
    );
    terminal.feed(b"\x1b[?12h");
    assert!(current_cursor(&mut terminal)?.blinking);
    terminal.feed(b"\x1b[?12l");
    assert!(!current_cursor(&mut terminal)?.blinking);
    Ok(())
}

#[test]
fn cursor_color_visibility_and_default_hollow_style_retain_cell_rows() -> Result<()> {
    let mut terminal = terminal()?;
    terminal
        .terminal
        .set_default_cursor_style(Some(libghostty_vt::terminal::CursorStyle::BlockHollow))?;
    assert_eq!(
        current_cursor(&mut terminal)?.style,
        CursorStyle::BlockHollow
    );
    terminal.feed(b"text\x1b[6 q");
    let before = terminal.snapshot_shared()?;
    terminal.feed(b"\x1b]12;rgb:12/ab/ef\x1b\\");
    let color = terminal.snapshot_shared()?;
    assert_eq!(
        color.cursor.context("cursor")?.color,
        Some(Rgb::from_hex(0x0012_abef))
    );
    assert!(
        before
            .rows
            .iter()
            .zip(&color.rows)
            .all(|(a, b)| Arc::ptr_eq(a, b))
    );
    assert!(!Arc::ptr_eq(&before, &color));
    terminal.feed(b"\x1b]112\x1b\\\x1b[?25l");
    assert_eq!(terminal.snapshot_shared()?.cursor, None);
    terminal.feed(b"\x1b[?25h");
    let visible = current_cursor(&mut terminal)?;
    assert_eq!(visible.color, None);
    assert_eq!(visible.style, CursorStyle::Bar);
    terminal.feed(b"\x1b[0 q");
    assert_eq!(
        current_cursor(&mut terminal)?.style,
        CursorStyle::BlockHollow
    );
    Ok(())
}

#[test]
fn wide_tail_metadata_keeps_native_column_and_cursor_clips_outside_history_view() -> Result<()> {
    let mut terminal = terminal()?;
    terminal.feed("界e\u{301}\x1b[2G".as_bytes());
    let tail = current_cursor(&mut terminal)?;
    assert_eq!((tail.column, tail.row, tail.wide_tail), (1, 0, true));
    let frame = terminal.snapshot_shared()?;
    assert_eq!(frame.rows[0][0].width, CellWidth::Wide);
    assert_eq!(frame.rows[0][1].width, CellWidth::Continuation);
    assert_eq!(frame.rows[0][2].text, "e\u{301}");
    terminal.feed(b"\x1b[1G");
    assert_eq!(
        (
            current_cursor(&mut terminal)?.column,
            current_cursor(&mut terminal)?.wide_tail
        ),
        (0, false)
    );
    terminal.resize(GridSize::new(10, 3)?, 0, 0)?;
    terminal.feed(b"\x1b[2G");
    assert!(current_cursor(&mut terminal)?.wide_tail);
    for _ in 0..10 {
        terminal.feed(b"\r\nline");
    }
    terminal.scroll_lines(20)?;
    assert_eq!(terminal.snapshot_shared()?.cursor, None);
    terminal.scroll_to_bottom();
    assert!(terminal.snapshot_shared()?.cursor.is_some());
    Ok(())
}

#[test]
fn snapshot_restore_preserves_extended_style_cursor_and_following_vt_resets() -> Result<()> {
    let mut source = terminal()?;
    source.feed(
        "\x1b[4:3m\x1b[58:2::21:43:65m界e\u{301}\x1b[6 q\x1b]12;rgb:ab/cd/ef\x1b\\\x1b[2G"
            .as_bytes(),
    );
    let before = source.snapshot_shared()?;
    let bytes = source.snapshot_bytes()?;
    let mut restored = GhosttyTerminal::from_snapshot(&bytes)?;
    let after = restored.snapshot_shared()?;
    assert_eq!(before.rows, after.rows);
    assert_eq!(before.cursor, after.cursor);
    assert_eq!(after.rows[0][0].style.underline_kind, UnderlineKind::Curly);
    assert_eq!(
        after.cursor.context("cursor")?.color,
        Some(Rgb::from_hex(0x00ab_cdef))
    );
    assert!(after.cursor.context("cursor")?.wide_tail);
    restored.feed(b"\x1b[2;1H\x1b[0m\x1b]112\x1b\\\x1b[4 qZ");
    let frame = restored.snapshot_shared()?;
    assert_eq!(frame.rows[1][0].style, CellStyle::default());
    let cursor = frame.cursor.context("cursor")?;
    assert_eq!(
        (cursor.style, cursor.blinking, cursor.color),
        (CursorStyle::Underline, false, None)
    );
    Ok(())
}
