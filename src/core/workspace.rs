use std::fmt;

macro_rules! identity {
    ($name:ident) => {
        #[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);
        impl $name {
            #[must_use]
            pub const fn value(self) -> u64 {
                self.0
            }
            #[must_use]
            pub const fn from_value(value: u64) -> Option<Self> {
                if value == 0 { None } else { Some(Self(value)) }
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}
identity!(SessionId);
identity!(WindowId);
identity!(PaneId);
identity!(SplitId);

#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// A binary split tree. Horizontal places children side by side; Vertical stacks them.
/// Closing a leaf collapses its parent into the surviving sibling.
#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Layout {
    Pane(PaneId),
    Split {
        id: SplitId,
        axis: SplitAxis,
        /// First child's share in permille, constrained to 100..=900.
        ratio: u16,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

impl Layout {
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
    pub fn contains(&self, pane: PaneId) -> bool {
        match self {
            Self::Pane(id) => *id == pane,
            Self::Split { first, second, .. } => first.contains(pane) || second.contains(pane),
        }
    }

    #[must_use]
    pub fn split_ids(&self) -> Vec<SplitId> {
        match self {
            Self::Pane(_) => Vec::new(),
            Self::Split {
                id, first, second, ..
            } => {
                let mut ids = vec![*id];
                ids.extend(first.split_ids());
                ids.extend(second.split_ids());
                ids
            }
        }
    }

    fn split(&mut self, target: PaneId, added: PaneId, split: SplitId, axis: SplitAxis) -> bool {
        match self {
            Self::Pane(id) if *id == target => {
                *self = Self::Split {
                    id: split,
                    axis,
                    ratio: 500,
                    first: Box::new(Self::Pane(target)),
                    second: Box::new(Self::Pane(added)),
                };
                true
            }
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                first.split(target, added, split, axis) || second.split(target, added, split, axis)
            }
        }
    }

    fn without(self, target: PaneId) -> Option<Self> {
        match self {
            Self::Pane(id) => (id != target).then_some(Self::Pane(id)),
            Self::Split {
                id,
                axis,
                ratio,
                first,
                second,
            } => match (first.without(target), second.without(target)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    id,
                    axis,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (survivor, None) | (None, survivor) => survivor,
            },
        }
    }

    fn insert_pane(
        &mut self,
        anchor: PaneId,
        pane: PaneId,
        split: SplitId,
        axis: SplitAxis,
        before: bool,
        ratio: u16,
    ) -> bool {
        match self {
            Self::Pane(id) if *id == anchor => {
                let (first, second) = if before {
                    (pane, anchor)
                } else {
                    (anchor, pane)
                };
                *self = Self::Split {
                    id: split,
                    axis,
                    ratio,
                    first: Box::new(Self::Pane(first)),
                    second: Box::new(Self::Pane(second)),
                };
                true
            }
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                first.insert_pane(anchor, pane, split, axis, before, ratio)
                    || second.insert_pane(anchor, pane, split, axis, before, ratio)
            }
        }
    }

    fn swap_panes(&mut self, pane: PaneId, other: PaneId) {
        match self {
            Self::Pane(id) if *id == pane => *id = other,
            Self::Pane(id) if *id == other => *id = pane,
            Self::Pane(_) => {}
            Self::Split { first, second, .. } => {
                first.swap_panes(pane, other);
                second.swap_panes(pane, other);
            }
        }
    }

    fn valid_ids(&self, insert: &mut impl FnMut(u64) -> bool) -> bool {
        match self {
            Self::Pane(pane) => insert(pane.value()),
            Self::Split {
                id,
                ratio,
                first,
                second,
                ..
            } => {
                (100..=900).contains(ratio)
                    && insert(id.value())
                    && first.valid_ids(insert)
                    && second.valid_ids(insert)
            }
        }
    }

    fn resize(&mut self, split: SplitId, share: u16) -> bool {
        match self {
            Self::Pane(_) => false,
            Self::Split { id, ratio, .. } if *id == split => {
                *ratio = share;
                true
            }
            Self::Split { first, second, .. } => {
                first.resize(split, share) || second.resize(split, share)
            }
        }
    }

    fn geometry(&self, bounds: PaneRect, panes: &mut Vec<(PaneId, PaneRect)>) {
        match self {
            Self::Pane(pane) => panes.push((*pane, bounds)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
                ..
            } => {
                let (before, after) = bounds.split(*axis, *ratio);
                first.geometry(before, panes);
                second.geometry(after, panes);
            }
        }
    }

    fn neighbor(&self, source: PaneId, direction: Direction, wrap: bool) -> Option<PaneId> {
        let mut panes = Vec::new();
        self.geometry(PaneRect::default(), &mut panes);
        let origin = panes
            .iter()
            .find(|(id, _)| *id == source)?
            .1
            .axes(direction);
        let choose = |wrapped| {
            panes
                .iter()
                .filter_map(|(id, bounds)| {
                    if *id == source {
                        return None;
                    }
                    let mut axes = bounds.axes(direction);
                    if wrapped {
                        axes.near += 1.;
                        axes.far += 1.;
                        axes.center += 1.;
                    }
                    (axes.center > origin.center).then(|| NeighborScore::new(*id, origin, axes))
                })
                .min_by(NeighborScore::compare)
                .map(|score| score.pane)
        };
        choose(false).or_else(|| wrap.then(|| choose(true)).flatten())
    }
}

#[derive(Clone, Copy)]
struct PaneRect {
    left: f64,
    top: f64,
    width: f64,
    height: f64,
}

impl Default for PaneRect {
    fn default() -> Self {
        Self {
            left: 0.,
            top: 0.,
            width: 1.,
            height: 1.,
        }
    }
}

impl PaneRect {
    fn split(self, axis: SplitAxis, ratio: u16) -> (Self, Self) {
        let share = f64::from(ratio) / 1000.;
        match axis {
            SplitAxis::Horizontal => {
                let width = self.width * share;
                (
                    Self { width, ..self },
                    Self {
                        left: self.left + width,
                        width: self.width - width,
                        ..self
                    },
                )
            }
            SplitAxis::Vertical => {
                let height = self.height * share;
                (
                    Self { height, ..self },
                    Self {
                        top: self.top + height,
                        height: self.height - height,
                        ..self
                    },
                )
            }
        }
    }

    fn axes(self, direction: Direction) -> DirectionAxes {
        let (near, far, orthogonal_near, orthogonal_far) = match direction {
            Direction::Left => (
                -self.left - self.width,
                -self.left,
                self.top,
                self.top + self.height,
            ),
            Direction::Right => (
                self.left,
                self.left + self.width,
                self.top,
                self.top + self.height,
            ),
            Direction::Up => (
                -self.top - self.height,
                -self.top,
                self.left,
                self.left + self.width,
            ),
            Direction::Down => (
                self.top,
                self.top + self.height,
                self.left,
                self.left + self.width,
            ),
        };
        DirectionAxes {
            near,
            far,
            center: near.midpoint(far),
            orthogonal_near,
            orthogonal_far,
        }
    }
}

#[derive(Clone, Copy)]
struct DirectionAxes {
    near: f64,
    far: f64,
    center: f64,
    orthogonal_near: f64,
    orthogonal_far: f64,
}

struct NeighborScore {
    pane: PaneId,
    off_axis: bool,
    gap: f64,
    orthogonal_gap: f64,
    center_distance: f64,
}

impl NeighborScore {
    fn new(pane: PaneId, source: DirectionAxes, target: DirectionAxes) -> Self {
        let overlap = source.orthogonal_far.min(target.orthogonal_far)
            - source.orthogonal_near.max(target.orthogonal_near);
        let center_distance = ((target.orthogonal_far + target.orthogonal_near)
            - (source.orthogonal_far + source.orthogonal_near))
            .abs();
        Self {
            pane,
            off_axis: overlap <= 0.,
            gap: (target.near - source.far).max(0.),
            orthogonal_gap: (-overlap).max(0.),
            center_distance,
        }
    }

    fn compare(left: &Self, right: &Self) -> std::cmp::Ordering {
        left.off_axis
            .cmp(&right.off_axis)
            .then_with(|| left.gap.total_cmp(&right.gap))
            .then_with(|| left.orthogonal_gap.total_cmp(&right.orthogonal_gap))
            .then_with(|| left.center_distance.total_cmp(&right.center_distance))
            .then_with(|| left.pane.cmp(&right.pane))
    }
}

#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceWindow {
    id: WindowId,
    title: String,
    layout: Layout,
    active_pane: PaneId,
    #[cfg_attr(feature = "ghostty", serde(default))]
    zoomed: Option<PaneId>,
}

impl WorkspaceWindow {
    #[must_use]
    pub const fn id(&self) -> WindowId {
        self.id
    }
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }
    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }
    #[must_use]
    pub const fn active_pane(&self) -> PaneId {
        self.active_pane
    }
    #[must_use]
    pub const fn zoomed_pane(&self) -> Option<PaneId> {
        self.zoomed
    }
}

