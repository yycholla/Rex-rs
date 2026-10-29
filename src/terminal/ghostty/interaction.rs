//! Native owner-thread search and selection. Native tick/feed/format each remain
//! single synchronous FFI calls; incremental scheduling is not a wall-clock bound.
use super::GhosttyTerminal;
use crate::core::{
    GridSize,
    interaction::{
        CellSpan, MAX_COPY_BYTES, MAX_SEARCH_QUERY_BYTES, SearchCommand, SearchState,
        SelectionCommand, TerminalAnnotations,
    },
};
use anyhow::{Context, Result, ensure};
use libghostty_vt::{
    Terminal,
    render::RowId,
    screen::{Screen, TrackedGridRef},
    search::{MatchBuffer, Search, Status},
    selection::{
        FormatOptions, Selection,
        gesture::{
            Autoscroll, AutoscrollTickEvent, DragEvent, Geometry, Gesture, PressEvent, ReleaseEvent,
        },
    },
    terminal::{Point, PointCoordinate, PointSpace},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

// Shared across replacement terminals: stale UI tokens cannot accidentally match.
static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);
const ANNOTATION_INTERVAL: Duration = Duration::from_millis(16);
const AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(30);
const MAX_COPY_CELLS: u64 = 262_144;
const CELL_PIXELS: u32 = 32;

fn next_revision() -> Result<u64> {
    let mut current = NEXT_REVISION.load(Ordering::Relaxed);
    loop {
        let next = current
            .checked_add(1)
            .context("terminal annotation generation exhausted")?;
        match NEXT_REVISION.compare_exchange_weak(
            current,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(current),
            Err(observed) => current = observed,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Visibility {
    Visible,
    Hidden,
}

pub(super) struct Interaction {
    search: Option<Search<'static, 'static>>,
    query: String,
    generation: u64,
    feed_pending: bool,
    visibility: Visibility,
    #[cfg(test)]
    highlight_extractions: u64,
    presentation_pending: bool,
    next_publication: Instant,
    match_buffer: MatchBuffer,
    gesture: Gesture<'static>,
    press: PressEvent<'static>,
    drag: DragEvent<'static>,
    release: ReleaseEvent<'static>,
    autoscroll: AutoscrollTickEvent<'static>,
    dragging: bool,
    pointer: (f64, f64, bool),
    pointer_column: u16,
    next_autoscroll: Instant,
    viewport_origin: Option<TrackedGridRef>,
    viewport_screen: Option<Screen>,
    viewport_rows: Vec<RowId>,
    viewport_size: Option<GridSize>,
    annotations: Arc<TerminalAnnotations>,
}

impl Interaction {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            search: None,
            query: String::new(),
            generation: 0,
            feed_pending: false,
            visibility: Visibility::Visible,
            #[cfg(test)]
            highlight_extractions: 0,
            presentation_pending: true,
            next_publication: Instant::now(),
            match_buffer: MatchBuffer::new(),
            gesture: Gesture::new()?,
            press: PressEvent::new()?,
            drag: DragEvent::new()?,
            release: ReleaseEvent::new()?,
            autoscroll: AutoscrollTickEvent::new()?,
            dragging: false,
            pointer: (0., 0., false),
            pointer_column: 0,
            next_autoscroll: Instant::now(),
            viewport_origin: None,
            viewport_screen: None,
            viewport_rows: Vec::new(),
            viewport_size: None,
            annotations: Arc::new(TerminalAnnotations::default()),
        })
    }

    pub(super) fn mutated(&mut self) {
        self.feed_pending = true;
        self.presentation_pending = true;
    }

    pub(super) fn interacted(&mut self) {
        self.mutated();
        self.next_publication = Instant::now();
    }

    pub(super) fn reset_gesture(&mut self, terminal: &Terminal<'_, '_>) {
        self.gesture.reset(terminal);
        self.dragging = false;
    }

    pub(super) fn before_drop(&mut self, terminal: &Terminal<'_, '_>) {
        self.reset_gesture(terminal);
        self.search = None;
        self.viewport_origin = None;
    }

    pub(super) fn resized(&mut self, terminal: &Terminal<'_, '_>) {
        self.reset_gesture(terminal);
        self.viewport_rows.clear();
        self.viewport_size = None;
        self.viewport_origin = None;
        self.interacted();
    }
}

