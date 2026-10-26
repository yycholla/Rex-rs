//! Platform-neutral input passed from the UI to the terminal adapter.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Keyboard modifiers are independent flags, not mutually exclusive states."
)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub super_key: bool,
}

/// Normalized key names follow GPUI ("enter", "up", "backspace", "f1", etc.).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInput {
    pub key: String,
    pub text: Option<String>,
    pub modifiers: Modifiers,
    pub repeat: bool,
}
