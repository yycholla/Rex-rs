//! Owned Rex views and command translation, independent of local workspace mutation.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::core::{Command, Direction, PaneId, SessionId, SplitAxis, SplitId, WindowId};

const MAX_SESSIONS: usize = 256;
const MAX_WINDOWS: usize = 256;
const MAX_BLOCKS: usize = 1024;
const MAX_STAGED_BLOCKS: usize = MAX_BLOCKS * 2;
const MAX_DEPTH: usize = 64;
const MAX_LAYOUT_NODES: usize = MAX_BLOCKS * 2;
const MAX_TERMINAL_TITLE_BYTES: usize = 16 * 1024;

/// Authoritative presentation layout. Rex ratios retain their full precision.
#[derive(Clone, Debug, PartialEq)]
pub enum ViewLayout {
    Pane(PaneId),
    Split {
        id: SplitId,
        axis: SplitAxis,
        /// First child's fraction of the split, strictly between zero and one.
        ratio: f64,
        first: Box<ViewLayout>,
        second: Box<ViewLayout>,
    },
}

impl ViewLayout {
    #[must_use]
    pub fn panes(&self) -> Vec<PaneId> {
        let mut panes = Vec::new();
        self.collect_panes(&mut panes);
        panes
    }

    fn collect_panes(&self, panes: &mut Vec<PaneId>) {
        match self {
            Self::Pane(id) => panes.push(*id),
            Self::Split { first, second, .. } => {
                first.collect_panes(panes);
                second.collect_panes(panes);
            }
        }
    }

    #[must_use]
    pub fn split_ids(&self) -> Vec<SplitId> {
        let mut splits = Vec::new();
        self.collect_splits(&mut splits);
        splits
    }

    fn collect_splits(&self, splits: &mut Vec<SplitId>) {
        if let Self::Split {
            id, first, second, ..
        } = self
        {
            splits.push(*id);
            first.collect_splits(splits);
            second.collect_splits(splits);
        }
    }
}

/// One ordered control request. Session selection itself is client-local.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiCall {
    pub method: &'static str,
    pub params: Value,
}

/// A remote block's metadata. Ownership arbitrates preferences, not input rights.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockView {
    pub id: PaneId,
    pub remote_id: String,
    pub session_id: String,
    pub label: String,
    /// Dynamic program title, independent of the authoritative block label.
    pub terminal_title: String,
    pub creator_name: String,
    pub flavor: String,
    pub host_id: String,
    pub owner: Option<String>,
}

impl BlockView {
    /// Prefer explicit labels; the default shell label can show its program title.
    #[must_use]
    pub fn title(&self) -> &str {
        if (self.label.is_empty() || self.label == "shell") && !self.terminal_title.is_empty() {
            &self.terminal_title
        } else {
            &self.label
        }
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.creator_name == "com.superlogical.terminal"
    }

    #[must_use]
    pub fn size_authority(&self, client_id: &str) -> bool {
        self.owner.as_deref().is_none_or(|owner| owner == client_id)
    }
}

/// An authoritative layer in back-to-front server order.
#[derive(Clone, Debug, PartialEq)]
pub struct LayerView {
    pub remote_id: String,
    pub kind: String,
    pub bounds: [f64; 4],
    pub raw_layout: Value,
    pub layout: Option<ViewLayout>,
    pub blocks: Vec<PaneId>,
    pub unsupported: Option<String>,
}

impl LayerView {
    /// The protocol renders unknown future layer kinds as floating.
    #[must_use]
    pub fn is_floating(&self) -> bool {
        self.kind != "tiled"
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WindowView {
    id: WindowId,
    remote_id: String,
    label: String,
    title: String,
    layout: Option<ViewLayout>,
    active_pane: Option<PaneId>,
    zoomed: Option<PaneId>,
    pub layers: Vec<LayerView>,
}

impl WindowView {
    #[must_use]
    pub const fn id(&self) -> WindowId {
        self.id
    }
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }
    /// The unmodified label from session.view, used when editing its name.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    fn refresh_title(&mut self, blocks: &BTreeMap<PaneId, BlockView>) {
        self.title.clone_from(&self.label);
        let generated = self
            .remote_id
            .strip_prefix("window:")
            .and_then(|id| id.get(id.len().saturating_sub(8)..))
            .is_some_and(|suffix| self.label == format!("Window {suffix}"));
        if (self.label.is_empty() || generated)
            && let Some(block) = self.active_pane.and_then(|pane| blocks.get(&pane))
            && !block.title().is_empty()
            && (block.label != "shell" || !block.terminal_title.is_empty())
        {
            block.title().clone_into(&mut self.title);
        }
    }
    #[must_use]
    pub fn remote_id(&self) -> &str {
        &self.remote_id
    }
    #[must_use]
    pub const fn layout(&self) -> Option<&ViewLayout> {
        self.layout.as_ref()
    }
    #[must_use]
    pub const fn active_pane(&self) -> Option<PaneId> {
        self.active_pane
    }
    #[must_use]
    pub const fn zoomed_pane(&self) -> Option<PaneId> {
        self.zoomed
    }

    /// All placed blocks, including floating and unsupported-content blocks.
    #[must_use]
    pub fn placed_panes(&self) -> Vec<PaneId> {
        self.layers
            .iter()
            .flat_map(|layer| layer.blocks.iter().copied())
            .collect()
    }

    /// Panes requiring frames for this window, honoring server zoom.
    #[must_use]
    pub fn visible_panes(&self) -> Vec<PaneId> {
        self.zoomed.map_or_else(
            || {
                self.layers
                    .iter()
                    .filter_map(|layer| layer.layout.as_ref())
                    .flat_map(ViewLayout::panes)
                    .collect()
            },
            |pane| vec![pane],
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionView {
    id: SessionId,
    remote_id: String,
    title: String,
    windows: Vec<WindowView>,
    active_window: Option<WindowId>,
    revision: Option<u64>,
}

impl SessionView {
    #[must_use]
    pub const fn id(&self) -> SessionId {
        self.id
    }
    #[must_use]
    pub fn remote_id(&self) -> &str {
        &self.remote_id
    }
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }
    /// The unmodified session label. Terminal titles never rename sessions.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.title
    }
    #[must_use]
    pub fn windows(&self) -> &[WindowView] {
        &self.windows
    }
    #[must_use]
    pub fn active_window(&self) -> Option<&WindowView> {
        self.windows
            .iter()
            .find(|window| Some(window.id) == self.active_window)
    }
    #[must_use]
    pub fn active_pane(&self) -> Option<PaneId> {
        self.active_window()?.active_pane()
    }
    #[must_use]
    pub fn layout(&self) -> Option<&ViewLayout> {
        self.active_window()?.layout()
    }
    #[must_use]
    pub const fn revision(&self) -> Option<u64> {
        self.revision
    }
}

#[derive(Clone, Debug)]
struct SplitTarget {
    session_id: String,
    before: String,
    after: String,
    axis: SplitAxis,
}

/// Numeric IDs are lossless local aliases; mappings are never hashed or reused.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceState {
    sessions: Vec<SessionView>,
    active_session: Option<SessionId>,
    blocks: BTreeMap<PaneId, BlockView>,
    // Metadata outside the current view, retained across a view/list refresh.
    unplaced: BTreeMap<PaneId, BlockView>,
    detached: BTreeSet<PaneId>,
    ids: BTreeMap<String, u64>,
    remote_ids: BTreeMap<u64, String>,
    next_id: u64,
    splits: BTreeMap<SplitId, SplitTarget>,
    client_id: String,
    instance_id: String,
}

