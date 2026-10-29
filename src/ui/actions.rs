use gpui::{App, KeyBinding, actions};

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

pub fn register(cx: &mut App) {
    // Quit remains a process-level shortcut; workspace shortcuts are resolved
    // from Rex's merged keymap, including removals and configuration overrides.
    cx.bind_keys([KeyBinding::new("ctrl-shift-q", Quit, Some("Workspace"))]);
}
