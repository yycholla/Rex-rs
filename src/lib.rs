//! Pure application state, a Ghostty adapter, and reusable GPUI views.
pub mod core;

#[cfg(feature = "ghostty")]
pub mod terminal;

#[cfg(feature = "desktop")]
pub mod ui;

#[cfg(feature = "ghostty")]
pub mod server;

#[cfg(feature = "ghostty")]
pub mod rex;