#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    id: SessionId,
    title: String,
    windows: Vec<WorkspaceWindow>,
    active_window: usize,
    #[cfg_attr(feature = "ghostty", serde(default))]
    detached: Vec<PaneId>,
}

impl Session {
    #[must_use]
    pub const fn id(&self) -> SessionId {
        self.id
    }
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }
    #[must_use]
    pub fn windows(&self) -> &[WorkspaceWindow] {
        &self.windows
    }
    #[must_use]
    pub fn active_window(&self) -> Option<&WorkspaceWindow> {
        self.windows.get(self.active_window)
    }
    #[must_use]
    pub fn layout(&self) -> Option<&Layout> {
        self.active_window().map(WorkspaceWindow::layout)
    }
    #[must_use]
    pub fn active_pane(&self) -> Option<PaneId> {
        self.active_window().map(WorkspaceWindow::active_pane)
    }
    #[must_use]
    pub fn detached_panes(&self) -> &[PaneId] {
        &self.detached
    }
}

/// Commands are the one mutation boundary shared by buttons and key bindings.
#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug)]
pub enum Command {
    FloatPane {
        pane: PaneId,
        bounds: [f64; 4],
    },
    ResizeLayer {
        session: SessionId,
        layer: String,
        bounds: [f64; 4],
    },
    RaiseLayer {
        session: SessionId,
        layer: String,
    },
    LowerLayer {
        session: SessionId,
        layer: String,
    },
    CloseLayer {
        session: SessionId,
        layer: String,
    },
    CreateSession {
        title: String,
    },
    SelectSession(SessionId),
    RenameSession {
        session: SessionId,
        title: String,
    },
    CloseSession(SessionId),
    CreateWindow {
        session: SessionId,
        title: String,
    },
    SelectWindow(WindowId),
    RenameWindow {
        window: WindowId,
        title: String,
    },
    CloseWindow(WindowId),
    NextWindow,
    PreviousWindow,
    SplitPane {
        pane: PaneId,
        axis: SplitAxis,
    },
    MovePane {
        pane: PaneId,
        anchor: PaneId,
        axis: SplitAxis,
        before: bool,
        ratio: f64,
    },
    SwapPanes {
        pane: PaneId,
        other: PaneId,
    },
    MoveWindow {
        window: WindowId,
        before: Option<WindowId>,
    },
    FocusPane(PaneId),
    ClosePane(PaneId),
    DetachPane(PaneId),
    RestorePane {
        pane: PaneId,
        title: String,
    },
    FocusNextPane,
    ToggleZoom(PaneId),
    ResizeSplit {
        split: SplitId,
        ratio: u16,
    },
    FocusDirection {
        direction: Direction,
        wrap: bool,
    },
}

#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Workspace {
    sessions: Vec<Session>,
    active_session: Option<SessionId>,
    next_id: u64,
}

