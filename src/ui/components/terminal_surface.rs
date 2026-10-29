//! Retained terminal text: one canvas, no per-row or per-span layout elements.
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    core::frame::{Cell, CellWidth, CursorStyle, Rgb, TerminalFrame, TerminalRow, UnderlineKind},
    ui::theme::{TerminalMetrics, Theme},
};
use gpui::{
    Bounds, ContentMask, FontId, GlyphId, Hsla, IntoElement, Pixels, Point, ShapedLine, TextRun,
    Window, canvas, fill, font, point, prelude::*, px, rgb, size,
};

mod fallback;
mod paint;
use paint::{DecorationKind, DecorationSpan, append_decoration, paint_cursor, paint_decorations};

/// Independent terminal blink phases; backgrounds are always painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalPaintPhase {
    pub cursor_visible: bool,
    pub text_visible: bool,
}

impl TerminalPaintPhase {
    fn shows_text(self, blinking: bool) -> bool {
        self.text_visible || !blinking
    }
}

impl Default for TerminalPaintPhase {
    fn default() -> Self {
        Self {
            cursor_visible: true,
            text_visible: true,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct MetricsKey {
    family: &'static str,
    font_size: f32,
    cell_width: f32,
    cell_height: f32,
}

impl From<TerminalMetrics> for MetricsKey {
    fn from(metrics: TerminalMetrics) -> Self {
        Self {
            family: metrics.font_family,
            font_size: metrics.font_size,
            cell_width: metrics.cell_width,
            cell_height: metrics.cell_height,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CellSpan {
    column: u16,
    columns: u16,
    color: Rgb,
}

#[derive(Clone)]
struct GlyphGeometry {
    position: Point<Pixels>,
    font_id: FontId,
    glyph_id: GlyphId,
    /// Conservative font bounds retain overhanging combining/ligature glyphs.
    horizontal_ink: Option<(Pixels, Pixels)>,
    is_emoji: bool,
    column: u16,
    end_column: u16,
}

struct PreparedGlyph {
    geometry: GlyphGeometry,
    color: Hsla,
    clip: Option<CellSpan>,
    blink: bool,
    faint_background: Option<Rgb>,
}

#[derive(Default)]
struct PreparedRow {
    glyphs: Vec<PreparedGlyph>,
    geometry: Arc<Vec<GlyphGeometry>>,
    backgrounds: Vec<CellSpan>,
    decorations: Vec<DecorationSpan>,
    underline_y: Pixels,
    strike_y: Pixels,
    blinking: bool,
    shape_calls: u8,
    style_spans: usize,
}

struct RowText {
    text: String,
    font_runs: Vec<TextRun>,
    columns_by_byte: Vec<u16>,
}

#[derive(Clone)]
struct CachedRow {
    source: TerminalRow,
    prepared: Arc<PreparedRow>,
}

/// Cumulative cache counters, useful while profiling real terminal workloads.
#[derive(Clone, Copy, Debug, Default)]
pub struct TerminalSurfaceStats {
    pub frames: u64,
    pub reused_rows: u64,
    pub prepared_rows: u64,
    pub shaped_runs: u64,
    pub shaped_lines: u64,
    pub fallback_probes: u64,
}

/// Keeps text shaping alive across frames; unchanged row identities are free to reuse.
/// Font/geometry changes invalidate all shaped rows.
#[derive(Clone)]
pub struct TerminalSurfaceCache {
    rows: Vec<CachedRow>,
    prepared: Arc<Vec<Arc<PreparedRow>>>,
    metrics: Option<MetricsKey>,
    background: Option<Rgb>,
    stats: TerminalSurfaceStats,
    profiling: bool,
    last_report: Option<Instant>,
    missing_glyph: fallback::MissingGlyphOracle,
}

impl Default for TerminalSurfaceCache {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            prepared: Arc::new(Vec::new()),
            metrics: None,
            background: None,
            stats: TerminalSurfaceStats::default(),
            profiling: std::env::var_os("REX_RS_RENDER_STATS").is_some(),
            last_report: None,
            missing_glyph: fallback::MissingGlyphOracle::default(),
        }
    }
}

/// Incremental CPU work stays on GPUI's thread: its shared line-layout cache
/// acquires frame/layout locks in opposite orders, making background use unsafe.
/// Keep the displayed screen complete while advancing a bounded pending screen.
pub(crate) struct TerminalSurfacePreparation {
    pub(crate) frame: Arc<TerminalFrame>,
    metrics: TerminalMetrics,
    cache: TerminalSurfaceCache,
    /// Only the preceding viewport is retained while this preparation runs.
    previous_rows: BTreeMap<usize, CachedRow>,
    next_row: usize,
    slices: u32,
    started: Instant,
    cpu_time: Duration,
    max_slice: Duration,
}

impl TerminalSurfacePreparation {
    pub(crate) fn new(
        cache: &TerminalSurfaceCache,
        frame: Arc<TerminalFrame>,
        metrics: TerminalMetrics,
    ) -> Self {
        let mut cache = cache.clone();
        let key = MetricsKey::from(metrics);
        if cache.metrics != Some(key) || cache.background != Some(frame.background) {
            cache.rows.clear();
        }
        // Capture identities before writing any new rows. A scroll can move
        // an old row into an index whose previous occupant was already replaced.
        // Strong references also prevent address reuse while the map is alive.
        let previous_rows = cache
            .rows
            .iter()
            .map(|row| (Arc::as_ptr(&row.source) as usize, row.clone()))
            .collect();
        cache.metrics = Some(key);
        cache.background = Some(frame.background);
        cache.rows.truncate(frame.rows.len());
        cache.stats.frames += 1;
        Self {
            frame,
            metrics,
            cache,
            previous_rows,
            next_row: 0,
            slices: 0,
            started: Instant::now(),
            cpu_time: Duration::ZERO,
            max_slice: Duration::ZERO,
        }
    }

    pub(crate) fn compatible(&self, frame: &TerminalFrame, metrics: TerminalMetrics) -> bool {
        self.frame.size == frame.size && MetricsKey::from(self.metrics) == MetricsKey::from(metrics)
    }

    /// A row's shaping call cannot be interrupted. Apart from that one-row
    /// lower bound, each pane yields after four milliseconds or 64 changed rows.
    /// A one-millisecond slice turns ordinary font shaping into one row per
    /// display refresh, adding seconds of presentation delay to tall terminals.
    pub(crate) fn advance(&mut self, window: &mut Window) -> bool {
        let mut oracle = std::mem::take(&mut self.cache.missing_glyph);
        let probes = oracle.probes();
        let complete = self.advance_with(|frame, index, previous, metrics| {
            prepare_row(frame, index, previous, metrics, &mut oracle, window)
        });
        self.cache.stats.fallback_probes += oracle.probes() - probes;
        self.cache.missing_glyph = oracle;
        complete
    }

    fn advance_with(
        &mut self,
        mut prepare: impl FnMut(
            &TerminalFrame,
            usize,
            Option<&CachedRow>,
            TerminalMetrics,
        ) -> PreparedRow,
    ) -> bool {
        let started = Instant::now();
        let mut changed = 0;
        while self.next_row < self.frame.rows.len() {
            let index = self.next_row;
            let source = &self.frame.rows[index];
            if self
                .cache
                .rows
                .get(index)
                .is_some_and(|row| Arc::ptr_eq(&row.source, source))
            {
                self.cache.stats.reused_rows += 1;
            } else if let Some(row) = self.previous_rows.get(&(Arc::as_ptr(source) as usize)) {
                if index < self.cache.rows.len() {
                    self.cache.rows[index].clone_from(row);
                } else {
                    self.cache.rows.push(row.clone());
                }
                self.cache.stats.reused_rows += 1;
            } else {
                let prepared = Arc::new(prepare(
                    &self.frame,
                    index,
                    self.cache.rows.get(index),
                    self.metrics,
                ));
                self.cache.stats.prepared_rows += 1;
                self.cache.stats.shaped_runs += prepared.style_spans as u64;
                self.cache.stats.shaped_lines += u64::from(prepared.shape_calls);
                let row = CachedRow {
                    source: Arc::clone(source),
                    prepared,
                };
                if index < self.cache.rows.len() {
                    self.cache.rows[index] = row;
                } else {
                    self.cache.rows.push(row);
                }
                changed += 1;
            }
            self.next_row += 1;
            if changed >= 64 || started.elapsed() >= Duration::from_millis(4) {
                break;
            }
        }
        let elapsed = started.elapsed();
        self.slices += 1;
        self.cpu_time += elapsed;
        self.max_slice = self.max_slice.max(elapsed);
        self.next_row == self.frame.rows.len()
    }

    pub(crate) fn finish(mut self) -> (TerminalSurfaceCache, Arc<TerminalFrame>) {
        debug_assert_eq!(self.next_row, self.frame.rows.len());
        if self.cache.profiling && self.cache.report_due() {
            eprintln!(
                "terminal-prepare rows={} slices={} prepare_cpu_us={} prepare_wall_us={} max_slice_us={} lines_shaped={} fallback_probes={}",
                self.next_row,
                self.slices,
                self.cpu_time.as_micros(),
                self.started.elapsed().as_micros(),
                self.max_slice.as_micros(),
                self.cache.stats.shaped_lines,
                self.cache.stats.fallback_probes,
            );
        }
        self.cache.prepared = Arc::new(
            self.cache
                .rows
                .iter()
                .map(|row| Arc::clone(&row.prepared))
                .collect(),
        );
        (self.cache, self.frame)
    }
}

impl TerminalSurfaceCache {
    #[must_use]
    pub fn has_blinking_text(&self) -> bool {
        self.prepared.iter().any(|row| row.blinking)
    }

    #[must_use]
    pub const fn stats(&self) -> TerminalSurfaceStats {
        self.stats
    }

    pub(crate) fn metrics_changed(&self, metrics: TerminalMetrics) -> bool {
        self.metrics != Some(metrics.into())
    }

    pub(crate) fn matches_rows(&self, frame: &TerminalFrame, metrics: TerminalMetrics) -> bool {
        self.metrics == Some(metrics.into())
            && self.background == Some(frame.background)
            && self.rows.len() == frame.rows.len()
            && self
                .rows
                .iter()
                .zip(&frame.rows)
                .all(|(cached, source)| Arc::ptr_eq(&cached.source, source))
    }

    fn report_due(&self) -> bool {
        self.last_report
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(1))
    }

    #[cfg(test)]
    fn update_rows(
        &mut self,
        frame: &TerminalFrame,
        key: MetricsKey,
        mut prepare: impl FnMut(usize, Option<&CachedRow>) -> PreparedRow,
    ) -> Arc<Vec<Arc<PreparedRow>>> {
        let mut changed = self.metrics != Some(key)
            || self.background != Some(frame.background)
            || self.rows.len() != frame.rows.len();
        if self.metrics != Some(key) || self.background != Some(frame.background) {
            self.rows.clear();
        }
        self.metrics = Some(key);
        self.background = Some(frame.background);
        self.rows.truncate(frame.rows.len());
        self.stats.frames += 1;
        for (index, source) in frame.rows.iter().enumerate() {
            if self
                .rows
                .get(index)
                .is_some_and(|cached| Arc::ptr_eq(&cached.source, source))
            {
                // The client worker preserves unchanged row identities. Never
                // scan every cell on the UI thread to compensate for transport.
                self.stats.reused_rows += 1;
                continue;
            }
            changed = true;
            let prepared = Arc::new(prepare(index, self.rows.get(index)));
            self.stats.prepared_rows += 1;
            self.stats.shaped_runs += prepared.style_spans as u64;
            self.stats.shaped_lines += u64::from(prepared.shape_calls);
            let row = CachedRow {
                source: Arc::clone(source),
                prepared,
            };
            if index < self.rows.len() {
                self.rows[index] = row;
            } else {
                self.rows.push(row);
            }
        }
        if changed {
            self.prepared = Arc::new(
                self.rows
                    .iter()
                    .map(|row| Arc::clone(&row.prepared))
                    .collect(),
            );
        }
        Arc::clone(&self.prepared)
    }
}

fn prepare_styles(cells: &[Cell], columns: u16, default_background: Rgb) -> PreparedRow {
    let mut prepared = PreparedRow::default();
    for (column, cell) in cells.iter().enumerate().take(usize::from(columns)) {
        if cell.width == CellWidth::Continuation {
            continue;
        }
        let width = if cell.width == CellWidth::Wide { 2 } else { 1 };
        let Some((column, columns)) = clipped_columns(column, width, columns) else {
            continue;
        };
        let (_, background) = cell_colors(cell);
        if background != default_background {
            append_span(&mut prepared.backgrounds, column, columns, background);
        }
        if !cell.style.invisible {
            prepared.blinking |= cell.style.blink
                && ((!cell.text.trim().is_empty() && cell.width != CellWidth::WrapSpacer)
                    || cell.style.resolved_underline() != UnderlineKind::None
                    || cell.style.strikethrough
                    || cell.style.overline);
            let span = CellSpan {
                column,
                columns,
                color: foreground_at(cells, column),
            };
            for kind in [
                (cell.style.resolved_underline() != UnderlineKind::None)
                    .then_some(DecorationKind::Underline(cell.style.resolved_underline())),
                cell.style.strikethrough.then_some(DecorationKind::Strike),
                cell.style.overline.then_some(DecorationKind::Overline),
            ]
            .into_iter()
            .flatten()
            {
                let mut span = span;
                if matches!(kind, DecorationKind::Underline(_)) {
                    span.color = cell.style.underline_color.map_or(span.color, |color| {
                        faint_color(color, background, cell.style.faint)
                    });
                }
                append_decoration(&mut prepared.decorations, span, kind, cell.style.blink);
            }
        }
    }
    prepared
}

fn prepare_row(
    frame: &TerminalFrame,
    index: usize,
    previous: Option<&CachedRow>,
    metrics: TerminalMetrics,
    oracle: &mut fallback::MissingGlyphOracle,
    window: &mut Window,
) -> PreparedRow {
    let cells = &frame.rows[index];
    let mut prepared = prepare_styles(cells, frame.size.columns(), frame.background);
    prepared.underline_y = px((metrics.cell_height - 2.).max(0.));
    prepared.strike_y = px(metrics.cell_height * 0.5);
    if let Some(previous) = previous
        && shaping_matches(&previous.source, cells)
    {
        prepared.geometry = Arc::clone(&previous.prepared.geometry);
        prepared.underline_y = previous.prepared.underline_y;
        prepared.strike_y = previous.prepared.strike_y;
    } else {
        let row = build_row_text(cells, frame.size.columns(), metrics);
        if row.text.is_empty() {
            return prepared;
        }
        prepared.style_spans = row.font_runs.len();
        let line = window.text_system().shape_line(
            row.text.clone().into(),
            px(metrics.font_size),
            &row.font_runs,
            None,
        );
        fallback::trace("primary", index, &row, &line, window);
        prepared.shape_calls = 1;
        let baseline = (px(metrics.cell_height) - line.ascent - line.descent) / 2. + line.ascent;
        prepared.underline_y = baseline + line.descent * 0.618;
        prepared.strike_y = baseline - line.ascent * 0.35;
        let resolved = fallback::geometry(
            fallback::ShapeContext {
                row: &row,
                index,
                cells,
                line: &line,
                columns: frame.size.columns(),
                metrics,
                baseline,
            },
            oracle,
            window,
        );
        prepared.shape_calls += u8::from(resolved.retry_runs > 0);
        prepared.style_spans += resolved.retry_runs;
        prepared.geometry = Arc::new(resolved.geometry);
    }
    prepared.glyphs = color_glyphs(&prepared.geometry, cells);

    prepared
}

fn prepare_geometry(
    row: &RowText,
    cells: &[Cell],
    columns: u16,
    line: &ShapedLine,
    metrics: TerminalMetrics,
    baseline: Pixels,
    window: &Window,
) -> Vec<GlyphGeometry> {
    let mut glyphs = Vec::with_capacity(line.runs.iter().map(|run| run.glyphs.len()).sum());
    let mut native_origins = vec![None; usize::from(columns)];
    let cluster_ends = cluster_ends(
        row.text.len(),
        line.runs
            .iter()
            .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.index)),
    );
    for run in &line.runs {
        for glyph in &run.glyphs {
            let Some(&column) = row.columns_by_byte.get(glyph.index) else {
                continue;
            };
            native_origins[usize::from(column)].get_or_insert(glyph.position.x);
        }
    }
    for run in &line.runs {
        let font_bounds = window
            .text_system()
            .bounding_box(run.font_id, px(metrics.font_size));
        for glyph in &run.glyphs {
            if row.text[glyph.index..].starts_with(' ') {
                continue;
            }
            let Some(&column) = row.columns_by_byte.get(glyph.index) else {
                continue;
            };
            let native_origin = native_origins[usize::from(column)].unwrap_or(glyph.position.x);
            let position = point(
                px(f32::from(column) * metrics.cell_width) + glyph.position.x - native_origin,
                baseline + glyph.position.y,
            );
            let end_byte = cluster_ends
                .get(glyph.index)
                .copied()
                .unwrap_or(row.text.len());
            let end_column = row
                .columns_by_byte
                .get(end_byte.saturating_sub(1))
                .copied()
                .unwrap_or(column);
            let end_width = if cells[usize::from(end_column)].width == CellWidth::Wide {
                2
            } else {
                1
            };
            let end = end_column.saturating_add(end_width).min(columns);
            glyphs.push(GlyphGeometry {
                position,
                font_id: run.font_id,
                glyph_id: glyph.id,
                horizontal_ink: horizontal_ink(position.x, font_bounds),
                is_emoji: glyph.is_emoji,
                column,
                end_column: end,
            });
        }
    }
    glyphs
}

/// Shapers can reorder glyphs and emit several glyphs for one cluster. Index
/// cluster ends once by byte rather than sorting and searching for every glyph.
fn cluster_ends(text_len: usize, starts: impl Iterator<Item = usize>) -> Vec<usize> {
    let mut ends = vec![text_len; text_len];
    for start in starts {
        if let Some(end) = ends.get_mut(start) {
            *end = start;
        }
    }
    let mut next_cluster = text_len;
    for (byte, end) in ends.iter_mut().enumerate().rev() {
        let starts_cluster = *end == byte;
        *end = next_cluster;
        if starts_cluster {
            next_cluster = byte;
        }
    }
    ends
}

/// Colors and decorations do not affect glyph geometry. Preserve shaping for
/// recoloring (for example syntax highlighting or full-screen color animation).
fn shaping_matches(previous: &[Cell], current: &[Cell]) -> bool {
    previous.len() == current.len()
        && previous.iter().zip(current).all(|(previous, current)| {
            previous.text == current.text
                && previous.width == current.width
                && previous.style.bold == current.style.bold
                && previous.style.italic == current.style.italic
                && previous.style.invisible == current.style.invisible
        })
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct GlyphPaint {
    color: Rgb,
    blink: bool,
    faint_background: Option<Rgb>,
}

fn glyph_paint(cells: &[Cell], column: u16) -> GlyphPaint {
    let cell = source_cell(cells, column);
    GlyphPaint {
        color: foreground_at(cells, column),
        blink: cell.style.blink,
        faint_background: (cell.style.faint && !cell.style.invisible)
            .then_some(cell_colors(cell).1),
    }
}

fn glyph_paint_spans(
    cells: &[Cell],
    mut column: u16,
    end_column: u16,
) -> impl Iterator<Item = (CellSpan, GlyphPaint)> + '_ {
    std::iter::from_fn(move || {
        if column >= end_column {
            return None;
        }
        let start = column;
        let paint = glyph_paint(cells, column);
        column += 1;
        while column < end_column && glyph_paint(cells, column) == paint {
            column += 1;
        }
        Some((
            CellSpan {
                column: start,
                columns: column - start,
                color: paint.color,
            },
            paint,
        ))
    })
}

fn color_glyphs(geometry: &[GlyphGeometry], cells: &[Cell]) -> Vec<PreparedGlyph> {
    let mut glyphs = Vec::with_capacity(geometry.len());
    for glyph in geometry {
        let initial = glyph_paint(cells, glyph.column);
        let uniform =
            (glyph.column..glyph.end_column).all(|column| glyph_paint(cells, column) == initial);
        for (span, paint) in glyph_paint_spans(cells, glyph.column, glyph.end_column) {
            glyphs.push(PreparedGlyph {
                geometry: glyph.clone(),
                color: rgb(paint.color.to_hex()).into(),
                clip: (!uniform).then_some(span),
                blink: paint.blink,
                faint_background: paint.faint_background,
            });
        }
    }
    glyphs
}

/// Color does not split shaping runs: it is applied to positioned glyphs later.
/// Each UTF-8 byte maps back to its physical terminal cell, including graphemes.
fn build_row_text(cells: &[Cell], columns: u16, metrics: TerminalMetrics) -> RowText {
    let mut row = RowText {
        text: String::new(),
        font_runs: Vec::new(),
        columns_by_byte: Vec::new(),
    };
    let mut previous_style = None;
    for (column, cell) in cells.iter().enumerate().take(usize::from(columns)) {
        if cell.width == CellWidth::Continuation {
            continue;
        }
        let Ok(column) = u16::try_from(column) else {
            break;
        };
        let width = if cell.width == CellWidth::Wide && column + 1 < columns {
            2
        } else {
            1
        };
        let start = row.text.len();
        if cell.style.invisible {
            for offset in 0..width {
                row.text.push(' ');
                row.columns_by_byte.push(column + offset);
            }
        } else {
            let text = if cell.text.is_empty() || cell.width == CellWidth::WrapSpacer {
                " "
            } else {
                &cell.text
            };
            row.text.push_str(text);
            row.columns_by_byte
                .extend(std::iter::repeat_n(column, text.len()));
        }
        let length = row.text.len() - start;
        let style = (cell.style.bold, cell.style.italic);
        if previous_style == Some(style) {
            if let Some(run) = row.font_runs.last_mut() {
                run.len += length;
            }
        } else {
            let mut face = font(metrics.font_family);
            if style.0 {
                face = face.bold();
            }
            if style.1 {
                face = face.italic();
            }
            row.font_runs.push(TextRun {
                len: length,
                font: face,
                color: rgb(0).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            });
            previous_style = Some(style);
        }
    }
    let trimmed = row.text.trim_end_matches(' ').len();
    row.text.truncate(trimmed);
    row.columns_by_byte.truncate(trimmed);
    let mut bytes = 0;
    row.font_runs.retain_mut(|run| {
        run.len = run.len.min(trimmed.saturating_sub(bytes));
        bytes += run.len;
        run.len != 0
    });
    row
}

fn cell_colors(cell: &Cell) -> (Rgb, Rgb) {
    if cell.style.inverse {
        (cell.background, cell.foreground)
    } else {
        (cell.foreground, cell.background)
    }
}

fn append_span(spans: &mut Vec<CellSpan>, column: u16, columns: u16, color: Rgb) {
    if let Some(last) = spans.last_mut()
        && last.color == color
        && last.column + last.columns == column
    {
        last.columns += columns;
    } else {
        spans.push(CellSpan {
            column,
            columns,
            color,
        });
    }
}

fn source_cell(cells: &[Cell], column: u16) -> &Cell {
    let index = usize::from(column);
    let cell = &cells[index];
    if cell.width == CellWidth::Continuation
        && index > 0
        && cells[index - 1].width == CellWidth::Wide
    {
        &cells[index - 1]
    } else {
        cell
    }
}

/// Local faint policy: compose foreground halfway over its effective background.
/// This changes paint only; shaping and glyph positioning remain reusable.
fn faint_color(foreground: Rgb, background: Rgb, faint: bool) -> Rgb {
    if !faint {
        return foreground;
    }
    let half = |a: u8, b: u8| (u16::from(a) + u16::from(b)).div_ceil(2) as u8;
    Rgb {
        r: half(foreground.r, background.r),
        g: half(foreground.g, background.g),
        b: half(foreground.b, background.b),
    }
}

fn foreground_at(cells: &[Cell], column: u16) -> Rgb {
    let cell = source_cell(cells, column);
    let (foreground, background) = cell_colors(cell);
    if cell.style.invisible {
        background
    } else {
        faint_color(foreground, background, cell.style.faint)
    }
}

#[cfg(test)]
fn foreground_spans(cells: &[Cell], start: u16, end: u16) -> Vec<CellSpan> {
    let mut spans = Vec::new();
    for column in start..end {
        if usize::from(column) >= cells.len() {
            break;
        }
        append_span(&mut spans, column, 1, foreground_at(cells, column));
    }
    spans
}

fn clipped_columns(column: usize, columns: usize, width: u16) -> Option<(u16, u16)> {
    let column = u16::try_from(column).ok()?;
    let columns = u16::try_from(columns).ok()?;
    (column < width).then(|| (column, columns.min(width - column)))
}

/// Paint cached spans directly. The returned canvas owns its data and adds a
/// single layout node regardless of terminal dimensions or color transitions.
#[must_use]
pub fn terminal_surface(
    cache: &mut TerminalSurfaceCache,
    frame: &TerminalFrame,
    focused: bool,
    phase: TerminalPaintPhase,
    theme: Theme,
    _window: &mut Window,
) -> impl IntoElement + use<> {
    let metrics = theme.terminal;
    let profile_paint = cache.profiling && cache.report_due();
    let rows = Arc::clone(&cache.prepared);
    if profile_paint {
        cache.last_report = Some(Instant::now());
    }
    let width = px(f32::from(frame.size.columns()) * metrics.cell_width);
    let height = px(f32::from(frame.size.rows()) * metrics.cell_height);
    let cursor = paint::cursor_plan(frame, focused, phase, metrics);
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _cx| {
            let started = profile_paint.then(Instant::now);
            let mut painted = PaintStats::default();
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                if profile_paint {
                    painted = paint_rows::<true>(&rows, bounds, metrics, phase, window);
                } else {
                    paint_rows::<false>(&rows, bounds, metrics, phase, window);
                }
                if let Some(cursor) = &cursor {
                    paint_cursor(cursor, &rows, bounds.origin, metrics, phase, window);
                }
            });
            if let Some(started) = started {
                eprintln!(
                    "terminal-paint rows={} glyphs_painted={} glyphs_culled={} paint_cpu_us={}",
                    painted.rows,
                    painted.glyphs,
                    painted.culled,
                    started.elapsed().as_micros()
                );
            }
        },
    )
    .w(width)
    .h(height)
    .flex_shrink_0()
}

