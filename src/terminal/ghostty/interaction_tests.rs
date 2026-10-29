use super::super::TerminalConfig;
use super::*;
use crate::core::GridSize;
use crate::core::interaction::{
    MAX_SEARCH_QUERY_BYTES, SearchCommand, SearchState, SelectionCommand,
};
use libghostty_vt::{
    selection::Selection,
    terminal::{Point, PointCoordinate},
};
use std::sync::Arc;

fn terminal(columns: u16, rows: u16) -> Result<GhosttyTerminal> {
    GhosttyTerminal::new(TerminalConfig {
        size: GridSize::new(columns, rows)?,
        scrollback_limit: 10_000,
        ..TerminalConfig::default()
    })
}

fn caught_up(terminal: &mut GhosttyTerminal) -> Result<SearchState> {
    for _ in 0..4096 {
        terminal.interaction.next_publication = std::time::Instant::now();
        terminal.interaction_tick()?;
        let state = terminal
            .annotations()?
            .search
            .clone()
            .context("active search")?;
        if state.complete {
            return Ok(state);
        }
    }
    anyhow::bail!("native incremental search did not catch up");
}

fn query(terminal: &mut GhosttyTerminal, text: &str, generation: u64) -> Result<SearchState> {
    terminal.search(SearchCommand::Set {
        query: text.to_owned(),
        generation,
    })?;
    caught_up(terminal)
}

fn select_points(terminal: &GhosttyTerminal, start: (u16, u32), end: (u16, u32)) -> Result<()> {
    let a = terminal.terminal.grid_ref(Point::Screen(PointCoordinate {
        x: start.0,
        y: start.1,
    }))?;
    let b = terminal
        .terminal
        .grid_ref(Point::Screen(PointCoordinate { x: end.0, y: end.1 }))?;
    terminal
        .terminal
        .set_selection(Some(&Selection::new(a, b, false)))?;
    Ok(())
}

#[test]
fn native_search_finds_history_wraps_unicode_and_ascii_case() -> Result<()> {
    let mut terminal = terminal(8, 3)?;
    terminal.feed("old NEEDLE\r\nwide界e\u{301}\r\nafter\r\nlast\r\n".as_bytes());
    assert_eq!(query(&mut terminal, "needle", 1)?.total, 1);
    assert_eq!(query(&mut terminal, "界e\u{301}", 2)?.total, 1);
    assert_eq!(query(&mut terminal, "NEEDLE", 3)?.total, 1);
    // "old NEEDLE" crosses a physical soft wrap at eight columns.
    assert_eq!(query(&mut terminal, "old needle", 4)?.total, 1);
    assert_eq!(query(&mut terminal, "afterlast", 5)?.total, 0);
    assert_eq!(query(&mut terminal, "missing", 6)?.total, 0);
    Ok(())
}

#[test]
fn native_search_navigation_wraps_older_and_newer_and_ignores_stale_generation() -> Result<()> {
    let mut terminal = terminal(20, 3)?;
    terminal.feed(b"hit old\r\nhit middle\r\nhit new\r\nend");
    assert_eq!(query(&mut terminal, "hit", 10)?.total, 3);
    terminal.search(SearchCommand::Next { generation: 10 })?;
    let first = terminal
        .annotations()?
        .search
        .clone()
        .context("search")?
        .selected
        .context("selected")?;
    terminal.search(SearchCommand::Next { generation: 10 })?;
    assert_eq!(
        terminal
            .annotations()?
            .search
            .as_ref()
            .context("search")?
            .selected,
        Some((first + 1) % 3)
    );
    terminal.search(SearchCommand::Previous { generation: 10 })?;
    assert_eq!(
        terminal
            .annotations()?
            .search
            .as_ref()
            .context("search")?
            .selected,
        Some(first)
    );
    for _ in 0..3 {
        terminal.search(SearchCommand::Next { generation: 10 })?;
    }
    assert_eq!(
        terminal
            .annotations()?
            .search
            .as_ref()
            .context("search")?
            .selected,
        Some(first)
    );
    terminal.search(SearchCommand::Cancel { generation: 9 })?;
    assert!(terminal.annotations()?.search.is_some());
    terminal.search(SearchCommand::Cancel { generation: 11 })?;
    assert!(terminal.annotations()?.search.is_none());
    assert_eq!(query(&mut terminal, "new", 12)?.total, 1);
    terminal.search(SearchCommand::Set {
        query: "hit".into(),
        generation: 10,
    })?;
    assert_eq!(
        terminal
            .annotations()?
            .search
            .as_ref()
            .context("search")?
            .generation,
        12
    );
    assert!(
        terminal
            .search(SearchCommand::Set {
                query: "x".repeat(MAX_SEARCH_QUERY_BYTES + 1),
                generation: 13
            })
            .is_err()
    );
    Ok(())
}

