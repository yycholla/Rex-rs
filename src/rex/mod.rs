//! Native client for the official Rex server.
mod action_worker;
pub mod actions;
pub mod automation;
mod client;
pub(crate) mod diagnostics;
pub mod insights;
pub mod protocol;
pub mod terminal;
pub mod transport;
pub mod workspace;

pub use client::{ClientScript, WorkspaceClient};

pub mod keymap;

mod inventory;
mod restore;
