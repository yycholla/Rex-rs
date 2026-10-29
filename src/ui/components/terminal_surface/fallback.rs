//! Targeted styled-glyph fallback; opt-in diagnostics can include terminal contents.
use super::*;
use std::sync::{
    OnceLock,
    atomic::{AtomicUsize, Ordering},
};

static TRACE_FILTER: OnceLock<Option<String>> = OnceLock::new();
static TRACE_ROWS: AtomicUsize = AtomicUsize::new(0);
const MAX_TRACE_ROWS: usize = 256;

/// `REX_RS_GLYPH_TRACE=1` traces nonempty shaped rows; any other nonempty value
/// filters by a text substring. It is disabled by default and capped per process.
pub(super) fn trace(stage: &str, index: usize, row: &RowText, line: &ShapedLine, window: &Window) {
    let Some(filter) = TRACE_FILTER.get_or_init(|| {
        std::env::var("REX_RS_GLYPH_TRACE")
            .ok()
            .filter(|filter| !filter.is_empty())
    }) else {
        return;
    };
    if row.text.is_empty()
        || (stage != "missing-oracle" && filter != "1" && !row.text.contains(filter))
        || TRACE_ROWS.fetch_add(1, Ordering::Relaxed) >= MAX_TRACE_ROWS
    {
        return;
    }
    let ends = cluster_ends(
        row.text.len(),
        line.runs
            .iter()
            .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.index)),
    );
    let mut start = 0;
    let requests: Vec<_> = row.font_runs.iter().map(|run| {
        let range = start..start+run.len;
        start = range.end;
        serde_json::json!({"start":range.start,"end":range.end,"family":run.font.family.as_ref(),"style":format!("{:?}",run.font.style),"weight":format!("{:?}",run.font.weight)})
    }).collect();
    let glyphs: Vec<_> = line.runs.iter().flat_map(|run| {
        let ends = &ends;
        let requested_font = window.text_system().get_font_for_id(run.font_id)
            .map(|font| format!("{font:?}"));
        run.glyphs.iter().map(move |glyph| {
            let end = ends.get(glyph.index).copied().unwrap_or(row.text.len());
            serde_json::json!({"font_id":run.font_id.0,"font_request":requested_font,"glyph_id":format!("{:?}",glyph.id),"start":glyph.index,"end":end,"column":row.columns_by_byte.get(glyph.index),"end_column_start":end.checked_sub(1).and_then(|end|row.columns_by_byte.get(end)),"x":f32::from(glyph.position.x),"y":f32::from(glyph.position.y),"emoji":glyph.is_emoji})
        })
    }).collect();
    eprintln!(
        "terminal-glyph-trace {}",
        serde_json::json!({"stage":stage,"row":index,"text":row.text,"requests":requests,"glyphs":glyphs})
    );
}

use gpui::{FontStyle, WindowTextSystem};
use std::{ops::Range, sync::Weak};
use unicode_segmentation::UnicodeSegmentation;

const NONCHARACTER: char = '\u{10ffff}';

#[derive(Clone, Default)]
pub(super) struct MissingGlyphOracle {
    context: Option<(Weak<WindowTextSystem>, FontId)>,
    missing: Option<GlyphId>,
    probes: u64,
}

impl MissingGlyphOracle {
    pub(super) const fn probes(&self) -> u64 {
        self.probes
    }

