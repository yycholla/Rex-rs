//! Paint-only terminal styles. Native state and glyph shaping stay elsewhere.
use super::*;
use gpui::{BorderStyle, UnderlineStyle, outline};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DecorationKind {
    Underline(UnderlineKind),
    Strike,
    Overline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DecorationSpan {
    pub span: CellSpan,
    pub kind: DecorationKind,
    pub blink: bool,
}

pub(super) fn append_decoration(
    spans: &mut Vec<DecorationSpan>,
    span: CellSpan,
    kind: DecorationKind,
    blink: bool,
) {
    if let Some(previous) = spans
        .iter_mut()
        .rev()
        .find(|previous| previous.kind == kind)
        && previous.blink == blink
        && previous.span.color == span.color
        && previous.span.column + previous.span.columns == span.column
    {
        previous.span.columns += span.columns;
    } else {
        spans.push(DecorationSpan { span, kind, blink });
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct DecorationLine {
    x: f32,
    y: f32,
    width: f32,
    wavy: bool,
}

fn decoration_lines(
    kind: DecorationKind,
    width: f32,
    height: f32,
    underline: f32,
    strike: f32,
) -> Vec<DecorationLine> {
    let bottom = (height - 1.).max(0.);
    let y = match kind {
        DecorationKind::Strike => strike.clamp(0., bottom),
        DecorationKind::Overline => 1_f32.min(bottom),
        DecorationKind::Underline(_) => underline.clamp(0., bottom),
    };
    let line = |x, y, width, wavy| DecorationLine { x, y, width, wavy };
    match kind {
        DecorationKind::Underline(UnderlineKind::None) => Vec::new(),
        DecorationKind::Underline(UnderlineKind::Double) => {
            let y = y.min((height - 3.).max(0.));
            vec![
                line(0., y, width, false),
                line(0., (y + 2.).min(bottom), width, false),
            ]
        }
        DecorationKind::Underline(UnderlineKind::Curly) => {
            vec![line(0., y.min((height - 3.).max(0.)), width, true)]
        }
        DecorationKind::Underline(UnderlineKind::Dotted | UnderlineKind::Dashed) => {
            let (dash, period): (f32, f32) =
                if kind == DecorationKind::Underline(UnderlineKind::Dotted) {
                    (1., 3.)
                } else {
                    (4., 7.)
                };
            let mut lines = Vec::new();
            let mut x = 0.;
            while x < width {
                lines.push(line(x, y, dash.min(width - x), false));
                x += period;
            }
            lines
        }
        _ => vec![line(0., y, width, false)],
    }
}

pub(super) fn paint_decorations(
    row: &PreparedRow,
    origin: Point<Pixels>,
    metrics: TerminalMetrics,
    phase: TerminalPaintPhase,
    window: &mut Window,
) {
    paint_decoration_color(row, origin, metrics, phase, None, window);
}

fn paint_decoration_color(
    row: &PreparedRow,
    origin: Point<Pixels>,
    metrics: TerminalMetrics,
    phase: TerminalPaintPhase,
    override_color: Option<Hsla>,
    window: &mut Window,
) {
    let mask = window.content_mask().bounds;
    for decoration in &row.decorations {
        if !phase.shows_text(decoration.blink)
            || !span_bounds(origin, decoration.span, metrics).intersects(&mask)
        {
            continue;
        }
        let width = f32::from(decoration.span.columns) * metrics.cell_width;
        let x = f32::from(decoration.span.column) * metrics.cell_width;
        let color = override_color.unwrap_or_else(|| rgb(decoration.span.color.to_hex()).into());
        for line in decoration_lines(
            decoration.kind,
            width,
            metrics.cell_height,
            row.underline_y.into(),
            row.strike_y.into(),
        ) {
            window.paint_underline(
                origin + point(px(x + line.x), px(line.y)),
                px(line.width),
                &UnderlineStyle {
                    color: Some(color),
                    thickness: px(1.),
                    wavy: line.wavy,
                },
            );
        }
    }
}

pub(super) struct CursorPlan {
    bounds: Bounds<Pixels>,
    row: u16,
    style: CursorStyle,
    color: Rgb,
    text_color: Rgb,
}

pub(super) fn cursor_plan(
    frame: &TerminalFrame,
    focused: bool,
    phase: TerminalPaintPhase,
    metrics: TerminalMetrics,
) -> Option<CursorPlan> {
    let cursor = frame.cursor?;
    if cursor.row >= frame.size.rows()
        || cursor.column >= frame.size.columns()
        || (focused && cursor.blinking && !phase.cursor_visible)
    {
        return None;
    }
    let cells = frame.rows.get(usize::from(cursor.row))?;
    let mut column = cursor.column;
    if cursor.wide_tail
        || (cells.get(usize::from(column))?.width == CellWidth::Continuation
            && column > 0
            && cells[usize::from(column - 1)].width == CellWidth::Wide)
    {
        column = column.saturating_sub(1);
    }
    let cell = cells.get(usize::from(column))?;
    let columns = if cursor.wide_tail || cell.width == CellWidth::Wide {
        2
    } else {
        1
    };
    let columns = columns.min(frame.size.columns() - column);
    let style = if focused {
        cursor.style
    } else {
        CursorStyle::BlockHollow
    };
    let mut bounds = Bounds::new(
        point(
            px(f32::from(column) * metrics.cell_width),
            px(f32::from(cursor.row) * metrics.cell_height),
        ),
        size(
            px(f32::from(columns) * metrics.cell_width),
            px(metrics.cell_height),
        ),
    );
    match style {
        CursorStyle::Bar => bounds.size.width = px(2_f32.min(metrics.cell_width)),
        CursorStyle::Underline => {
            let thickness = 2_f32.min(metrics.cell_height);
            bounds.origin.y += px(metrics.cell_height - thickness);
            bounds.size.height = px(thickness);
        }
        CursorStyle::Block | CursorStyle::BlockHollow => {}
    }
    let color = cursor.color.unwrap_or(frame.foreground);
    Some(CursorPlan {
        bounds,
        row: cursor.row,
        style,
        color,
        text_color: cursor_text_color(color, cell_colors(cell).1),
    })
}

fn cursor_text_color(cursor: Rgb, background: Rgb) -> Rgb {
    let brightness = |color: Rgb| {
        (u32::from(color.r) * 299 + u32::from(color.g) * 587 + u32::from(color.b) * 114) / 1000
    };
    if brightness(cursor).abs_diff(brightness(background)) >= 96 {
        return background;
    }
    Rgb::from_hex(if brightness(cursor) >= 128 {
        0
    } else {
        0x00ff_ffff
    })
}

pub(super) fn paint_cursor(
    cursor: &CursorPlan,
    rows: &[Arc<PreparedRow>],
    origin: Point<Pixels>,
    metrics: TerminalMetrics,
    phase: TerminalPaintPhase,
    window: &mut Window,
) {
    let bounds = Bounds::new(origin + cursor.bounds.origin, cursor.bounds.size);
    window.paint_layer(bounds, |window| {
        if cursor.style == CursorStyle::BlockHollow {
            window.paint_quad(outline(
                bounds,
                rgb(cursor.color.to_hex()),
                BorderStyle::Solid,
            ));
        } else {
            window.paint_quad(fill(bounds, rgb(cursor.color.to_hex())));
        }
        if cursor.style == CursorStyle::Block
            && let Some(row) = rows.get(usize::from(cursor.row))
        {
            let origin = origin + point(px(0.), px(f32::from(cursor.row) * metrics.cell_height));
            let color = rgb(cursor.text_color.to_hex()).into();
            // Repaint cached geometry through the cursor mask. This preserves
            // ligatures/combining overhang and leaves block glyphs readable.
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                paint_glyphs::<false>(row, origin, metrics, phase, Some(color), window);
                paint_decoration_color(row, origin, metrics, phase, Some(color), window);
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{GridSize, frame::Cursor};
    use anyhow::{Context as _, Result};

    fn frame() -> Result<TerminalFrame> {
        Ok(TerminalFrame {
            images: Arc::default(),
            graphics_warning: None,
            size: GridSize::new(3, 1)?,
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
                Cell::default(),
            ])],
            cursor: Some(Cursor {
                column: 1,
                row: 0,
                wide_tail: true,
                color: Some(Rgb::from_hex(0x0012_3456)),
                blinking: true,
                ..Cursor::default()
            }),
            foreground: Rgb::default(),
            background: Rgb::default(),
        })
    }

    #[test]
    fn cursor_tail_normalization_and_focus_policy_preserve_native_shape_color() -> Result<()> {
        let frame = frame()?;
        let metrics = Theme::default().terminal;
        let mut phase = TerminalPaintPhase::default();
        let block = cursor_plan(&frame, true, phase, metrics).context("valid fixture cursor")?;
        assert_eq!(block.bounds.origin.x, px(0.));
        assert_eq!(block.bounds.size.width, px(2. * metrics.cell_width));
        assert_eq!(block.color, Rgb::from_hex(0x0012_3456));
        phase.cursor_visible = false;
        assert!(cursor_plan(&frame, true, phase, metrics).is_none());
        let unfocused =
            cursor_plan(&frame, false, phase, metrics).context("unfocused cursor stays visible")?;
        assert_eq!(unfocused.style, CursorStyle::BlockHollow);
        assert_ne!(unfocused.text_color, unfocused.color);
        let mut unconfigured = frame.clone();
        unconfigured.foreground = Rgb::from_hex(0x00ab_cdef);
        unconfigured.cursor = unconfigured.cursor.map(|mut cursor| {
            cursor.color = None;
            cursor
        });
        assert_eq!(
            cursor_plan(&unconfigured, true, TerminalPaintPhase::default(), metrics)
                .context("default foreground cursor")?
                .color,
            unconfigured.foreground
        );
        Ok(())
    }

    #[test]
    fn underline_patterns_are_distinct_and_stay_inside_row() {
        for kind in [
            UnderlineKind::Single,
            UnderlineKind::Double,
            UnderlineKind::Curly,
            UnderlineKind::Dotted,
            UnderlineKind::Dashed,
        ] {
            let lines = decoration_lines(DecorationKind::Underline(kind), 17., 16., 15., 8.);
            assert!(lines.iter().all(|line| line.width > 0.
                && line.x + line.width <= 17.
                && line.y + if line.wavy { 3. } else { 1. } <= 16.));
            match kind {
                UnderlineKind::Double => assert_eq!(lines.len(), 2),
                UnderlineKind::Curly => assert!(lines[0].wavy),
                UnderlineKind::Dotted | UnderlineKind::Dashed => assert!(lines.len() > 1),
                _ => assert_eq!(lines.len(), 1),
            }
        }
        assert_ne!(
            decoration_lines(
                DecorationKind::Underline(UnderlineKind::Dotted),
                17.,
                16.,
                12.,
                8.
            ),
            decoration_lines(
                DecorationKind::Underline(UnderlineKind::Dashed),
                17.,
                16.,
                12.,
                8.
            )
        );
    }

    #[test]
    fn cursor_styles_stay_in_normalized_cell_and_steady_cursor_ignores_phase() -> Result<()> {
        let mut frame = frame()?;
        let metrics = Theme::default().terminal;
        for style in [
            CursorStyle::Block,
            CursorStyle::Bar,
            CursorStyle::Underline,
            CursorStyle::BlockHollow,
        ] {
            frame.cursor = frame.cursor.map(|mut cursor| {
                cursor.style = style;
                cursor.blinking = false;
                cursor
            });
            let plan = cursor_plan(
                &frame,
                true,
                TerminalPaintPhase {
                    cursor_visible: false,
                    text_visible: false,
                },
                metrics,
            )
            .context("steady cursor")?;
            assert_eq!(plan.style, style);
            assert_eq!(plan.bounds.origin.x, px(0.));
            assert!(plan.bounds.bottom() <= px(metrics.cell_height));
            match style {
                CursorStyle::Bar => {
                    assert_eq!(plan.bounds.size.width, px(2_f32.min(metrics.cell_width)));
                }
                CursorStyle::Underline => {
                    assert_eq!(plan.bounds.size.height, px(2_f32.min(metrics.cell_height)));
                }
                _ => assert_eq!(
                    plan.bounds.size,
                    size(px(2. * metrics.cell_width), px(metrics.cell_height))
                ),
            }
        }
        assert_eq!(
            cursor_text_color(Rgb::from_hex(0x00ff_ffff), Rgb::from_hex(0x00ff_ffff)),
            Rgb::default()
        );
        assert_eq!(
            cursor_text_color(Rgb::default(), Rgb::default()),
            Rgb::from_hex(0x00ff_ffff)
        );
        Ok(())
    }

    #[test]
    fn decoration_batching_merges_each_kind_without_crossing_blink_or_color_boundaries() {
        let mut spans = Vec::new();
        for column in 0..3 {
            for kind in [
                DecorationKind::Underline(UnderlineKind::Single),
                DecorationKind::Strike,
            ] {
                append_decoration(
                    &mut spans,
                    CellSpan {
                        column,
                        columns: 1,
                        color: Rgb::default(),
                    },
                    kind,
                    column == 2,
                );
            }
        }
        assert_eq!(spans.len(), 4);
        assert_eq!(spans[0].span.columns, 2);
        assert_eq!(spans[1].span.columns, 2);
        assert!(spans[2].blink);
        assert!(spans[3].blink);
    }
}