#[derive(Default)]
struct PaintStats {
    rows: usize,
    glyphs: usize,
    culled: usize,
}

fn paint_rows<const PROFILE: bool>(
    rows: &[Arc<PreparedRow>],
    bounds: Bounds<Pixels>,
    metrics: TerminalMetrics,
    phase: TerminalPaintPhase,
    window: &mut Window,
) -> PaintStats {
    let mut stats = PaintStats::default();
    let mask = window.content_mask().bounds;
    let mut top = bounds.origin.y;
    for row in rows {
        if top >= mask.bottom() {
            break;
        }
        if top + px(metrics.cell_height) > mask.origin.y {
            let origin = point(bounds.origin.x, top);
            let row_bounds = Bounds::new(origin, size(bounds.size.width, px(metrics.cell_height)));
            window.with_content_mask(Some(ContentMask { bounds: row_bounds }), |window| {
                let culled = paint_row::<PROFILE>(row, row_bounds, metrics, phase, window);
                if PROFILE {
                    stats.rows += 1;
                    stats.glyphs += row.glyphs.len() - culled;
                    stats.culled += culled;
                }
            });
        }
        top += px(metrics.cell_height);
    }
    stats
}

fn paint_row<const PROFILE: bool>(
    row: &PreparedRow,
    bounds: Bounds<Pixels>,
    metrics: TerminalMetrics,
    phase: TerminalPaintPhase,
    window: &mut Window,
) -> usize {
    // GPUI's native line painter batches a line with paint_layer. Without a
    // layer, every raw glyph enters the scene's spatial ordering tree, even
    // though terminal rows already have disjoint content masks. Keep separate
    // passes so backgrounds, text and underlines retain their draw order.
    if !row.backgrounds.is_empty() {
        window.paint_layer(bounds, |window| {
            paint_backgrounds(row, bounds.origin, metrics, window);
        });
    }
    let culled = if row.glyphs.is_empty() {
        0
    } else {
        window.paint_layer(bounds, |window| {
            paint_glyphs::<PROFILE>(row, bounds.origin, metrics, phase, None, window)
        })
    };
    if !row.decorations.is_empty() {
        window.paint_layer(bounds, |window| {
            paint_decorations(row, bounds.origin, metrics, phase, window);
        });
    }
    culled
}