impl GhosttyTerminal {
    /// Limit hidden replicas to search progress/counts and viewport identity.
    /// Revealing restores complete visible spans even without new VT output.
    pub fn set_interaction_visible(&mut self, visible: bool) {
        let visibility = if visible {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
        if self.interaction.visibility != visibility {
            if visibility == Visibility::Hidden {
                self.interaction.reset_gesture(&self.terminal);
            }
            self.interaction.visibility = visibility;
            self.interaction.presentation_pending = true;
            self.interaction.next_publication = Instant::now();
        }
    }

    #[cfg(test)]
    pub(crate) fn interaction_highlight_extractions(&self) -> u64 {
        self.interaction.highlight_extractions
    }

    /// Update a native, incremental history search. ASCII matching ignores case.
    ///
    /// # Errors
    /// Rejects oversized queries and reports native allocation/search failures.
    pub fn search(&mut self, command: SearchCommand) -> Result<()> {
        let state = &mut self.interaction;
        match command {
            SearchCommand::Set { query, generation } => {
                ensure!(
                    query.len() <= MAX_SEARCH_QUERY_BYTES,
                    "terminal search query exceeds1024 bytes"
                );
                if generation < state.generation {
                    return Ok(());
                }
                if state.search.is_none() {
                    state.search = Some(Search::new(&mut self.terminal)?);
                }
                if let Some(search) = &mut state.search {
                    search.set_needle(&mut self.terminal, query.as_bytes())?;
                }
                state.generation = generation;
                state.query = query;
                state.feed_pending = true;
            }
            SearchCommand::Cancel { generation } => {
                if generation < state.generation {
                    return Ok(());
                }
                state.search = None;
                state.query.clear();
                state.generation = generation;
                state.feed_pending = false;
            }
            SearchCommand::Next { generation } | SearchCommand::Previous { generation } => {
                if generation != state.generation {
                    return Ok(());
                }
                if let Some(search) = &mut state.search {
                    if matches!(command, SearchCommand::Next { .. }) {
                        search.select_next(&mut self.terminal)?;
                    } else {
                        search.select_prev(&mut self.terminal)?;
                    }
                    state.feed_pending = true;
                }
            }
        }
        state.presentation_pending = true;
        state.next_publication = Instant::now();
        Ok(())
    }

    /// Apply an owner-thread native selection gesture against its displayed viewport.
    /// Cancel stops dragging while preserving selection; Clear removes it too.
    ///
    /// # Errors
    /// Rejects stale viewport events, nonfinite coordinates and native failures.
    pub fn select(&mut self, command: SelectionCommand) -> Result<()> {
        // An accepted pointer event may reach the worker after a tab/zoom hides
        // its pane. It must not restart a gesture against an invisible grid.
        if self.interaction.visibility == Visibility::Hidden
            && matches!(
                command,
                SelectionCommand::Press { .. } | SelectionCommand::Drag { .. }
            )
        {
            self.interaction.reset_gesture(&self.terminal);
            return Ok(());
        }

        if let SelectionCommand::Press { viewport_token, .. }
        | SelectionCommand::Drag { viewport_token, .. } = command
        {
            self.update_viewport()?;
            if viewport_token != self.interaction.annotations.viewport_token {
                self.interaction.reset_gesture(&self.terminal);
                anyhow::bail!("terminal selection viewport changed");
            }
        }
        match command {
            SelectionCommand::Press {
                column,
                row,
                time_millis,
                ..
            } => self.selection_press(column, row, time_millis)?,
            SelectionCommand::Drag {
                column,
                row,
                rectangle,
                ..
            } => self.selection_drag(column, row, rectangle)?,
            SelectionCommand::Release => {
                self.interaction.release.apply(
                    &mut self.interaction.gesture,
                    &self.terminal,
                    None,
                )?;
                self.interaction.dragging = false;
            }
            SelectionCommand::Cancel => self.interaction.reset_gesture(&self.terminal),
            SelectionCommand::Clear => {
                self.interaction.reset_gesture(&self.terminal);
                self.terminal.set_selection(None)?;
            }
        }
        self.interaction.presentation_pending = true;
        self.interaction.next_publication = Instant::now();
        Ok(())
    }

    fn selection_press(&mut self, column: f32, row: f32, time_millis: u64) -> Result<()> {
        let point = pointer_point(column, row, self.size.columns(), self.size.rows())?;
        let state = &mut self.interaction;
        state
            .press
            .set_position(
                f64::from(column) * f64::from(CELL_PIXELS),
                f64::from(row) * f64::from(CELL_PIXELS),
            )?
            .set_time(Duration::from_millis(time_millis))?
            .set_repeat_interval(Duration::from_millis(400))?
            .set_repeat_distance(f64::from(CELL_PIXELS) / 2.)?;
        let location = self.terminal.grid_ref(Point::Viewport(point))?;
        let selection = state
            .press
            .apply(&mut state.gesture, &self.terminal, location)?;
        self.terminal.set_selection(selection.as_ref())?;
        state.dragging = true;
        state.pointer_column = point.x;
        state.pointer = (
            f64::from(column) * f64::from(CELL_PIXELS),
            f64::from(row) * f64::from(CELL_PIXELS),
            false,
        );
        state.next_autoscroll = Instant::now() + AUTOSCROLL_INTERVAL;
        Ok(())
    }

    fn selection_drag(&mut self, column: f32, row: f32, rectangle: bool) -> Result<()> {
        let point = pointer_point(column, row, self.size.columns(), self.size.rows())?;
        let geometry = self.selection_geometry();
        let state = &mut self.interaction;
        if !state.dragging {
            return Ok(());
        }
        state.pointer_column = point.x;
        state.pointer = (
            f64::from(column) * f64::from(CELL_PIXELS),
            f64::from(row) * f64::from(CELL_PIXELS),
            rectangle,
        );
        state
            .drag
            .set_position(state.pointer.0, state.pointer.1)?
            .set_rectangle(rectangle)?;
        let location = self.terminal.grid_ref(Point::Viewport(point))?;
        if let Some(selection) =
            state
                .drag
                .apply(&mut state.gesture, &self.terminal, location, geometry)?
        {
            self.terminal.set_selection(Some(&selection))?;
        }
        Ok(())
    }

    fn selection_geometry(&self) -> Geometry {
        Geometry {
            columns: u32::from(self.size.columns()),
            cell_width: CELL_PIXELS,
            padding_left: 0,
            screen_height: u32::from(self.size.rows()) * CELL_PIXELS,
        }
    }

    /// Advance one native search tick and at most one timed drag-autoscroll step.
    /// A native feed/tick is incremental, but has no strict per-call time bound.
    ///
    /// # Errors
    /// Reports native search, gesture or presentation extraction failures.
    pub fn interaction_tick(&mut self) -> Result<bool> {
        let before = self.interaction.annotations.revision;
        let geometry = self.selection_geometry();
        let state = &mut self.interaction;
        if let Some(search) = &mut state.search {
            state.presentation_pending |=
                state.feed_pending || search.status()? != Status::Complete;
            if state.feed_pending || search.status()? == Status::FeedRequired {
                search.feed(&mut self.terminal)?;
                state.feed_pending = false;
            }
            if search.status()? != Status::Complete {
                search.tick()?;
            }
        }
        if state.dragging && Instant::now() >= state.next_autoscroll {
            state.next_autoscroll = Instant::now() + AUTOSCROLL_INTERVAL;
            if state.gesture.autoscroll(&self.terminal)? != Autoscroll::None {
                state
                    .autoscroll
                    .set_position(state.pointer.0, state.pointer.1)?
                    .set_rectangle(state.pointer.2)?;
                // Native event scrolls exactly one row and resolves its viewport point afterwards.
                let viewport = PointCoordinate {
                    x: state.pointer_column,
                    y: if state.pointer.1 < 0. {
                        0
                    } else {
                        u32::from(self.size.rows() - 1)
                    },
                };
                if let Some(selection) = state.autoscroll.apply(
                    &mut state.gesture,
                    &self.terminal,
                    viewport,
                    geometry,
                )? {
                    self.terminal.set_selection(Some(&selection))?;
                }
                state.feed_pending = true;
                state.presentation_pending = true;
            }
        }
        let after = if self.interaction.presentation_pending
            && Instant::now() >= self.interaction.next_publication
        {
            self.annotations()?.revision
        } else {
            self.interaction.annotations.revision
        };
        Ok(before != after)
    }

    fn update_viewport(&mut self) -> Result<()> {
        // Read native row identity only. Hidden search progress must never copy
        // glyphs or defeat snapshot_shared's visible-frame materialization gate.
        let snapshot = self.render_state.update(&self.terminal)?;
        let mut iterator = self.row_iterator.update(&snapshot)?;
        let mut row_ids = Vec::with_capacity(usize::from(self.size.rows()));
        while let Some(row) = iterator.next() {
            row_ids.push(row.id()?);
        }
        let screen = self.terminal.active_screen()?;
        let state = &mut self.interaction;
        let origin_same = state
            .viewport_origin
            .as_ref()
            .map(|origin| origin.point(PointSpace::Viewport))
            .transpose()?
            .flatten()
            .is_some_and(|point| point.x == 0 && point.y == 0);
        if state
            .viewport_screen
            .is_some_and(|previous| previous != screen)
        {
            state.reset_gesture(&self.terminal);
        }
        if !origin_same
            || state.viewport_rows != row_ids
            || state.viewport_size != Some(self.size)
            || state.viewport_screen != Some(screen)
        {
            state.viewport_origin = Some(
                self.terminal
                    .track_grid_ref(Point::Viewport(PointCoordinate { x: 0, y: 0 }))?,
            );
            state.viewport_screen = Some(screen);
            state.viewport_rows = row_ids;
            state.viewport_size = Some(self.size);
            let mut annotations = (*state.annotations).clone();
            annotations.viewport_token = next_revision()?;
            annotations.revision = next_revision()?;
            state.annotations = Arc::new(annotations);
        }
        Ok(())
    }

    /// Return owned viewport annotations, retaining Arc identity when unchanged.
    ///
    /// # Errors
    /// Reports native rendering/search extraction failures.
    pub fn annotations(&mut self) -> Result<Arc<TerminalAnnotations>> {
        if !self.interaction.presentation_pending {
            return Ok(Arc::clone(&self.interaction.annotations));
        }
        self.update_viewport()?;
        let mut selection = Vec::new();
        if self.interaction.visibility == Visibility::Visible {
            #[cfg(test)]
            {
                self.interaction.highlight_extractions =
                    self.interaction.highlight_extractions.saturating_add(1);
            }
            let snapshot = self.render_state.update(&self.terminal)?;
            let mut rows = self.row_iterator.update(&snapshot)?;
            let mut row_index = 0;
            while let Some(row) = rows.next() {
                if let Some(range) = row.selection()? {
                    selection.push(CellSpan {
                        row: row_index,
                        start: range.start_x,
                        end: range.end_x.saturating_add(1).min(self.size.columns()),
                    });
                }
                row_index += 1;
            }
        }
        let search = self.search_annotations()?;
        self.interaction.presentation_pending = false;
        self.interaction.next_publication = Instant::now() + ANNOTATION_INTERVAL;
        let previous = &self.interaction.annotations;
        if previous.selection == selection && previous.search == search {
            return Ok(Arc::clone(previous));
        }
        let annotations = Arc::new(TerminalAnnotations {
            revision: next_revision()?,
            viewport_token: previous.viewport_token,
            selection,
            search,
        });
        self.interaction.annotations = Arc::clone(&annotations);
        Ok(annotations)
    }

    fn search_annotations(&mut self) -> Result<Option<SearchState>> {
        let state = &mut self.interaction;
        let Some(search) = &mut state.search else {
            return Ok(None);
        };
        if state.visibility == Visibility::Hidden {
            if state.feed_pending || search.status()? == Status::FeedRequired {
                search.feed(&mut self.terminal)?;
                state.feed_pending = false;
            }
            return Ok(Some(SearchState {
                generation: state.generation,
                query: state.query.clone(),
                complete: search.status()? == Status::Complete,
                total: u64::try_from(search.total_matches()?)?,
                selected: search.selected_index()?.map(u64::try_from).transpose()?,
                matches: Vec::new(),
                selected_match: Vec::new(),
            }));
        }
        let snapshot = search.snapshot(&mut self.terminal)?;
        let terminal = snapshot.terminal();
        let viewport = terminal.grid_ref(Point::Viewport(PointCoordinate { x: 0, y: 0 }))?;
        let origin_y = terminal
            .point_from_grid_ref(&viewport, PointSpace::Screen)?
            .context("viewport origin missing")?
            .y;
        let mut highlights =
            vec![false; usize::from(self.size.columns()) * usize::from(self.size.rows())];
        for selection in snapshot.viewport_matches(&mut state.match_buffer)? {
            mark_selection(
                terminal,
                &selection,
                self.size.columns(),
                self.size.rows(),
                origin_y,
                &mut highlights,
            )?;
        }
        let mut selected_highlight = vec![false; highlights.len()];
        if let Some(selection) = snapshot.selected_match()? {
            mark_selection(
                terminal,
                &selection,
                self.size.columns(),
                self.size.rows(),
                origin_y,
                &mut selected_highlight,
            )?;
        }
        // Snapshot::snapshot itself feeds. Read counts afterwards so progress
        // describes the same native state used for these highlights.
        state.feed_pending = false;
        let complete = search.status()? == Status::Complete;
        let total = u64::try_from(search.total_matches()?)?;
        let selected = search.selected_index()?.map(u64::try_from).transpose()?;
        Ok(Some(SearchState {
            generation: state.generation,
            query: state.query.clone(),
            complete,
            total,
            selected,
            matches: mask_spans(&highlights, self.size.columns()),
            selected_match: mask_spans(&selected_highlight, self.size.columns()),
        }))
    }

    /// Copy the native persistent selection with soft wraps unwrapped and whitespace trimmed.
    ///
    /// # Errors
    /// Rejects selections spanning over262144 cells or4MiB, without truncation.
    pub fn copy_selection(&self) -> Result<Option<String>> {
        let Some(selection) = self.terminal.selection()? else {
            return Ok(None);
        };
        let a = self
            .terminal
            .point_from_grid_ref(&selection.start(), PointSpace::Screen)?
            .context("selection start no longer exists")?;
        let b = self
            .terminal
            .point_from_grid_ref(&selection.end(), PointSpace::Screen)?
            .context("selection end no longer exists")?;
        let cells = (u64::from(a.y.abs_diff(b.y)) + 1) * u64::from(self.size.columns());
        ensure!(
            cells <= MAX_COPY_CELLS,
            "terminal selection is too large to copy (262144-cell span limit)"
        );
        let options = FormatOptions::new()
            .with_emit_format(libghostty_vt::fmt::Format::Plain)
            .with_unwrap(true)
            .with_trim(true)
            .with_selection(&selection);
        let mut output = vec![0; MAX_COPY_BYTES];
        let Some(written) = self
            .terminal
            .format_selection_buf(options, &mut output)
            .context("copy terminal selection (maximum4MiB; oversized text is not truncated)")?
        else {
            return Ok(None);
        };
        ensure!(written <= MAX_COPY_BYTES, "terminal selection exceeds4MiB");
        output.truncate(written);
        Ok(Some(
            String::from_utf8(output).context("native selection is not UTF-8")?,
        ))
    }
}

fn pointer_point(column: f32, row: f32, columns: u16, rows: u16) -> Result<PointCoordinate> {
    ensure!(
        column.is_finite() && row.is_finite(),
        "nonfinite terminal selection pointer"
    );
    // Decode the nonnegative integral f32 directly, avoiding lossy float casts.
    let cell = |value: f32, limit: u16| -> Result<u16> {
        let bits = value.clamp(0., f32::from(limit - 1)).to_bits();
        let exponent = i32::try_from((bits >> 23) & 255)? - 127;
        if exponent < 0 {
            return Ok(0);
        }
        let mantissa = (bits & 0x7f_ffff) | (1 << 23);
        Ok(u16::try_from(mantissa >> u32::try_from(23 - exponent)?)?)
    };
    Ok(PointCoordinate {
        x: cell(column, columns)?,
        y: u32::from(cell(row, rows)?),
    })
}

fn mark_selection(
    terminal: &Terminal<'_, '_>,
    selection: &Selection<'_>,
    columns: u16,
    rows: u16,
    origin_y: u32,
    mask: &mut [bool],
) -> Result<()> {
    // Selection::to_ordered invokes two additional history-head traversals.
    // Resolve visible endpoints from the much nearer viewport origin instead,
    // and order their owned coordinates locally. Offscreen/long matches retain
    // the full Screen-space fallback, including both endpoints outside view.
    let start = terminal.point_from_grid_ref(&selection.start(), PointSpace::Viewport)?;
    let end = terminal.point_from_grid_ref(&selection.end(), PointSpace::Viewport)?;
    if let (Some(a), Some(b)) = (start, end) {
        return mark_range(a, b, columns, rows, 0, mask);
    }
    let Some(a) = terminal.point_from_grid_ref(&selection.start(), PointSpace::Screen)? else {
        return Ok(());
    };
    let Some(b) = terminal.point_from_grid_ref(&selection.end(), PointSpace::Screen)? else {
        return Ok(());
    };
    mark_range(a, b, columns, rows, origin_y, mask)
}

fn mark_range(
    mut a: PointCoordinate,
    mut b: PointCoordinate,
    columns: u16,
    rows: u16,
    origin_y: u32,
    mask: &mut [bool],
) -> Result<()> {
    if (a.y, a.x) > (b.y, b.x) {
        std::mem::swap(&mut a, &mut b);
    }
    let bottom = origin_y.saturating_add(u32::from(rows));
    for y in a.y.max(origin_y)..=b.y.min(bottom.saturating_sub(1)) {
        let start = if y == a.y { a.x } else { 0 };
        let end = if y == b.y {
            b.x.saturating_add(1).min(columns)
        } else {
            columns
        };
        let offset = usize::try_from(y - origin_y)? * usize::from(columns);
        if start < end {
            mask[offset + usize::from(start)..offset + usize::from(end)].fill(true);
        }
    }
    Ok(())
}

fn mask_spans(mask: &[bool], columns: u16) -> Vec<CellSpan> {
    let mut spans = Vec::new();
    for (row, cells) in (0u16..u16::MAX).zip(mask.chunks(usize::from(columns))) {
        let mut start = None;
        for (column, selected) in (0u16..columns).zip(cells) {
            if *selected && start.is_none() {
                start = Some(column);
            }
            if !selected && let Some(start) = start.take() {
                spans.push(CellSpan {
                    row,
                    start,
                    end: column,
                });
            }
        }
        if let Some(start) = start {
            spans.push(CellSpan {
                row,
                start,
                end: columns,
            });
        }
    }
    spans
}

#[cfg(test)]
#[path = "interaction_tests.rs"]
mod tests;