impl Workspace {
    /// Validate owned state received from storage or a local server before use.
    /// This does not allocate IDs or change focus.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let mut ids = std::collections::BTreeSet::new();
        let mut insert = |id: u64| id > 0 && id <= self.next_id && ids.insert(id);
        if self.sessions.is_empty() {
            return self.active_session.is_none();
        }
        if !self
            .sessions
            .iter()
            .any(|session| Some(session.id) == self.active_session)
        {
            return false;
        }
        for session in &self.sessions {
            if !insert(session.id.value())
                || session.title.trim().is_empty()
                || (session.windows.is_empty()
                    && (session.detached.is_empty() || session.active_window != 0))
                || (!session.windows.is_empty() && session.active_window >= session.windows.len())
            {
                return false;
            }
            for window in &session.windows {
                let panes = window.layout.panes();
                if !insert(window.id.value())
                    || window.title.trim().is_empty()
                    || !panes.contains(&window.active_pane)
                    || window
                        .zoomed
                        .is_some_and(|pane| pane != window.active_pane || !panes.contains(&pane))
                    || !window.layout.valid_ids(&mut insert)
                {
                    return false;
                }
            }
            if !session.detached.iter().all(|pane| insert(pane.value())) {
                return false;
            }
        }
        true
    }

    #[must_use]
    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }
    #[must_use]
    pub fn active_session(&self) -> Option<&Session> {
        self.active_session
            .and_then(|id| self.sessions.iter().find(|session| session.id == id))
    }
    #[must_use]
    pub fn pane_ids(&self) -> Vec<PaneId> {
        self.sessions
            .iter()
            .flat_map(|session| {
                session
                    .windows
                    .iter()
                    .flat_map(|window| window.layout.panes())
                    .chain(session.detached.iter().copied())
            })
            .collect()
    }

    /// Apply a command without rebuilding terminals or reusing identities.
    ///
    /// # Errors
    /// Returns an error for stale IDs, empty labels, missing active sessions, or
    /// exhausted IDs. Validation failures leave state unchanged.
    pub fn apply(&mut self, command: Command) -> Result<(), WorkspaceError> {
        match command {
            Command::FloatPane { .. }
            | Command::ResizeLayer { .. }
            | Command::RaiseLayer { .. }
            | Command::LowerLayer { .. }
            | Command::CloseLayer { .. } => return Err(WorkspaceError::UnsupportedNativeLayout),
            Command::CreateSession { title } => self.create_session(&title)?,
            Command::SelectSession(id) => {
                self.session_index(id)?;
                self.active_session = Some(id);
            }
            Command::RenameSession { session, title } => {
                let index = self.session_index(session)?;
                self.sessions[index].title = valid_title(&title)?;
            }
            Command::CloseSession(id) => {
                let index = self.session_index(id)?;
                self.remove_session(index);
            }
            Command::CreateWindow { session, title } => self.create_window(session, &title)?,
            Command::SelectWindow(id) => {
                let (session, window) = self.window_index(id)?;
                self.select_window(session, window);
            }
            Command::RenameWindow { window, title } => {
                let (session, index) = self.window_index(window)?;
                self.sessions[session].windows[index].title = valid_title(&title)?;
            }
            Command::CloseWindow(id) => {
                let (session, window) = self.window_index(id)?;
                self.remove_window(session, window);
            }
            Command::NextWindow => self.cycle_window(true)?,
            Command::PreviousWindow => self.cycle_window(false)?,
            Command::SplitPane { pane, axis } => self.split_pane(pane, axis)?,
            Command::MovePane {
                pane,
                anchor,
                axis,
                before,
                ratio,
            } => {
                self.move_pane(pane, anchor, axis, before, ratio)?;
            }
            Command::SwapPanes { pane, other } => self.swap_panes(pane, other)?,
            Command::MoveWindow { window, before } => self.move_window(window, before)?,
            Command::FocusPane(pane) => {
                let (session, window) = self.pane_index(pane)?;
                self.focus_pane(session, window, pane);
            }
            Command::ClosePane(pane) => self.close_pane(pane)?,
            Command::DetachPane(pane) => self.detach_pane(pane)?,
            Command::RestorePane { pane, title } => self.restore_pane(pane, &title)?,
            Command::FocusNextPane => self.cycle_pane()?,
            Command::ToggleZoom(pane) => self.toggle_zoom(pane)?,
            Command::ResizeSplit { split, ratio } => self.resize_split(split, ratio)?,
            Command::FocusDirection { direction, wrap } => self.focus_direction(direction, wrap)?,
        }
        Ok(())
    }

    fn create_session(&mut self, title: &str) -> Result<(), WorkspaceError> {
        let title = valid_title(title)?;
        let last = self
            .next_id
            .checked_add(3)
            .ok_or(WorkspaceError::IdExhausted)?;
        let id = SessionId(last - 2);
        let pane = PaneId(last);
        self.sessions.push(Session {
            id,
            title,
            active_window: 0,
            windows: vec![WorkspaceWindow {
                id: WindowId(last - 1),
                title: "Terminal".into(),
                layout: Layout::Pane(pane),
                active_pane: pane,
                zoomed: None,
            }],
            detached: Vec::new(),
        });
        self.active_session = Some(id);
        self.next_id = last;
        Ok(())
    }

    fn create_window(&mut self, session: SessionId, title: &str) -> Result<(), WorkspaceError> {
        let title = valid_title(title)?;
        let index = self.session_index(session)?;
        let last = self
            .next_id
            .checked_add(2)
            .ok_or(WorkspaceError::IdExhausted)?;
        let pane = PaneId(last);
        self.sessions[index].windows.push(WorkspaceWindow {
            id: WindowId(last - 1),
            title,
            layout: Layout::Pane(pane),
            active_pane: pane,
            zoomed: None,
        });
        self.select_window(index, self.sessions[index].windows.len() - 1);
        self.next_id = last;
        Ok(())
    }

    fn split_pane(&mut self, pane: PaneId, axis: SplitAxis) -> Result<(), WorkspaceError> {
        let (session, window) = self.pane_index(pane)?;
        let next = self
            .next_id
            .checked_add(2)
            .ok_or(WorkspaceError::IdExhausted)?;
        let added = PaneId(next);
        let split = SplitId(next - 1);
        let target = &mut self.sessions[session].windows[window];
        target.layout.split(pane, added, split, axis);
        target.active_pane = added;
        target.zoomed = None;
        self.select_window(session, window);
        self.next_id = next;
        Ok(())
    }

    fn close_pane(&mut self, pane: PaneId) -> Result<(), WorkspaceError> {
        if let Ok((session, detached)) = self.detached_index(pane) {
            self.sessions[session].detached.remove(detached);
            if self.sessions[session].windows.is_empty()
                && self.sessions[session].detached.is_empty()
            {
                self.remove_session(session);
            }
            return Ok(());
        }
        let (session, window) = self.pane_index(pane)?;
        let target = &mut self.sessions[session].windows[window];
        if let Some(layout) = target.layout.clone().without(pane) {
            if target.active_pane == pane {
                target.active_pane = layout.panes()[0];
            }
            target.layout = layout;
            if target.zoomed == Some(pane) {
                target.zoomed = None;
            }
        } else {
            self.remove_window(session, window);
        }
        Ok(())
    }

    fn detach_pane(&mut self, pane: PaneId) -> Result<(), WorkspaceError> {
        if self.detached_index(pane).is_ok() {
            return Ok(());
        }
        let (session, window) = self.pane_index(pane)?;
        let mut candidate = self.sessions[session].clone();
        candidate.detached.push(pane);
        let source = &mut candidate.windows[window];
        if let Some(layout) = source.layout.clone().without(pane) {
            if source.active_pane == pane {
                source.active_pane = layout.panes()[0];
            }
            source.layout = layout;
            if source.zoomed == Some(pane) {
                source.zoomed = None;
            }
        } else {
            candidate.windows.remove(window);
            if candidate.windows.is_empty() {
                candidate.active_window = 0;
            } else if candidate.active_window > window {
                candidate.active_window -= 1;
            } else {
                candidate.active_window = candidate.active_window.min(candidate.windows.len() - 1);
            }
        }
        self.sessions[session] = candidate;
        Ok(())
    }

    fn restore_pane(&mut self, pane: PaneId, title: &str) -> Result<(), WorkspaceError> {
        let title = valid_title(title)?;
        let (session, detached) = self.detached_index(pane)?;
        let next = self
            .next_id
            .checked_add(1)
            .ok_or(WorkspaceError::IdExhausted)?;
        let mut candidate = self.sessions[session].clone();
        candidate.detached.remove(detached);
        candidate.windows.push(WorkspaceWindow {
            id: WindowId(next),
            title,
            layout: Layout::Pane(pane),
            active_pane: pane,
            zoomed: None,
        });
        candidate.active_window = candidate.windows.len() - 1;
        self.active_session = Some(candidate.id);
        self.sessions[session] = candidate;
        self.next_id = next;
        Ok(())
    }

    fn move_pane(
        &mut self,
        pane: PaneId,
        anchor: PaneId,
        axis: SplitAxis,
        before: bool,
        ratio: f64,
    ) -> Result<(), WorkspaceError> {
        if pane == anchor {
            return Err(WorkspaceError::SamePane(pane));
        }
        if !ratio.is_finite() || ratio <= 0. || ratio >= 1. {
            return Err(WorkspaceError::InvalidSplitRatio);
        }
        let (session, source) = self.pane_location(pane)?;
        let (target_session, target) = self.pane_index(anchor)?;
        if session != target_session {
            return Err(WorkspaceError::CrossSessionMove);
        }
        let next = self
            .next_id
            .checked_add(1)
            .ok_or(WorkspaceError::IdExhausted)?;
        let original = &self.sessions[session];
        let destination = original.windows[target].id;
        let active_window = original
            .active_window()
            .ok_or(WorkspaceError::NoActiveWindow)?
            .id;
        let follows_focus =
            source.is_some_and(|source| original.windows[source].active_pane == pane);
        let mut candidate = original.clone();
        if let Some(source) = source {
            let source_window = &mut candidate.windows[source];
            if let Some(layout) = source_window.layout.clone().without(pane) {
                if source_window.active_pane == pane {
                    source_window.active_pane = layout.panes()[0];
                }
                source_window.layout = layout;
                source_window.zoomed = None;
            } else {
                candidate.windows.remove(source);
            }
        } else {
            candidate.detached.retain(|detached| *detached != pane);
        }
        let target = candidate
            .windows
            .iter()
            .position(|window| window.id == destination)
            .ok_or(WorkspaceError::UnknownWindow(destination))?;
        let destination_window = &mut candidate.windows[target];
        if !destination_window.layout.insert_pane(
            anchor,
            pane,
            SplitId(next),
            axis,
            before,
            legacy_move_ratio(ratio),
        ) {
            return Err(WorkspaceError::UnknownPane(anchor));
        }
        destination_window.zoomed = None;
        if follows_focus {
            destination_window.active_pane = pane;
            candidate.active_window = target;
        } else {
            candidate.active_window = candidate
                .windows
                .iter()
                .position(|window| window.id == active_window)
                .ok_or(WorkspaceError::UnknownWindow(active_window))?;
        }
        self.sessions[session] = candidate;
        self.next_id = next;
        Ok(())
    }

    fn swap_panes(&mut self, pane: PaneId, other: PaneId) -> Result<(), WorkspaceError> {
        if pane == other {
            return Err(WorkspaceError::SamePane(pane));
        }
        let (session, source) = self.pane_index(pane)?;
        let (other_session, destination) = self.pane_index(other)?;
        if session != other_session {
            return Err(WorkspaceError::CrossSessionMove);
        }
        let mut candidate = self.sessions[session].clone();
        let active_pane = candidate
            .active_pane()
            .ok_or(WorkspaceError::NoActiveWindow)?;
        let follow = if active_pane == pane {
            Some((destination, pane))
        } else if active_pane == other {
            Some((source, other))
        } else {
            None
        };
        for window in &mut candidate.windows {
            window.layout.swap_panes(pane, other);
            if !window.layout.contains(window.active_pane) {
                window.active_pane = if window.active_pane == pane {
                    other
                } else {
                    pane
                };
            }
            if window.zoomed.is_some_and(|zoomed| {
                !window.layout.contains(zoomed) || zoomed != window.active_pane
            }) {
                window.zoomed = None;
            }
        }
        if let Some((window, focused)) = follow {
            let target = &mut candidate.windows[window];
            target.active_pane = focused;
            if target.zoomed.is_some_and(|zoomed| zoomed != focused) {
                target.zoomed = None;
            }
            candidate.active_window = window;
        }
        self.sessions[session] = candidate;
        Ok(())
    }

    fn move_window(
        &mut self,
        window: WindowId,
        before: Option<WindowId>,
    ) -> Result<(), WorkspaceError> {
        let (session, source) = self.window_index(window)?;
        if before == Some(window) {
            return Err(WorkspaceError::SameWindow(window));
        }
        if let Some(before) = before
            && self.window_index(before)?.0 != session
        {
            return Err(WorkspaceError::CrossSessionMove);
        }
        let mut candidate = self.sessions[session].clone();
        let selected = candidate
            .active_window()
            .ok_or(WorkspaceError::NoActiveWindow)?
            .id;
        let moving = candidate.windows.remove(source);
        let target = match before {
            Some(before) => candidate
                .windows
                .iter()
                .position(|window| window.id == before)
                .ok_or(WorkspaceError::UnknownWindow(before))?,
            None => candidate.windows.len(),
        };
        candidate.windows.insert(target, moving);
        candidate.active_window = candidate
            .windows
            .iter()
            .position(|window| window.id == selected)
            .ok_or(WorkspaceError::UnknownWindow(selected))?;
        self.sessions[session] = candidate;
        Ok(())
    }

    fn cycle_pane(&mut self) -> Result<(), WorkspaceError> {
        let active = self.active_session.ok_or(WorkspaceError::NoActiveSession)?;
        let index = self.session_index(active)?;
        let session = &mut self.sessions[index];
        let window = session
            .windows
            .get_mut(session.active_window)
            .ok_or(WorkspaceError::NoActiveWindow)?;
        let panes = window.layout.panes();
        let position = panes
            .iter()
            .position(|pane| *pane == window.active_pane)
            .ok_or(WorkspaceError::UnknownPane(window.active_pane))?;
        let next = panes[(position + 1) % panes.len()];
        if next != window.active_pane {
            window.zoomed = None;
        }
        window.active_pane = next;
        Ok(())
    }

    fn cycle_window(&mut self, forward: bool) -> Result<(), WorkspaceError> {
        let active = self.active_session.ok_or(WorkspaceError::NoActiveSession)?;
        let index = self.session_index(active)?;
        let session = &mut self.sessions[index];
        if session.windows.is_empty() {
            return Err(WorkspaceError::NoActiveWindow);
        }
        session.active_window = if forward {
            (session.active_window + 1) % session.windows.len()
        } else {
            (session.active_window + session.windows.len() - 1) % session.windows.len()
        };
        Ok(())
    }

    fn focus_pane(&mut self, session: usize, window: usize, pane: PaneId) {
        let target = &mut self.sessions[session].windows[window];
        if target.active_pane != pane {
            target.zoomed = None;
        }
        target.active_pane = pane;
        self.select_window(session, window);
    }

    fn toggle_zoom(&mut self, pane: PaneId) -> Result<(), WorkspaceError> {
        let (session, window) = self.pane_index(pane)?;
        let target = &mut self.sessions[session].windows[window];
        target.zoomed = if target.zoomed == Some(pane) {
            None
        } else {
            Some(pane)
        };
        target.active_pane = pane;
        self.select_window(session, window);
        Ok(())
    }

    fn resize_split(&mut self, split: SplitId, ratio: u16) -> Result<(), WorkspaceError> {
        if !(100..=900).contains(&ratio) {
            return Err(WorkspaceError::InvalidSplitRatio);
        }
        for session in &mut self.sessions {
            for window in &mut session.windows {
                if window.layout.resize(split, ratio) {
                    return Ok(());
                }
            }
        }
        Err(WorkspaceError::UnknownSplit(split))
    }

    fn focus_direction(&mut self, direction: Direction, wrap: bool) -> Result<(), WorkspaceError> {
        let active = self.active_session.ok_or(WorkspaceError::NoActiveSession)?;
        let index = self.session_index(active)?;
        let session = &self.sessions[index];
        let window = session
            .active_window()
            .ok_or(WorkspaceError::NoActiveWindow)?;
        if let Some(pane) = window.layout.neighbor(window.active_pane, direction, wrap) {
            self.focus_pane(index, session.active_window, pane);
        }
        Ok(())
    }

    fn session_index(&self, id: SessionId) -> Result<usize, WorkspaceError> {
        self.sessions
            .iter()
            .position(|session| session.id == id)
            .ok_or(WorkspaceError::UnknownSession(id))
    }
    fn window_index(&self, id: WindowId) -> Result<(usize, usize), WorkspaceError> {
        self.sessions
            .iter()
            .enumerate()
            .find_map(|(s, session)| {
                session
                    .windows
                    .iter()
                    .position(|window| window.id == id)
                    .map(|w| (s, w))
            })
            .ok_or(WorkspaceError::UnknownWindow(id))
    }
    fn pane_index(&self, pane: PaneId) -> Result<(usize, usize), WorkspaceError> {
        self.sessions
            .iter()
            .enumerate()
            .find_map(|(s, session)| {
                session
                    .windows
                    .iter()
                    .position(|window| window.layout.contains(pane))
                    .map(|w| (s, w))
            })
            .ok_or(WorkspaceError::UnknownPane(pane))
    }
    fn detached_index(&self, pane: PaneId) -> Result<(usize, usize), WorkspaceError> {
        self.sessions
            .iter()
            .enumerate()
            .find_map(|(session, state)| {
                state
                    .detached
                    .iter()
                    .position(|entry| *entry == pane)
                    .map(|index| (session, index))
            })
            .ok_or(WorkspaceError::UnknownPane(pane))
    }

    fn pane_location(&self, pane: PaneId) -> Result<(usize, Option<usize>), WorkspaceError> {
        if let Ok((session, window)) = self.pane_index(pane) {
            return Ok((session, Some(window)));
        }
        self.detached_index(pane)
            .map(|(session, _)| (session, None))
    }
    fn select_window(&mut self, session: usize, window: usize) {
        self.sessions[session].active_window = window;
        self.active_session = Some(self.sessions[session].id);
    }
    fn remove_window(&mut self, session: usize, window: usize) {
        let target = &mut self.sessions[session];
        target.windows.remove(window);
        if target.windows.is_empty() && target.detached.is_empty() {
            self.remove_session(session);
        } else if target.windows.is_empty() {
            target.active_window = 0;
        } else if target.active_window > window {
            target.active_window -= 1;
        } else {
            target.active_window = target.active_window.min(target.windows.len() - 1);
        }
    }
    fn remove_session(&mut self, index: usize) {
        let removed = self.sessions.remove(index);
        if self.active_session == Some(removed.id) {
            self.active_session = self
                .sessions
                .get(index.min(self.sessions.len().saturating_sub(1)))
                .map(|session| session.id);
        }
    }
}