#[test]
fn search_reconciles_output_alternate_screen_reflow_reset_and_pruning() -> Result<()> {
    let mut terminal = terminal(12, 3)?;
    terminal.feed(b"needle\r\n");
    assert_eq!(query(&mut terminal, "needle", 1)?.total, 1);
    terminal.feed(b"NEEDLE\r\n");
    assert_eq!(caught_up(&mut terminal)?.total, 2);
    terminal.feed(b"\x1b[?1049hOTHER");
    assert_eq!(caught_up(&mut terminal)?.total, 0);
    terminal.feed(b"\x1b[?1049l");
    assert_eq!(caught_up(&mut terminal)?.total, 2);
    terminal.resize(GridSize::new(4, 3)?, 0, 0)?;
    assert_eq!(caught_up(&mut terminal)?.total, 2);
    terminal.feed(b"\x1bc");
    assert_eq!(caught_up(&mut terminal)?.total, 0);
    // Native standard pages hold roughly215*215 cells. At80columns a
    // single page exceeds500rows;256columns forces this fixture across pages.
    terminal.resize(GridSize::new(256, 3)?, 0, 0)?;
    terminal.terminal.set_scrollback_max_lines(Some(5))?;
    terminal.feed(b"needle\r\n");
    for _ in 0..500 {
        terminal.feed(b"blank\r\n");
        terminal.interaction_tick()?;
    }
    assert!(
        terminal.terminal.scrollbar()?.total < 500,
        "history was not pruned"
    );
    assert_eq!(caught_up(&mut terminal)?.total, 0);
    Ok(())
}

#[test]
fn cancellation_while_running_clears_highlights_without_mutating_user_selection() -> Result<()> {
    let mut terminal = terminal(20, 3)?;
    for _ in 0..2000 {
        terminal.feed(b"hit hit hit\r\n");
    }
    terminal.search(SearchCommand::Set {
        query: "hit".into(),
        generation: 1,
    })?;
    terminal.interaction_tick()?;
    terminal.search(SearchCommand::Cancel { generation: 2 })?;
    assert!(terminal.annotations()?.search.is_none());
    terminal.feed(b"copy me");
    let top = terminal.terminal.scrollbar()?.total - terminal.terminal.scrollbar()?.len;
    select_points(
        &terminal,
        (0, u32::try_from(top)?),
        (6, u32::try_from(top)?),
    )?;
    let before = terminal.copy_selection()?;
    query(&mut terminal, "hit", 3)?;
    terminal.search(SearchCommand::Next { generation: 3 })?;
    terminal.search(SearchCommand::Cancel { generation: 4 })?;
    assert_eq!(terminal.copy_selection()?, before);
    Ok(())
}

