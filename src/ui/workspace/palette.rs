//! Searchable native commands dispatched through the existing ordered action lane.
use super::{WorkspaceView, shortcuts::Work};
use crate::{
    rex::{actions, automation::ActionError},
    ui::components::{
        ButtonVariant, button, modal, overlay_footer,
        text_field::{TextField, TextFieldEvent},
    },
};
use gpui::{
    Context, Div, Entity, EntityInputHandler as _, Focusable, KeyDownEvent, Subscription, Window,
    div, prelude::*, px, rgb,
};
use serde_json::{Value, json};

const VISIBLE_RESULTS: usize = 8;

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    title: String,
    name: String,
    args: Value,
}

pub(super) struct PaletteState {
    field: Entity<TextField>,
    entries: Vec<Entry>,
    selected: usize,
    connection: u64,
    _subscription: Subscription,
}

impl WorkspaceView {
    pub(super) fn palette_focus(&self, cx: &gpui::App) -> Option<gpui::FocusHandle> {
        self.palette
            .as_ref()
            .map(|palette| palette.field.focus_handle(cx))
    }

    pub(super) fn palette_is_composing(&self, cx: &gpui::App) -> bool {
        self.palette
            .as_ref()
            .is_some_and(|palette| palette.field.read(cx).is_composing())
    }

    pub(super) fn toggle_palette(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), ActionError> {
        if self.palette.is_some() {
            self.close_palette(window, cx);
            return Ok(());
        }
        if self.has_modal() {
            return Err(ActionError::refused(
                "Finish the current dialog before opening commands",
            ));
        }
        self.close_find(window, cx);
        let entries = self.palette_entries();
        let field = cx.new(|cx| TextField::new(String::new(), "Search commands".into(), cx));
        let subscription =
            cx.subscribe_in(&field, window, |view, _, event, window, cx| match event {
                TextFieldEvent::Changed => {
                    if let Some(palette) = &mut view.palette {
                        palette.selected = 0;
                    }
                    cx.notify();
                }
                TextFieldEvent::Submitted => view.run_palette_selection(window, cx),
                TextFieldEvent::Cancelled => view.close_palette(window, cx),
            });
        let focus = field.focus_handle(cx);
        self.palette = Some(PaletteState {
            field,
            entries,
            selected: 0,
            connection: self.runtime.connection_generation(),
            _subscription: subscription,
        });
        if let Ok(resolver) = self.shortcuts.resolver_mut() {
            resolver.cancel_sequence();
        }
        focus.focus(window);
        cx.notify();
        Ok(())
    }

    fn palette_entries(&self) -> Vec<Entry> {
        let mut entries = registry_entries();
        for session in self.runtime.state().sessions() {
            entries.push(Entry {
                title: format!("Switch to Session: {}", session.title()),
                name: "session.select".into(),
                args: json!({"session_id":session.remote_id()}),
            });
            for tab in session.windows() {
                entries.push(Entry {
                    title: format!("Switch to Tab: {} / {}", session.title(), tab.label()),
                    name: "session.select".into(),
                    args: json!({"session_id":session.remote_id(),"window_id":tab.remote_id()}),
                });
            }
        }
        entries.retain(|entry| {
            entry.name != "client.palette.toggle"
                && if self.runtime.connected() {
                    self.shortcut_available(&entry.name, &entry.args)
                } else {
                    entry.name == "client.quit"
                }
        });
        entries.sort_by_key(|entry| entry.title.to_lowercase());
        entries
    }