fn valid_title(title: &str) -> Result<String, WorkspaceError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(WorkspaceError::EmptyTitle);
    }
    Ok(title.to_owned())
}

/// The legacy fixture stores 10%-90% permille; official Rex keeps the f64 ratio.
fn legacy_move_ratio(ratio: f64) -> u16 {
    let scaled = ratio * 1000.;
    (100_u16..=900)
        .find(|share| scaled < f64::from(*share) + 0.5)
        .unwrap_or(900)
}

#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceError {
    UnsupportedNativeLayout,
    EmptyTitle,
    NoActiveSession,
    NoActiveWindow,
    UnknownSession(SessionId),
    UnknownPane(PaneId),
    UnknownWindow(WindowId),
    IdExhausted,
    UnknownSplit(SplitId),
    InvalidSplitRatio,
    SamePane(PaneId),
    SameWindow(WindowId),
    CrossSessionMove,
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedNativeLayout => {
                f.write_str("floating layers require the official Rex backend")
            }
            Self::EmptyTitle => f.write_str("title cannot be empty"),
            Self::NoActiveSession => f.write_str("no session is selected"),
            Self::NoActiveWindow => f.write_str("the selected session has no attached window"),
            Self::UnknownSession(id) => write!(f, "session {id} no longer exists"),
            Self::UnknownWindow(id) => write!(f, "window {id} no longer exists"),
            Self::UnknownPane(id) => write!(f, "pane {id} no longer exists"),
            Self::IdExhausted => f.write_str("workspace ID space exhausted"),
            Self::UnknownSplit(id) => write!(f, "split {id} no longer exists"),
            Self::SamePane(id) => write!(f, "pane {id} cannot be moved or swapped with itself"),
            Self::SameWindow(id) => write!(f, "window {id} cannot be ordered before itself"),
            Self::CrossSessionMove => f.write_str("layout moves must stay within one session"),
            Self::InvalidSplitRatio => {
                f.write_str("split ratio must be between 100 and 900 permille")
            }
        }
    }
}
impl std::error::Error for WorkspaceError {}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests assert fixture preconditions and should fail immediately when those preconditions break."
)]
mod tests {
    use super::*;

    fn workspace() -> Workspace {
        let mut workspace = Workspace::default();
        workspace
            .apply(Command::CreateSession {
                title: "Development".into(),
            })
            .unwrap();
        workspace
    }
    fn focused(workspace: &Workspace) -> PaneId {
        workspace.active_session().unwrap().active_pane().unwrap()
    }

    fn active_window(workspace: &Workspace) -> &WorkspaceWindow {
        workspace.active_session().unwrap().active_window().unwrap()
    }

    fn active_layout(workspace: &Workspace) -> &Layout {
        active_window(workspace).layout()
    }

    #[test]
    fn legacy_workspace_rejects_native_layers_without_mutation() {
        let mut workspace = workspace();
        let before = workspace.clone();
        let pane = focused(&workspace);
        let session = workspace.active_session().unwrap().id();
        for command in [
            Command::FloatPane {
                pane,
                bounds: [0.1, 0.1, 0.8, 0.8],
            },
            Command::ResizeLayer {
                session,
                layer: "layer:test".into(),
                bounds: [0.0, 0.0, 1.0, 1.0],
            },
            Command::RaiseLayer {
                session,
                layer: "layer:test".into(),
            },
            Command::LowerLayer {
                session,
                layer: "layer:test".into(),
            },
            Command::CloseLayer {
                session,
                layer: "layer:test".into(),
            },
        ] {
            assert_eq!(
                workspace.apply(command),
                Err(WorkspaceError::UnsupportedNativeLayout)
            );
            assert_eq!(workspace, before);
        }
    }