impl WorkspaceState {
    #[must_use]
    pub fn sessions(&self) -> &[SessionView] {
        &self.sessions
    }
    #[must_use]
    pub fn active_session(&self) -> Option<&SessionView> {
        self.sessions
            .iter()
            .find(|session| Some(session.id) == self.active_session)
    }
    #[must_use]
    pub fn pane_ids(&self) -> Vec<PaneId> {
        self.blocks
            .keys()
            .chain(self.unplaced.keys())
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
    #[must_use]
    pub fn placed_pane_ids(&self) -> Vec<PaneId> {
        self.blocks.keys().copied().collect()
    }
    #[must_use]
    pub fn detached_blocks(&self, session: SessionId) -> Vec<&BlockView> {
        let Some(session) = self.sessions.iter().find(|entry| entry.id == session) else {
            return Vec::new();
        };
        self.unplaced
            .values()
            .filter(|block| {
                block.session_id == session.remote_id && self.detached.contains(&block.id)
            })
            .collect()
    }
    #[must_use]
    pub fn is_detached(&self, pane: PaneId) -> bool {
        self.detached.contains(&pane) && self.unplaced.contains_key(&pane)
    }
    /// A formerly known block disappeared from the authoritative inventory.
    #[must_use]
    pub(crate) fn is_retired_pane(&self, pane: PaneId) -> bool {
        self.pane(pane).is_none()
            && self
                .remote_ids
                .get(&pane.value())
                .is_some_and(|remote| remote.starts_with("block:"))
    }
    #[must_use]
    pub fn pane(&self, pane: PaneId) -> Option<&BlockView> {
        self.blocks.get(&pane).or_else(|| self.unplaced.get(&pane))
    }

    /// Apply the complete block inventory paired with the session's latest view.
    /// Current view placement wins over list metadata, because lists lack revisions.
    ///
    /// # Errors
    /// Rejects malformed metadata, foreign identities, duplicate IDs and resource
    /// limits without changing the prior state. The caller must refresh view/list
    /// together; this method cannot detect omitted detached nodes in an old list.
    pub fn apply_block_list(&mut self, session_id: &str, value: &Value) -> Result<bool> {
        let mut next = self.clone();
        next.apply_inventory_refs(&[(session_id, value)])?;
        let changed = next.unplaced != self.unplaced || next.detached != self.detached;
        *self = next;
        Ok(changed)
    }

    /// Apply complete inventories together so cross-session churn cannot fail
    /// against stale unplaced records from a session not yet refreshed.
    ///
    /// # Errors
    /// Rejects duplicate sessions, malformed/foreign records and the final
    /// combined resource limit atomically. Placement still follows current views.
    pub fn apply_block_lists(&mut self, inventories: &[(String, Value)]) -> Result<bool> {
        let mut next = self.clone();
        let inventories: Vec<_> = inventories
            .iter()
            .map(|(id, value)| (id.as_str(), value))
            .collect();
        next.apply_inventory_refs(&inventories)?;
        let changed = next.unplaced != self.unplaced || next.detached != self.detached;
        *self = next;
        Ok(changed)
    }

    /// Commit paired authoritative views and inventories as one presentation.
    /// Same-revision views are reparsed after staging their old placement, which
    /// preserves aliases and terminal titles without stale window membership.
    ///
    /// # Errors
    /// Rejects older revisions, duplicate sessions and malformed/oversized
    /// snapshots without changing prior state.
    pub fn apply_snapshots(&mut self, snapshots: &[(Value, Value)]) -> Result<bool> {
        ensure!(
            snapshots.len() <= MAX_SESSIONS,
            "too many Rex snapshot sessions"
        );
        let mut next = self.clone();
        let mut covered = BTreeSet::new();
        for (view, _) in snapshots {
            let id = typed_id(view, "session_id", "session:")?;
            ensure!(
                covered.insert(id.to_owned()),
                "duplicate Rex snapshot session"
            );
            let revision = view["revision"]
                .as_u64()
                .context("missing Rex session revision")?;
            if let Some(known) = self
                .sessions
                .iter()
                .find(|session| session.remote_id == id)
                .and_then(SessionView::revision)
            {
                ensure!(revision >= known, "stale Rex session snapshot");
            }
        }
        next.stage_sessions(&covered);
        for (view, _) in snapshots {
            next.apply_view_inner(view)?;
        }
        let inventories: Vec<_> = snapshots
            .iter()
            .map(|(view, list)| {
                // Identities were validated before staging any candidate state.
                (view["session_id"].as_str().unwrap_or_default(), list)
            })
            .collect();
        next.apply_inventory_refs(&inventories)?;
        let changed = next.sessions != self.sessions
            || next.active_session != self.active_session
            || next.blocks != self.blocks
            || next.unplaced != self.unplaced
            || next.detached != self.detached;
        *self = next;
        Ok(changed)
    }

    fn stage_sessions(&mut self, covered: &BTreeSet<String>) {
        let previous: Vec<_> = self
            .blocks
            .iter()
            .filter(|(_, block)| covered.contains(&block.session_id))
            .map(|(pane, _)| *pane)
            .collect();
        for pane in previous {
            if let Some(block) = self.blocks.remove(&pane) {
                self.unplaced.insert(pane, block);
            }
        }
        for session in self
            .sessions
            .iter_mut()
            .filter(|session| covered.contains(&session.remote_id))
        {
            session.windows.clear();
            session.active_window = None;
            session.revision = None;
        }
        self.splits
            .retain(|_, target| !covered.contains(&target.session_id));
    }

    fn apply_inventory_refs(&mut self, inventories: &[(&str, &Value)]) -> Result<()> {
        let mut covered = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let mut parsed = Vec::new();
        let mut record_count = 0_usize;
        // Prepare every metadata record while previous titles/creator metadata
        // are still available. Nothing is discarded before all lists validate.
        for (session_id, value) in inventories {
            ensure!(
                covered.insert((*session_id).to_owned()),
                "duplicate Rex block inventory session"
            );
            self.validate_inventory_session(session_id, value)?;
            let entries = list(value, "blocks")?;
            record_count = record_count
                .checked_add(entries.len())
                .context("Rex inventory size overflow")?;
            ensure!(record_count <= MAX_BLOCKS, "too many Rex inventory records");
            for entry in entries {
                let (block, placement) = self.parse_list_block(session_id, entry)?;
                ensure!(seen.insert(block.id), "duplicate Rex block list ID");
                parsed.push((block, placement));
            }
        }
        self.unplaced
            .retain(|_, block| !covered.contains(&block.session_id));
        self.detached
            .retain(|pane| self.unplaced.contains_key(pane));
        for (block, placement) in parsed {
            // A delayed list must not undo authoritative placement or metadata.
            if self.blocks.contains_key(&block.id) {
                continue;
            }
            if placement != "placed" {
                self.detached.insert(block.id);
            }
            self.unplaced.insert(block.id, block);
        }
        ensure!(
            self.blocks.len() + self.unplaced.len() <= MAX_BLOCKS,
            "too many Rex blocks"
        );
        Ok(())
    }

    fn validate_inventory_session(&self, session_id: &str, value: &Value) -> Result<()> {
        ensure!(
            self.sessions
                .iter()
                .any(|session| session.remote_id == session_id),
            "Rex block list session is no longer present"
        );
        if value.get("session_id").is_some() {
            ensure!(
                typed_id(value, "session_id", "session:")? == session_id,
                "Rex block list belongs to another session"
            );
        }
        ensure!(value.get("blocks").is_some(), "missing Rex block list");
        ensure!(
            list(value, "blocks")?.len() <= MAX_BLOCKS,
            "too many Rex blocks"
        );
        Ok(())
    }

    fn parse_list_block(&mut self, session_id: &str, entry: &Value) -> Result<(BlockView, String)> {
        let remote = typed_id(entry, "block_id", "block:")?;
        let pane = PaneId::from_value(self.alias(remote)?).context("invalid pane alias")?;
        if let Some(known) = self.pane(pane) {
            ensure!(
                known.session_id == session_id,
                "Rex block list contains a foreign block"
            );
        }
        if let Some(window) = optional_text(entry, "window_id")? {
            typed_id(entry, "window_id", "window:")?;
            ensure!(
                !self
                    .sessions
                    .iter()
                    .filter(|session| session.remote_id != session_id)
                    .flat_map(|session| &session.windows)
                    .any(|entry| entry.remote_id == window),
                "Rex block list window belongs to another session"
            );
        }
        let owner = optional_text(entry, "owner")?.map(str::to_owned);
        if owner.is_some() {
            typed_id(entry, "owner", "client:")?;
        }
        let label = text(entry, "label")?;
        ensure!(
            label.len() <= 512 && label.chars().count() <= 128,
            "Rex block label exceeds limit"
        );
        let previous = self.pane(pane);
        let terminal_title =
            previous.map_or_else(String::new, |block| block.terminal_title.clone());
        let block = BlockView {
            id: pane,
            remote_id: remote.to_owned(),
            session_id: session_id.to_owned(),
            label: label.to_owned(),
            terminal_title,
            creator_name: metadata_string(
                entry,
                "creator_name",
                previous.map_or("", |block| block.creator_name.as_str()),
            )?,
            flavor: metadata_string(
                entry,
                "flavor",
                previous.map_or("", |block| block.flavor.as_str()),
            )?,
            host_id: metadata_string(
                entry,
                "host_id",
                previous.map_or("", |block| block.host_id.as_str()),
            )?,
            owner,
        };
        Ok((block, text(entry, "placement")?.to_owned()))
    }

    /// Update metadata from a terminal title response or `title_changed` event.
    ///
    /// # Errors
    /// Rejects missing/nonterminal blocks and titles over the metadata limit.
    pub fn update_terminal_title(&mut self, pane: PaneId, title: &str) -> Result<bool> {
        ensure!(
            title.len() <= MAX_TERMINAL_TITLE_BYTES,
            "Rex terminal title exceeds metadata limit"
        );
        let block = if self.blocks.contains_key(&pane) {
            self.blocks.get_mut(&pane)
        } else {
            self.unplaced.get_mut(&pane)
        }
        .context("Rex block is no longer present")?;
        ensure!(block.is_terminal(), "Rex block is not a terminal");
        if block.terminal_title == title {
            return Ok(false);
        }
        title.clone_into(&mut block.terminal_title);
        let session_id = block.session_id.clone();
        for window in self
            .sessions
            .iter_mut()
            .filter(|session| session.remote_id == session_id)
            .flat_map(|session| &mut session.windows)
        {
            window.refresh_title(&self.blocks);
        }
        Ok(true)
    }
    #[must_use]
    pub fn client_id(&self) -> &str {
        &self.client_id
    }
    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Resolve a currently present pane, rejecting stale IDs.
    ///
    /// # Errors
    /// Returns an error if the pane is absent from the known server workspace
    /// and block inventory.
    pub fn remote_pane(&self, pane: PaneId) -> Result<&str> {
        Ok(&self
            .pane(pane)
            .context("Rex block is no longer present")?
            .remote_id)
    }

    /// Select locally; the worker separately attaches to the chosen session.
    ///
    /// # Errors
    /// Returns an error if the session is no longer present.
    pub fn select_session(&mut self, session: SessionId) -> Result<()> {
        ensure!(
            self.sessions.iter().any(|entry| entry.id == session),
            "Rex session is no longer present"
        );
        self.active_session = Some(session);
        Ok(())
    }

    /// Install bootstrap atomically, preserving aliases on same-instance reconnect.
    ///
    /// # Errors
    /// Returns an error for invalid identity, membership or resource limits.
    pub fn apply_bootstrap(&mut self, value: &Value) -> Result<()> {
        let instance = text(&value["server_status"], "instance_id")?;
        let mut next = if self.instance_id.is_empty() || self.instance_id == instance {
            self.clone()
        } else {
            Self::default()
        };
        next.instance_id = instance.into();
        next.client_id = text(value, "client_id")?.into();
        let covered = next
            .sessions
            .iter()
            .map(|session| session.remote_id.clone())
            .collect();
        next.stage_sessions(&covered);
        next.update_sessions_inner(&value["attach_all"])?;
        for attached in list(value, "attached_sessions")? {
            next.apply_view_inner(&attached["view"])?;
        }
        if let Some(selected) = value
            .get("selected_session")
            .filter(|value| !value.is_null())
        {
            next.apply_view_inner(&selected["view"])?;
            let remote = text(&selected["session"], "session_id")?;
            next.active_session = next
                .sessions
                .iter()
                .find(|session| session.remote_id == remote)
                .map(|session| session.id);
        }
        *self = next;
        Ok(())
    }

    /// Apply a complete session list without discarding unchanged cached views.
    ///
    /// # Errors
    /// Returns an error for malformed, duplicate or excessive session entries.
    pub fn update_sessions(&mut self, value: &Value) -> Result<bool> {
        let mut next = self.clone();
        next.update_sessions_inner(value)?;
        let changed = next.sessions != self.sessions || next.active_session != self.active_session;
        *self = next;
        Ok(changed)
    }

    fn update_sessions_inner(&mut self, value: &Value) -> Result<()> {
        ensure!(value.get("sessions").is_some(), "missing Rex sessions");
        let entries = list(value, "sessions")?;
        ensure!(entries.len() <= MAX_SESSIONS, "too many Rex sessions");
        let mut sessions = Vec::new();
        let mut seen = BTreeSet::new();
        for entry in entries {
            let remote = typed_id(entry, "session_id", "session:")?;
            ensure!(seen.insert(remote.to_owned()), "duplicate Rex session ID");
            let id = SessionId::from_value(self.alias(remote)?).context("invalid session alias")?;
            let mut session = self
                .sessions
                .iter()
                .find(|session| session.id == id)
                .cloned()
                .unwrap_or(SessionView {
                    id,
                    remote_id: remote.into(),
                    title: String::new(),
                    windows: Vec::new(),
                    active_window: None,
                    revision: None,
                });
            // Session lists carry no revision. Once a view exists its label is
            // authoritative; a delayed list must not undo a newer view rename.
            if session.revision.is_none() {
                session.title = label(entry);
            }
            sessions.push(session);
        }
        self.sessions = sessions;
        if !self
            .sessions
            .iter()
            .any(|session| Some(session.id) == self.active_session)
        {
            self.active_session = self.sessions.first().map(|session| session.id);
        }
        self.retain_membership();
        Ok(())
    }

    /// Discard stale per-session revisions. Malformed views leave prior state intact.
    ///
    /// # Errors
    /// Returns an error for invalid identity, membership or resource limits.
    pub fn apply_view(&mut self, value: &Value) -> Result<bool> {
        let mut next = self.clone();
        let changed = next.apply_view_inner(value)?;
        if changed {
            *self = next;
        }
        Ok(changed)
    }

    fn apply_view_inner(&mut self, value: &Value) -> Result<bool> {
        let remote = typed_id(value, "session_id", "session:")?.to_owned();
        let revision = value["revision"]
            .as_u64()
            .context("missing Rex session revision")?;
        if self.sessions.iter().any(|session| {
            session.remote_id == remote && session.revision.is_some_and(|known| known >= revision)
        }) {
            return Ok(false);
        }
        let id = SessionId::from_value(self.alias(&remote)?).context("invalid session alias")?;
        let entries = list(value, "windows")?;
        ensure!(entries.len() <= MAX_WINDOWS, "too many Rex windows");
        let terminal_titles: BTreeMap<_, _> = self
            .blocks
            .values()
            .chain(self.unplaced.values())
            .filter(|block| block.session_id == remote)
            .map(|block| (block.remote_id.clone(), block.terminal_title.clone()))
            .collect();
        let previous: Vec<_> = self
            .blocks
            .iter()
            .filter(|(_, block)| block.session_id == remote)
            .map(|(pane, _)| *pane)
            .collect();
        for pane in previous {
            if let Some(block) = self.blocks.remove(&pane) {
                self.unplaced.insert(pane, block);
            }
        }
        self.splits.retain(|_, target| target.session_id != remote);
        let mut windows = Vec::new();
        let mut seen = BTreeSet::new();
        let mut layer_ids = BTreeSet::new();
        let mut split_ids = BTreeSet::new();
        for entry in entries {
            let window = self.parse_window(
                entry,
                &remote,
                &mut layer_ids,
                &mut split_ids,
                &terminal_titles,
            )?;
            ensure!(seen.insert(window.id), "duplicate Rex window ID");
            windows.push(window);
        }
        ensure!(
            self.blocks.len() <= MAX_BLOCKS
                && self.blocks.len() + self.unplaced.len() <= MAX_STAGED_BLOCKS,
            "too many staged Rex blocks"
        );
        let active_window = optional_text(value, "active_window_id")?
            .map(|remote| {
                windows
                    .iter()
                    .find(|window| window.remote_id == remote)
                    .map(|window| window.id)
                    .context("active Rex window absent from view")
            })
            .transpose()?;
        let session = SessionView {
            id,
            remote_id: remote,
            title: label(value),
            windows,
            active_window,
            revision: Some(revision),
        };
        if let Some(existing) = self.sessions.iter_mut().find(|entry| entry.id == id) {
            *existing = session;
        } else {
            ensure!(self.sessions.len() < MAX_SESSIONS, "too many Rex sessions");
            self.sessions.push(session);
        }
        if self.active_session.is_none() {
            self.active_session = Some(id);
        }
        Ok(true)
    }

    fn parse_block(
        &mut self,
        block: &Value,
        session_id: &str,
        terminal_titles: &BTreeMap<String, String>,
    ) -> Result<(String, PaneId)> {
        let remote = typed_id(block, "block_id", "block:")?.to_owned();
        let pane = PaneId::from_value(self.alias(&remote)?).context("invalid pane alias")?;
        ensure!(!self.blocks.contains_key(&pane), "duplicate Rex block ID");
        if let Some(previous) = self.unplaced.get(&pane) {
            ensure!(
                previous.session_id == session_id,
                "Rex block belongs to another session"
            );
        }
        self.unplaced.remove(&pane);
        self.detached.remove(&pane);
        ensure!(
            self.blocks.len() < MAX_BLOCKS
                && self.blocks.len() + self.unplaced.len() < MAX_STAGED_BLOCKS,
            "too many staged Rex blocks"
        );
        self.blocks.insert(
            pane,
            BlockView {
                id: pane,
                terminal_title: terminal_titles.get(&remote).cloned().unwrap_or_default(),
                remote_id: remote.clone(),
                session_id: session_id.into(),
                label: label(block),
                creator_name: field_string(block, "creator_name"),
                flavor: field_string(block, "flavor"),
                host_id: field_string(block, "host_id"),
                owner: optional_text(block, "owner")?.map(str::to_owned),
            },
        );
        Ok((remote, pane))
    }

    fn parse_window(
        &mut self,
        value: &Value,
        session_id: &str,
        layer_ids: &mut BTreeSet<String>,
        split_ids: &mut BTreeSet<String>,
        terminal_titles: &BTreeMap<String, String>,
    ) -> Result<WindowView> {
        let remote_id = typed_id(value, "window_id", "window:")?.to_owned();
        ensure!(
            !self
                .sessions
                .iter()
                .filter(|session| session.remote_id != session_id)
                .flat_map(|session| &session.windows)
                .any(|window| window.remote_id == remote_id),
            "Rex window belongs to another session"
        );
        let id = WindowId::from_value(self.alias(&remote_id)?).context("invalid window alias")?;
        let mut layers = Vec::new();
        let mut layout = None;
        let mut block_ids = BTreeMap::new();
        ensure!(value.get("layers").is_some(), "missing Rex layers");
        let entries = list(value, "layers")?;
        ensure!(entries.len() <= MAX_BLOCKS, "too many Rex layers");
        for layer in entries {
            let layer_id = typed_id(layer, "layer_id", "layer:")?.to_owned();
            ensure!(layer_ids.insert(layer_id.clone()), "duplicate Rex layer ID");
            let mut blocks = Vec::new();
            for block in list(layer, "blocks")? {
                let (remote, pane) = self.parse_block(block, session_id, terminal_titles)?;
                block_ids.insert(remote, pane);
                blocks.push(pane);
            }
            let bounds = rectangle(&layer["bounds"])?;
            validate_layer_bounds(bounds)?;
            let kind = text(layer, "kind")?.to_owned();
            let raw_layout = layer.get("layout").cloned().unwrap_or(Value::Null);
            validate_layer_layout(&raw_layout, &blocks, &self.remote_ids, split_ids)?;
            let parsed = if raw_layout.is_null() {
                Ok(None)
            } else {
                self.parse_layout(&raw_layout, session_id, &blocks, 0)
                    .map(Some)
            };
            let (parsed, unsupported) = match parsed {
                Ok(parsed) => (parsed, None),
                Err(error) => (None, Some(error.to_string())),
            };
            if kind == "tiled" && layers.is_empty() {
                layout.clone_from(&parsed);
            }
            layers.push(LayerView {
                remote_id: layer_id,
                kind,
                bounds,
                raw_layout,
                layout: parsed,
                blocks,
                unsupported,
            });
        }
        let resolve = |key| -> Result<Option<PaneId>> {
            optional_text(value, key)?
                .map(|remote| {
                    block_ids
                        .get(remote)
                        .copied()
                        .context("focused or zoomed Rex block absent from window")
                })
                .transpose()
        };
        let active_pane = resolve("focused_block_id")?;
        let zoomed = resolve("zoomed_block_id")?;
        let mut window = WindowView {
            id,
            remote_id,
            label: label(value),
            title: String::new(),
            layout,
            active_pane,
            zoomed,
            layers,
        };
        window.refresh_title(&self.blocks);
        Ok(window)
    }

    fn parse_layout(
        &mut self,
        value: &Value,
        session: &str,
        members: &[PaneId],
        depth: usize,
    ) -> Result<ViewLayout> {
        ensure!(depth <= MAX_DEPTH, "Rex layout exceeds depth limit");
        if let Some(remote) = optional_text(value, "block_id")? {
            ensure!(
                value.get("split").is_none_or(Value::is_null),
                "layout has both block and split"
            );
            let alias = self
                .ids
                .get(remote)
                .copied()
                .context("layout block absent from metadata")?;
            let id = PaneId::from_value(alias).context("invalid layout block alias")?;
            ensure!(
                members.contains(&id),
                "layout block belongs to another layer"
            );
            return Ok(ViewLayout::Pane(id));
        }
        let split = value
            .get("split")
            .filter(|value| value.is_object())
            .context("layout node has neither block nor split")?;
        let remote = typed_id(split, "split_id", "split:")?;
        let id = SplitId::from_value(self.alias(remote)?).context("invalid split alias")?;
        let axis = match text(split, "direction")? {
            "horizontal" => SplitAxis::Horizontal,
            "vertical" => SplitAxis::Vertical,
            _ => bail!("unsupported Rex split direction"),
        };
        let ratio = split["ratio"].as_f64().context("invalid Rex split ratio")?;
        ensure!(
            ratio.is_finite() && ratio > 0.0 && ratio < 1.0,
            "invalid Rex split ratio"
        );
        let first = self.parse_layout(&split["before"], session, members, depth + 1)?;
        let second = self.parse_layout(&split["after"], session, members, depth + 1)?;
        let before = self
            .remote_pane(*first.panes().first().context("empty first split child")?)?
            .to_owned();
        let after = self
            .remote_pane(*second.panes().first().context("empty second split child")?)?
            .to_owned();
        ensure!(!self.splits.contains_key(&id), "duplicate Rex split ID");
        self.splits.insert(
            id,
            SplitTarget {
                session_id: session.into(),
                before,
                after,
                axis,
            },
        );
        Ok(ViewLayout::Split {
            id,
            axis,
            ratio,
            first: Box::new(first),
            second: Box::new(second),
        })
    }

    fn alias(&mut self, remote: &str) -> Result<u64> {
        if let Some(id) = self.ids.get(remote) {
            return Ok(*id);
        }
        ensure!(self.ids.len() < 65_536, "Rex identity cache limit reached");
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("Rex identity cache exhausted")?;
        self.ids.insert(remote.into(), self.next_id);
        self.remote_ids.insert(self.next_id, remote.into());
        Ok(self.next_id)
    }

    fn retain_membership(&mut self) {
        let sessions: BTreeSet<_> = self
            .sessions
            .iter()
            .map(|session| session.remote_id.as_str())
            .collect();
        self.blocks
            .retain(|_, block| sessions.contains(block.session_id.as_str()));
        self.unplaced
            .retain(|_, block| sessions.contains(block.session_id.as_str()));
        self.detached
            .retain(|pane| self.unplaced.contains_key(pane));
        self.splits
            .retain(|_, split| sessions.contains(split.session_id.as_str()));
    }

    fn session_for_window(&self, id: WindowId) -> Result<(&SessionView, &WindowView)> {
        self.sessions
            .iter()
            .find_map(|session| {
                session
                    .windows
                    .iter()
                    .find(|window| window.id == id)
                    .map(|window| (session, window))
            })
            .context("Rex window is no longer present")
    }

    fn session(&self, id: SessionId) -> Result<&SessionView> {
        self.sessions
            .iter()
            .find(|session| session.id == id)
            .context("Rex session is no longer present")
    }

    /// Translate shared UI commands without applying local workspace invariants.
    ///
    /// # Errors
    /// Returns an error for stale targets, absent selection or invalid ratios.
    pub fn command(&self, command: &Command) -> Result<Option<ApiCall>> {
        let active = || self.active_session().context("no Rex session selected");
        let shell = || json!({"block":{"flavor":"com.superlogical.terminal.shell"}});
        let (method, params) = match command {
            Command::SelectSession(id) => {
                self.session(*id)?;
                return Ok(None);
            }
            Command::CreateSession { title } => (
                "session.create",
                json!({"label":title,"initial_windows":[{"window_label":"Terminal","layout":shell()}]}),
            ),
            Command::RenameSession { session, title } => (
                "session.set_label",
                json!({"session_id":self.session(*session)?.remote_id,"label":title}),
            ),
            Command::CloseSession(id) => (
                "session.destroy",
                json!({"session_id":self.session(*id)?.remote_id}),
            ),
            Command::CreateWindow { session, title } => (
                "session.new_window",
                json!({"session_id":self.session(*session)?.remote_id,"window_label":title,"layout":shell()}),
            ),
            Command::SelectWindow(id)
            | Command::CloseWindow(id)
            | Command::RenameWindow { window: id, .. } => {
                let (session, window) = self.session_for_window(*id)?;
                let mut params =
                    json!({"session_id":session.remote_id,"window_id":window.remote_id});
                let method = match command {
                    Command::SelectWindow(_) => "session.focus_window",
                    Command::RenameWindow { title, .. } => {
                        params["label"] = json!(title);
                        "session.set_window_label"
                    }
                    _ => "session.close_window",
                };
                (method, params)
            }
            Command::MovePane { .. } | Command::SwapPanes { .. } => {
                self.relocation_command(command)?
            }
            Command::MoveWindow { window, before } => self.move_window_command(*window, *before)?,
            Command::RestorePane { pane, title } => self.restore_pane_command(*pane, title)?,
            Command::FloatPane { pane, bounds } => self.float_pane_command(*pane, *bounds)?,
            Command::ResizeLayer { .. }
            | Command::RaiseLayer { .. }
            | Command::LowerLayer { .. }
            | Command::CloseLayer { .. } => self.layer_command(command)?,
            Command::NextWindow => (
                "session.focus_next_window",
                json!({"session_id":active()?.remote_id}),
            ),
            Command::PreviousWindow => (
                "session.focus_previous_window",
                json!({"session_id":active()?.remote_id}),
            ),
            Command::FocusNextPane => (
                "session.focus_next_pane",
                json!({"session_id":active()?.remote_id}),
            ),
            Command::FocusDirection { direction, wrap } => (
                "session.focus_direction",
                json!({"session_id":active()?.remote_id,"direction":direction_name(*direction),"wrap":wrap}),
            ),
            Command::FocusPane(id)
            | Command::ClosePane(id)
            | Command::ToggleZoom(id)
            | Command::SplitPane { pane: id, .. }
            | Command::DetachPane(id) => self.block_command(*id, command)?,
            Command::ResizeSplit { split, ratio } => {
                ensure!(*ratio > 0 && *ratio < 1000, "invalid split ratio");
                let target = self
                    .splits
                    .get(split)
                    .context("Rex split is no longer present")?;
                (
                    "session.resize_pane",
                    json!({"session_id":target.session_id,"direction":axis_name(target.axis),"before_block_id":target.before,"after_block_id":target.after,"ratio":f64::from(*ratio)/1000.0}),
                )
            }
        };
        Ok(Some(ApiCall { method, params }))
    }

    fn float_pane_command(&self, pane: PaneId, bounds: [f64; 4]) -> Result<(&'static str, Value)> {
        validate_layer_bounds(bounds)?;
        let block = self.pane(pane).context("Rex block is no longer present")?;
        let layer = self
            .sessions
            .iter()
            .flat_map(|session| &session.windows)
            .flat_map(|window| &window.layers)
            .find(|layer| layer.blocks.contains(&pane))
            .context("Rex block is not placed")?;
        ensure!(!layer.is_floating(), "Rex block is already floating");
        Ok((
            "session.float_block",
            json!({"session_id":block.session_id,"block_id":block.remote_id,"bounds":bounds}),
        ))
    }

    fn layer_command(&self, command: &Command) -> Result<(&'static str, Value)> {
        let (session, layer, method, bounds) = match command {
            Command::ResizeLayer {
                session,
                layer,
                bounds,
            } => (*session, layer, "session.resize_layer", Some(*bounds)),
            Command::RaiseLayer { session, layer } => {
                (*session, layer, "session.raise_layer", None)
            }
            Command::LowerLayer { session, layer } => {
                (*session, layer, "session.lower_layer", None)
            }
            Command::CloseLayer { session, layer } => {
                (*session, layer, "session.close_layer", None)
            }
            _ => bail!("invalid Rex layer command"),
        };
        let session = self.session(session)?;
        let layer = session
            .windows
            .iter()
            .flat_map(|window| &window.layers)
            .find(|candidate| candidate.remote_id == *layer)
            .context("Rex layer is no longer present in session")?;
        ensure!(
            layer.is_floating(),
            "Rex tiled layer cannot be moved or closed independently"
        );
        let mut params = json!({"session_id":session.remote_id,"layer_id":layer.remote_id});
        if let Some(bounds) = bounds {
            validate_layer_bounds(bounds)?;
            params["bounds"] = json!(bounds);
        }
        Ok((method, params))
    }

    /// The executor must finish restoration by moving the existing block beside
    /// this disposable carrier and closing only the carrier, never its window.
    fn restore_pane_command(&self, pane: PaneId, title: &str) -> Result<(&'static str, Value)> {
        let block = self.pane(pane).context("Rex block is no longer present")?;
        ensure!(self.is_detached(pane), "Rex restore source is not detached");
        ensure!(
            !title.trim().is_empty() && title.len() <= 512 && title.chars().count() <= 128,
            "invalid Rex restore window label"
        );
        Ok((
            "session.new_window",
            json!({
                "session_id":block.session_id,"window_label":title,"focus":true,
                "layout":{"block":{"flavor":"com.superlogical.terminal.shell","label":"Restore anchor","options":{"shell":"none","command":["/bin/true"],"exit":{"on_completion":false}}}}
            }),
        ))
    }

    fn move_window_command(
        &self,
        window: WindowId,
        before: Option<WindowId>,
    ) -> Result<(&'static str, Value)> {
        let (session, window) = self.session_for_window(window)?;
        let mut params = json!({"session_id":session.remote_id,"window_id":window.remote_id});
        if let Some(before) = before {
            let (other_session, other_window) = self.session_for_window(before)?;
            ensure!(
                session.id == other_session.id,
                "Rex windows belong to different sessions"
            );
            ensure!(
                window.id != other_window.id,
                "cannot move a Rex window before itself"
            );
            params["before_window_id"] = json!(other_window.remote_id);
        }
        Ok(("session.move_window", params))
    }

    fn relocation_command(&self, command: &Command) -> Result<(&'static str, Value)> {
        let (pane, other) = match command {
            Command::MovePane { pane, anchor, .. } => (*pane, *anchor),
            Command::SwapPanes { pane, other } => (*pane, *other),
            _ => bail!("invalid Rex relocation command"),
        };
        ensure!(pane != other, "Rex relocation requires two distinct panes");
        let block = self
            .pane(pane)
            .context("Rex source block is no longer present")?;
        let other_block = self
            .blocks
            .get(&other)
            .context("Rex destination block is no longer present")?;
        ensure!(
            block.session_id == other_block.session_id,
            "Rex blocks belong to different sessions"
        );
        ensure!(
            self.pane_is_placed(other),
            "Rex destination block is not in a window"
        );
        let mut params = json!({"session_id":block.session_id,"block_id":block.remote_id});
        match command {
            Command::MovePane {
                axis,
                before,
                ratio,
                ..
            } => {
                ensure!(
                    ratio.is_finite() && *ratio > 0.0 && *ratio < 1.0,
                    "invalid Rex move split ratio"
                );
                params["anchor_block_id"] = json!(other_block.remote_id);
                params["direction"] = json!(axis_name(*axis));
                params["side"] = json!(if *before { "before" } else { "after" });
                params["ratio"] = json!(ratio);
                Ok(("session.move_block", params))
            }
            Command::SwapPanes { .. } => {
                ensure!(
                    self.pane_is_placed(pane),
                    "Rex source block is not in a window"
                );
                params["other_block_id"] = json!(other_block.remote_id);
                Ok(("session.swap_blocks", params))
            }
            _ => bail!("invalid Rex relocation command"),
        }
    }

    fn pane_is_placed(&self, pane: PaneId) -> bool {
        self.sessions
            .iter()
            .flat_map(|session| &session.windows)
            .flat_map(|window| &window.layers)
            .any(|layer| layer.blocks.contains(&pane))
    }

    fn block_command(&self, id: PaneId, command: &Command) -> Result<(&'static str, Value)> {
        let block = self.pane(id).context("Rex block is no longer present")?;
        if matches!(
            command,
            Command::FocusPane(_) | Command::ToggleZoom(_) | Command::SplitPane { .. }
        ) {
            ensure!(self.pane_is_placed(id), "Rex block is not in a window");
        }
        let params = json!({"session_id":block.session_id,"block_id":block.remote_id});
        Ok(match command {
            Command::FocusPane(_) => ("session.focus_block", params),
            Command::ClosePane(_) => ("block.close", params),
            Command::DetachPane(_) => ("session.detach_block", params),
            Command::ToggleZoom(_) => ("session.zoom_block", params),
            Command::SplitPane { axis, .. } => (
                "session.new_split",
                json!({"session_id":block.session_id,"anchor_id":block.remote_id,"direction":axis_name(*axis),"side":"after","layout":{"block":{"flavor":"com.superlogical.terminal.shell"}},"focus":true}),
            ),
            _ => bail!("invalid block command"),
        })
    }
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing Rex {key}"))
}

fn typed_id<'a>(value: &'a Value, key: &str, prefix: &str) -> Result<&'a str> {
    let id = text(value, key)?;
    ensure!(
        id.starts_with(prefix) && id.len() > prefix.len() && id.len() <= 256,
        "invalid Rex {key}"
    );
    Ok(id)
}

