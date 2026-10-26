use crate::{
    core::frame::{CellWidth, TerminalFrame},
    ui::theme::Theme,
};
use gpui::{Div, FontWeight, div, prelude::*, px, rgb};

/// Preview renderer: batches equal-style cells into positioned runs.
/// Input, selection, font measurement and viewport resizing belong in a future
/// stateful terminal view; this element only consumes an owned frame.
#[must_use]
pub fn terminal_surface(frame: &TerminalFrame, focused: bool, theme: Theme) -> Div {
    let metrics = theme.terminal;
    let rows = frame.rows.iter().enumerate().map(|(row_index, _)| {
        let runs = frame.row_runs(row_index).into_iter().filter_map(|run| {
            // Grid coordinates are u16. Reject malformed runs rather than round
            // potentially large usize values when converting positions to pixels.
            let column = u16::try_from(run.column).ok()?;
            let columns = u16::try_from(run.columns).ok()?;
            if column >= frame.size.columns() {
                return None;
            }
            let columns = columns.min(frame.size.columns() - column);
            let (mut foreground, mut background) = (run.foreground, run.background);
            if run.style.inverse {
                std::mem::swap(&mut foreground, &mut background);
            }
            if run.style.invisible {
                foreground = background;
            }
            Some(
                div()
                    .absolute()
                    .left(px(f32::from(column) * metrics.cell_width))
                    .w(px(f32::from(columns) * metrics.cell_width))
                    .h(px(metrics.cell_height))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_color(rgb(foreground.to_hex()))
                    .bg(rgb(background.to_hex()))
                    .when(run.style.bold, |element| {
                        element.font_weight(FontWeight::BOLD)
                    })
                    .when(run.style.italic, gpui::Styled::italic)
                    .when(run.style.underline, gpui::Styled::underline)
                    .child(run.text),
            )
        });
        div()
            .relative()
            .w(px(f32::from(frame.size.columns()) * metrics.cell_width))
            .h(px(metrics.cell_height))
            .flex_shrink_0()
            .children(runs)
    });
    let mut surface = div()
        .relative()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .font_family(metrics.font_family)
        .text_size(px(metrics.font_size))
        .line_height(px(metrics.cell_height))
        .bg(rgb(frame.background.to_hex()))
        .w(px(f32::from(frame.size.columns()) * metrics.cell_width))
        .h(px(f32::from(frame.size.rows()) * metrics.cell_height))
        .children(rows);
    if focused && let Some(cursor) = frame.cursor {
        let wide = frame
            .rows
            .get(usize::from(cursor.row))
            .and_then(|row| row.get(usize::from(cursor.column)))
            .is_some_and(|cell| cell.width == CellWidth::Wide);
        surface = surface.child(
            div()
                .absolute()
                .left(px(f32::from(cursor.column) * metrics.cell_width))
                .top(px(f32::from(cursor.row) * metrics.cell_height))
                .w(px(metrics.cell_width * if wide { 2. } else { 1. }))
                .h(px(metrics.cell_height))
                .border_b_2()
                .border_color(rgb(theme.colors.accent)),
        );
    }
    surface
}