    #[test]
    fn detaching_the_last_pane_retains_a_valid_session_without_an_active_window() {
        let mut workspace = workspace();
        let pane = focused(&workspace);
        let session = workspace.active_session().unwrap().id();
        let next = workspace.next_id;
        workspace.apply(Command::DetachPane(pane)).unwrap();
        let detached = workspace.active_session().unwrap();
        assert_eq!(detached.id(), session);
        assert_eq!(detached.windows(), []);
        assert!(detached.active_window().is_none());
        assert!(detached.active_pane().is_none());
        assert!(detached.layout().is_none());
        assert_eq!(detached.detached_panes(), &[pane]);
        assert_eq!(workspace.pane_ids(), vec![pane]);
        assert_eq!(workspace.next_id, next);
        assert!(workspace.is_valid());
        let unchanged = workspace.clone();
        workspace.apply(Command::DetachPane(pane)).unwrap();
        assert_eq!(workspace, unchanged);
        for command in [
            Command::NextWindow,
            Command::PreviousWindow,
            Command::FocusNextPane,
            Command::FocusDirection {
                direction: Direction::Left,
                wrap: true,
            },
        ] {
            assert_eq!(
                workspace.apply(command),
                Err(WorkspaceError::NoActiveWindow)
            );
            assert_eq!(workspace, unchanged);
        }
        workspace.apply(Command::ClosePane(pane)).unwrap();
        assert_eq!(workspace.sessions(), []);
        assert_eq!(workspace.pane_ids(), []);
        assert!(workspace.is_valid());
    }

    #[test]
    fn restoring_a_detached_pane_creates_only_a_new_window_identity() {
        let mut workspace = workspace();
        let pane = focused(&workspace);
        let old_window = active_window(&workspace).id();
        workspace.apply(Command::DetachPane(pane)).unwrap();
        let next = workspace.next_id;
        workspace
            .apply(Command::RestorePane {
                pane,
                title: " Restored ".into(),
            })
            .unwrap();
        let session = workspace.active_session().unwrap();
        let restored = session.active_window().unwrap();
        assert_eq!(restored.id(), WindowId(next + 1));
        assert_ne!(restored.id(), old_window);
        assert_eq!(restored.title(), "Restored");
        assert_eq!(restored.layout(), &Layout::Pane(pane));
        assert_eq!(restored.active_pane(), pane);
        assert_eq!(session.detached_panes(), []);
        assert_eq!(workspace.pane_ids(), vec![pane]);
        assert_eq!(workspace.next_id, next + 1);
        assert!(workspace.is_valid());
    }

