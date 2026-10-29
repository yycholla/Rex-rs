use gpui::{App, actions};

actions!(
    rex_rs,
    [
        NewSession,
        SplitRight,
        SplitDown,
        ClosePane,
        CloseSession,
        NextPane,
        FocusLeft,
        FocusRight,
        FocusUp,
        FocusDown,
        Quit,
        NewWindow,
        CloseWindow,
        NextWindow,
        PreviousWindow,
        Copy,
        Paste,
        ToggleZoom,
        RenameSession,
        RenameWindow
    ]
);

pub fn register(_cx: &mut App) {
    // All keyboard defaults, including Quit, go through Rex's merged keymap.
    // The workspace supplies a disconnected fallback before a keymap is ready.
}
