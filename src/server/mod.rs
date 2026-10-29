//! Local Linux session server and nonblocking desktop client.
//!
//! This is a private, versioned Rex-rs protocol, not the official Rex protocol.
pub mod cli;
mod client;
mod host;
pub mod protocol;
mod storage;
pub mod updates;

pub use client::WorkspaceClient;
pub use host::{run_default, serve};

use anyhow::{Result, ensure};
use std::path::PathBuf;

/// The host validates ownership and permissions before binding this endpoint.
///
/// # Errors
/// Returns an error when an explicitly configured runtime directory is relative.
pub fn default_socket_path() -> Result<PathBuf> {
    let parent = if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(path);
        ensure!(path.is_absolute(), "XDG_RUNTIME_DIR must be absolute");
        path.join("rex-rs")
    } else {
        std::env::temp_dir().join(format!("rex-rs-{}", nix::unistd::geteuid()))
    };
    Ok(parent.join("server.sock"))
}
