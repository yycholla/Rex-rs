//! Compact row updates for a single acknowledged desktop connection baseline.
use super::protocol::{MAX_GRID_CELLS, PaneSnapshot, Snapshot};
use crate::core::{
    GridSize, PaneId, Workspace,
    frame::{Cell, CellStyle, CellWidth, Cursor, Rgb, TerminalFrame, TerminalRow, UnderlineKind},
};
use crate::terminal::PaneStatus;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

/// Only the workspace metadata and rows that changed since `base_revision`.
/// A missing base denotes a complete resynchronization, never a partial merge.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotUpdate {
    pub base_revision: Option<u64>,
    pub state: Option<Workspace>,
    pub panes: BTreeMap<PaneId, PaneUpdate>,
    pub removed: Vec<PaneId>,
    pub restored_metadata: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaneUpdate {
    pub size: GridSize,
    pub cursor: Option<Cursor>,
    pub foreground: u32,
    pub background: u32,
    pub rows: Vec<(u16, Vec<PackedCell>)>,
    pub status: Option<PaneStatus>,
    pub process_id: Option<u32>,
}

/// Tuple fields avoid repeating JSON property names for every terminal cell.
/// Text, width, foreground, background, decoration flags/kind, optional underline color.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackedCell(
    pub String,
    pub u8,
    pub u32,
    pub u32,
    pub u16,
    #[serde(default)] pub Option<u32>,
);

impl SnapshotUpdate {
    #[must_use]
    pub fn between(current: &Snapshot, previous: Option<(&Snapshot, u64)>) -> Self {
        let old = previous.map(|(snapshot, _)| snapshot);
        Self {
            base_revision: previous.map(|(_, revision)| revision),
            state: old
                .filter(|snapshot| snapshot.state == current.state)
                .map_or_else(|| Some(current.state.clone()), |_| None),
            panes: current
                .panes
                .iter()
                .filter_map(|(id, pane)| {
                    let before = old.and_then(|snapshot| snapshot.panes.get(id));
                    let changed = before.is_none_or(|before| {
                        !Arc::ptr_eq(&before.frame, &pane.frame)
                            || before.status != pane.status
                            || before.process_id != pane.process_id
                    });
                    changed.then(|| (*id, PaneUpdate::between(pane, before)))
                })
                .collect(),
            removed: old.map_or_else(Vec::new, |snapshot| {
                snapshot
                    .panes
                    .keys()
                    .filter(|id| !current.panes.contains_key(id))
                    .copied()
                    .collect()
            }),
            restored_metadata: current.restored_metadata,
        }
    }