    #[test]
    fn moving_a_detached_pane_into_a_split_preserves_existing_focus() {
        let mut workspace = workspace();
        let session = workspace.active_session().unwrap().id();
        let anchor = focused(&workspace);
        let destination = active_window(&workspace).id();
        workspace
            .apply(Command::SplitPane {
                pane: anchor,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        let detached = focused(&workspace);
        workspace.apply(Command::DetachPane(detached)).unwrap();
        workspace
            .apply(Command::CreateWindow {
                session,
                title: "Selected".into(),
            })
            .unwrap();
        let selected = active_window(&workspace).id();
        let focused = focused(&workspace);
        workspace
            .apply(Command::MovePane {
                pane: detached,
                anchor,
                axis: SplitAxis::Vertical,
                before: true,
                ratio: 0.25,
            })
            .unwrap();
        let session = workspace.active_session().unwrap();
        assert_eq!(session.active_window().unwrap().id(), selected);
        assert_eq!(session.active_pane(), Some(focused));
        let target = session
            .windows()
            .iter()
            .find(|window| window.id() == destination)
            .unwrap();
        assert_eq!(target.active_pane(), anchor);
        assert_eq!(target.layout().panes(), vec![detached, anchor]);
        assert_eq!(session.detached_panes(), []);
        assert_eq!(workspace.pane_ids().len(), 3);
        assert!(workspace.is_valid());
    }

    #[test]
    fn closing_last_attached_window_preserves_detached_process_identities() {
        let mut workspace = workspace();
        let pane = focused(&workspace);
        let session = workspace.active_session().unwrap().id();
        workspace.apply(Command::DetachPane(pane)).unwrap();
        workspace
            .apply(Command::CreateWindow {
                session,
                title: "Transient".into(),
            })
            .unwrap();
        let window = active_window(&workspace).id();
        let transient = focused(&workspace);
        assert!(workspace.pane_ids().contains(&pane));
        assert!(workspace.pane_ids().contains(&transient));
        workspace.apply(Command::CloseWindow(window)).unwrap();
        assert_eq!(workspace.active_session().unwrap().id(), session);
        assert_eq!(workspace.active_session().unwrap().windows(), []);
        assert_eq!(workspace.pane_ids(), vec![pane]);
        assert!(workspace.is_valid());
        workspace.apply(Command::CloseSession(session)).unwrap();
        assert_eq!(workspace.pane_ids(), []);
        assert!(workspace.is_valid());
    }

    #[test]
    fn detached_aliases_share_the_same_global_uniqueness_and_allocation_invariants() {
        let mut workspace = workspace();
        let pane = focused(&workspace);
        workspace.apply(Command::DetachPane(pane)).unwrap();
        assert!(workspace.is_valid());
        let original = workspace.clone();
        for aliases in [
            vec![pane, pane],
            vec![PaneId(0)],
            vec![PaneId(workspace.next_id + 1)],
            vec![],
        ] {
            workspace.sessions[0].detached = aliases;
            assert!(!workspace.is_valid());
            workspace = original.clone();
        }
        workspace.sessions[0].active_window = 1;
        assert!(!workspace.is_valid());
        workspace = original;
        workspace
            .apply(Command::CreateSession {
                title: "Other".into(),
            })
            .unwrap();
        workspace.sessions[1].detached.push(pane);
        assert!(!workspace.is_valid());
    }

    #[test]
    fn detached_restore_and_close_validation_is_atomic() {
        let mut workspace = workspace();
        let anchor = focused(&workspace);
        workspace
            .apply(Command::SplitPane {
                pane: anchor,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        let pane = focused(&workspace);
        workspace.apply(Command::DetachPane(pane)).unwrap();
        let original = workspace.clone();
        for command in [
            Command::RestorePane {
                pane,
                title: " ".into(),
            },
            Command::RestorePane {
                pane: anchor,
                title: "Attached".into(),
            },
            Command::RestorePane {
                pane: PaneId(999),
                title: "Missing".into(),
            },
            Command::DetachPane(PaneId(999)),
            Command::ClosePane(PaneId(999)),
            Command::FocusPane(pane),
            Command::ToggleZoom(pane),
            Command::SwapPanes {
                pane,
                other: anchor,
            },
        ] {
            assert!(workspace.apply(command).is_err());
            assert_eq!(workspace, original);
        }
        workspace.next_id = u64::MAX;
        let original = workspace.clone();
        assert_eq!(
            workspace.apply(Command::RestorePane {
                pane,
                title: "Cannot allocate".into()
            }),
            Err(WorkspaceError::IdExhausted)
        );
        assert_eq!(workspace, original);
    }

    #[test]
    fn moving_a_pane_preserves_unaffected_splits_and_allocates_only_one_new_id() {
        let mut workspace = workspace();
        let a = focused(&workspace);
        workspace
            .apply(Command::SplitPane {
                pane: a,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        let b = focused(&workspace);
        let root = active_layout(&workspace).split_ids()[0];
        workspace
            .apply(Command::ResizeSplit {
                split: root,
                ratio: 650,
            })
            .unwrap();
        workspace
            .apply(Command::SplitPane {
                pane: b,
                axis: SplitAxis::Vertical,
            })
            .unwrap();
        let c = focused(&workspace);
        let removed = active_layout(&workspace).split_ids()[1];
        workspace
            .apply(Command::SplitPane {
                pane: c,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        let d = focused(&workspace);
        let surviving = active_layout(&workspace).split_ids()[2];
        workspace
            .apply(Command::ResizeSplit {
                split: surviving,
                ratio: 350,
            })
            .unwrap();
        let last_id = workspace.next_id;
        workspace
            .apply(Command::MovePane {
                pane: b,
                anchor: a,
                axis: SplitAxis::Vertical,
                before: true,
                ratio: 0.3125,
            })
            .unwrap();
        assert_eq!(workspace.next_id, last_id + 1);
        assert_eq!(focused(&workspace), d);
        assert_eq!(
            active_layout(&workspace),
            &Layout::Split {
                id: root,
                axis: SplitAxis::Horizontal,
                ratio: 650,
                first: Box::new(Layout::Split {
                    id: SplitId(last_id + 1),
                    axis: SplitAxis::Vertical,
                    ratio: 313,
                    first: Box::new(Layout::Pane(b)),
                    second: Box::new(Layout::Pane(a)),
                }),
                second: Box::new(Layout::Split {
                    id: surviving,
                    axis: SplitAxis::Horizontal,
                    ratio: 350,
                    first: Box::new(Layout::Pane(c)),
                    second: Box::new(Layout::Pane(d)),
                }),
            }
        );
        assert!(!active_layout(&workspace).split_ids().contains(&removed));
        assert!(workspace.is_valid());
    }

    #[test]
    fn moving_an_inactive_windows_only_pane_closes_it_and_follows_its_focus() {
        let mut workspace = workspace();
        let session = workspace.active_session().unwrap().id();
        let original_window = active_window(&workspace).id();
        workspace
            .apply(Command::CreateWindow {
                session,
                title: "Destination".into(),
            })
            .unwrap();
        let destination = active_window(&workspace).id();
        let anchor = focused(&workspace);
        workspace
            .apply(Command::CreateWindow {
                session,
                title: "Source".into(),
            })
            .unwrap();
        let source = active_window(&workspace).id();
        let moving = focused(&workspace);
        workspace
            .apply(Command::SelectWindow(original_window))
            .unwrap();
        workspace
            .apply(Command::CreateSession {
                title: "Unrelated selected session".into(),
            })
            .unwrap();
        let selected = workspace.active_session().unwrap().id();
        workspace
            .apply(Command::MovePane {
                pane: moving,
                anchor,
                axis: SplitAxis::Horizontal,
                before: false,
                ratio: 0.75,
            })
            .unwrap();
        assert_eq!(workspace.active_session().unwrap().id(), selected);
        let moved_session = workspace
            .sessions()
            .iter()
            .find(|entry| entry.id() == session)
            .unwrap();
        assert_eq!(moved_session.windows().len(), 2);
        assert!(
            !moved_session
                .windows()
                .iter()
                .any(|window| window.id() == source)
        );
        assert_eq!(moved_session.active_window().unwrap().id(), destination);
        assert_eq!(moved_session.active_pane().unwrap(), moving);
        assert_eq!(
            moved_session.layout().unwrap().panes(),
            vec![anchor, moving]
        );
        assert!(workspace.is_valid());
    }

    #[test]
    fn swapping_across_windows_preserves_split_geometry_and_follows_active_focus() {
        let mut workspace = workspace();
        let session = workspace.active_session().unwrap().id();
        let first = active_window(&workspace).id();
        let a = focused(&workspace);
        workspace
            .apply(Command::SplitPane {
                pane: a,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        let b = focused(&workspace);
        let first_split = active_layout(&workspace).split_ids()[0];
        workspace
            .apply(Command::ResizeSplit {
                split: first_split,
                ratio: 650,
            })
            .unwrap();
        workspace
            .apply(Command::CreateWindow {
                session,
                title: "Second".into(),
            })
            .unwrap();
        let second = active_window(&workspace).id();
        let c = focused(&workspace);
        workspace
            .apply(Command::SplitPane {
                pane: c,
                axis: SplitAxis::Vertical,
            })
            .unwrap();
        let d = focused(&workspace);
        let second_split = active_layout(&workspace).split_ids()[0];
        workspace
            .apply(Command::ResizeSplit {
                split: second_split,
                ratio: 300,
            })
            .unwrap();
        workspace.apply(Command::FocusPane(a)).unwrap();
        let last_id = workspace.next_id;
        workspace
            .apply(Command::SwapPanes { pane: a, other: d })
            .unwrap();
        let windows = workspace.active_session().unwrap().windows();
        assert_eq!(windows[0].id(), first);
        assert_eq!(
            windows[0].layout(),
            &Layout::Split {
                id: first_split,
                axis: SplitAxis::Horizontal,
                ratio: 650,
                first: Box::new(Layout::Pane(d)),
                second: Box::new(Layout::Pane(b)),
            }
        );
        assert_eq!(
            windows[1].layout(),
            &Layout::Split {
                id: second_split,
                axis: SplitAxis::Vertical,
                ratio: 300,
                first: Box::new(Layout::Pane(c)),
                second: Box::new(Layout::Pane(a)),
            }
        );
        assert_eq!(active_window(&workspace).id(), second);
        assert_eq!(focused(&workspace), a);
        assert_eq!(workspace.next_id, last_id);
        // Swapping two nonfocused panes must not change the active pane/window.
        workspace
            .apply(Command::SwapPanes { pane: d, other: c })
            .unwrap();
        assert_eq!(focused(&workspace), a);
        assert_eq!(active_window(&workspace).id(), second);
        assert!(workspace.is_valid());
    }

    #[test]
    fn moving_windows_preserves_the_active_identity_and_tab_contents() {
        let mut workspace = workspace();
        let session = workspace.active_session().unwrap().id();
        let a = active_window(&workspace).id();
        workspace
            .apply(Command::CreateWindow {
                session,
                title: "B".into(),
            })
            .unwrap();
        let b = active_window(&workspace).id();
        workspace
            .apply(Command::CreateWindow {
                session,
                title: "C".into(),
            })
            .unwrap();
        let c = active_window(&workspace).id();
        let original: std::collections::BTreeMap<_, _> = workspace
            .active_session()
            .unwrap()
            .windows()
            .iter()
            .map(|window| (window.id(), window.clone()))
            .collect();
        workspace.apply(Command::SelectWindow(b)).unwrap();
        workspace
            .apply(Command::MoveWindow {
                window: a,
                before: Some(c),
            })
            .unwrap();
        assert_eq!(
            workspace
                .active_session()
                .unwrap()
                .windows()
                .iter()
                .map(WorkspaceWindow::id)
                .collect::<Vec<_>>(),
            vec![b, a, c]
        );
        assert_eq!(active_window(&workspace).id(), b);
        workspace
            .apply(Command::MoveWindow {
                window: b,
                before: None,
            })
            .unwrap();
        assert_eq!(
            workspace
                .active_session()
                .unwrap()
                .windows()
                .iter()
                .map(WorkspaceWindow::id)
                .collect::<Vec<_>>(),
            vec![a, c, b]
        );
        assert_eq!(active_window(&workspace).id(), b);
        for window in workspace.active_session().unwrap().windows() {
            assert_eq!(Some(window), original.get(&window.id()));
        }
        let unchanged = workspace.clone();
        workspace
            .apply(Command::MoveWindow {
                window: b,
                before: None,
            })
            .unwrap();
        assert_eq!(workspace, unchanged);
        workspace.apply(Command::NextWindow).unwrap();
        assert_eq!(active_window(&workspace).id(), a);
        assert!(workspace.is_valid());
    }

    #[test]
    fn invalid_moves_and_swaps_are_atomic_before_id_allocation() {
        let mut workspace = workspace();
        let a = focused(&workspace);
        let first_window = active_window(&workspace).id();
        workspace
            .apply(Command::SplitPane {
                pane: a,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        let b = focused(&workspace);
        workspace
            .apply(Command::CreateSession {
                title: "Foreign".into(),
            })
            .unwrap();
        let foreign = focused(&workspace);
        let foreign_window = active_window(&workspace).id();
        let original = workspace.clone();
        let mut invalid = vec![
            Command::MovePane {
                pane: a,
                anchor: a,
                axis: SplitAxis::Horizontal,
                before: false,
                ratio: 0.5,
            },
            Command::MovePane {
                pane: a,
                anchor: foreign,
                axis: SplitAxis::Horizontal,
                before: false,
                ratio: 0.5,
            },
            Command::MovePane {
                pane: PaneId(999),
                anchor: b,
                axis: SplitAxis::Horizontal,
                before: false,
                ratio: 0.5,
            },
            Command::MovePane {
                pane: a,
                anchor: PaneId(999),
                axis: SplitAxis::Horizontal,
                before: false,
                ratio: 0.5,
            },
            Command::SwapPanes { pane: a, other: a },
            Command::SwapPanes {
                pane: a,
                other: foreign,
            },
            Command::SwapPanes {
                pane: a,
                other: PaneId(999),
            },
            Command::SwapPanes {
                pane: PaneId(999),
                other: b,
            },
            Command::MoveWindow {
                window: first_window,
                before: Some(first_window),
            },
            Command::MoveWindow {
                window: first_window,
                before: Some(foreign_window),
            },
            Command::MoveWindow {
                window: first_window,
                before: Some(WindowId(999)),
            },
            Command::MoveWindow {
                window: WindowId(999),
                before: None,
            },
        ];
        for ratio in [0., 1., -0.1, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            invalid.push(Command::MovePane {
                pane: a,
                anchor: b,
                axis: SplitAxis::Vertical,
                before: true,
                ratio,
            });
        }
        for command in invalid {
            assert!(workspace.apply(command).is_err());
            assert_eq!(workspace, original);
        }
    }

    #[test]
    fn moving_a_pane_with_exhausted_ids_leaves_the_tree_unchanged() {
        let mut workspace = workspace();
        let a = focused(&workspace);
        workspace
            .apply(Command::SplitPane {
                pane: a,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        let b = focused(&workspace);
        workspace.next_id = u64::MAX;
        let original = workspace.clone();
        assert_eq!(
            workspace.apply(Command::MovePane {
                pane: a,
                anchor: b,
                axis: SplitAxis::Horizontal,
                before: false,
                ratio: 0.5
            }),
            Err(WorkspaceError::IdExhausted)
        );
        assert_eq!(workspace, original);
    }

    #[test]
    fn received_state_requires_valid_focus_and_unique_allocated_ids() {
        let original = workspace();
        assert!(original.is_valid());
        assert!(Workspace::default().is_valid());
        let mut invalid = original.clone();
        invalid.sessions[0].active_window = usize::MAX;
        assert!(!invalid.is_valid());
        invalid = original.clone();
        invalid.sessions[0].windows[0].active_pane = PaneId(0);
        assert!(!invalid.is_valid());
        invalid = original.clone();
        let pane = focused(&invalid);
        invalid.sessions[0].windows[0].layout = Layout::Split {
            id: SplitId(1),
            ratio: 500,
            axis: SplitAxis::Horizontal,
            first: Box::new(Layout::Pane(pane)),
            second: Box::new(Layout::Pane(pane)),
        };
        assert!(!invalid.is_valid());
        invalid = original.clone();
        invalid.next_id = 0;
        assert!(!invalid.is_valid());
        invalid = original;
        invalid.active_session = None;
        assert!(!invalid.is_valid());
    }

    #[test]
    fn nested_splits_collapse_and_focus_stays_valid() {
        let mut workspace = workspace();
        let first = focused(&workspace);
        workspace
            .apply(Command::SplitPane {
                pane: first,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        let second = focused(&workspace);
        workspace
            .apply(Command::SplitPane {
                pane: second,
                axis: SplitAxis::Vertical,
            })
            .unwrap();
        let third = focused(&workspace);
        workspace.apply(Command::ClosePane(second)).unwrap();
        assert_eq!(focused(&workspace), third);
        assert_eq!(
            active_layout(&workspace),
            &Layout::Split {
                id: SplitId(4),
                ratio: 500,
                axis: SplitAxis::Horizontal,
                first: Box::new(Layout::Pane(first)),
                second: Box::new(Layout::Pane(third)),
            }
        );
        workspace.apply(Command::ClosePane(third)).unwrap();
        assert_eq!(focused(&workspace), first);
        assert_eq!(active_layout(&workspace), &Layout::Pane(first));
    }

    #[test]
    fn last_pane_closes_its_session_and_empty_workspace_can_restart() {
        let mut workspace = workspace();
        let old_pane = focused(&workspace);
        workspace.apply(Command::ClosePane(old_pane)).unwrap();
        assert_eq!(workspace.sessions(), []);
        assert!(workspace.active_session().is_none());
        workspace
            .apply(Command::CreateSession {
                title: "New".into(),
            })
            .unwrap();
        assert_ne!(focused(&workspace), old_pane);
    }

    #[test]
    fn closing_session_selects_neighbor_and_preserves_other_focus() {
        let mut workspace = workspace();
        let first = workspace.active_session().unwrap().id();
        workspace
            .apply(Command::CreateSession {
                title: "Second".into(),
            })
            .unwrap();
        let second = workspace.active_session().unwrap().id();
        workspace
            .apply(Command::CreateSession {
                title: "Third".into(),
            })
            .unwrap();
        let third = workspace.active_session().unwrap().id();
        workspace.apply(Command::CloseSession(first)).unwrap();
        assert_eq!(workspace.active_session().unwrap().id(), third);
        workspace.apply(Command::CloseSession(third)).unwrap();
        assert_eq!(workspace.active_session().unwrap().id(), second);
    }

    #[test]
    fn focus_cycle_follows_visual_tree_order_and_wraps() {
        let mut workspace = workspace();
        let first = focused(&workspace);
        workspace
            .apply(Command::SplitPane {
                pane: first,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        let second = focused(&workspace);
        workspace.apply(Command::FocusNextPane).unwrap();
        assert_eq!(focused(&workspace), first);
        workspace.apply(Command::FocusNextPane).unwrap();
        assert_eq!(focused(&workspace), second);
    }

    #[test]
    fn stale_commands_and_invalid_titles_leave_workspace_unchanged() {
        let mut workspace = workspace();
        let original = workspace.clone();
        for command in [
            Command::CreateSession { title: "  ".into() },
            Command::SelectSession(SessionId(999)),
            Command::ClosePane(PaneId(999)),
            Command::SplitPane {
                pane: PaneId(999),
                axis: SplitAxis::Horizontal,
            },
            Command::RenameSession {
                session: workspace.sessions()[0].id(),
                title: " ".into(),
            },
        ] {
            assert!(workspace.apply(command).is_err());
            assert_eq!(workspace, original);
        }
    }

    #[test]
    fn ids_are_not_reused_and_focus_can_switch_sessions() {
        let mut workspace = workspace();
        let old_pane = focused(&workspace);
        workspace
            .apply(Command::CreateSession {
                title: "Other".into(),
            })
            .unwrap();
        let new_pane = focused(&workspace);
        assert_ne!(old_pane, new_pane);
        workspace.apply(Command::FocusPane(old_pane)).unwrap();
        assert_eq!(focused(&workspace), old_pane);
        workspace.apply(Command::ClosePane(new_pane)).unwrap();
        workspace
            .apply(Command::SplitPane {
                pane: old_pane,
                axis: SplitAxis::Vertical,
            })
            .unwrap();
        assert_ne!(focused(&workspace), new_pane);
    }

    #[test]
    fn allocation_failure_does_not_partially_create_a_session() {
        let mut workspace = Workspace {
            next_id: u64::MAX - 1,
            ..Workspace::default()
        };
        let original = workspace.clone();
        assert_eq!(
            workspace.apply(Command::CreateSession {
                title: "Test".into()
            }),
            Err(WorkspaceError::IdExhausted)
        );
        assert_eq!(workspace, original);
    }
    #[test]
    fn tabs_preserve_layout_and_focus_independently() {
        let mut workspace = workspace();
        let session = workspace.active_session().unwrap().id();
        let first_window = active_window(&workspace).id();
        let first_pane = focused(&workspace);
        workspace
            .apply(Command::SplitPane {
                pane: first_pane,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        let split_pane = focused(&workspace);
        workspace
            .apply(Command::CreateWindow {
                session,
                title: "Build".into(),
            })
            .unwrap();
        let second_window = active_window(&workspace).id();
        let second_pane = focused(&workspace);
        assert_eq!(workspace.pane_ids().len(), 3);
        workspace
            .apply(Command::SelectWindow(first_window))
            .unwrap();
        assert_eq!(focused(&workspace), split_pane);
        workspace.apply(Command::NextWindow).unwrap();
        assert_eq!(active_window(&workspace).id(), second_window);
        assert_eq!(focused(&workspace), second_pane);
        workspace.apply(Command::FocusPane(first_pane)).unwrap();
        assert_eq!(active_window(&workspace).id(), first_window);
    }

    #[test]
    fn closing_inactive_tabs_preserves_selection_and_last_tab_closes_session() {
        let mut workspace = workspace();
        let session = workspace.active_session().unwrap().id();
        let first_window = active_window(&workspace).id();
        workspace
            .apply(Command::CreateWindow {
                session,
                title: "Other".into(),
            })
            .unwrap();
        let second_window = active_window(&workspace).id();
        let second_pane = focused(&workspace);
        workspace.apply(Command::CloseWindow(first_window)).unwrap();
        assert_eq!(active_window(&workspace).id(), second_window);
        assert_eq!(focused(&workspace), second_pane);
        workspace.apply(Command::ClosePane(second_pane)).unwrap();
        assert_eq!(workspace.sessions(), []);
    }

    #[test]
    fn invalid_tab_commands_are_atomic_and_labels_can_change() {
        let mut workspace = workspace();
        let session = workspace.active_session().unwrap().id();
        let window = active_window(&workspace).id();
        let original = workspace.clone();
        for command in [
            Command::CreateWindow {
                session,
                title: " ".into(),
            },
            Command::SelectWindow(WindowId(999)),
            Command::CloseWindow(WindowId(999)),
            Command::RenameWindow {
                window,
                title: " ".into(),
            },
        ] {
            assert!(workspace.apply(command).is_err());
            assert_eq!(workspace, original);
        }
        workspace
            .apply(Command::RenameWindow {
                window,
                title: " Build ".into(),
            })
            .unwrap();
        assert_eq!(active_window(&workspace).title(), "Build");
    }

    #[test]
    fn split_identity_and_ratio_survive_descendant_collapse() {
        let mut workspace = workspace();
        let first = focused(&workspace);
        workspace
            .apply(Command::SplitPane {
                pane: first,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        let second = focused(&workspace);
        let split = active_layout(&workspace).split_ids()[0];
        workspace
            .apply(Command::ResizeSplit { split, ratio: 650 })
            .unwrap();
        workspace
            .apply(Command::SplitPane {
                pane: second,
                axis: SplitAxis::Vertical,
            })
            .unwrap();
        let removed_split = active_layout(&workspace).split_ids()[1];
        let third = focused(&workspace);
        workspace.apply(Command::ClosePane(second)).unwrap();
        assert_eq!(
            active_layout(&workspace),
            &Layout::Split {
                id: split,
                axis: SplitAxis::Horizontal,
                ratio: 650,
                first: Box::new(Layout::Pane(first)),
                second: Box::new(Layout::Pane(third)),
            }
        );
        assert!(workspace.is_valid());
        let original = workspace.clone();
        assert_eq!(
            workspace.apply(Command::ResizeSplit {
                split: removed_split,
                ratio: 500
            }),
            Err(WorkspaceError::UnknownSplit(removed_split))
        );
        assert_eq!(workspace, original);
        workspace.apply(Command::ClosePane(third)).unwrap();
        workspace
            .apply(Command::SplitPane {
                pane: first,
                axis: SplitAxis::Vertical,
            })
            .unwrap();
        let replacement = active_layout(&workspace).split_ids()[0];
        assert_ne!(split, replacement);
        assert_ne!(removed_split, replacement);
    }

    #[test]
    fn invalid_ratio_and_exhausted_split_allocation_are_atomic() {
        let mut workspace = workspace();
        let pane = focused(&workspace);
        workspace
            .apply(Command::SplitPane {
                pane,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        let split = active_layout(&workspace).split_ids()[0];
        let original = workspace.clone();
        for ratio in [0, 99, 901, u16::MAX] {
            assert_eq!(
                workspace.apply(Command::ResizeSplit { split, ratio }),
                Err(WorkspaceError::InvalidSplitRatio)
            );
            assert_eq!(workspace, original);
        }
        for ratio in [100, 900] {
            workspace
                .apply(Command::ResizeSplit { split, ratio })
                .unwrap();
            assert!(workspace.is_valid());
        }
        workspace.next_id = u64::MAX - 1;
        let original = workspace.clone();
        assert_eq!(
            workspace.apply(Command::SplitPane {
                pane,
                axis: SplitAxis::Vertical
            }),
            Err(WorkspaceError::IdExhausted)
        );
        assert_eq!(workspace, original);
    }

    #[test]
    fn zoom_is_per_window_and_tracks_focus_and_close() {
        let mut workspace = workspace();
        let session = workspace.active_session().unwrap().id();
        let first_window = active_window(&workspace).id();
        let first = focused(&workspace);
        workspace
            .apply(Command::SplitPane {
                pane: first,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        let second = focused(&workspace);
        workspace.apply(Command::ToggleZoom(first)).unwrap();
        assert_eq!(focused(&workspace), first);
        assert_eq!(active_window(&workspace).zoomed_pane(), Some(first));
        workspace
            .apply(Command::CreateWindow {
                session,
                title: "Other".into(),
            })
            .unwrap();
        let other = focused(&workspace);
        workspace.apply(Command::ToggleZoom(other)).unwrap();
        workspace
            .apply(Command::SelectWindow(first_window))
            .unwrap();
        assert_eq!(active_window(&workspace).zoomed_pane(), Some(first));
        workspace.apply(Command::FocusPane(second)).unwrap();
        assert_eq!(active_window(&workspace).zoomed_pane(), None);
        workspace.apply(Command::ToggleZoom(second)).unwrap();
        workspace.apply(Command::ClosePane(first)).unwrap();
        assert_eq!(active_window(&workspace).zoomed_pane(), Some(second));
        workspace.apply(Command::ToggleZoom(second)).unwrap();
        assert_eq!(active_window(&workspace).zoomed_pane(), None);
        workspace.apply(Command::ToggleZoom(second)).unwrap();
        workspace.apply(Command::ClosePane(second)).unwrap();
        assert_eq!(focused(&workspace), other);
        assert_eq!(active_window(&workspace).zoomed_pane(), Some(other));
        assert!(workspace.is_valid());
    }

    #[test]
    fn new_split_unzooms_and_invalid_zoom_preserves_state() {
        let mut workspace = workspace();
        let pane = focused(&workspace);
        workspace.apply(Command::ToggleZoom(pane)).unwrap();
        let original = workspace.clone();
        assert_eq!(
            workspace.apply(Command::ToggleZoom(PaneId(999))),
            Err(WorkspaceError::UnknownPane(PaneId(999)))
        );
        assert_eq!(workspace, original);
        workspace
            .apply(Command::SplitPane {
                pane,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        assert_eq!(active_window(&workspace).zoomed_pane(), None);
        assert!(workspace.is_valid());
    }

    #[test]
    fn directional_focus_uses_ratios_and_geometric_alignment() {
        let mut workspace = workspace();
        let left = focused(&workspace);
        workspace
            .apply(Command::SplitPane {
                pane: left,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        let top_right = focused(&workspace);
        workspace
            .apply(Command::SplitPane {
                pane: top_right,
                axis: SplitAxis::Vertical,
            })
            .unwrap();
        let bottom_right = focused(&workspace);
        let vertical = active_layout(&workspace).split_ids()[1];
        workspace
            .apply(Command::ResizeSplit {
                split: vertical,
                ratio: 300,
            })
            .unwrap();
        workspace.apply(Command::FocusPane(left)).unwrap();
        workspace
            .apply(Command::FocusDirection {
                direction: Direction::Right,
                wrap: false,
            })
            .unwrap();
        assert_eq!(focused(&workspace), bottom_right);
        workspace
            .apply(Command::FocusDirection {
                direction: Direction::Up,
                wrap: false,
            })
            .unwrap();
        assert_eq!(focused(&workspace), top_right);
        workspace
            .apply(Command::FocusDirection {
                direction: Direction::Left,
                wrap: false,
            })
            .unwrap();
        assert_eq!(focused(&workspace), left);
        workspace
            .apply(Command::ResizeSplit {
                split: vertical,
                ratio: 700,
            })
            .unwrap();
        workspace
            .apply(Command::FocusDirection {
                direction: Direction::Right,
                wrap: false,
            })
            .unwrap();
        assert_eq!(focused(&workspace), top_right);
        workspace
            .apply(Command::FocusDirection {
                direction: Direction::Down,
                wrap: false,
            })
            .unwrap();
        assert_eq!(focused(&workspace), bottom_right);
        assert!(workspace.is_valid());
    }

    #[test]
    fn directional_edges_are_noops_unless_wrapping_requested() {
        let mut workspace = workspace();
        let left = focused(&workspace);
        workspace
            .apply(Command::SplitPane {
                pane: left,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        let right = focused(&workspace);
        workspace.apply(Command::ToggleZoom(right)).unwrap();
        let original = workspace.clone();
        workspace
            .apply(Command::FocusDirection {
                direction: Direction::Right,
                wrap: false,
            })
            .unwrap();
        assert_eq!(workspace, original);
        workspace
            .apply(Command::FocusDirection {
                direction: Direction::Right,
                wrap: true,
            })
            .unwrap();
        assert_eq!(focused(&workspace), left);
        assert_eq!(active_window(&workspace).zoomed_pane(), None);
        workspace
            .apply(Command::FocusDirection {
                direction: Direction::Left,
                wrap: true,
            })
            .unwrap();
        assert_eq!(focused(&workspace), right);
        workspace.apply(Command::ClosePane(right)).unwrap();
        let original = workspace.clone();
        workspace
            .apply(Command::FocusDirection {
                direction: Direction::Down,
                wrap: true,
            })
            .unwrap();
        assert_eq!(workspace, original);
    }

    #[test]
    fn decoded_state_rejects_invalid_split_or_zoom_metadata() {
        let mut workspace = workspace();
        let pane = focused(&workspace);
        workspace
            .apply(Command::SplitPane {
                pane,
                axis: SplitAxis::Horizontal,
            })
            .unwrap();
        let original = workspace.clone();
        if let Layout::Split { ratio, .. } = &mut workspace.sessions[0].windows[0].layout {
            *ratio = 0;
        }
        assert!(!workspace.is_valid());
        workspace = original.clone();
        if let Layout::Split { id, .. } = &mut workspace.sessions[0].windows[0].layout {
            *id = SplitId(pane.value());
        }
        assert!(!workspace.is_valid());
        workspace = original.clone();
        workspace.sessions[0].windows[0].zoomed = Some(pane);
        assert!(!workspace.is_valid());
        workspace = original;
        workspace.sessions[0].windows[0].zoomed = Some(PaneId(999));
        assert!(!workspace.is_valid());
        assert_eq!(PaneId::from_value(0), None);
        assert_eq!(PaneId::from_value(pane.value()), Some(pane));
    }
}