#[test]
fn native_selection_copies_softwraps_hard_lines_wide_and_combining_both_directions() -> Result<()> {
    let mut terminal = terminal(8, 3)?;
    terminal.feed("abcdefghIJ\r\n界e\u{301} done\r\nlast".as_bytes());
    select_points(&terminal, (0, 0), (1, 1))?;
    assert_eq!(terminal.copy_selection()?.as_deref(), Some("abcdefghIJ"));
    select_points(&terminal, (1, 1), (0, 0))?;
    assert_eq!(terminal.copy_selection()?.as_deref(), Some("abcdefghIJ"));
    select_points(&terminal, (0, 0), (7, 2))?;
    let copied = terminal.copy_selection()?.context("selection")?;
    assert!(copied.contains("abcdefghIJ\n界e\u{301} done"));
    terminal.scroll_lines(2)?;
    assert_eq!(terminal.copy_selection()?.as_deref(), Some(copied.as_str()));
    terminal.feed(b"\r\nnew output");
    assert_eq!(terminal.copy_selection()?.as_deref(), Some(copied.as_str()));
    terminal.resize(GridSize::new(12, 3)?, 0, 0)?;
    assert_eq!(terminal.copy_selection()?.as_deref(), Some(copied.as_str()));
    Ok(())
}

#[test]
fn native_word_line_gestures_and_cancel_preserve_completed_selection() -> Result<()> {
    let mut terminal = terminal(20, 3)?;
    terminal.feed(b"hello world");
    let token = terminal.annotations()?.viewport_token;
    let press = |time_millis| SelectionCommand::Press {
        column: 2.5,
        row: 0.5,
        time_millis,
        viewport_token: token,
    };
    terminal.select(press(1000))?;
    terminal.select(SelectionCommand::Release)?;
    terminal.select(press(1100))?;
    terminal.select(SelectionCommand::Release)?;
    assert_eq!(terminal.copy_selection()?.as_deref(), Some("hello"));
    terminal.select(press(1200))?;
    terminal.select(SelectionCommand::Release)?;
    assert_eq!(terminal.copy_selection()?.as_deref(), Some("hello world"));
    terminal.select(SelectionCommand::Cancel)?;
    assert_eq!(terminal.copy_selection()?.as_deref(), Some("hello world"));
    terminal.select(SelectionCommand::Clear)?;
    assert_eq!(terminal.copy_selection()?, None);
    Ok(())
}

#[test]
fn viewport_tokens_reject_moved_rows_and_replacement_but_keep_anchored_history() -> Result<()> {
    let mut terminal = terminal(16, 3)?;
    terminal.feed(b"old1\r\nold2\r\nold3\r\nnew1\r\nnew2");
    terminal.scroll_lines(2)?;
    let original = terminal.annotations()?;
    terminal.feed(b"\r\nnew3");
    terminal.interaction.next_publication = std::time::Instant::now();
    assert_eq!(
        terminal.annotations()?.viewport_token,
        original.viewport_token
    );
    terminal.scroll_to_bottom();
    let current = terminal.annotations()?;
    assert_ne!(current.viewport_token, original.viewport_token);
    assert!(
        terminal
            .select(SelectionCommand::Press {
                column: 0.5,
                row: 0.5,
                time_millis: 0,
                viewport_token: original.viewport_token
            })
            .is_err()
    );
    terminal.feed(b"\x1b[Hdifferent");
    terminal.interaction.next_publication = std::time::Instant::now();
    assert_eq!(
        terminal.annotations()?.viewport_token,
        current.viewport_token
    );
    let bytes = terminal.snapshot_bytes()?;
    let mut replacement = GhosttyTerminal::from_snapshot(&bytes)?;
    assert_ne!(
        replacement.annotations()?.viewport_token,
        terminal.annotations()?.viewport_token
    );
    let a = terminal.annotations()?;
    let b = terminal.annotations()?;
    assert!(Arc::ptr_eq(&a, &b));
    Ok(())
}

#[test]
fn autoscroll_stops_on_release_and_cancel_without_clearing_selection() -> Result<()> {
    let mut terminal = terminal(20, 3)?;
    for _ in 0..30 {
        terminal.feed(b"history\r\n");
    }
    let token = terminal.annotations()?.viewport_token;
    terminal.select(SelectionCommand::Press {
        column: 3.5,
        row: 1.5,
        time_millis: 1000,
        viewport_token: token,
    })?;
    terminal.select(SelectionCommand::Drag {
        column: 3.5,
        row: -1.,
        rectangle: false,
        viewport_token: token,
    })?;
    terminal.interaction.next_autoscroll = std::time::Instant::now();
    terminal.interaction_tick()?;
    assert!(!terminal.is_at_bottom()?);
    terminal.select(SelectionCommand::Release)?;
    let after_release = terminal.terminal.scrollbar()?;
    terminal.interaction.next_autoscroll = std::time::Instant::now();
    terminal.interaction_tick()?;
    let stopped = terminal.terminal.scrollbar()?;
    assert_eq!(
        (stopped.offset, stopped.total, stopped.len),
        (after_release.offset, after_release.total, after_release.len)
    );
    let copied = terminal.copy_selection()?;
    terminal.select(SelectionCommand::Cancel)?;
    assert_eq!(terminal.copy_selection()?, copied);
    Ok(())
}