    /// Reconstruct a cache on the socket worker while retaining untouched Arcs.
    ///
    /// # Errors
    /// Rejects mismatched baselines, incomplete resets, invalid frame dimensions,
    /// duplicate/out-of-range rows or invalid packed cell declarations.
    pub fn apply(self, previous: Option<(&Snapshot, u64)>) -> Result<Snapshot> {
        let old = match self.base_revision {
            Some(revision) => {
                let (snapshot, known) = previous.context("update requires a baseline")?;
                ensure!(revision == known, "update baseline revision mismatch");
                Some(snapshot)
            }
            None => None,
        };
        let state = self
            .state
            .or_else(|| old.map(|snapshot| snapshot.state.clone()))
            .context("complete update is missing workspace metadata")?;
        ensure!(state.is_valid(), "invalid update workspace");
        let expected: BTreeSet<_> = state.pane_ids().into_iter().collect();
        ensure!(expected.len() <= 16, "update exceeds pane limit");
        let mut panes = old.map_or_else(BTreeMap::new, |snapshot| snapshot.panes.clone());
        let mut removed = BTreeSet::new();
        for id in self.removed {
            ensure!(removed.insert(id), "duplicate removed pane");
            ensure!(
                panes.remove(&id).is_some(),
                "removed pane is absent from baseline"
            );
            ensure!(
                !self.panes.contains_key(&id),
                "pane cannot be removed and updated together"
            );
        }
        // Inspect claimed dimensions before allocating any reconstructed cell rows.
        let mut total = 0usize;
        for id in &expected {
            let size = self
                .panes
                .get(id)
                .map(|pane| pane.size)
                .or_else(|| panes.get(id).map(|pane| pane.frame.size))
                .context("update is missing a pane")?;
            total = total
                .checked_add(usize::from(size.columns()) * usize::from(size.rows()))
                .context("update grid overflow")?;
            ensure!(total <= MAX_GRID_CELLS, "update exceeds cell limit");
        }
        for (id, update) in self.panes {
            ensure!(expected.contains(&id), "update contains an unknown pane");
            let pane = update.apply(panes.get(&id))?;
            panes.insert(id, pane);
        }
        ensure!(
            panes.len() == expected.len(),
            "update did not remove obsolete panes"
        );
        let snapshot = Snapshot {
            state,
            panes,
            restored_metadata: self.restored_metadata,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }
}

impl PaneUpdate {
    fn between(current: &PaneSnapshot, previous: Option<&PaneSnapshot>) -> Self {
        let frame = &current.frame;
        let before = previous
            .map(|pane| &pane.frame)
            .filter(|old| old.size == frame.size);
        let rows = frame
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                let unchanged = before
                    .and_then(|old| old.rows.get(index))
                    .is_some_and(|old| Arc::ptr_eq(old, row) || old.as_ref() == row.as_ref());
                (!unchanged).then(|| (index as u16, row.iter().map(PackedCell::from).collect()))
            })
            .collect();
        Self {
            size: frame.size,
            cursor: frame.cursor,
            foreground: frame.foreground.to_hex(),
            background: frame.background.to_hex(),
            rows,
            status: current.status.clone(),
            process_id: current.process_id,
        }
    }

    fn apply(self, previous: Option<&PaneSnapshot>) -> Result<PaneSnapshot> {
        ensure!(
            self.foreground <= 0x00ff_ffff && self.background <= 0x00ff_ffff,
            "invalid frame color"
        );
        let columns = usize::from(self.size.columns());
        let count = usize::from(self.size.rows());
        ensure!(
            columns * count <= MAX_GRID_CELLS,
            "pane update exceeds cell limit"
        );
        if let Some(cursor) = self.cursor {
            ensure!(
                cursor.column < self.size.columns() && cursor.row < self.size.rows(),
                "invalid update cursor"
            );
        }
        let before = previous
            .map(|pane| &pane.frame)
            .filter(|old| old.size == self.size && old.rows.len() == count);
        let mut rows: Vec<Option<TerminalRow>> = before.map_or_else(
            || vec![None; count],
            |old| old.rows.iter().cloned().map(Some).collect(),
        );
        let mut changed = BTreeSet::new();
        for (index, cells) in self.rows {
            let index = usize::from(index);
            ensure!(
                index < count && changed.insert(index),
                "duplicate or out-of-range update row"
            );
            ensure!(
                cells.len() == columns,
                "update row has invalid column count"
            );
            let cells: Result<Vec<_>> = cells.into_iter().map(PackedCell::unpack).collect();
            rows[index] = Some(Arc::new(cells?));
        }
        let rows: Result<Vec<_>> = rows
            .into_iter()
            .map(|row| row.context("incomplete pane reset"))
            .collect();
        let frame = TerminalFrame {
            size: self.size,
            cursor: self.cursor,
            rows: rows?,
            foreground: Rgb::from_hex(self.foreground),
            background: Rgb::from_hex(self.background),
        };
        let frame = before
            .filter(|old| same_frame(old, &frame))
            .map_or_else(|| Arc::new(frame), Arc::clone);
        Ok(PaneSnapshot {
            frame,
            status: self.status,
            process_id: self.process_id,
        })
    }
}

fn same_frame(first: &TerminalFrame, second: &TerminalFrame) -> bool {
    first.size == second.size
        && first.cursor == second.cursor
        && first.foreground == second.foreground
        && first.background == second.background
        && first.rows.len() == second.rows.len()
        && first
            .rows
            .iter()
            .zip(&second.rows)
            .all(|(a, b)| Arc::ptr_eq(a, b))
}

