use std::fmt;

macro_rules! identity {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);
        impl $name {
            #[must_use]
            pub const fn value(self) -> u64 {
                self.0
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

/// A binary split tree. Horizontal places children side by side; Vertical stacks them.
/// Closing a leaf collapses its parent into the surviving sibling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Layout {
    Pane(PaneId),
    Split {
        axis: SplitAxis,
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

    fn split(&mut self, target: PaneId, added: PaneId, axis: SplitAxis) -> bool {
        match self {
            Self::Pane(id) if *id == target => {
                *self = Self::Split {
                    axis,
                    first: Box::new(Self::Pane(target)),
                    second: Box::new(Self::Pane(added)),
                };
                true
            }
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                first.split(target, added, axis) || second.split(target, added, axis)
            }
        }
    }

    fn without(self, target: PaneId) -> Option<Self> {
        match self {
            Self::Pane(id) => (id != target).then_some(Self::Pane(id)),
            Self::Split {
                axis,
                first,
                second,
            } => match (first.without(target), second.without(target)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    axis,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (survivor, None) | (None, survivor) => survivor,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceWindow {
    id: WindowId,
    title: String,
    layout: Layout,
    active_pane: PaneId,
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    id: SessionId,
    title: String,
    windows: Vec<WorkspaceWindow>,
    active_window: usize,
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
    pub fn active_window(&self) -> &WorkspaceWindow {
        &self.windows[self.active_window]
    }
    #[must_use]
    pub fn layout(&self) -> &Layout {
        self.active_window().layout()
    }
    #[must_use]
    pub fn active_pane(&self) -> PaneId {
        self.active_window().active_pane()
    }
}

/// Commands are the one mutation boundary shared by buttons and key bindings.
#[derive(Clone, Debug)]
pub enum Command {
    CreateSession { title: String },
    SelectSession(SessionId),
    RenameSession { session: SessionId, title: String },
    CloseSession(SessionId),
    CreateWindow { session: SessionId, title: String },
    SelectWindow(WindowId),
    RenameWindow { window: WindowId, title: String },
    CloseWindow(WindowId),
    NextWindow,
    PreviousWindow,
    SplitPane { pane: PaneId, axis: SplitAxis },
    FocusPane(PaneId),
    ClosePane(PaneId),
    FocusNextPane,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Workspace {
    sessions: Vec<Session>,
    active_session: Option<SessionId>,
    next_id: u64,
}

impl Workspace {
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
            .flat_map(|session| session.windows.iter())
            .flat_map(|window| window.layout.panes())
            .collect()
    }

    /// Apply a command without rebuilding terminals or reusing identities.
    ///
    /// # Errors
    /// Returns an error for stale IDs, empty labels, missing active sessions, or
    /// exhausted IDs. Validation failures leave state unchanged.
    pub fn apply(&mut self, command: Command) -> Result<(), WorkspaceError> {
        match command {
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
            Command::FocusPane(pane) => {
                let (session, window) = self.pane_index(pane)?;
                self.sessions[session].windows[window].active_pane = pane;
                self.select_window(session, window);
            }
            Command::ClosePane(pane) => self.close_pane(pane)?,
            Command::FocusNextPane => self.cycle_pane()?,
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
            }],
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
        });
        self.select_window(index, self.sessions[index].windows.len() - 1);
        self.next_id = last;
        Ok(())
    }

    fn split_pane(&mut self, pane: PaneId, axis: SplitAxis) -> Result<(), WorkspaceError> {
        let (session, window) = self.pane_index(pane)?;
        let next = self
            .next_id
            .checked_add(1)
            .ok_or(WorkspaceError::IdExhausted)?;
        let added = PaneId(next);
        let target = &mut self.sessions[session].windows[window];
        target.layout.split(pane, added, axis);
        target.active_pane = added;
        self.select_window(session, window);
        self.next_id = next;
        Ok(())
    }

    fn close_pane(&mut self, pane: PaneId) -> Result<(), WorkspaceError> {
        let (session, window) = self.pane_index(pane)?;
        let target = &mut self.sessions[session].windows[window];
        if let Some(layout) = target.layout.clone().without(pane) {
            if target.active_pane == pane {
                target.active_pane = layout.panes()[0];
            }
            target.layout = layout;
        } else {
            self.remove_window(session, window);
        }
        Ok(())
    }

    fn cycle_pane(&mut self) -> Result<(), WorkspaceError> {
        let active = self.active_session.ok_or(WorkspaceError::NoActiveSession)?;
        let index = self.session_index(active)?;
        let session = &mut self.sessions[index];
        let window = &mut session.windows[session.active_window];
        let panes = window.layout.panes();
        let position = panes
            .iter()
            .position(|pane| *pane == window.active_pane)
            .ok_or(WorkspaceError::UnknownPane(window.active_pane))?;
        window.active_pane = panes[(position + 1) % panes.len()];
        Ok(())
    }

    fn cycle_window(&mut self, forward: bool) -> Result<(), WorkspaceError> {
        let active = self.active_session.ok_or(WorkspaceError::NoActiveSession)?;
        let index = self.session_index(active)?;
        let session = &mut self.sessions[index];
        session.active_window = if forward {
            (session.active_window + 1) % session.windows.len()
        } else {
            (session.active_window + session.windows.len() - 1) % session.windows.len()
        };
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
    fn select_window(&mut self, session: usize, window: usize) {
        self.sessions[session].active_window = window;
        self.active_session = Some(self.sessions[session].id);
    }
    fn remove_window(&mut self, session: usize, window: usize) {
        let target = &mut self.sessions[session];
        target.windows.remove(window);
        if target.windows.is_empty() {
            self.remove_session(session);
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceError {
    EmptyTitle,
    NoActiveSession,
    UnknownSession(SessionId),
    UnknownPane(PaneId),
    UnknownWindow(WindowId),
    IdExhausted,
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTitle => f.write_str("title cannot be empty"),
            Self::NoActiveSession => f.write_str("no session is selected"),
            Self::UnknownSession(id) => write!(f, "session {id} no longer exists"),
            Self::UnknownWindow(id) => write!(f, "window {id} no longer exists"),
            Self::UnknownPane(id) => write!(f, "pane {id} no longer exists"),
            Self::IdExhausted => f.write_str("workspace ID space exhausted"),
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
        workspace.active_session().unwrap().active_pane()
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
            workspace.active_session().unwrap().layout(),
            &Layout::Split {
                axis: SplitAxis::Horizontal,
                first: Box::new(Layout::Pane(first)),
                second: Box::new(Layout::Pane(third)),
            }
        );
        workspace.apply(Command::ClosePane(third)).unwrap();
        assert_eq!(focused(&workspace), first);
        assert_eq!(
            workspace.active_session().unwrap().layout(),
            &Layout::Pane(first)
        );
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
        let first_window = workspace.active_session().unwrap().active_window().id();
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
        let second_window = workspace.active_session().unwrap().active_window().id();
        let second_pane = focused(&workspace);
        assert_eq!(workspace.pane_ids().len(), 3);
        workspace
            .apply(Command::SelectWindow(first_window))
            .unwrap();
        assert_eq!(focused(&workspace), split_pane);
        workspace.apply(Command::NextWindow).unwrap();
        assert_eq!(
            workspace.active_session().unwrap().active_window().id(),
            second_window
        );
        assert_eq!(focused(&workspace), second_pane);
        workspace.apply(Command::FocusPane(first_pane)).unwrap();
        assert_eq!(
            workspace.active_session().unwrap().active_window().id(),
            first_window
        );
    }

    #[test]
    fn closing_inactive_tabs_preserves_selection_and_last_tab_closes_session() {
        let mut workspace = workspace();
        let session = workspace.active_session().unwrap().id();
        let first_window = workspace.active_session().unwrap().active_window().id();
        workspace
            .apply(Command::CreateWindow {
                session,
                title: "Other".into(),
            })
            .unwrap();
        let second_window = workspace.active_session().unwrap().active_window().id();
        let second_pane = focused(&workspace);
        workspace.apply(Command::CloseWindow(first_window)).unwrap();
        assert_eq!(
            workspace.active_session().unwrap().active_window().id(),
            second_window
        );
        assert_eq!(focused(&workspace), second_pane);
        workspace.apply(Command::ClosePane(second_pane)).unwrap();
        assert_eq!(workspace.sessions(), []);
    }

    #[test]
    fn invalid_tab_commands_are_atomic_and_labels_can_change() {
        let mut workspace = workspace();
        let session = workspace.active_session().unwrap().id();
        let window = workspace.active_session().unwrap().active_window().id();
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
        assert_eq!(
            workspace.active_session().unwrap().active_window().title(),
            "Build"
        );
    }
}