    fn resolve(&mut self, metrics: TerminalMetrics, window: &Window) -> Option<GlyphId> {
        let system = window.text_system();
        let request = font(metrics.font_family);
        let font_id = system.resolve_font(&request);
        if self
            .context
            .as_ref()
            .is_some_and(|(previous, previous_font)| {
                *previous_font == font_id
                    && previous
                        .upgrade()
                        .is_some_and(|previous| Arc::ptr_eq(&previous, system))
            })
        {
            return self.missing;
        }
        self.context = Some((Arc::downgrade(system), font_id));
        self.missing = None;
        let text = NONCHARACTER.to_string();
        let run = TextRun {
            len: text.len(),
            font: request,
            color: rgb(0).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let row = RowText {
            text,
            font_runs: vec![run],
            columns_by_byte: vec![0; NONCHARACTER.len_utf8()],
        };
        let line = system.shape_line(
            row.text.clone().into(),
            px(metrics.font_size),
            &row.font_runs,
            None,
        );
        self.probes += 1;
        trace("missing-oracle", 0, &row, &line, window);
        let mut glyphs = line
            .runs
            .iter()
            .flat_map(|run| run.glyphs.iter().map(move |glyph| (run.font_id, glyph)));
        // Pinned cosmic-text shape.rs treats glyph0 as missing; Linux GPUI
        // copies that ID unchanged. A single unsupported noncharacter validates
        // an opaque sentinel without parsing Debug or accessing private fields.
        self.missing = glyphs
            .next()
            .filter(|(resolved_font, glyph)| {
                glyph.index == 0
                    && !glyph.is_emoji
                    && glyphs.next().is_none()
                    && system
                        .advance(*resolved_font, px(metrics.font_size), NONCHARACTER)
                        .is_err()
            })
            .map(|(_, glyph)| glyph.id);
        if self.missing.is_none() && TRACE_FILTER.get().is_some_and(Option::is_some) {
            eprintln!(
                "terminal-glyph-fallback: missing-glyph oracle validation failed; retaining primary shape"
            );
        }
        self.missing
    }
}

#[derive(Clone, Copy)]
pub(super) struct ShapeContext<'a> {
    pub row: &'a RowText,
    pub index: usize,
    pub cells: &'a [Cell],
    pub line: &'a ShapedLine,
    pub columns: u16,
    pub metrics: TerminalMetrics,
    pub baseline: Pixels,
}