fn paint_backgrounds(
    row: &PreparedRow,
    origin: Point<Pixels>,
    metrics: TerminalMetrics,
    window: &mut Window,
) {
    let mask = window.content_mask().bounds;
    for span in &row.backgrounds {
        let bounds = span_bounds(origin, *span, metrics);
        if bounds.intersects(&mask) {
            window.paint_quad(fill(bounds, rgb(span.color.to_hex())));
        }
    }
}

fn paint_glyphs<const PROFILE: bool>(
    row: &PreparedRow,
    origin: Point<Pixels>,
    metrics: TerminalMetrics,
    phase: TerminalPaintPhase,
    override_color: Option<Hsla>,
    window: &mut Window,
) -> usize {
    let mask = window.content_mask().bounds;
    let visible_left = mask.left() - origin.x;
    let visible_right = mask.right() - origin.x;
    let font_size = px(metrics.font_size);
    let mut culled = 0;
    for glyph in &row.glyphs {
        if !phase.shows_text(glyph.blink)
            || !ink_overlaps(glyph.geometry.horizontal_ink, visible_left, visible_right)
            || glyph
                .clip
                .is_some_and(|span| !span_bounds(origin, span, metrics).intersects(&mask))
        {
            if PROFILE {
                culled += 1;
            }
            continue;
        }
        let paint = |window: &mut Window| {
            let result = if glyph.geometry.is_emoji {
                window.paint_emoji(
                    origin + glyph.geometry.position,
                    glyph.geometry.font_id,
                    glyph.geometry.glyph_id,
                    font_size,
                )
            } else {
                window.paint_glyph(
                    origin + glyph.geometry.position,
                    glyph.geometry.font_id,
                    glyph.geometry.glyph_id,
                    font_size,
                    override_color.unwrap_or(glyph.color),
                )
            };
            if let Err(error) = result {
                eprintln!("Could not paint terminal glyph: {error:#}");
            }
            if glyph.geometry.is_emoji
                && override_color.is_none()
                && let Some(background) = glyph.faint_background
            {
                // GPUI emoji paint has no tint/opacity parameter. Restrict the
                // half-background wash to this glyph's own terminal cells.
                let span = glyph.clip.unwrap_or(CellSpan {
                    column: glyph.geometry.column,
                    columns: glyph.geometry.end_column - glyph.geometry.column,
                    color: background,
                });
                let mut color: Hsla = rgb(background.to_hex()).into();
                color.a = 0.5;
                window.paint_quad(fill(span_bounds(origin, span, metrics), color));
            }
        };
        if let Some(span) = glyph.clip {
            window.with_content_mask(
                Some(ContentMask {
                    bounds: span_bounds(origin, span, metrics),
                }),
                paint,
            );
        } else {
            paint(window);
        }
    }
    culled
}