#[test]
fn oversized_copy_span_is_rejected_and_long_match_is_clipped_at_both_viewport_edges() -> Result<()>
{
    let mut terminal = terminal(80, 3)?;
    // This fixture tests the copy admission limit, not Ghostty's independent
    // byte-based history eviction. Retain the entire over-limit selection.
    terminal.terminal.set_scrollback_max_bytes(None)?;
    terminal.feed(b"begin");
    for _ in 0..4000 {
        terminal.feed(b"\r\nline");
    }
    ensure!(
        terminal.terminal.scrollbar()?.total >= 4001,
        "oversized copy fixture lost history"
    );
    select_points(&terminal, (0, 0), (3, 4000))?;
    assert!(terminal.copy_selection().is_err());
    let mut terminal = self::terminal(4, 3)?;
    terminal.feed(b"abcdefghijklmnopqrstuvwx");
    terminal.scroll_lines(1)?;
    let state = query(&mut terminal, "abcdefghijklmnopqrstuvwx", 1)?;
    assert_eq!(state.total, 1);
    assert_ne!(state.matches, [] as [crate::core::interaction::CellSpan; 0]);
    assert!(
        state
            .matches
            .iter()
            .all(|span| span.row < 3 && span.start < span.end && span.end <= 4)
    );
    Ok(())
}

#[test]
fn hidden_dense_search_has_no_highlight_extraction_and_reveal_restores_spans_without_vt()
-> Result<()> {
    let mut terminal = terminal(80, 5)?;
    terminal.set_interaction_visible(false);
    for _ in 0..1400 {
        terminal.feed(
            b"qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq\r\n",
        );
    }
    let hidden = query(&mut terminal, "q", 1)?;
    assert!(hidden.complete);
    assert!(hidden.total > 1000);
    assert_eq!(
        hidden.matches,
        [] as [crate::core::interaction::CellSpan; 0]
    );
    assert_eq!(terminal.interaction_highlight_extractions(), 0);
    assert!(terminal.cached_frame.is_none());
    let token = terminal.annotations()?.viewport_token;
    terminal.set_interaction_visible(true);
    let revealed = terminal.annotations()?;
    assert_eq!(revealed.viewport_token, token);
    assert_ne!(
        revealed.search.as_ref().context("revealed search")?.matches,
        [] as [crate::core::interaction::CellSpan; 0]
    );
    assert_eq!(
        revealed.search.as_ref().context("revealed search")?.total,
        hidden.total
    );
    assert_eq!(terminal.interaction_highlight_extractions(), 1);
    assert!(terminal.cached_frame.is_none());
    Ok(())
}

#[test]
fn hidden_selection_stays_copyable_and_reveal_restores_exact_word_spans() -> Result<()> {
    use crate::core::interaction::CellSpan;
    let mut terminal = terminal(12, 3)?;
    terminal.feed(b"hit hit");
    select_points(&terminal, (0, 0), (2, 0))?;
    let visible = query(&mut terminal, "hit", 1)?;
    assert_eq!(
        visible.matches,
        vec![
            CellSpan {
                row: 0,
                start: 0,
                end: 3
            },
            CellSpan {
                row: 0,
                start: 4,
                end: 7
            }
        ]
    );
    let frame = terminal.snapshot_shared()?;
    let before = terminal.interaction_highlight_extractions();
    terminal.set_interaction_visible(false);
    let hidden = terminal.annotations()?;
    assert_eq!(hidden.selection, [] as [CellSpan; 0]);
    assert_eq!(
        hidden.search.as_ref().context("hidden search")?.matches,
        [] as [CellSpan; 0]
    );
    assert_eq!(terminal.interaction_highlight_extractions(), before);
    assert_eq!(terminal.copy_selection()?.as_deref(), Some("hit"));
    terminal.set_interaction_visible(true);
    let revealed = terminal.annotations()?;
    assert_eq!(
        revealed.selection,
        vec![CellSpan {
            row: 0,
            start: 0,
            end: 3
        }]
    );
    assert_eq!(
        revealed.search.as_ref().context("search")?.matches,
        visible.matches
    );
    assert!(Arc::ptr_eq(&frame, &terminal.snapshot_shared()?));
    Ok(())
}

