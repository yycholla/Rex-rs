//! Local history search. The overlay never changes the terminal's layout size.
use super::WorkspaceView;
use crate::{
    core::{
        PaneId,
        interaction::{MAX_SEARCH_QUERY_BYTES, SearchCommand},
    },
    rex::automation::{ActionContext, ActionError},
    ui::components::{
        ButtonVariant, Icon, button, icon_button,
        text_field::{TextField, TextFieldEvent},
    },
};
use gpui::{
    Context, Div, Entity, Focusable, KeyDownEvent, Subscription, Window, div, prelude::*, px, rgb,
};

pub(super) struct FindState {
    pane: PaneId,
    server: String,
    field: Entity<TextField>,
    generation: u64,
    connection: u64,
    pending: bool,
    error: Option<String>,
    _subscription: Subscription,
}

impl WorkspaceView {
    pub(super) fn find_focus(&self, cx: &gpui::App) -> Option<gpui::FocusHandle> {
        self.find.as_ref().map(|find| find.field.focus_handle(cx))
    }

    pub(super) fn find_is_composing(&self, cx: &gpui::App) -> bool {
        self.find
            .as_ref()
            .is_some_and(|find| find.field.read(cx).is_composing())
    }

    fn find_target(&self, context: &ActionContext) -> Result<PaneId, ActionError> {
        crate::rex::automation::validate_context(self.runtime.state(), context)
            .map_err(|error| ActionError::refused(error.to_string()))?;
        let pane = self
            .active_pane()
            .ok_or_else(|| ActionError::refused("Focus a terminal before searching"))?;
        let block = self
            .runtime
            .state()
            .pane(pane)
            .ok_or_else(|| ActionError::refused("The focused terminal no longer exists"))?;
        if !block.is_terminal()
            || self.runtime.state().is_detached(pane)
            || context
                .block_id
                .as_ref()
                .is_some_and(|remote| remote != &block.remote_id)
            || context
                .session_id
                .as_ref()
                .is_some_and(|remote| remote != &block.session_id)
        {
            return Err(ActionError::refused(
                "Focus the requested terminal before searching",
            ));
        }
        Ok(pane)
    }

    fn next_find_generation(&mut self) -> Result<u64, ActionError> {
        self.search_counter = self
            .search_counter
            .checked_add(1)
            .ok_or_else(|| ActionError::refused("Terminal search generation exhausted"))?;
        Ok(self.search_counter)
    }

    pub(super) fn toggle_find(
        &mut self,
        context: &ActionContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), ActionError> {
        let pane = self.find_target(context)?;
        if self.has_modal() {
            return Err(ActionError::refused(
                "Finish the current dialog before searching",
            ));
        }
        if self.find.as_ref().is_some_and(|find| find.pane == pane) {
            self.close_find(window, cx);
            return Ok(());
        }
        self.discard_find(cx);
        let generation = self.next_find_generation()?;
        let field = cx.new(|cx| {
            TextField::new(String::new(), "Find in terminal history".into(), cx).compact()
        });
        let subscription = cx.subscribe_in(&field, window, |view, _, event, window, cx| {
            let result = match event {
                TextFieldEvent::Changed => view.change_find(cx),
                TextFieldEvent::Submitted => {
                    view.navigate_find(&ActionContext::default(), false, window, cx)
                }
                TextFieldEvent::Cancelled => {
                    view.close_find(window, cx);
                    Ok(())
                }
            };
            if let Err(error) = result {
                view.report_error(error.message, cx);
            }
        });
        let focus = field.focus_handle(cx);
        self.find = Some(FindState {
            pane,
            server: self.runtime.state().instance_id().to_owned(),
            field,
            generation,
            connection: self.runtime.connection_generation(),
            pending: true,
            error: None,
            _subscription: subscription,
        });
        if let Ok(resolver) = self.shortcuts.resolver_mut() {
            resolver.cancel_sequence();
        }
        self.reveal_terminal(cx);
        self.apply_find_query(cx);
        focus.focus(window);
        cx.notify();
        Ok(())
    }

    fn change_find(&mut self, cx: &mut Context<Self>) -> Result<(), ActionError> {
        let generation = self.next_find_generation()?;
        if let Some(find) = &mut self.find {
            find.generation = generation;
            find.pending = true;
        }
        self.apply_find_query(cx);
        Ok(())
    }

    fn apply_find_query(&mut self, cx: &mut Context<Self>) {
        let Some(find) = &self.find else { return };
        if !find.pending {
            return;
        }
        let pane = find.pane;
        let generation = find.generation;
        let query = find.field.read(cx).value().to_owned();
        let ready = self.runtime.connected()
            && self
                .runtime
                .annotations(pane)
                .is_some_and(|annotations| annotations.viewport_token != 0);
        let (pending, error) = if query.len() > MAX_SEARCH_QUERY_BYTES {
            // Clear the old query rather than highlighting results that no
            // longer correspond to the visible (too long) input.
            let _ = self
                .runtime
                .search(pane, SearchCommand::Cancel { generation });
            (
                false,
                Some(format!(
                    "Search is limited to {MAX_SEARCH_QUERY_BYTES} UTF-8 bytes"
                )),
            )
        } else if !ready {
            (
                true,
                Some("Search will resume when terminal history is available".into()),
            )
        } else {
            match self
                .runtime
                .search(pane, SearchCommand::Set { query, generation })
            {
                Ok(()) => (false, None),
                Err(error) => (true, Some(error.to_string())),
            }
        };
        let connection = self.runtime.connection_generation();
        if let Some(find) = &mut self.find {
            let changed =
                find.pending != pending || find.error != error || find.connection != connection;
            find.pending = pending;
            find.error = error;
            find.connection = connection;
            if changed {
                cx.notify();
            }
        }
    }

