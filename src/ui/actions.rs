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
        Quit
    ]
);

pub fn register(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-shift-n", NewSession, Some("Workspace")),
        KeyBinding::new("ctrl-shift-d", SplitRight, Some("Workspace")),
        KeyBinding::new("ctrl-shift-e", SplitDown, Some("Workspace")),
        KeyBinding::new("ctrl-shift-w", ClosePane, Some("Workspace")),
        KeyBinding::new("ctrl-alt-w", CloseSession, Some("Workspace")),
        KeyBinding::new("ctrl-tab", NextPane, Some("Workspace")),
        KeyBinding::new("ctrl-shift-q", Quit, Some("Workspace")),
    ]);
}
