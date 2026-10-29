//! Platform-neutral input passed from the UI to the terminal adapter.

#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
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
#[cfg_attr(feature = "ghostty", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInput {
    pub key: String,
    pub text: Option<String>,
    pub modifiers: Modifiers,
    pub repeat: bool,
}

/// Terminal pointer events use zero-based cells, independent of the UI toolkit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseInput {
    pub action: MouseAction,
    pub button: MouseButton,
    pub column: u16,
    pub row: u16,
    pub modifiers: Modifiers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Move,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    None,
    Left,
    Middle,
    Right,
}

/// The application-requested pointer traffic; encoding remains the server's job.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseTracking {
    #[default]
    None,
    Press,
    Drag,
    Any,
}