impl From<&Cell> for PackedCell {
    fn from(cell: &Cell) -> Self {
        let width = match cell.width {
            CellWidth::Narrow => 0,
            CellWidth::Wide => 1,
            CellWidth::Continuation => 2,
            CellWidth::WrapSpacer => 3,
        };
        let style = cell.style;
        let flags = u16::from(style.bold)
            | u16::from(style.italic) << 1
            | u16::from(style.underline) << 2
            | u16::from(style.inverse) << 3
            | u16::from(style.invisible) << 4
            | u16::from(style.faint) << 5
            | u16::from(style.blink) << 6
            | u16::from(style.strikethrough) << 7
            | u16::from(style.overline) << 8
            | match style.underline_kind {
                UnderlineKind::None => 0,
                UnderlineKind::Single => 1,
                UnderlineKind::Double => 2,
                UnderlineKind::Curly => 3,
                UnderlineKind::Dotted => 4,
                UnderlineKind::Dashed => 5,
            } << 9;
        Self(
            cell.text.clone(),
            width,
            cell.foreground.to_hex(),
            cell.background.to_hex(),
            flags,
            style.underline_color.map(Rgb::to_hex),
        )
    }
}

impl PackedCell {
    fn unpack(self) -> Result<Cell> {
        ensure!(
            self.2 <= 0x00ff_ffff
                && self.3 <= 0x00ff_ffff
                && self.4 <= 0x0fff
                && self.5.is_none_or(|color| color <= 0x00ff_ffff),
            "invalid packed cell color or style flags"
        );
        let underline_kind = match self.4 >> 9 {
            0 => UnderlineKind::None,
            1 => UnderlineKind::Single,
            2 => UnderlineKind::Double,
            3 => UnderlineKind::Curly,
            4 => UnderlineKind::Dotted,
            5 => UnderlineKind::Dashed,
            _ => anyhow::bail!("invalid packed underline kind"),
        };
        let width = match self.1 {
            0 => CellWidth::Narrow,
            1 => CellWidth::Wide,
            2 => CellWidth::Continuation,
            3 => CellWidth::WrapSpacer,
            _ => anyhow::bail!("invalid packed cell width"),
        };
        Ok(Cell {
            text: self.0,
            width,
            foreground: Rgb::from_hex(self.2),
            background: Rgb::from_hex(self.3),
            style: CellStyle {
                bold: self.4 & 1 != 0,
                italic: self.4 & 2 != 0,
                underline: self.4 & 4 != 0,
                underline_kind,
                underline_color: self.5.map(Rgb::from_hex),
                inverse: self.4 & 8 != 0,
                invisible: self.4 & 16 != 0,
                faint: self.4 & 32 != 0,
                blink: self.4 & 64 != 0,
                strikethrough: self.4 & 128 != 0,
                overline: self.4 & 256 != 0,
            },
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests fail immediately when owned frame fixtures are invalid."
)]
mod tests {
    use super::*;
    use crate::core::{Command, SplitAxis};

    fn fixture() -> (Snapshot, PaneId) {
        let mut state = Workspace::default();
        state
            .apply(Command::CreateSession {
                title: "sparse".into(),
            })
            .unwrap();
        let id = state.pane_ids()[0];
        let frame = TerminalFrame {
            size: GridSize::new(80, 24).unwrap(),
            rows: (0..24)
                .map(|_| {
                    Arc::new(vec![
                        Cell {
                            text: "x".into(),
                            foreground: Rgb::from_hex(0x0012_3456),
                            background: Rgb::from_hex(0x0011_2233),
                            ..Cell::default()
                        };
                        80
                    ])
                })
                .collect(),
            cursor: Some(Cursor {
                column: 0,
                row: 0,
                ..Cursor::default()
            }),
            foreground: Rgb::from_hex(0x0012_3456),
            background: Rgb::from_hex(0x0011_2233),
        };
        (
            Snapshot {
                state,
                panes: BTreeMap::from([(
                    id,
                    PaneSnapshot {
                        frame: Arc::new(frame),
                        status: Some(PaneStatus::Running),
                        process_id: Some(123),
                    },
                )]),
                restored_metadata: false,
            },
            id,
        )
    }

    #[test]
    fn single_row_update_is_small_and_reuses_untouched_rows() {
        let (old, id) = fixture();
        let mut current = old.clone();
        let frame = Arc::make_mut(&mut current.panes.get_mut(&id).unwrap().frame);
        Arc::make_mut(&mut frame.rows[5])[1].text = "界e\u{301}".into();
        let update = SnapshotUpdate::between(&current, Some((&old, 7)));
        assert!(update.state.is_none());
        assert_eq!(update.panes[&id].rows.len(), 1);
        let packed = serde_json::to_vec(&update).unwrap();
        let full = serde_json::to_vec(&current).unwrap();
        assert!(packed.len() * 20 < full.len());
        let decoded: SnapshotUpdate = serde_json::from_slice(&packed).unwrap();
        let restored = decoded.apply(Some((&old, 7))).unwrap();
        assert_eq!(restored.panes[&id].frame, current.panes[&id].frame);
        assert!(Arc::ptr_eq(
            &restored.panes[&id].frame.rows[0],
            &old.panes[&id].frame.rows[0]
        ));
        assert!(!Arc::ptr_eq(
            &restored.panes[&id].frame.rows[5],
            &old.panes[&id].frame.rows[5]
        ));
    }

    #[test]
    fn cursor_only_and_equal_rows_do_not_rebuild_text_rows() {
        let (old, id) = fixture();
        let mut current = old.clone();
        let frame = Arc::make_mut(&mut current.panes.get_mut(&id).unwrap().frame);
        frame.cursor = Some(Cursor {
            column: 2,
            row: 3,
            ..Cursor::default()
        });
        let update = SnapshotUpdate::between(&current, Some((&old, 2)));
        assert_eq!(update.panes[&id].rows.len(), 0);
        assert!(serde_json::to_vec(&update).unwrap().len() < 512);
        let restored = update.apply(Some((&old, 2))).unwrap();
        for (next, before) in restored.panes[&id]
            .frame
            .rows
            .iter()
            .zip(&old.panes[&id].frame.rows)
        {
            assert!(Arc::ptr_eq(next, before));
        }
        let mut equal = old.clone();
        let frame = Arc::make_mut(&mut equal.panes.get_mut(&id).unwrap().frame);
        frame.rows[0] = Arc::new(frame.rows[0].as_ref().clone());
        let restored = SnapshotUpdate::between(&equal, Some((&old, 3)))
            .apply(Some((&old, 3)))
            .unwrap();
        assert!(Arc::ptr_eq(
            &restored.panes[&id].frame,
            &old.panes[&id].frame
        ));
    }

    #[test]
    fn complete_resync_round_trips_all_cell_declarations() {
        let (mut current, id) = fixture();
        let frame = Arc::make_mut(&mut current.panes.get_mut(&id).unwrap().frame);
        let row = Arc::make_mut(&mut frame.rows[0]);
        for (index, width) in [
            CellWidth::Narrow,
            CellWidth::Wide,
            CellWidth::Continuation,
            CellWidth::WrapSpacer,
        ]
        .into_iter()
        .enumerate()
        {
            row[index] = Cell {
                text: "界e\u{301}".into(),
                width,
                foreground: Rgb::from_hex(0x00ff_ffff),
                background: Rgb::from_hex(0x00ab_cdef),
                style: CellStyle {
                    bold: true,
                    italic: true,
                    underline: true,
                    underline_kind: UnderlineKind::Curly,
                    underline_color: Some(Rgb::from_hex(0x12_ab34)),
                    inverse: true,
                    invisible: true,
                    faint: true,
                    blink: true,
                    strikethrough: true,
                    overline: true,
                },
            };
        }
        let update = SnapshotUpdate::between(&current, None);
        let restored = update.apply(None).unwrap();
        assert_eq!(restored.state, current.state);
        assert_eq!(restored.panes[&id].frame, current.panes[&id].frame);
    }

    #[test]
    fn rejects_incomplete_resets_and_mismatched_baselines() {
        let (current, id) = fixture();
        let mut update = SnapshotUpdate::between(&current, None);
        update.panes.get_mut(&id).unwrap().rows.pop();
        assert!(update.apply(None).is_err());
        let update = SnapshotUpdate::between(&current, Some((&current, 4)));
        assert!(update.clone().apply(None).is_err());
        assert!(update.apply(Some((&current, 5))).is_err());
        let mut update = SnapshotUpdate::between(&current, None);
        update.state = None;
        assert!(update.apply(Some((&current, 4))).is_err());
    }

    #[test]
    fn rejects_duplicate_rows_invalid_cells_and_excessive_dimensions() {
        let (current, id) = fixture();
        let original = SnapshotUpdate::between(&current, None);
        let mut update = original.clone();
        let rows = &mut update.panes.get_mut(&id).unwrap().rows;
        rows.push(rows[0].clone());
        assert!(update.apply(None).is_err());
        for bad in [
            PackedCell("x".into(), 4, 0, 0, 0, None),
            PackedCell("x".into(), 0, 0x0100_0000, 0, 0, None),
            PackedCell("x".into(), 0, 0, 0, 0x1000, None),
        ] {
            let mut update = original.clone();
            update.panes.get_mut(&id).unwrap().rows[0].1[0] = bad;
            assert!(update.apply(None).is_err());
        }
        let mut update = original;
        update.panes.get_mut(&id).unwrap().size = GridSize::new(u16::MAX, u16::MAX).unwrap();
        assert!(update.apply(None).is_err());
    }

    #[test]
    fn pane_removal_keeps_survivor_frame_and_rejects_unremoved_extras() {
        let (mut old, id) = fixture();
        old.state
            .apply(Command::SplitPane {
                pane: id,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        let added = old
            .state
            .pane_ids()
            .into_iter()
            .find(|pane| *pane != id)
            .unwrap();
        old.panes.insert(added, old.panes[&id].clone());
        let mut current = old.clone();
        current.state.apply(Command::ClosePane(added)).unwrap();
        current.panes.remove(&added);
        let update = SnapshotUpdate::between(&current, Some((&old, 9)));
        assert_eq!(update.removed, [added]);
        let restored = update.clone().apply(Some((&old, 9))).unwrap();
        assert_eq!(restored.state, current.state);
        assert!(Arc::ptr_eq(
            &restored.panes[&id].frame,
            &old.panes[&id].frame
        ));
        let mut incomplete = update;
        incomplete.removed.clear();
        assert!(incomplete.apply(Some((&old, 9))).is_err());
    }
    #[test]
    fn compact_style_extension_preserves_kinds_and_reads_legacy_rows() -> Result<()> {
        let legacy: PackedCell = serde_json::from_str(r#"["x",0,0,0,4]"#)?;
        assert_eq!(
            legacy.unpack()?.style.resolved_underline(),
            UnderlineKind::Single
        );
        for kind in [
            UnderlineKind::None,
            UnderlineKind::Single,
            UnderlineKind::Double,
            UnderlineKind::Curly,
            UnderlineKind::Dotted,
            UnderlineKind::Dashed,
        ] {
            let cell = Cell {
                style: CellStyle {
                    underline: kind != UnderlineKind::None,
                    underline_kind: kind,
                    underline_color: Some(Rgb::from_hex(0x0012_3456)),
                    ..CellStyle::default()
                },
                ..Cell::default()
            };
            let bytes = serde_json::to_vec(&PackedCell::from(&cell))?;
            let restored: PackedCell = serde_json::from_slice(&bytes)?;
            assert_eq!(restored.unpack()?, cell);
        }
        assert!(
            PackedCell("x".into(), 0, 0, 0, 6 << 9, None)
                .unpack()
                .is_err()
        );
        assert!(
            PackedCell("x".into(), 0, 0, 0, 0, Some(0x0100_0000))
                .unpack()
                .is_err()
        );
        Ok(())
    }
}
