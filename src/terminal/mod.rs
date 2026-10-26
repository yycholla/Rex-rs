mod ghostty;
mod pty;
mod workspace;

pub use ghostty::{GhosttyTerminal, TerminalConfig};
pub use pty::{PaneStatus, ShellConfig};
pub use workspace::TerminalWorkspace;