    pub(super) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn run_palette_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(palette) = &self.palette else { return };
        let indices = matching_entries(&palette.entries, palette.field.read(cx).value());
        let Some(entry) = indices
            .get(palette.selected)
            .map(|index| palette.entries[*index].clone())
        else {
            return;
        };
        if entry.name != "client.quit" && palette.connection != self.runtime.connection_generation()
        {
            self.close_palette(window, cx);
            self.report_error(
                "The workspace connection changed; reopen commands".into(),
                cx,
            );
            return;
        }
        self.close_palette(window, cx);
        self.submit_palette_action(entry.name, entry.args, window, cx);
    }

    fn move_palette_selection(&mut self, older: bool, cx: &mut Context<Self>) {
        if let Some(palette) = &mut self.palette {
            let count = matching_entries(&palette.entries, palette.field.read(cx).value()).len();
            palette.selected = next_selection(palette.selected, count, older);
            cx.notify();
        }
    }

    /// Return true for every key while open so terminal shortcuts never consume
    /// query text. Unhandled keys continue to the field's normal IME/editing path.
    pub(super) fn palette_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.palette.is_none() {
            return false;
        }
        if self.palette_is_composing(cx) {
            return true;
        }
        match event.keystroke.key.as_str() {
            "up" | "down" => self.move_palette_selection(event.keystroke.key == "up", cx),
            "escape" => self.close_palette(window, cx),
            _ => return true,
        }
        window.prevent_default();
        cx.stop_propagation();
        true
    }

    pub(super) fn replay_palette_input(
        &mut self,
        work: &Work,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(field) = self.palette.as_ref().map(|palette| palette.field.clone()) else {
            return false;
        };
        match work {
            Work::Key(key, _) if matches!(key.key.as_str(), "up" | "down") => {
                self.move_palette_selection(key.key == "up", cx);
            }
            Work::Key(key, _) => field.update(cx, |field, cx| field.replay_key(key, window, cx)),
            Work::Text(text) => field.update(cx, |field, cx| {
                field.replace_text_in_range(None, text, window, cx);
            }),
            _ => return false,
        }
        true
    }

    pub(super) fn palette_dialog(&self, cx: &Context<Self>) -> Div {
        let Some(palette) = &self.palette else {
            return div();
        };
        let theme = self.theme;
        let indices = matching_entries(&palette.entries, palette.field.read(cx).value());
        let start = palette.selected.saturating_sub(VISIBLE_RESULTS - 1);
        let mut list = div().flex().flex_col().gap_1().px_3().pb_3();
        for (position, index) in indices.iter().enumerate().skip(start).take(VISIBLE_RESULTS) {
            let entry = &palette.entries[*index];
            list = list.child(
                div()
                    .id(("palette-command", *index))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(rgb(if position == palette.selected {
                        theme.colors.selected
                    } else {
                        theme.colors.surface
                    }))
                    .hover(|style| style.bg(rgb(theme.colors.hover)))
                    .child(entry.title.clone())
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(theme.colors.muted))
                            .child(entry.name.clone()),
                    )
                    .on_click(cx.listener(move |view, _, window, cx| {
                        if let Some(palette) = &mut view.palette {
                            palette.selected = position;
                        }
                        view.run_palette_selection(window, cx);
                    })),
            );
        }
        if indices.is_empty() {
            list = list.child(
                div()
                    .p_3()
                    .text_color(rgb(theme.colors.muted))
                    .child("No matching commands"),
            );
        }
        modal(
            560.,
            div()
                .child(div().p_3().child(palette.field.clone()))
                .child(list)
                .child(
                    overlay_footer(theme)
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(11.))
                                .text_color(rgb(theme.colors.muted))
                                .child(format!(
                                    "{} commands · ↑↓ select · Enter run",
                                    indices.len()
                                )),
                        )
                        .child(
                            button("palette-close", "Cancel", ButtonVariant::Quiet, theme)
                                .on_click(cx.listener(Self::close_palette_clicked)),
                        ),
                ),
            theme,
        )
    }

    fn close_palette_clicked(
        &mut self,
        _: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_palette(window, cx);
    }
}

/// Generate runnable no-argument and direction variants from the declared schema,
/// rather than maintaining a second command catalog. Targeted session/tab entries
/// are filled from the authoritative view by the workspace.
fn registry_entries() -> Vec<Entry> {
    let mut result = Vec::new();
    for action in actions::declarations().as_array().into_iter().flatten() {
        let (Some(name), Some(title)) = (action["name"].as_str(), action["title"].as_str()) else {
            continue;
        };
        if action["args"]["required"]
            .as_array()
            .is_some_and(Vec::is_empty)
        {
            result.push(Entry {
                title: title.into(),
                name: name.into(),
                args: json!({}),
            });
        } else if action["args"]["required"] == json!(["direction"]) {
            for direction in action["args"]["properties"]["direction"]["enum"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                result.push(Entry {
                    title: format!("{title}: {direction}"),
                    name: name.into(),
                    args: json!({"direction":direction}),
                });
            }
        }
    }
    result
}

fn matching_entries(entries: &[Entry], query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    let words: Vec<_> = query.split_whitespace().collect();
    entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let searchable = format!("{} {}", entry.title, entry.name).to_lowercase();
            words
                .iter()
                .all(|word| searchable.contains(word))
                .then_some(index)
        })
        .collect()
}

fn next_selection(current: usize, count: usize, up: bool) -> usize {
    if count == 0 {
        return 0;
    }
    if up {
        current.checked_sub(1).unwrap_or(count - 1)
    } else {
        (current + 1) % count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_registry_variants_have_valid_arguments_and_all_empty_actions() -> anyhow::Result<()>
    {
        let entries = registry_entries();
        for entry in &entries {
            actions::parse(&entry.name, &entry.args)?;
        }
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.name == "pane.split")
                .count(),
            4
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.name == "pane.focus")
                .count(),
            4
        );
        assert!(entries.iter().any(|entry| entry.name == "client.quit"));
        assert!(!entries.iter().any(|entry| entry.name == "pane.move"));
        Ok(())
    }

    #[test]
    fn filtering_matches_all_words_in_titles_or_action_names_without_losing_unicode() {
        let entries = vec![
            Entry {
                title: "Switch to Session: 東京".into(),
                name: "session.select".into(),
                args: json!({}),
            },
            Entry {
                title: "New Session".into(),
                name: "session.new".into(),
                args: json!({}),
            },
        ];
        assert_eq!(matching_entries(&entries, "東京 SESSION"), vec![0]);
        assert_eq!(matching_entries(&entries, "new session"), vec![1]);
        assert_eq!(matching_entries(&entries, "no match"), Vec::<usize>::new());
    }

    #[test]
    fn selection_wraps_and_empty_results_are_safe() {
        assert_eq!(next_selection(0, 0, true), 0);
        assert_eq!(next_selection(0, 3, true), 2);
        assert_eq!(next_selection(2, 3, false), 0);
        assert_eq!(next_selection(1, 3, false), 2);
    }
}