fn optional_text<'a>(value: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        _ => bail!("invalid Rex {key}"),
    }
}

fn list<'a>(value: &'a Value, key: &str) -> Result<&'a [Value]> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(entries)) => Ok(entries),
        _ => bail!("invalid Rex {key}"),
    }
}

fn label(value: &Value) -> String {
    field_string(value, "label")
}
fn field_string(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().into()
}

fn metadata_string(value: &Value, key: &str, fallback: &str) -> Result<String> {
    match value.get(key) {
        None => Ok(fallback.to_owned()),
        Some(Value::String(text)) if text.len() <= 16 * 1024 => Ok(text.clone()),
        _ => bail!("invalid Rex block metadata {key}"),
    }
}

fn rectangle(value: &Value) -> Result<[f64; 4]> {
    let entries = value.as_array().context("missing layer bounds")?;
    ensure!(entries.len() == 4, "invalid layer bounds");
    let mut bounds = [0.0; 4];
    for (out, entry) in bounds.iter_mut().zip(entries) {
        *out = entry.as_f64().context("invalid layer bound")?;
        ensure!(out.is_finite(), "nonfinite layer bound");
    }
    ensure!(
        bounds[2] >= 0.0 && bounds[3] >= 0.0,
        "negative layer dimensions"
    );
    Ok(bounds)
}