/// Font bounds enclose every glyph, including combining marks and ligature
/// overhang. The extra logical pixel accounts for raster rounding/hinting.
fn horizontal_ink(position: Pixels, font_bounds: Bounds<Pixels>) -> Option<(Pixels, Pixels)> {
    let left = position + font_bounds.left() - px(1.);
    let right = position + font_bounds.right() + px(1.);
    (f32::from(left).is_finite() && f32::from(right).is_finite() && font_bounds.size.width > px(0.))
        .then_some((left, right))
}

fn ink_overlaps(ink: Option<(Pixels, Pixels)>, left: Pixels, right: Pixels) -> bool {
    ink.is_none_or(|(start, end)| end > left && start < right)
}

fn span_bounds(origin: Point<Pixels>, span: CellSpan, metrics: TerminalMetrics) -> Bounds<Pixels> {
    Bounds::new(
        origin + point(px(f32::from(span.column) * metrics.cell_width), px(0.)),
        size(
            px(f32::from(span.columns) * metrics.cell_width),
            px(metrics.cell_height),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        GridSize,
        frame::{CellStyle, Cursor},
    };

    fn cell(text: &str, color: u32) -> Cell {
        Cell {
            text: text.into(),
            foreground: Rgb::from_hex(color),
            ..Cell::default()
        }
    }

    #[allow(
        clippy::unwrap_used,
        reason = "Literal fixture grid dimensions are known nonzero."
    )]
    fn cache_frame() -> TerminalFrame {
        TerminalFrame {
            size: GridSize::new(3, 2).unwrap(),
            rows: vec![
                Arc::new(vec![cell("a", 1), cell("b", 1), cell("c", 1)]),
                Arc::new(vec![cell("1", 2), cell("2", 2), cell("3", 2)]),
            ],
            cursor: None,
            foreground: Rgb::from_hex(1),
            background: Rgb::default(),
        }
    }

    fn prepared_marker(frame: &TerminalFrame, row: usize) -> PreparedRow {
        PreparedRow {
            backgrounds: vec![CellSpan {
                column: 0,
                columns: 1,
                color: frame.rows[row][0].foreground,
            }],
            ..PreparedRow::default()
        }
    }

    fn prepare_without_window(
        cache: &TerminalSurfaceCache,
        frame: Arc<TerminalFrame>,
        metrics: TerminalMetrics,
    ) -> (TerminalSurfaceCache, Vec<usize>) {
        let mut pending = TerminalSurfacePreparation::new(cache, frame, metrics);
        let mut prepared = Vec::new();
        while !pending.advance_with(|frame, row, _, _| {
            prepared.push(row);
            prepared_marker(frame, row)
        }) {}
        (pending.finish().0, prepared)
    }

    #[test]
    fn scrolling_reuses_moved_rows_even_after_their_old_index_is_overwritten() {
        let metrics = Theme::default().terminal;
        let frame = Arc::new(cache_frame());
        let (initial, _) = prepare_without_window(
            &TerminalSurfaceCache::default(),
            Arc::clone(&frame),
            metrics,
        );
        let mut shifted = (*frame).clone();
        shifted.rows = vec![Arc::new(vec![cell("n", 3); 3]), Arc::clone(&frame.rows[0])];
        let (scrolled, changed) = prepare_without_window(&initial, Arc::new(shifted), metrics);
        assert_eq!(changed, [0]);
        assert!(Arc::ptr_eq(&scrolled.prepared[1], &initial.prepared[0]));
        assert_eq!(
            scrolled.prepared[1].backgrounds[0].color,
            frame.rows[0][0].foreground
        );
        assert_eq!(
            scrolled.stats().prepared_rows - initial.stats().prepared_rows,
            1
        );
        assert_eq!(
            scrolled.stats().reused_rows - initial.stats().reused_rows,
            1
        );

        // The cache retains only the latest viewport, so a row that left it
        // must be prepared when brought back on a subsequent scroll.
        let (returned, changed) = prepare_without_window(&scrolled, frame, metrics);
        assert_eq!(changed, [1]);
        assert!(Arc::ptr_eq(&returned.prepared[0], &scrolled.prepared[1]));
        assert!(!Arc::ptr_eq(&returned.prepared[1], &initial.prepared[1]));
    }

    #[test]
    fn moved_row_reuse_does_not_bypass_font_or_background_invalidation() {
        let metrics = Theme::default().terminal;
        let frame = Arc::new(cache_frame());
        let (initial, _) = prepare_without_window(
            &TerminalSurfaceCache::default(),
            Arc::clone(&frame),
            metrics,
        );
        let mut shifted = (*frame).clone();
        shifted.rows.swap(0, 1);
        let mut changed_metrics = metrics;
        changed_metrics.font_size += 1.;
        let (font_changed, changed) =
            prepare_without_window(&initial, Arc::new(shifted.clone()), changed_metrics);
        assert_eq!(changed, [0, 1]);
        assert!(!Arc::ptr_eq(
            &font_changed.prepared[0],
            &initial.prepared[1]
        ));
        shifted.background = Rgb::from_hex(0x0012_3456);
        let (background_changed, changed) =
            prepare_without_window(&initial, Arc::new(shifted), metrics);
        assert_eq!(changed, [0, 1]);
        assert!(!Arc::ptr_eq(
            &background_changed.prepared[0],
            &initial.prepared[1]
        ));
    }

    #[test]
    fn cursor_only_frames_keep_prepared_rows_and_one_changed_row_rebuilds_only_it() {
        let mut cache = TerminalSurfaceCache::default();
        let mut frame = cache_frame();
        let key = MetricsKey::from(Theme::default().terminal);
        let initial = cache.update_rows(&frame, key, |row, _| prepared_marker(&frame, row));
        frame.cursor = Some(Cursor {
            column: 1,
            row: 0,
            ..Cursor::default()
        });
        let mut prepared = Vec::new();
        let cursor_only = cache.update_rows(&frame, key, |row, _| {
            prepared.push(row);
            prepared_marker(&frame, row)
        });
        assert!(Arc::ptr_eq(&initial, &cursor_only));
        assert_eq!(prepared, [] as [usize; 0]);
        Arc::make_mut(&mut frame.rows[1])[0].foreground = Rgb::from_hex(3);
        let changed = cache.update_rows(&frame, key, |row, _| {
            prepared.push(row);
            prepared_marker(&frame, row)
        });
        assert_eq!(prepared, [1]);
        assert!(Arc::ptr_eq(&initial[0], &changed[0]));
        assert!(!Arc::ptr_eq(&initial[1], &changed[1]));
        assert_eq!(changed[1].backgrounds[0].color, Rgb::from_hex(3));
        assert_eq!(cache.stats().prepared_rows, 3);
    }

    #[test]
    fn font_geometry_and_default_background_changes_invalidate_prepared_rows() {
        let mut cache = TerminalSurfaceCache::default();
        let mut frame = cache_frame();
        let mut metrics = Theme::default().terminal;
        let initial = cache.update_rows(&frame, metrics.into(), |row, _| {
            prepared_marker(&frame, row)
        });
        metrics.font_size += 1.;
        let resized = cache.update_rows(&frame, metrics.into(), |row, _| {
            prepared_marker(&frame, row)
        });
        assert!(!Arc::ptr_eq(&initial[0], &resized[0]));
        assert!(!Arc::ptr_eq(&initial[1], &resized[1]));
        frame.background = Rgb::from_hex(0x0012_3456);
        let recolored = cache.update_rows(&frame, metrics.into(), |row, _| {
            prepared_marker(&frame, row)
        });
        assert!(!Arc::ptr_eq(&resized[0], &recolored[0]));
        assert!(!Arc::ptr_eq(&resized[1], &recolored[1]));
        assert_eq!(cache.stats().prepared_rows, 6);
    }

    #[test]
    #[allow(clippy::unwrap_used, reason = "Literal fixture dimensions are valid.")]
    fn pending_preparation_keeps_complete_screen_until_bounded_work_finishes() {
        let mut frame = cache_frame();
        frame.rows = (0..131).map(|_| Arc::new(vec![cell("x", 1); 3])).collect();
        frame.size = GridSize::new(3, 131).unwrap();
        let frame = Arc::new(frame);
        let cache = TerminalSurfaceCache::default();
        let mut pending =
            TerminalSurfacePreparation::new(&cache, Arc::clone(&frame), Theme::default().terminal);
        assert!(!pending.advance_with(|frame, row, _, _| prepared_marker(frame, row)));
        assert!(pending.next_row <= 64);
        assert!(
            cache.prepared.is_empty(),
            "pending rows never enter the displayed cache"
        );
        while !pending.advance_with(|frame, row, _, _| prepared_marker(frame, row)) {}
        let (completed, displayed) = pending.finish();
        assert_eq!(completed.prepared.len(), 131);
        assert!(Arc::ptr_eq(&displayed, &frame));
    }

    #[test]
    #[allow(clippy::unwrap_used, reason = "Literal fixture dimensions are valid.")]
    fn preparation_invalidates_size_and_font_changes_but_keeps_progress_on_new_content() {
        let frame = Arc::new(cache_frame());
        let metrics = Theme::default().terminal;
        let pending = TerminalSurfacePreparation::new(
            &TerminalSurfaceCache::default(),
            Arc::clone(&frame),
            metrics,
        );
        let mut newer = (*frame).clone();
        Arc::make_mut(&mut newer.rows[0])[0] = cell("z", 2);
        assert!(pending.compatible(&newer, metrics));
        assert_eq!(
            pending.frame.rows[0][0].text, "a",
            "new input coalesces without resetting work"
        );
        newer.size = GridSize::new(2, 3).unwrap();
        assert!(!pending.compatible(&newer, metrics));
        let mut changed = metrics;
        changed.font_size += 1.;
        assert!(!pending.compatible(&frame, changed));
    }

    #[test]
    fn cluster_ends_handle_reordered_and_repeated_grapheme_glyphs() {
        assert_eq!(
            cluster_ends(7, [4, 0, 1, 1, 7].into_iter()),
            [1, 4, 4, 4, 7, 7, 7]
        );
        assert_eq!(cluster_ends(0, [0].into_iter()), [] as [usize; 0]);
    }

    #[test]
    fn recoloring_reuses_geometry_but_font_and_grapheme_changes_do_not() {
        let previous = vec![cell("e\u{301}", 1), cell("x", 1)];
        let mut current = previous.clone();
        current[0].foreground = Rgb::from_hex(2);
        current[0].background = Rgb::from_hex(3);
        current[0].style.inverse = true;
        current[0].style.underline = true;
        current[0].style.underline_kind = UnderlineKind::Double;
        current[0].style.underline_color = Some(Rgb::from_hex(7));
        current[0].style.faint = true;
        current[0].style.blink = true;
        current[0].style.strikethrough = true;
        current[0].style.overline = true;
        assert!(shaping_matches(&previous, &current));
        current[0].style.bold = true;
        assert!(!shaping_matches(&previous, &current));
        current[0].style.bold = false;
        current[0].style.invisible = true;
        assert!(!shaping_matches(&previous, &current));
        current[0].style.invisible = false;
        current[0].text = "a".into();
        assert!(!shaping_matches(&previous, &current));
        assert!(!shaping_matches(&previous, &current[..1]));
    }

    #[test]
    fn viewport_culling_preserves_glyph_overhang_and_unknown_font_bounds() {
        let font_bounds = Bounds::new(point(px(-5.), px(-10.)), size(px(10.), px(14.)));
        let overhang = horizontal_ink(px(11.), font_bounds);
        assert!(ink_overlaps(overhang, px(0.), px(10.)));
        let distant = horizontal_ink(px(40.), font_bounds);
        assert!(!ink_overlaps(distant, px(0.), px(10.)));
        let edge = horizontal_ink(
            px(10.),
            Bounds::new(point(px(0.), px(0.)), size(px(1.), px(1.))),
        );
        assert!(ink_overlaps(edge, px(0.), px(10.)));
        let unknown = horizontal_ink(px(40.), Bounds::default());
        assert!(unknown.is_none());
        assert!(ink_overlaps(unknown, px(0.), px(10.)));
    }

    #[test]
    fn row_batch_preserves_wide_and_combining_byte_to_cell_positions() {
        let cells = vec![
            cell("a", 1),
            Cell {
                width: CellWidth::Wide,
                ..cell("界", 2)
            },
            Cell {
                width: CellWidth::Continuation,
                ..Cell::default()
            },
            cell("e\u{301}", 3),
            cell("z", 4),
            Cell::default(),
        ];
        let row = build_row_text(&cells, 6, Theme::default().terminal);
        assert_eq!(row.text, "a界e\u{301}z");
        assert_eq!(row.columns_by_byte, vec![0, 1, 1, 1, 3, 3, 3, 4]);
        assert_eq!(
            row.font_runs.len(),
            1,
            "color boundaries must not split shaping"
        );
        assert_eq!(row.font_runs[0].len, row.text.len());
    }

    #[test]
    fn font_boundaries_preserve_shaping_and_mixed_color_glyph_clips() {
        let mut cells = vec![cell("f", 1), cell("i", 2), cell("x", 2)];
        cells[2].style = CellStyle {
            bold: true,
            ..CellStyle::default()
        };
        let row = build_row_text(&cells, 3, Theme::default().terminal);
        assert_eq!(row.text, "fix");
        assert_eq!(row.font_runs.len(), 2);
        assert_eq!(
            row.font_runs.iter().map(|run| run.len).collect::<Vec<_>>(),
            vec![2, 1]
        );
        let spans = foreground_spans(&cells, 0, 2);
        assert_eq!(
            spans,
            vec![
                CellSpan {
                    column: 0,
                    columns: 1,
                    color: Rgb::from_hex(1)
                },
                CellSpan {
                    column: 1,
                    columns: 1,
                    color: Rgb::from_hex(2)
                }
            ]
        );
    }

    #[test]
    fn invisible_wide_cells_reserve_columns_and_inversion_keeps_background() {
        let mut wide = cell("界", 1);
        wide.width = CellWidth::Wide;
        wide.style.invisible = true;
        let cells = vec![
            wide,
            Cell {
                width: CellWidth::Continuation,
                ..Cell::default()
            },
            cell("x", 2),
        ];
        let row = build_row_text(&cells, 3, Theme::default().terminal);
        assert_eq!(row.text, "  x");
        assert_eq!(row.columns_by_byte, vec![0, 1, 2]);
        let mut inverted = cell(" ", 1);
        inverted.style.inverse = true;
        assert_eq!(cell_colors(&inverted), (Rgb::default(), Rgb::from_hex(1)));
        assert_eq!(clipped_columns(3, 2, 4), Some((3, 1)));
        assert_eq!(clipped_columns(usize::MAX, 1, 4), None);
    }

    #[test]
    fn decoration_preparation_batches_wide_cells_and_suppresses_invisible_ink() {
        let mut wide = cell("界", 0x00ff_ffff);
        wide.width = CellWidth::Wide;
        wide.background = Rgb::from_hex(0x0020_2020);
        wide.style = CellStyle {
            underline: true,
            underline_kind: UnderlineKind::Double,
            underline_color: Some(Rgb::from_hex(0x00ff_0000)),
            faint: true,
            blink: true,
            strikethrough: true,
            overline: true,
            ..CellStyle::default()
        };
        let mut hidden = wide.clone();
        hidden.style.invisible = true;
        hidden.width = CellWidth::Narrow;
        let cells = vec![
            wide,
            Cell {
                width: CellWidth::Continuation,
                ..Cell::default()
            },
            hidden,
        ];
        let row = prepare_styles(&cells, 3, Rgb::default());
        assert_eq!(row.decorations.len(), 3);
        assert!(row.blinking);
        assert!(
            row.decorations
                .iter()
                .all(|span| span.span.column == 0 && span.span.columns == 2 && span.blink)
        );
        assert_eq!(row.decorations[0].span.color, Rgb::from_hex(0x0090_1010));
        assert_eq!(row.decorations[1].span.color, Rgb::from_hex(0x0090_9090));
        assert_eq!(row.backgrounds.len(), 1);
        assert_eq!(row.backgrounds[0].columns, 3);
        let clipped = prepare_styles(&cells, 1, Rgb::default());
        assert!(
            clipped
                .decorations
                .iter()
                .all(|span| span.span.columns == 1)
        );
    }

    #[test]
    fn ligature_paint_clips_split_blink_faint_and_wide_tail_styles_without_reshaping() {
        let mut cells = vec![cell("f", 0x00ff_ffff), cell("i", 0x00ff_ffff)];
        let previous = cells.clone();
        cells[1].style.blink = true;
        cells[1].style.faint = true;
        let spans: Vec<_> = glyph_paint_spans(&cells, 0, 2).collect();
        assert_eq!(spans.len(), 2);
        assert!(!spans[0].1.blink);
        assert!(spans[1].1.blink);
        assert_eq!(spans[1].1.color, Rgb::from_hex(0x0080_8080));
        assert!(shaping_matches(&previous, &cells));
        cells[0].width = CellWidth::Wide;
        cells[0].style.blink = true;
        cells[1].width = CellWidth::Continuation;
        let spans: Vec<_> = glyph_paint_spans(&cells, 0, 2).collect();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0.columns, 2);
    }

    #[test]
    fn blink_metadata_uses_prepared_rows_and_ignores_invisible_or_blank_ink() {
        let mut visible = cell("x", 1);
        visible.style.blink = true;
        let mut hidden = visible.clone();
        hidden.style.invisible = true;
        let mut blank = visible.clone();
        blank.text = " ".into();
        assert!(!prepare_styles(&[hidden, blank], 2, Rgb::default()).blinking);
        let cache = TerminalSurfaceCache {
            prepared: Arc::new(vec![Arc::new(prepare_styles(
                &[visible],
                1,
                Rgb::default(),
            ))]),
            ..TerminalSurfaceCache::default()
        };
        assert!(cache.has_blinking_text());
        let phase = TerminalPaintPhase {
            text_visible: false,
            ..TerminalPaintPhase::default()
        };
        assert!(!phase.shows_text(true));
        assert!(phase.shows_text(false));
    }
}