pub(super) struct GeometryResult {
    pub geometry: Vec<GlyphGeometry>,
    pub retry_runs: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Cluster {
    bytes: Range<usize>,
    missing: bool,
}

fn clusters(text: &str, glyphs: impl Iterator<Item = (usize, bool)>) -> Vec<Cluster> {
    let mut starts = BTreeMap::new();
    for (start, missing) in glyphs {
        if start < text.len() && text.is_char_boundary(start) {
            *starts.entry(start).or_insert(false) |= missing;
        }
    }
    let starts: Vec<_> = starts.into_iter().collect();
    starts
        .iter()
        .enumerate()
        .map(|(index, (start, missing))| Cluster {
            bytes: *start
                ..starts
                    .get(index + 1)
                    .map_or(text.len(), |(start, _)| *start),
            missing: *missing,
        })
        .collect()
}

fn line_clusters(text: &str, line: &ShapedLine, missing: GlyphId) -> Vec<Cluster> {
    clusters(
        text,
        line.runs.iter().flat_map(|run| {
            run.glyphs
                .iter()
                .map(|glyph| (glyph.index, glyph.id == missing))
        }),
    )
}

fn intersects<T: PartialOrd>(a: &Range<T>, b: &Range<T>) -> bool {
    a.start < b.end && b.start < a.end
}
fn contains<T: PartialOrd>(outer: &Range<T>, inner: &Range<T>) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

fn missing_ranges(text: &str, clusters: &[Cluster], runs: &[TextRun]) -> Vec<Range<usize>> {
    // A safe boundary belongs to both partitions. Snapping to their intersection
    // keeps whole graphemes AND whole shaping clusters without quadratic scans.
    let mut cluster_boundaries = vec![0, text.len()];
    cluster_boundaries.extend(
        clusters
            .iter()
            .flat_map(|cluster| [cluster.bytes.start, cluster.bytes.end]),
    );
    cluster_boundaries.sort_unstable();
    cluster_boundaries.dedup();
    let mut safe: Vec<_> = text
        .grapheme_indices(true)
        .map(|(start, _)| start)
        .filter(|start| cluster_boundaries.binary_search(start).is_ok())
        .collect();
    safe.push(text.len());
    let mut start = 0;
    let styles: Vec<_> = runs
        .iter()
        .map(|run| {
            let range = start..start + run.len;
            start = range.end;
            (range, run.font.style != FontStyle::Normal)
        })
        .collect();
    let mut targets = Vec::<Range<usize>>::new();
    for cluster in clusters.iter().filter(|cluster| cluster.missing) {
        let first = safe
            .partition_point(|boundary| *boundary <= cluster.bytes.start)
            .saturating_sub(1);
        let end = safe.partition_point(|boundary| *boundary < cluster.bytes.end);
        let target = safe[first]..safe[end];
        let first_style = styles.partition_point(|(range, _)| range.end <= target.start);
        if styles[first_style..]
            .iter()
            .take_while(|(range, _)| range.start < target.end)
            .any(|(_, slanted)| *slanted)
        {
            targets.push(target);
        }
    }
    targets.sort_by_key(|range| range.start);
    let mut merged = Vec::<Range<usize>>::new();
    for range in targets {
        if let Some(previous) = merged.last_mut()
            && intersects(previous, &range)
        {
            previous.end = previous.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    merged
}

fn normal_runs(runs: &[TextRun], targets: &[Range<usize>]) -> Vec<TextRun> {
    let mut result = Vec::<TextRun>::new();
    let mut start = 0;
    for run in runs {
        let end = start + run.len;
        let mut boundaries = vec![start, end];
        let first = targets.partition_point(|target| target.end <= start);
        for target in targets[first..]
            .iter()
            .take_while(|target| target.start < end)
        {
            if intersects(&(start..end), target) {
                boundaries.extend([target.start.max(start), target.end.min(end)]);
            }
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        for range in boundaries.windows(2) {
            let mut segment = run.clone();
            segment.len = range[1] - range[0];
            let target = targets.partition_point(|target| target.end <= range[0]);
            if targets
                .get(target)
                .is_some_and(|target| contains(target, &(range[0]..range[1])))
            {
                segment.font.style = FontStyle::Normal;
            }
            if let Some(previous) = result.last_mut() {
                let mut comparable = segment.clone();
                comparable.len = previous.len;
                if comparable == *previous {
                    previous.len += segment.len;
                    continue;
                }
            }
            result.push(segment);
        }
        start = end;
    }
    result
}

fn resolved_ranges(targets: Vec<Range<usize>>, candidate: &[Cluster]) -> Vec<Range<usize>> {
    targets
        .into_iter()
        .filter(|target| {
            let first = candidate.partition_point(|cluster| cluster.bytes.end <= target.start);
            let end = candidate.partition_point(|cluster| cluster.bytes.start < target.end);
            let overlapping = &candidate[first..end];
            !overlapping.is_empty()
                && overlapping
                    .iter()
                    .all(|cluster| !cluster.missing && contains(target, &cluster.bytes))
                && overlapping
                    .first()
                    .is_some_and(|cluster| cluster.bytes.start == target.start)
                && overlapping
                    .last()
                    .is_some_and(|cluster| cluster.bytes.end == target.end)
        })
        .collect()
}

fn column_ranges(
    row: &RowText,
    cells: &[Cell],
    columns: u16,
    targets: &[Range<usize>],
) -> Vec<Range<u16>> {
    targets
        .iter()
        .filter_map(|target| {
            let start = *row.columns_by_byte.get(target.start)?;
            let last = *row.columns_by_byte.get(target.end.checked_sub(1)?)?;
            let width = if source_cell(cells, last).width == CellWidth::Wide {
                2
            } else {
                1
            };
            Some(start..last.saturating_add(width).min(columns))
        })
        .collect()
}

fn target_at(targets: &[Range<u16>], columns: Range<u16>) -> Option<usize> {
    let index = targets.partition_point(|target| target.end <= columns.start);
    targets
        .get(index)
        .filter(|target| intersects(target, &columns))
        .map(|_| index)
}

fn splice_geometry<T>(
    primary: Vec<T>,
    candidate: Vec<T>,
    targets: &[Range<u16>],
    columns: impl Fn(&T) -> Range<u16>,
) -> Vec<T> {
    let mut replacements: Vec<Vec<T>> = (0..targets.len()).map(|_| Vec::new()).collect();
    for glyph in candidate {
        if let Some(index) = target_at(targets, columns(&glyph))
            && contains(&targets[index], &columns(&glyph))
        {
            replacements[index].push(glyph);
        }
    }
    let mut emitted = vec![false; targets.len()];
    let mut result = Vec::with_capacity(primary.len());
    for glyph in primary {
        if let Some(index) = target_at(targets, columns(&glyph)) {
            if !emitted[index] {
                result.append(&mut replacements[index]);
                emitted[index] = true;
            }
        } else {
            result.push(glyph);
        }
    }
    result
}

pub(super) fn geometry(
    context: ShapeContext<'_>,
    oracle: &mut MissingGlyphOracle,
    window: &Window,
) -> GeometryResult {
    let ShapeContext {
        row,
        index,
        cells,
        line,
        columns,
        metrics,
        baseline,
    } = context;
    let primary = prepare_geometry(row, cells, columns, line, metrics, baseline, window);
    let mut result = GeometryResult {
        geometry: primary,
        retry_runs: 0,
    };
    // Normal rows keep the existing shaping path: no probe and no retry.
    if !row
        .font_runs
        .iter()
        .any(|run| run.font.style != FontStyle::Normal)
    {
        return result;
    }
    let Some(missing) = oracle.resolve(metrics, window) else {
        return result;
    };
    // Resolved styled text needs no cluster/grapheme partition or second shape.
    if !line
        .runs
        .iter()
        .any(|run| run.glyphs.iter().any(|glyph| glyph.id == missing))
    {
        return result;
    }
    let original = line_clusters(&row.text, line, missing);
    let targets = missing_ranges(&row.text, &original, &row.font_runs);
    if targets.is_empty() {
        return result;
    }
    let retry_row = RowText {
        text: row.text.clone(),
        font_runs: normal_runs(&row.font_runs, &targets),
        columns_by_byte: row.columns_by_byte.clone(),
    };
    let retry = window.text_system().shape_line(
        retry_row.text.clone().into(),
        px(metrics.font_size),
        &retry_row.font_runs,
        None,
    );
    result.retry_runs = retry_row.font_runs.len();
    trace("normal-retry", index, &retry_row, &retry, window);
    let accepted = resolved_ranges(targets, &line_clusters(&row.text, &retry, missing));
    if accepted.is_empty() {
        return result;
    }
    // Keep the primary baseline and every unaffected glyph's exact geometry.
    // The available upright face is used only for complete resolved targets;
    // GPUI exposes no synthetic-oblique glyph raster/paint transform here.
    let candidate = prepare_geometry(
        &retry_row, cells, columns, &retry, metrics, baseline, window,
    );
    let accepted_columns = column_ranges(row, cells, columns, &accepted);
    result.geometry = splice_geometry(result.geometry, candidate, &accepted_columns, |glyph| {
        glyph.column..glyph.end_column
    });
    trace_selected(index, row, &accepted, &accepted_columns, &result.geometry);
    result
}

fn trace_selected(
    index: usize,
    row: &RowText,
    accepted: &[Range<usize>],
    columns: &[Range<u16>],
    geometry: &[GlyphGeometry],
) {
    let Some(Some(filter)) = TRACE_FILTER.get() else {
        return;
    };
    if (filter != "1" && !row.text.contains(filter))
        || TRACE_ROWS.fetch_add(1, Ordering::Relaxed) >= MAX_TRACE_ROWS
    {
        return;
    }
    let glyphs:Vec<_>=geometry.iter().map(|glyph| serde_json::json!({
        "font_id":glyph.font_id.0,"glyph_id":format!("{:?}",glyph.glyph_id),
        "column":glyph.column,"end_column":glyph.end_column,
        "x":f32::from(glyph.position.x),"y":f32::from(glyph.position.y),"emoji":glyph.is_emoji,
        "source":if target_at(columns,glyph.column..glyph.end_column).is_some() {"retry"} else {"primary"}
    })).collect();
    let accepted: Vec<_> = accepted
        .iter()
        .map(|range| [range.start, range.end])
        .collect();
    eprintln!(
        "terminal-glyph-trace {}",
        serde_json::json!({"stage":"selected","row":index,"text":row.text,"accepted":accepted,"glyphs":glyphs})
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_range(range: Range<usize>) -> Vec<Range<usize>> {
        std::iter::once(range).collect()
    }

    fn run(text: &str, style: FontStyle) -> TextRun {
        let mut font = font("JetBrains Mono");
        font.style = style;
        TextRun {
            len: text.len(),
            font,
            color: rgb(0).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }
    }

    #[test]
    fn missing_cluster_selection_uses_sorted_starts_and_keeps_valid_neighbors() {
        let text = "界e\u{301}🙂";
        let original = clusters(
            text,
            [(6, false), (0, true), (3, false), (3, false)].into_iter(),
        );
        assert_eq!(
            original,
            vec![
                Cluster {
                    bytes: 0..3,
                    missing: true
                },
                Cluster {
                    bytes: 3..6,
                    missing: false
                },
                Cluster {
                    bytes: 6..10,
                    missing: false
                }
            ]
        );
        assert_eq!(
            missing_ranges(text, &original, &[run(text, FontStyle::Italic)]),
            one_range(0..3)
        );
        assert_eq!(
            missing_ranges(text, &original, &[run(text, FontStyle::Normal)]),
            Vec::<Range<usize>>::new()
        );
    }

    #[test]
    fn retry_expands_split_combining_and_zwj_graphemes_without_cutting_ligatures() {
        let text = "e\u{301}fi🙂\u{200d}🙂";
        let emoji = "e\u{301}fi".len();
        let original = clusters(
            text,
            [
                (0, false),
                (1, true),
                (3, false),
                (emoji, false),
                (emoji + 4, true),
                (emoji + 7, false),
            ]
            .into_iter(),
        );
        let targets = missing_ranges(text, &original, &[run(text, FontStyle::Italic)]);
        assert_eq!(targets, vec![0..3, emoji..text.len()]);
        assert!(!targets.iter().any(|range| intersects(range, &(3..emoji))));
    }

    #[test]
    fn retry_changes_only_target_style_preserving_bold_features_and_source_runs() {
        let text = "界e\u{301}🙂";
        let mut source = run(text, FontStyle::Italic);
        source.font = source.font.bold();
        let original = source.clone();
        let retry = normal_runs(&[source], &one_range(0..3));
        assert_eq!(
            retry.iter().map(|run| run.len).collect::<Vec<_>>(),
            vec![3, 7]
        );
        assert_eq!(retry[0].font.style, FontStyle::Normal);
        assert_eq!(retry[1].font.style, FontStyle::Italic);
        assert_eq!(retry[0].font.weight, original.font.weight);
        assert_eq!(retry[1].font.weight, original.font.weight);
        assert_eq!(retry[0].font.features, original.font.features);
        assert_eq!(original.font.style, FontStyle::Italic);
    }

    #[test]
    fn candidate_acceptance_rejects_unresolved_and_cross_boundary_clusters() {
        let candidate = vec![
            Cluster {
                bytes: 0..3,
                missing: false,
            },
            Cluster {
                bytes: 3..6,
                missing: true,
            },
            Cluster {
                bytes: 6..10,
                missing: false,
            },
        ];
        assert_eq!(
            resolved_ranges(vec![0..3, 3..6], &candidate),
            one_range(0..3)
        );
        assert_eq!(
            resolved_ranges(one_range(0..6), &candidate),
            Vec::<Range<usize>>::new()
        );
        assert_eq!(
            resolved_ranges(
                one_range(0..3),
                &[Cluster {
                    bytes: 0..6,
                    missing: false
                }]
            ),
            Vec::<Range<usize>>::new()
        );
        assert_eq!(
            resolved_ranges(one_range(0..3), &[]),
            Vec::<Range<usize>>::new()
        );
    }

    #[test]
    fn splicing_preserves_unaffected_geometry_and_visual_order() {
        // Opaque payloads stand in for font IDs, positions and ink bounds.
        // Visual ordering can differ from logical column ordering (RTL runs).
        let primary = vec![
            (6..7, "right"),
            (2..4, "missing-a"),
            (2..4, "missing-mark"),
            (4..6, "missing-b"),
            (0..2, "left"),
        ];
        let candidate = vec![
            (0..2, "changed-left"),
            (2..4, "resolved-a"),
            (2..4, "resolved-mark"),
            (4..6, "resolved-b"),
            (6..7, "changed-right"),
        ];
        assert_eq!(
            splice_geometry(primary, candidate, &[2..4, 4..6], |glyph| glyph.0.clone()),
            vec![
                (6..7, "right"),
                (2..4, "resolved-a"),
                (2..4, "resolved-mark"),
                (4..6, "resolved-b"),
                (0..2, "left")
            ]
        );
    }

    #[test]
    fn wide_and_combining_targets_map_to_physical_cell_intervals() {
        let cells = vec![
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
            Cell {
                text: "🙂".into(),
                width: CellWidth::Wide,
                ..Cell::default()
            },
            Cell {
                width: CellWidth::Continuation,
                ..Cell::default()
            },
        ];
        let row = build_row_text(&cells, 5, Theme::default().terminal);
        assert_eq!(
            column_ranges(&row, &cells, 5, &[0..3, 3..6, 6..10]),
            vec![0..2, 2..3, 3..5]
        );
        assert_eq!(target_at(&[0..2, 3..5], 2..3), None);
        assert_eq!(target_at(&[0..2, 3..5], 3..5), Some(1));
    }
}