/// Mutation bounds use the live API's normalized, inside-window contract.
fn validate_layer_bounds([x, y, width, height]: [f64; 4]) -> Result<()> {
    ensure!(
        [x, y, width, height].iter().all(|value| value.is_finite()),
        "nonfinite floating layer bounds"
    );
    ensure!(
        x >= 0.0
            && y >= 0.0
            && width > 0.0
            && height > 0.0
            && x + width <= 1.0
            && y + height <= 1.0,
        "floating layer bounds must fit inside the window"
    );
    Ok(())
}

/// Check the complete remote tree before allocating a representable UI tree.
/// Unsupported rendering must not hide duplicate identities or unbounded trees.
fn validate_layer_layout(
    value: &Value,
    blocks: &[PaneId],
    remote_ids: &BTreeMap<u64, String>,
    split_ids: &mut BTreeSet<String>,
) -> Result<()> {
    let expected: BTreeSet<_> = blocks
        .iter()
        .filter_map(|pane| remote_ids.get(&pane.value()).map(String::as_str))
        .collect();
    let mut found = BTreeSet::new();
    let mut pending = vec![(value, 0)];
    let mut nodes = 0;
    while let Some((node, depth)) = pending.pop() {
        if node.is_null() {
            ensure!(
                depth == 0 && expected.is_empty(),
                "empty Rex layout has blocks"
            );
            continue;
        }
        nodes += 1;
        ensure!(
            nodes <= MAX_LAYOUT_NODES && depth <= MAX_DEPTH,
            "Rex layout exceeds resource limits"
        );
        rectangle(&node["rect"])?;
        if optional_text(node, "block_id")?.is_some() {
            let id = typed_id(node, "block_id", "block:")?;
            ensure!(expected.contains(id), "layout block absent from layer");
            ensure!(found.insert(id), "duplicate Rex layout block");
            ensure!(
                node.get("split").is_none_or(Value::is_null),
                "layout has both block and split"
            );
        } else {
            let split = node
                .get("split")
                .filter(|value| value.is_object())
                .context("layout node has neither block nor split")?;
            let id = typed_id(split, "split_id", "split:")?;
            ensure!(split_ids.insert(id.into()), "duplicate Rex split ID");
            text(split, "direction")?;
            let ratio = split["ratio"].as_f64().context("missing Rex split ratio")?;
            ensure!(
                ratio.is_finite() && ratio > 0.0 && ratio < 1.0,
                "invalid Rex split ratio"
            );
            pending.push((&split["before"], depth + 1));
            pending.push((&split["after"], depth + 1));
        }
    }
    ensure!(found == expected, "Rex layer metadata and layout differ");
    Ok(())
}

