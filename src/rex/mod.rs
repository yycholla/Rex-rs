//! Native client for the official Rex server.
mod action_worker;
pub mod actions;
pub mod automation;
mod client;
mod clipboard;
pub(crate) mod diagnostics;
pub mod discovery;
pub mod endpoint;
pub mod insights;
pub mod protocol;
pub mod servers;
pub mod ssh;
pub mod terminal;
pub mod transport;
pub mod workspace;

pub use client::{ClientScript, WorkspaceClient};

pub mod keymap;

mod inventory;
mod restore;
