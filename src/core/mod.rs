//! Application data and behavior. No graphics, terminal FFI, or IO dependencies.
pub mod frame;
pub mod grid;
pub mod input;
pub mod workspace;

pub use grid::{GridSize, InvalidGridSize};
pub use workspace::{
    Command, Layout, PaneId, Session, SessionId, SplitAxis, WindowId, Workspace, WorkspaceError,
    WorkspaceWindow,
};