const fn axis_name(axis: SplitAxis) -> &'static str {
    match axis {
        SplitAxis::Horizontal => "horizontal",
        SplitAxis::Vertical => "vertical",
    }
}

const fn direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::Left => "left",
        Direction::Right => "right",
        Direction::Up => "up",
        Direction::Down => "down",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(revision: u64) -> Value {
        json!({"session_id":"session:a","label":"Work","revision":revision,"active_window_id":"window:a","windows":[{
            "window_id":"window:a","label":"Shell","focused_block_id":"block:a","layers":[{
                "layer_id":"layer:a","kind":"tiled","bounds":[0,0,1,1],"layout":{"block_id":"block:a","rect":[0,0,1,1]},
                "blocks":[{"block_id":"block:a","creator_name":"com.superlogical.terminal","owner":"client:other"}]
            }]
        }]})
    }

    fn inventory_block(id: &str, placement: &str) -> Value {
        json!({"block_id":id,"label":"shell","creator_name":"com.superlogical.terminal","flavor":"com.superlogical.terminal.shell","placement":placement})
    }

    #[test]
    fn detached_inventory_retains_aliases_titles_and_unknown_placements() -> Result<()> {
        let mut state = WorkspaceState::default();
        state.apply_view(&view(1))?;
        let pane = state.pane_ids()[0];
        let session = state.active_session().context("session")?.id();
        state.update_terminal_title(pane, "live title")?;
        state.apply_view(&json!({"session_id":"session:a","revision":2,"windows":[]}))?;
        assert_eq!(state.pane_ids(), vec![pane]);
        assert_eq!(state.placed_pane_ids(), []);
        assert_eq!(state.detached_blocks(session), [] as [&BlockView; 0]);
        let list = json!({"blocks":[inventory_block("block:a","future-placement")]});
        assert!(state.apply_block_list("session:a", &list)?);
        assert!(state.is_detached(pane));
        assert_eq!(
            state.detached_blocks(session)[0].terminal_title,
            "live title"
        );
        assert!(!state.apply_block_list("session:a", &list)?);
        state.update_terminal_title(pane, "new live title")?;
        state.apply_view(&view(3))?;
        assert_eq!(state.placed_pane_ids(), vec![pane]);
        assert!(!state.is_detached(pane));
        assert_eq!(state.detached_blocks(session), [] as [&BlockView; 0]);
        assert_eq!(
            state.pane(pane).context("restored pane")?.terminal_title,
            "new live title"
        );
        // A delayed list cannot detach or relabel a currently placed block.
        state.apply_block_list("session:a", &list)?;
        assert!(!state.is_detached(pane));
        assert_eq!(state.pane(pane).context("placed pane")?.label, "");
        state.apply_block_list("session:a", &json!({"blocks":[]}))?;
        assert_eq!(state.placed_pane_ids(), vec![pane]);
        Ok(())
    }

    #[test]
    fn malformed_or_foreign_inventory_is_atomic() -> Result<()> {
        let mut state = relocation_state()?;
        state.apply_block_list(
            "session:a",
            &json!({"blocks":[inventory_block("block:detached","detached")]}),
        )?;
        let ids = state.pane_ids();
        for value in [
            json!({}),
            json!({"blocks":[inventory_block("block:c","detached")]}),
            json!({"blocks":[inventory_block("block:new","detached"),inventory_block("block:new","detached")]}),
            json!({"blocks":[{"block_id":"block:new","label":7,"placement":"detached"}]}),
            json!({"blocks":[{"block_id":"block:new","label":"x","placement":"detached","owner":5}]}),
            json!({"blocks":[{"block_id":"block:new","label":"x","placement":"detached","window_id":"window:c"}]}),
        ] {
            assert!(state.apply_block_list("session:a", &value).is_err());
            assert_eq!(state.pane_ids(), ids);
        }
        assert!(
            state
                .apply_block_list("session:missing", &json!({"blocks":[]}))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn detach_and_move_accept_detached_source_but_swap_requires_placement() -> Result<()> {
        let mut state = relocation_state()?;
        let pane = state.pane_ids()[0];
        let anchor = state.pane_ids()[1];
        let detach = state
            .command(&Command::DetachPane(pane))?
            .context("detach")?;
        assert_eq!(detach.method, "session.detach_block");
        assert_eq!(detach.params["block_id"], "block:a");
        let mut current = view(2);
        let mut second = current["windows"][0].clone();
        second["window_id"] = json!("window:b");
        second["focused_block_id"] = json!("block:b");
        second["layers"][0]["layer_id"] = json!("layer:b");
        second["layers"][0]["layout"]["block_id"] = json!("block:b");
        second["layers"][0]["blocks"][0]["block_id"] = json!("block:b");
        current["windows"] = json!([second]);
        current["active_window_id"] = json!("window:b");
        state.apply_view(&current)?;
        state.apply_block_list("session:a",&json!({"blocks":[inventory_block("block:a","detached"),inventory_block("block:b","placed")]}))?;
        assert!(state.is_detached(pane));
        assert!(state.command(&Command::DetachPane(pane))?.is_some());
        assert!(state.command(&Command::ClosePane(pane))?.is_some());
        assert!(
            state
                .command(&Command::MovePane {
                    pane,
                    anchor,
                    axis: SplitAxis::Horizontal,
                    before: false,
                    ratio: 0.5
                })?
                .is_some()
        );
        assert!(
            state
                .command(&Command::SwapPanes {
                    pane,
                    other: anchor
                })
                .is_err()
        );
        assert!(
            state
                .command(&Command::MovePane {
                    pane: anchor,
                    anchor: pane,
                    axis: SplitAxis::Horizontal,
                    before: false,
                    ratio: 0.5
                })
                .is_err()
        );
        assert!(state.command(&Command::FocusPane(pane)).is_err());
        let restore = state
            .command(&Command::RestorePane {
                pane,
                title: "Restored".into(),
            })?
            .context("restore carrier")?;
        assert_eq!(restore.method, "session.new_window");
        assert_eq!(restore.params["session_id"], "session:a");
        assert_eq!(
            restore.params["layout"]["block"]["options"]["exit"]["on_completion"],
            false
        );
        assert_eq!(
            restore.params["layout"]["block"]["options"]["command"],
            json!(["/bin/true"])
        );
        assert!(
            state
                .command(&Command::RestorePane {
                    pane: anchor,
                    title: "Restored".into()
                })
                .is_err()
        );
        assert!(
            state
                .command(&Command::RestorePane {
                    pane,
                    title: String::new()
                })
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn authoritative_inventory_removal_prunes_unplaced_membership() -> Result<()> {
        let mut state = WorkspaceState::default();
        state.apply_view(&view(1))?;
        state.apply_block_list(
            "session:a",
            &json!({"blocks":[inventory_block("block:detached","detached")]}),
        )?;
        let detached = state
            .pane_ids()
            .into_iter()
            .find(|pane| state.is_detached(*pane))
            .context("detached")?;
        state.apply_block_list("session:a", &json!({"blocks":[]}))?;
        assert!(state.pane(detached).is_none());
        assert!(!state.is_detached(detached));
        state.update_sessions(&json!({"sessions":[]}))?;
        assert_eq!(state.pane_ids(), []);
        Ok(())
    }

    #[test]
    fn block_inventory_bound_includes_placed_and_detached_records() -> Result<()> {
        let mut state = WorkspaceState::default();
        state.apply_view(&view(1))?;
        let before = state.pane_ids();
        let blocks: Vec<_> = (0..MAX_BLOCKS)
            .map(|index| inventory_block(&format!("block:d{index}"), "detached"))
            .collect();
        assert!(
            state
                .apply_block_list("session:a", &json!({"blocks":blocks}))
                .is_err()
        );
        assert_eq!(state.pane_ids(), before);
        Ok(())
    }

    #[test]
    fn new_server_instance_clears_detached_metadata_and_terminal_titles() -> Result<()> {
        let mut state = WorkspaceState::default();
        let bootstrap = |instance| json!({"server_status":{"instance_id":instance},"client_id":"client:a","attach_all":{"sessions":[{"session_id":"session:a"}]},"attached_sessions":[{"view":view(1)}]});
        state.apply_bootstrap(&bootstrap("instance:a"))?;
        let pane = state.placed_pane_ids()[0];
        state.update_terminal_title(pane, "prior server")?;
        state.apply_block_list(
            "session:a",
            &json!({"blocks":[inventory_block("block:detached","detached")]}),
        )?;
        assert_eq!(state.pane_ids().len(), 2);
        state.apply_bootstrap(&bootstrap("instance:b"))?;
        assert_eq!(state.pane_ids().len(), 1);
        assert_eq!(
            state.detached_blocks(state.active_session().context("session")?.id()),
            [] as [&BlockView; 0]
        );
        assert_eq!(
            state
                .pane(state.placed_pane_ids()[0])
                .context("new pane")?
                .terminal_title,
            ""
        );
        Ok(())
    }

    #[test]
    fn batch_inventory_handles_cross_session_churn_at_capacity() -> Result<()> {
        let mut state = WorkspaceState::default();
        for id in ["session:a", "session:b"] {
            state.apply_view(&json!({"session_id":id,"revision":1,"windows":[]}))?;
        }
        let old: Vec<_> = (0..MAX_BLOCKS)
            .map(|index| inventory_block(&format!("block:a-{index}"), "detached"))
            .collect();
        state.apply_block_list("session:a", &json!({"blocks":old}))?;
        let new: Vec<_> = (0..MAX_BLOCKS)
            .map(|index| inventory_block(&format!("block:b-{index}"), "detached"))
            .collect();
        let replacement = json!({"blocks":new});
        assert!(state.apply_block_list("session:b", &replacement).is_err());
        assert!(state.apply_block_lists(&[
            ("session:b".into(), replacement),
            ("session:a".into(), json!({"blocks":[]}))
        ])?);
        assert_eq!(state.pane_ids().len(), MAX_BLOCKS);
        let session_a = state
            .sessions()
            .iter()
            .find(|session| session.remote_id() == "session:a")
            .context("session a")?
            .id();
        assert_eq!(state.detached_blocks(session_a), [] as [&BlockView; 0]);
        Ok(())
    }

    fn inventory_for_view(view: &Value) -> Value {
        let mut blocks = Vec::new();
        for window in view["windows"].as_array().into_iter().flatten() {
            for block in window["layers"][0]["blocks"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let mut block = block.clone();
                block["placement"] = json!("placed");
                block["window_id"] = window["window_id"].clone();
                blocks.push(block);
            }
        }
        json!({"blocks":blocks})
    }

    fn capacity_view(suffix: &str, revision: u64, count: usize) -> Value {
        fn layout(ids: &[String]) -> Value {
            if ids.len() == 1 {
                return json!({"block_id":ids[0],"rect":[0,0,1,1]});
            }
            let middle = ids.len() / 2;
            json!({"rect":[0,0,1,1],"split":{"split_id":format!("split:{}-{}",ids[0],ids.len()),"direction":"horizontal","ratio":0.5,"before":layout(&ids[..middle]),"after":layout(&ids[middle..])}})
        }
        if count == 0 {
            return json!({"session_id":format!("session:{suffix}"),"revision":revision,"windows":[]});
        }
        let ids: Vec<_> = (0..count)
            .map(|index| format!("block:{suffix}-{index}"))
            .collect();
        let blocks: Vec<_> = ids.iter().map(|id|json!({"block_id":id,"label":"shell","creator_name":"com.superlogical.terminal"})).collect();
        json!({"session_id":format!("session:{suffix}"),"revision":revision,"active_window_id":format!("window:{suffix}"),"windows":[{"window_id":format!("window:{suffix}"),"focused_block_id":ids[0],"layers":[{"layer_id":format!("layer:{suffix}"),"kind":"tiled","bounds":[0,0,1,1],"layout":layout(&ids),"blocks":blocks}]}]})
    }

    #[test]
    fn paired_snapshots_and_bootstrap_replace_placed_membership_atomically_at_capacity()
    -> Result<()> {
        let mut baseline = WorkspaceState::default();
        baseline.apply_view(&capacity_view("a", 1, 0))?;
        baseline.apply_view(&capacity_view("b", 1, MAX_BLOCKS))?;
        let first = baseline.pane_ids()[0];
        baseline.update_terminal_title(first, "retained title")?;
        baseline.instance_id = "instance:a".into();
        let a = capacity_view("a", 2, 1);
        let b = capacity_view("b", 2, MAX_BLOCKS - 1);
        let mut paired = baseline.clone();
        assert!(paired.apply_snapshots(&[
            (a.clone(), inventory_for_view(&a)),
            (b.clone(), inventory_for_view(&b))
        ])?);
        assert_eq!(paired.placed_pane_ids().len(), MAX_BLOCKS);
        assert_eq!(paired.pane_ids().len(), MAX_BLOCKS);
        assert_eq!(
            paired.pane(first).context("retained block")?.terminal_title,
            "retained title"
        );
        baseline.apply_bootstrap(&json!({"server_status":{"instance_id":"instance:a"},"client_id":"client:a","attach_all":{"sessions":[{"session_id":"session:a"},{"session_id":"session:b"}]},"attached_sessions":[{"view":a},{"view":b}]}))?;
        assert_eq!(baseline.placed_pane_ids().len(), MAX_BLOCKS);
        baseline.apply_block_lists(&[
            ("session:a".into(), inventory_for_view(&a)),
            ("session:b".into(), inventory_for_view(&b)),
        ])?;
        assert_eq!(baseline.pane_ids().len(), MAX_BLOCKS);
        assert_eq!(
            baseline
                .pane(first)
                .context("retained bootstrap block")?
                .terminal_title,
            "retained title"
        );
        Ok(())
    }

    #[test]
    fn invalid_snapshot_batches_leave_views_inventories_and_aliases_unchanged() -> Result<()> {
        let mut state = WorkspaceState::default();
        state.apply_view(&view(3))?;
        state.apply_block_list(
            "session:a",
            &json!({"blocks":[inventory_block("block:detached","detached")]}),
        )?;
        let before = state.clone();
        assert!(
            state
                .apply_snapshots(&[(view(2), json!({"blocks":[]}))])
                .is_err()
        );
        assert!(
            state
                .apply_snapshots(&[
                    (view(3), json!({"blocks":[]})),
                    (view(3), json!({"blocks":[]}))
                ])
                .is_err()
        );
        assert!(
            state
                .apply_snapshots(&[(view(4), json!({"blocks":[{"block_id":"broken"}]}))])
                .is_err()
        );
        assert_eq!(state.sessions, before.sessions);
        assert_eq!(state.pane_ids(), before.pane_ids());
        assert_eq!(state.ids, before.ids);
        assert!(
            state
                .apply_block_lists(&[
                    ("session:a".into(), json!({"blocks":[]})),
                    ("session:a".into(), json!({"blocks":[]}))
                ])
                .is_err()
        );
        assert_eq!(state.pane_ids(), before.pane_ids());
        Ok(())
    }

    fn relocation_state() -> Result<WorkspaceState> {
        let mut state = WorkspaceState::default();
        let mut session = view(1);
        let mut second = session["windows"][0].clone();
        second["window_id"] = json!("window:b");
        second["focused_block_id"] = json!("block:b");
        second["layers"][0]["layer_id"] = json!("layer:b");
        second["layers"][0]["layout"]["block_id"] = json!("block:b");
        second["layers"][0]["blocks"][0]["block_id"] = json!("block:b");
        session["windows"]
            .as_array_mut()
            .context("windows")?
            .push(second);
        state.apply_view(&session)?;
        let mut foreign = view(1);
        foreign["session_id"] = json!("session:foreign");
        foreign["active_window_id"] = json!("window:c");
        foreign["windows"][0]["window_id"] = json!("window:c");
        foreign["windows"][0]["focused_block_id"] = json!("block:c");
        foreign["windows"][0]["layers"][0]["layer_id"] = json!("layer:c");
        foreign["windows"][0]["layers"][0]["layout"]["block_id"] = json!("block:c");
        foreign["windows"][0]["layers"][0]["blocks"][0]["block_id"] = json!("block:c");
        state.apply_view(&foreign)?;
        Ok(state)
    }

    #[test]
    fn move_and_swap_requests_use_same_session_ids_and_full_precision_ratios() -> Result<()> {
        let state = relocation_state()?;
        let panes = state.pane_ids();
        let pane = panes[0];
        let anchor = panes[1];
        let moved = state
            .command(&Command::MovePane {
                pane,
                anchor,
                axis: SplitAxis::Vertical,
                before: true,
                ratio: 0.38125,
            })?
            .context("move call")?;
        assert_eq!(moved.method, "session.move_block");
        assert_eq!(
            moved.params,
            json!({"session_id":"session:a","block_id":"block:a","anchor_block_id":"block:b","direction":"vertical","side":"before","ratio":0.38125})
        );
        let swapped = state
            .command(&Command::SwapPanes {
                pane,
                other: anchor,
            })?
            .context("swap call")?;
        assert_eq!(swapped.method, "session.swap_blocks");
        assert_eq!(swapped.params["other_block_id"], "block:b");
        assert_eq!(state.pane_ids(), panes);
        Ok(())
    }

    #[test]
    fn relocation_rejects_self_foreign_targets_and_invalid_ratios() -> Result<()> {
        let state = relocation_state()?;
        let panes = state.pane_ids();
        let (pane, anchor, foreign) = (panes[0], panes[1], panes[2]);
        for other in [pane, foreign] {
            assert!(state.command(&Command::SwapPanes { pane, other }).is_err());
            assert!(
                state
                    .command(&Command::MovePane {
                        pane,
                        anchor: other,
                        axis: SplitAxis::Horizontal,
                        before: false,
                        ratio: 0.5
                    })
                    .is_err()
            );
        }
        for ratio in [0.0, 1.0, -0.1, f64::NAN, f64::INFINITY] {
            assert!(
                state
                    .command(&Command::MovePane {
                        pane,
                        anchor,
                        axis: SplitAxis::Horizontal,
                        before: false,
                        ratio
                    })
                    .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn tab_reorder_validates_membership_and_omits_end_anchor() -> Result<()> {
        let state = relocation_state()?;
        let windows = state.sessions()[0].windows();
        let (window, before) = (windows[0].id(), windows[1].id());
        let call = state
            .command(&Command::MoveWindow {
                window,
                before: Some(before),
            })?
            .context("window move")?;
        assert_eq!(call.method, "session.move_window");
        assert_eq!(call.params["before_window_id"], "window:b");
        let end = state
            .command(&Command::MoveWindow {
                window,
                before: None,
            })?
            .context("window end")?;
        assert!(end.params.get("before_window_id").is_none());
        assert!(
            state
                .command(&Command::MoveWindow {
                    window,
                    before: Some(window)
                })
                .is_err()
        );
        let foreign = state.sessions()[1].windows()[0].id();
        assert!(
            state
                .command(&Command::MoveWindow {
                    window,
                    before: Some(foreign)
                })
                .is_err()
        );
        Ok(())
    }

    fn titled_view(revision: u64) -> Value {
        let mut value = view(revision);
        value["windows"][0]["window_id"] = json!("window:01234567-89ab-cdef-0123-456789abcdef");
        value["active_window_id"] = value["windows"][0]["window_id"].clone();
        value["windows"][0]["label"] = json!("Window 89abcdef");
        value["windows"][0]["layers"][0]["blocks"][0]["label"] = json!("shell");
        value
    }

    #[test]
    fn dynamic_titles_replace_generated_window_names_and_survive_view_refresh() -> Result<()> {
        let mut state = WorkspaceState::default();
        state.apply_view(&titled_view(1))?;
        let pane = state.pane_ids()[0];
        assert!(state.update_terminal_title(pane, "Project terminal")?);
        assert!(!state.update_terminal_title(pane, "Project terminal")?);
        state.apply_view(&titled_view(2))?;
        let session = state.active_session().context("session")?;
        assert_eq!(session.label(), "Work");
        assert_eq!(session.title(), "Work");
        let window = session.active_window().context("window")?;
        assert_eq!(window.label(), "Window 89abcdef");
        assert_eq!(window.title(), "Project terminal");
        let block = state.pane(pane).context("block")?;
        assert_eq!(block.label, "shell");
        assert_eq!(block.terminal_title, "Project terminal");
        assert_eq!(block.title(), "Project terminal");
        state.update_terminal_title(pane, "")?;
        assert_eq!(
            state
                .active_session()
                .and_then(SessionView::active_window)
                .context("window")?
                .title(),
            "Window 89abcdef"
        );
        Ok(())
    }

    #[test]
    fn explicit_window_and_block_labels_win_over_terminal_titles() -> Result<()> {
        let mut state = WorkspaceState::default();
        let mut value = titled_view(1);
        value["windows"][0]["label"] = json!("Terminal 2");
        value["windows"][0]["layers"][0]["blocks"][0]["label"] = json!("Build logs");
        state.apply_view(&value)?;
        let pane = state.pane_ids()[0];
        state.update_terminal_title(pane, "Editor running")?;
        assert_eq!(
            state
                .active_session()
                .and_then(SessionView::active_window)
                .context("window")?
                .title(),
            "Terminal 2"
        );
        assert_eq!(state.pane(pane).context("pane")?.title(), "Build logs");
        value["revision"] = json!(2);
        value["windows"][0]["label"] = json!("Window 89abcdef");
        state.apply_view(&value)?;
        assert_eq!(
            state
                .active_session()
                .and_then(SessionView::active_window)
                .context("window")?
                .title(),
            "Build logs"
        );
        Ok(())
    }

    #[test]
    fn terminal_titles_are_cleared_on_server_replacement() -> Result<()> {
        let bootstrap = |instance| {
            json!({
                "server_status":{"instance_id":instance},"client_id":"client:me",
                "attach_all":{"sessions":[{"session_id":"session:a","label":"Work"}]},
                "selected_session":{"session":{"session_id":"session:a"},"view":titled_view(1)}
            })
        };
        let mut state = WorkspaceState::default();
        state.apply_bootstrap(&bootstrap("first"))?;
        let pane = state.pane_ids()[0];
        state.update_terminal_title(pane, "Old process")?;
        state.apply_bootstrap(&bootstrap("first"))?;
        assert_eq!(
            state.pane(pane).context("pane")?.terminal_title,
            "Old process"
        );
        state.apply_bootstrap(&bootstrap("replacement"))?;
        assert_eq!(
            state
                .pane(state.pane_ids()[0])
                .context("replacement pane")?
                .terminal_title,
            ""
        );
        assert_eq!(
            state
                .active_session()
                .and_then(SessionView::active_window)
                .context("window")?
                .title(),
            "Window 89abcdef"
        );
        Ok(())
    }

    #[test]
    fn terminal_title_limits_leave_existing_metadata_unchanged() -> Result<()> {
        let mut state = WorkspaceState::default();
        state.apply_view(&titled_view(1))?;
        let pane = state.pane_ids()[0];
        state.update_terminal_title(pane, "Existing")?;
        assert!(
            state
                .update_terminal_title(pane, &"x".repeat(MAX_TERMINAL_TITLE_BYTES + 1))
                .is_err()
        );
        assert_eq!(state.pane(pane).context("pane")?.terminal_title, "Existing");
        Ok(())
    }

    #[test]
    fn aliases_survive_view_updates_and_stale_views_do_not_replace_state() -> Result<()> {
        let mut state = WorkspaceState::default();
        assert!(state.apply_view(&view(2))?);
        let pane = state.pane_ids()[0];
        assert!(!state.apply_view(&view(1))?);
        assert!(state.apply_view(&view(3))?);
        assert_eq!(state.pane_ids(), vec![pane]);
        assert_eq!(state.remote_pane(pane)?, "block:a");
        assert!(
            !state
                .pane(pane)
                .context("pane")?
                .size_authority("client:me")
        );
        Ok(())
    }

    #[test]
    fn empty_remote_session_does_not_require_a_fake_terminal() -> Result<()> {
        let mut state = WorkspaceState::default();
        state.apply_view(&json!({"session_id":"session:empty","revision":0,"windows":null}))?;
        assert!(
            state
                .active_session()
                .context("session")?
                .active_window()
                .is_none()
        );
        assert_eq!(state.pane_ids().len(), 0);
        Ok(())
    }

    #[test]
    fn invalid_membership_is_atomic_and_old_panes_cannot_be_targeted() -> Result<()> {
        let mut state = WorkspaceState::default();
        state.apply_view(&view(1))?;
        let pane = state.pane_ids()[0];
        let mut invalid = view(2);
        invalid["windows"][0]["focused_block_id"] = json!("block:missing");
        assert!(state.apply_view(&invalid).is_err());
        assert_eq!(state.remote_pane(pane)?, "block:a");
        state.update_sessions(&json!({"sessions":[]}))?;
        assert!(state.command(&Command::ClosePane(pane)).is_err());
        Ok(())
    }

    #[test]
    fn floating_layers_and_unknown_creators_remain_visible_metadata() -> Result<()> {
        let mut state = WorkspaceState::default();
        let mut value = view(1);
        value["windows"][0]["layers"][0]["kind"] = json!("floating");
        value["windows"][0]["layers"][0]["blocks"][0]["creator_name"] = json!("custom.creator");
        state.apply_view(&value)?;
        let window = state
            .active_session()
            .context("session")?
            .active_window()
            .context("window")?;
        assert!(window.layout().is_none());
        assert!(window.layers[0].unsupported.is_none());
        assert!(window.layers[0].layout.is_some());
        assert_eq!(window.visible_panes(), state.placed_pane_ids());
        assert!(
            !state
                .pane(state.pane_ids()[0])
                .context("pane")?
                .is_terminal()
        );
        Ok(())
    }

    fn floating_view() -> Value {
        let mut value = view(1);
        let mut layer = value["windows"][0]["layers"][0].clone();
        layer["layer_id"] = json!("layer:b");
        layer["kind"] = json!("floating");
        layer["bounds"] = json!([0.125, 0.25, 0.5, 0.625]);
        layer["layout"]["block_id"] = json!("block:b");
        layer["layout"]["rect"] = layer["bounds"].clone();
        layer["blocks"][0]["block_id"] = json!("block:b");
        value["windows"][0]["layers"] = json!([value["windows"][0]["layers"][0], layer]);
        value
    }

    #[test]
    fn floating_layers_keep_order_bounds_and_aliases_with_zoom_and_empty_tiled_layer() -> Result<()>
    {
        let mut state = WorkspaceState::default();
        let mut value = floating_view();
        state.apply_view(&value)?;
        let window = state
            .active_session()
            .context("session")?
            .active_window()
            .context("window")?;
        let tiled = window.layers[0].blocks[0];
        let floating = window.layers[1].blocks[0];
        assert_eq!(window.visible_panes(), vec![tiled, floating]);
        assert_eq!(window.placed_panes(), vec![tiled, floating]);
        assert_eq!(window.layers[1].bounds, [0.125, 0.25, 0.5, 0.625]);
        assert_eq!(
            window.layers[1]
                .layout
                .as_ref()
                .context("floating layout")?
                .panes(),
            vec![floating]
        );
        value["revision"] = json!(2);
        value["windows"][0]["zoomed_block_id"] = json!("block:b");
        state.apply_view(&value)?;
        assert_eq!(
            state
                .active_session()
                .context("session")?
                .active_window()
                .context("window")?
                .visible_panes(),
            vec![floating]
        );
        value["revision"] = json!(3);
        value["windows"][0]["zoomed_block_id"] = Value::Null;
        value["windows"][0]["focused_block_id"] = json!("block:b");
        value["windows"][0]["layers"][0]["layout"] = Value::Null;
        value["windows"][0]["layers"][0]["blocks"] = json!([]);
        value["windows"][0]["layers"][1]["kind"] = json!("future-overlay");
        state.apply_view(&value)?;
        let window = state
            .active_session()
            .context("session")?
            .active_window()
            .context("window")?;
        assert!(window.layout().is_none());
        assert_eq!(window.visible_panes(), vec![floating]);
        assert_eq!(window.layers[0].remote_id, "layer:a");
        assert!(window.layers[1].is_floating());
        Ok(())
    }

    #[test]
    fn floating_commands_validate_membership_bounds_and_preserve_exact_fractions() -> Result<()> {
        let mut state = WorkspaceState::default();
        state.apply_view(&floating_view())?;
        let session = state.active_session().context("session")?;
        let session_id = session.id();
        let window = session.active_window().context("window")?;
        let tiled = window.layers[0].blocks[0];
        let floating = window.layers[1].blocks[0];
        let bounds = [1.0 / 7.0, 0.2, 0.5, 0.6];
        let call = state
            .command(&Command::FloatPane {
                pane: tiled,
                bounds,
            })?
            .context("float call")?;
        assert_eq!(call.method, "session.float_block");
        assert_eq!(call.params["block_id"], "block:a");
        assert_eq!(call.params["bounds"], json!(bounds));
        assert!(
            state
                .command(&Command::FloatPane {
                    pane: floating,
                    bounds
                })
                .is_err()
        );
        for (command, method) in [
            (
                Command::ResizeLayer {
                    session: session_id,
                    layer: "layer:b".into(),
                    bounds,
                },
                "session.resize_layer",
            ),
            (
                Command::RaiseLayer {
                    session: session_id,
                    layer: "layer:b".into(),
                },
                "session.raise_layer",
            ),
            (
                Command::LowerLayer {
                    session: session_id,
                    layer: "layer:b".into(),
                },
                "session.lower_layer",
            ),
            (
                Command::CloseLayer {
                    session: session_id,
                    layer: "layer:b".into(),
                },
                "session.close_layer",
            ),
        ] {
            let call = state.command(&command)?.context("layer call")?;
            assert_eq!(call.method, method);
            assert_eq!(call.params["session_id"], "session:a");
            assert_eq!(call.params["layer_id"], "layer:b");
        }
        for layer in ["layer:a", "layer:unknown"] {
            assert!(
                state
                    .command(&Command::CloseLayer {
                        session: session_id,
                        layer: layer.into()
                    })
                    .is_err()
            );
        }
        for bounds in [
            [-0.1, 0.0, 0.5, 0.5],
            [0.0, 0.0, 0.0, 0.5],
            [0.8, 0.0, 0.3, 0.5],
            [0.0, 0.8, 0.5, 0.3],
            [0.0, 0.0, f64::NAN, 0.5],
        ] {
            assert!(
                state
                    .command(&Command::ResizeLayer {
                        session: session_id,
                        layer: "layer:b".into(),
                        bounds
                    })
                    .is_err()
            );
        }
        assert!(
            state
                .command(&Command::ResizeLayer {
                    session: session_id,
                    layer: "layer:b".into(),
                    bounds: [0.0, 0.0, 1.0, 1.0]
                })
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn command_translation_uses_remote_identity_and_server_authority() -> Result<()> {
        let mut state = WorkspaceState::default();
        state.apply_view(&view(1))?;
        let pane = state.pane_ids()[0];
        let call = state
            .command(&Command::SplitPane {
                pane,
                axis: SplitAxis::Vertical,
            })?
            .context("call")?;
        assert_eq!(call.method, "session.new_split");
        assert_eq!(call.params["anchor_id"], "block:a");
        assert_eq!(call.params["direction"], "vertical");
        assert_eq!(state.pane_ids().len(), 1);
        Ok(())
    }

    #[test]
    fn precise_remote_ratio_is_retained_instead_of_clamped() -> Result<()> {
        let mut state = WorkspaceState::default();
        let mut value = view(1);
        let layer = &mut value["windows"][0]["layers"][0];
        layer["blocks"]
            .as_array_mut()
            .context("blocks")?
            .push(json!({
                "block_id":"block:b", "creator_name":"com.superlogical.terminal"
            }));
        layer["layout"] = json!({"rect":[0,0,1,1],"split":{
            "split_id":"split:a","direction":"horizontal","ratio":0.03125,
            "before":{"block_id":"block:a","rect":[0,0,0.03125,1]},
            "after":{"block_id":"block:b","rect":[0.03125,0,0.96875,1]}
        }});
        state.apply_view(&value)?;
        let window = state
            .active_session()
            .context("session")?
            .active_window()
            .context("window")?;
        let ViewLayout::Split { ratio, .. } = window.layout().context("split layout")? else {
            bail!("expected split layout");
        };
        assert_eq!(ratio.to_bits(), 0.03125_f64.to_bits());
        assert!(window.layers[0].unsupported.is_none());
        assert_eq!(
            window.layers[0].raw_layout["split"]["ratio"],
            json!(0.03125)
        );
        assert_eq!(state.pane_ids().len(), 2);
        value["revision"] = json!(2);
        value["windows"][0]["layers"][0]["layout"]["split"]["ratio"] = json!(1.0 / 3.0);
        state.apply_view(&value)?;
        let layout = state
            .active_session()
            .context("session")?
            .layout()
            .context("layout")?;
        let ViewLayout::Split { ratio, .. } = layout else {
            bail!("expected split layout");
        };
        assert_eq!(ratio.to_bits(), (1.0_f64 / 3.0).to_bits());
        assert_eq!(layout.panes().len(), 2);
        assert_eq!(layout.split_ids().len(), 1);
        Ok(())
    }

    #[test]
    fn same_instance_bootstrap_keeps_aliases_and_new_instance_resets_views() -> Result<()> {
        let mut state = WorkspaceState::default();
        let bootstrap = |instance| {
            json!({
                "server_status":{"instance_id":instance}, "client_id":"client:me",
                "attach_all":{"sessions":[{"session_id":"session:a","label":"Work"}]},
                "selected_session":{"session":{"session_id":"session:a"},"view":view(1)}
            })
        };
        state.apply_bootstrap(&bootstrap("first"))?;
        let pane = state.pane_ids()[0];
        state.apply_bootstrap(&bootstrap("first"))?;
        assert_eq!(state.pane_ids(), vec![pane]);
        let mut replacement = bootstrap("second");
        replacement["attach_all"]["sessions"] = json!([]);
        replacement["selected_session"] = Value::Null;
        state.apply_bootstrap(&replacement)?;
        assert_eq!(state.instance_id(), "second");
        assert_eq!(state.sessions().len(), 0);
        assert_eq!(state.pane_ids().len(), 0);
        Ok(())
    }

    #[test]
    fn unversioned_session_list_cannot_regress_authoritative_view_labels() -> Result<()> {
        let mut state = WorkspaceState::default();
        state.apply_view(&view(2))?;
        state.update_sessions(
            &json!({"sessions":[{"session_id":"session:a","label":"Old label"}]}),
        )?;
        assert_eq!(state.active_session().context("session")?.title(), "Work");
        assert!(!state.apply_view(&view(2))?);
        let mut renamed = view(3);
        renamed["label"] = json!("New label");
        state.apply_view(&renamed)?;
        assert_eq!(
            state.active_session().context("session")?.title(),
            "New label"
        );
        Ok(())
    }

    #[test]
    fn duplicate_layout_members_fail_for_arbitrary_remote_ratios() {
        let mut state = WorkspaceState::default();
        let mut value = view(1);
        value["windows"][0]["layers"][0]["layout"] = json!({"rect":[0,0,1,1],"split":{
            "split_id":"split:a","direction":"horizontal","ratio":0.03125,
            "before":{"block_id":"block:a","rect":[0,0,0.03125,1]},
            "after":{"block_id":"block:a","rect":[0.03125,0,0.96875,1]}
        }});
        assert!(state.apply_view(&value).is_err());
        assert_eq!(state.pane_ids().len(), 0);
    }
}