// The pre-optimization reference uses native endpoint ordering and full Screen
// coordinates. Keep it only as a broad oracle for page edges and offscreen ends.
fn reference_mark(
    terminal: &libghostty_vt::Terminal<'_, '_>,
    selection: &Selection<'_>,
    columns: u16,
    rows: u16,
    origin: u32,
    mask: &mut [bool],
) -> Result<()> {
    use libghostty_vt::{selection::Order, terminal::PointSpace};
    let selection = selection.to_ordered(terminal, Order::Forward)?;
    let a = terminal
        .point_from_grid_ref(&selection.start(), PointSpace::Screen)?
        .context("reference start")?;
    let b = terminal
        .point_from_grid_ref(&selection.end(), PointSpace::Screen)?
        .context("reference end")?;
    for y in a.y.max(origin)..=b.y.min(origin + u32::from(rows) - 1) {
        let start = if y == a.y { a.x } else { 0 };
        let end = if y == b.y {
            b.x.saturating_add(1).min(columns)
        } else {
            columns
        };
        for x in start..end {
            mask[usize::try_from(y - origin)? * usize::from(columns) + usize::from(x)] = true;
        }
    }
    Ok(())
}

fn assert_reference_highlights(terminal: &mut GhosttyTerminal) -> Result<()> {
    use libghostty_vt::terminal::PointSpace;
    let actual = terminal.annotations()?.search.clone().context("query")?;
    let columns = terminal.size.columns();
    let rows = terminal.size.rows();
    let search = terminal
        .interaction
        .search
        .as_mut()
        .context("native search")?;
    let snapshot = search.snapshot(&mut terminal.terminal)?;
    let native = snapshot.terminal();
    let origin = native.grid_ref(Point::Viewport(PointCoordinate { x: 0, y: 0 }))?;
    let origin = native
        .point_from_grid_ref(&origin, PointSpace::Screen)?
        .context("reference viewport")?
        .y;
    let mut mask = vec![false; usize::from(columns) * usize::from(rows)];
    let mut buffer = libghostty_vt::search::MatchBuffer::new();
    for selection in snapshot.viewport_matches(&mut buffer)? {
        reference_mark(native, &selection, columns, rows, origin, &mut mask)?;
    }
    assert_eq!(actual.matches, mask_spans(&mask, columns));
    mask.fill(false);
    if let Some(selection) = snapshot.selected_match()? {
        reference_mark(native, &selection, columns, rows, origin, &mut mask)?;
    }
    assert_eq!(actual.selected_match, mask_spans(&mask, columns));
    Ok(())
}

#[test]
fn optimized_highlights_match_screen_reference_across_dense_history_page_edges() -> Result<()> {
    let mut terminal = terminal(80, 5)?;
    terminal.terminal.set_scrollback_max_bytes(None)?;
    for _ in 0..1400 {
        terminal.feed(
            b"qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq\r\n",
        );
    }
    assert!(terminal.terminal.scrollbar()?.total >= 1400);
    query(&mut terminal, "q", 1)?;
    for delta in [0, 1, 175, 400, 570, -10, 300] {
        terminal.scroll_lines(delta)?;
        assert_reference_highlights(&mut terminal)?;
    }
    terminal.search(SearchCommand::Next { generation: 1 })?;
    terminal.annotations()?;
    // Keep selected result fixed while scrolling its endpoints above and below
    // the viewport; compare both aggregate and selected-match clipping.
    terminal.scroll_to_bottom();
    assert_reference_highlights(&mut terminal)?;
    terminal.scroll_lines(1200)?;
    assert_reference_highlights(&mut terminal)?;
    Ok(())
}