    pub(super) fn navigate_find(
        &mut self,
        context: &ActionContext,
        previous: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), ActionError> {
        let pane = self.find_target(context)?;
        self.apply_find_query(cx);
        let find = self
            .find
            .as_ref()
            .filter(|find| find.pane == pane)
            .ok_or_else(|| ActionError::refused("Open terminal search and enter a query first"))?;
        if find.pending || find.error.is_some() {
            return Err(ActionError::refused(
                find.error.as_deref().unwrap_or("Search is unavailable"),
            ));
        }
        if find.field.read(cx).value().is_empty() {
            return Ok(());
        }
        let generation = find.generation;
        let command = if previous {
            SearchCommand::Previous { generation }
        } else {
            SearchCommand::Next { generation }
        };
        self.runtime
            .search(pane, command)
            .map_err(|error| ActionError::refused(error.to_string()))?;
        cx.notify();
        Ok(())
    }

    fn discard_find(&mut self, cx: &mut Context<Self>) {
        if let Some(find) = self.find.take() {
            if let Ok(generation) = self.next_find_generation() {
                let _ = self
                    .runtime
                    .search(find.pane, SearchCommand::Cancel { generation });
            }
            cx.notify();
        }
    }

    pub(super) fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.discard_find(cx);
        self.focus_active(window, cx);
    }

    pub(super) fn reconcile_find(&mut self, cx: &mut Context<Self>) {
        let Some(find) = &self.find else { return };
        if find.server != self.runtime.state().instance_id() {
            // Pane aliases can be reused by another server instance. Never
            // deliver the old instance's cancellation to its replacement.
            self.find = None;
            cx.notify();
            return;
        }
        if self.overview.is_some()
            || self.active_pane() != Some(find.pane)
            || self.runtime.state().pane(find.pane).is_none()
            || self.runtime.state().is_detached(find.pane)
        {
            self.discard_find(cx);
            return;
        }
        if find.connection != self.runtime.connection_generation()
            && let Some(find) = &mut self.find
        {
            find.pending = true;
        }
        self.apply_find_query(cx);
    }

    /// True bypasses workspace shortcuts; unhandled text still reaches `TextField`.
    pub(super) fn find_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(find) = &self.find else { return false };
        if !find.field.focus_handle(cx).is_focused(window) {
            return false;
        }
        if find.field.read(cx).is_composing() {
            return true;
        }
        let key = &event.keystroke;
        if key.key == "escape" {
            self.close_find(window, cx);
            cx.stop_propagation();
        } else if key.key == "enter" && key.modifiers.shift {
            if let Err(error) = self.navigate_find(&ActionContext::default(), true, window, cx) {
                self.report_error(error.message, cx);
            }
            cx.stop_propagation();
        }
        true
    }

    pub(super) fn find_bar(&self, cx: &Context<Self>) -> Div {
        let Some(find) = &self.find else { return div() };
        let theme = self.theme;
        let status = self.find_status(find, cx);
        div()
            .absolute()
            .top(px(8.))
            .right(px(12.))
            .w(px(450.))
            .max_w_full()
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .bg(rgb(theme.colors.raised))
            .border_1()
            .border_color(rgb(theme.colors.border_strong))
            .rounded_md()
            .shadow_md()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(div().flex_1().min_w_0().child(find.field.clone()))
                    .child(
                        button("find-older", "Older", ButtonVariant::Quiet, theme).on_click(
                            cx.listener(|view, _, window, cx| {
                                if let Err(error) =
                                    view.navigate_find(&ActionContext::default(), false, window, cx)
                                {
                                    view.report_error(error.message, cx);
                                }
                            }),
                        ),
                    )
                    .child(
                        button("find-newer", "Newer", ButtonVariant::Quiet, theme).on_click(
                            cx.listener(|view, _, window, cx| {
                                if let Err(error) =
                                    view.navigate_find(&ActionContext::default(), true, window, cx)
                                {
                                    view.report_error(error.message, cx);
                                }
                            }),
                        ),
                    )
                    .child(
                        icon_button("find-close", Icon::Close, theme).on_click(
                            cx.listener(|view, _, window, cx| view.close_find(window, cx)),
                        ),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(theme.colors.muted))
                    .child(status),
            )
    }

    fn find_status(&self, find: &FindState, cx: &Context<Self>) -> String {
        if let Some(error) = &find.error {
            return error.clone();
        }
        let query = find.field.read(cx).value();
        if query.is_empty() {
            return "ASCII case-insensitive · Enter: older · Shift+Enter: newer".into();
        }
        let state = self
            .runtime
            .annotations(find.pane)
            .and_then(|annotations| annotations.search.as_ref())
            .filter(|search| search.generation == find.generation);
        let Some(state) = state else {
            return "Searching…".into();
        };
        let progress = if state.complete {
            ""
        } else {
            " · searching…"
        };
        state.selected.map_or_else(
            || format!("{} matches{progress}", state.total),
            |selected| {
                format!(
                    "{} of {}{progress}",
                    selected.saturating_add(1),
                    state.total
                )
            },
        )
    }
}
