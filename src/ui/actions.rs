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
    cx.bind_keys([
        KeyBinding::new("ctrl-shift-r", RenameSession, Some("Workspace")),
        KeyBinding::new("ctrl-alt-shift-r", RenameWindow, Some("Workspace")),
        KeyBinding::new("ctrl-shift-t", NewWindow, Some("Workspace")),
        KeyBinding::new("ctrl-alt-shift-w", CloseWindow, Some("Workspace")),
        KeyBinding::new("ctrl-pagedown", NextWindow, Some("Workspace")),
        KeyBinding::new("ctrl-pageup", PreviousWindow, Some("Workspace")),
        KeyBinding::new("ctrl-shift-c", Copy, Some("Workspace")),
        KeyBinding::new("ctrl-shift-v", Paste, Some("Workspace")),
        KeyBinding::new("ctrl-shift-z", ToggleZoom, Some("Workspace")),
        KeyBinding::new("ctrl-shift-n", NewSession, Some("Workspace")),
        KeyBinding::new("ctrl-shift-d", SplitRight, Some("Workspace")),
        KeyBinding::new("ctrl-shift-e", SplitDown, Some("Workspace")),
        KeyBinding::new("ctrl-shift-w", ClosePane, Some("Workspace")),
        KeyBinding::new("ctrl-alt-w", CloseSession, Some("Workspace")),
        KeyBinding::new("ctrl-tab", NextPane, Some("Workspace")),
        KeyBinding::new("ctrl-shift-q", Quit, Some("Workspace")),
    ]);
}