#[test]
fn clipped_wrap_and_unicode_highlights_have_exact_spans_and_match_reference() -> Result<()> {
    use crate::core::interaction::CellSpan;
    let mut terminal = terminal(4, 3)?;
    terminal.feed(b"abcdefghijklmnopqrstuvwx");
    terminal.scroll_lines(1)?;
    let state = query(&mut terminal, "abcdefghijklmnopqrstuvwx", 1)?;
    assert_eq!(
        state.matches,
        vec![
            CellSpan {
                row: 0,
                start: 0,
                end: 4
            },
            CellSpan {
                row: 1,
                start: 0,
                end: 4
            },
            CellSpan {
                row: 2,
                start: 0,
                end: 4
            }
        ]
    );
    assert_reference_highlights(&mut terminal)?;
    let mut terminal = self::terminal(8, 3)?;
    terminal.feed("a界e\u{301} zz\r\nnext".as_bytes());
    let state = query(&mut terminal, "界e\u{301}", 1)?;
    assert_eq!(
        state.matches,
        vec![CellSpan {
            row: 0,
            start: 1,
            end: 4
        }]
    );
    assert_reference_highlights(&mut terminal)?;
    terminal.resize(GridSize::new(3, 3)?, 0, 0)?;
    caught_up(&mut terminal)?;
    assert_reference_highlights(&mut terminal)?;
    Ok(())
}

#[test]
fn hiding_active_drag_stops_autoscroll_and_ignores_late_pointer_events() -> Result<()> {
    let mut terminal = terminal(20, 3)?;
    for _ in 0..30 {
        terminal.feed(b"history\r\n");
    }
    let token = terminal.annotations()?.viewport_token;
    let press = SelectionCommand::Press {
        column: 3.5,
        row: 1.5,
        time_millis: 1000,
        viewport_token: token,
    };
    let drag = SelectionCommand::Drag {
        column: 3.5,
        row: -1.,
        rectangle: false,
        viewport_token: token,
    };
    terminal.select(press)?;
    terminal.select(drag)?;
    terminal.interaction.next_autoscroll = std::time::Instant::now();
    terminal.interaction_tick()?;
    assert!(!terminal.is_at_bottom()?);
    let stopped = terminal.terminal.scrollbar()?;
    let copied = terminal.copy_selection()?.context("drag selection")?;
    terminal.set_interaction_visible(false);
    // The old token is now stale after autoscroll. Hidden events are quietly
    // discarded before token validation, and cannot restart pointer capture.
    terminal.select(press)?;
    terminal.select(drag)?;
    for _ in 0..3 {
        terminal.interaction.next_autoscroll = std::time::Instant::now();
        terminal.interaction_tick()?;
        let current = terminal.terminal.scrollbar()?;
        assert_eq!(
            (current.offset, current.total, current.len),
            (stopped.offset, stopped.total, stopped.len)
        );
        assert_eq!(terminal.copy_selection()?.as_deref(), Some(copied.as_str()));
    }
    terminal.select(SelectionCommand::Release)?;
    terminal.select(SelectionCommand::Cancel)?;
    terminal.set_interaction_visible(true);
    let revealed = terminal.annotations()?;
    assert_ne!(
        revealed.selection,
        [] as [crate::core::interaction::CellSpan; 0]
    );
    terminal.interaction.next_autoscroll = std::time::Instant::now();
    terminal.interaction_tick()?;
    let current = terminal.terminal.scrollbar()?;
    assert_eq!(current.offset, stopped.offset);
    assert_eq!(terminal.copy_selection()?.as_deref(), Some(copied.as_str()));
    terminal.set_interaction_visible(false);
    terminal.select(SelectionCommand::Clear)?;
    assert_eq!(terminal.copy_selection()?, None);
    Ok(())
}
