//! Native action delivery on the owning UI window, separate from visual styling.
use super::WorkspaceView;
use crate::{
    core::PaneId,
    rex::{
        actions::{self, NativeAction},
        automation::{
            ActionContext, ActionError, ActionFuture, ClientActionRequest, completion,
            validate_context,
        },
    },
};
use gpui::{ClipboardItem, Context, Window};
use serde_json::{Value, json};

pub(super) enum Execution {
    Local(Value),
    Worker(ActionFuture),
}

impl WorkspaceView {
    pub(super) fn invoke_local_action(
        &mut self,
        action: NativeAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.presentation.visible {
            return;
        }
        if self.defer_native_action(&action, cx) {
            return;
        }
        match self.execute_native_action(action, &ActionContext::default(), window, cx) {
            Ok(Execution::Local(_)) => {}
            Ok(Execution::Worker(future)) => {
                cx.spawn_in(window, async move |this, cx| {
                    let result = future.await;
                    let _ = this.update_in(cx, |view, _window, cx| {
                        view.poll(cx);
                        if let Err(error) = result {
                            view.report_error(format!("{error:#}"), cx);
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            Err(error) => self.report_error(error.message, cx),
        }
    }

    /// Revealing a hidden terminal must also restore its input focus even when
    /// the action keeps the same active pane (paste, split ratios or zoom).
    pub(super) fn reveal_terminal(&mut self, cx: &mut Context<Self>) {
        if self.overview.take().is_some() {
            self.focus_pending = true;
            cx.notify();
        }
    }

    pub(super) fn schedule_client_actions(&mut self, cx: &mut Context<Self>) {
        if self.action_running || self.client_actions.is_empty() {
            return;
        }
        let Some(handle) = self.window_handle else {
            return;
        };
        self.action_running = true;
        let entity = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| {
                let _ = entity.update(cx, |view, cx| view.start_action_dispatch(window, cx));
            });
        });
    }

    fn start_action_dispatch(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.action_task = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let request = this.update(cx, |view, _| {
                    let request = view.client_actions.pop_front();
                    if request.is_none() {
                        view.action_running = false;
                    }
                    request
                });
                let Ok(Some(request)) = request else {
                    return;
                };
                let execution = this.update_in(cx, |view, window, cx| {
                    view.begin_client_action(&request, window, cx)
                });
                let result = match execution {
                    Ok(Ok(Execution::Local(value))) => Ok(value),
                    Ok(Ok(Execution::Worker(future))) => future
                        .await
                        .map_err(|error| ActionError::refused(format!("{error:#}"))),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(ActionError::internal(error.to_string())),
                };
                let result = this
                    .update_in(cx, |view, _window, cx| {
                        view.poll(cx);
                        if !view.runtime.action_request_current(&request) {
                            return Err(ActionError::refused(
                                "Client reconnected before the action completed",
                            ));
                        }
                        // Worker completion follows publication of its authoritative view.
                        view.poll(cx);
                        cx.notify();
                        result
                    })
                    .unwrap_or_else(|error| Err(ActionError::internal(error.to_string())));
                let quit = request.action == "client.quit" && result.is_ok();
                if let Ok(reply) = request.reply(result)
                    && reply.await.is_ok()
                    && quit
                {
                    let _ = this.update(cx, |_, cx| cx.quit());
                    return;
                }
            }
        }));
    }

    fn begin_client_action(
        &mut self,
        request: &ClientActionRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Execution, ActionError> {
        if !self.presentation.visible {
            return Err(ActionError::refused(
                "Select this server before running a desktop action",
            ));
        }
        if !self.runtime.action_request_current(request) {
            return Err(ActionError::refused(
                "The action belongs to an old client connection",
            ));
        }
        validate_context(self.runtime.state(), &request.context)
            .map_err(|error| ActionError::refused(error.to_string()))?;
        let action = actions::parse(&request.action, &request.args).map_err(|error| {
            if actions::declarations().as_array().is_some_and(|actions| {
                actions
                    .iter()
                    .any(|action| action["name"] == request.action)
            }) {
                ActionError::refused(error.to_string())
            } else {
                ActionError::unsupported(error.to_string())
            }
        })?;
        if action == NativeAction::Quit {
            return Ok(Execution::Local(Value::Null));
        }
        if action == NativeAction::Copy {
            return self.copy_action(&request.context, true, window, cx);
        }
        self.execute_native_action(action, &request.context, window, cx)
    }

    pub(super) fn execute_native_action(
        &mut self,
        action: NativeAction,
        context: &ActionContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Execution, ActionError> {
        if !self.presentation.visible {
            return Err(ActionError::refused(
                "Select this server before running a desktop action",
            ));
        }
        // This entry point is shared by buttons, resolved shortcuts and remote
        // callbacks. Validate sender context before changing presentation too.
        validate_context(self.runtime.state(), context)
            .map_err(|error| ActionError::refused(error.to_string()))?;
        if native_reveals_terminal(&action) && action != NativeAction::Paste {
            self.reveal_terminal(cx);
        }
        let value = match action {
            NativeAction::TogglePalette => {
                self.toggle_palette(window, cx)?;
                Value::Null
            }
            NativeAction::Quit => {
                cx.quit();
                Value::Null
            }
            NativeAction::Find => {
                self.toggle_find(context, window, cx)?;
                Value::Null
            }
            NativeAction::FindNext | NativeAction::FindPrevious => {
                self.navigate_find(context, action == NativeAction::FindPrevious, window, cx)?;
                Value::Null
            }
            NativeAction::ToggleDetached => {
                return self.toggle_detached_action(context, window, cx);
            }
            NativeAction::ArrangePane => {
                if self.rename.is_some() || self.palette.is_some() {
                    return Err(ActionError::refused(
                        "Finish renaming before arranging a pane",
                    ));
                }
                let pane = self.action_pane(context)?;
                self.open_arrangement(pane, window, cx);
                Value::Null
            }
            NativeAction::EnterMode { name, once } => {
                self.shortcuts
                    .resolver_mut()?
                    .enter_mode(&name, once)
                    .map_err(|error| ActionError::refused(error.to_string()))?;
                cx.notify();
                Value::Null
            }
            NativeAction::ExitMode => {
                self.shortcuts.resolver_mut()?.exit_mode();
                cx.notify();
                Value::Null
            }
            NativeAction::ExitAllModes => {
                self.shortcuts.resolver_mut()?.exit_all_modes();
                cx.notify();
                Value::Null
            }
            NativeAction::RenameSession | NativeAction::RenameWindow => {
                self.open_action_rename(&action, context, window, cx)?;
                Value::Null
            }
            NativeAction::FontIncrease | NativeAction::FontDecrease | NativeAction::FontReset => {
                self.font_action(&action, cx)
            }
            NativeAction::ToggleSidebar => {
                self.sidebar_visible = !self.sidebar_visible;
                cx.notify();
                json!({"visible":self.sidebar_visible})
            }
            NativeAction::Copy => {
                return self.copy_action(context, false, window, cx);
            }
            NativeAction::Paste => {
                let pane = self.action_pane(context)?;
                let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
                    return Ok(Execution::Local(Value::Null));
                };
                self.reveal_terminal(cx);
                self.restore_viewport(pane, cx)
                    .map_err(|error| ActionError::refused(error.to_string()))?;
                return self
                    .runtime
                    .paste_action(pane, text)
                    .map(Execution::Worker)
                    .map_err(|error| ActionError::refused(error.to_string()));
            }
            action => {
                return self
                    .runtime
                    .perform_native_action(action, context.clone())
                    .map(Execution::Worker)
                    .map_err(|error| ActionError::refused(error.to_string()));
            }
        };
        let _ = window;
        Ok(Execution::Local(value))
    }

    fn font_action(&mut self, action: &NativeAction, cx: &mut Context<Self>) -> Value {
        let current = self.theme.terminal.font_size;
        self.theme.terminal.font_size = match action {
            NativeAction::FontIncrease => (current + 1.).min(40.),
            NativeAction::FontDecrease => (current - 1.).max(8.),
            _ => super::Theme::default().terminal.font_size,
        };
        let size = self.theme.terminal.font_size;
        for pane in self.panes.values() {
            pane.view
                .update(cx, |view, cx| view.set_font_size(size, cx));
        }
        cx.notify();
        json!({"font_size":size})
    }

    /// Resolve the action only after owner-thread formatting and the clipboard
    /// write, so a following paste observes this copy in every dispatch lane.
    fn copy_action(
        &mut self,
        context: &ActionContext,
        callback: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Execution, ActionError> {
        let pane = self.action_pane(context)?;
        let block = self
            .runtime
            .state()
            .pane(pane)
            .ok_or_else(|| ActionError::refused("The requested block no longer exists"))?;
        let remote = block.remote_id.clone();
        let session = block.session_id.clone();
        let instance = self.runtime.state().instance_id().to_owned();
        let generation = self.runtime.connection_generation();
        let presentation = self.presentation.generation;
        let native = if callback {
            self.runtime.copy_callback_selection(pane)
        } else {
            self.runtime.copy_selection(pane)
        }
        .map_err(|error| ActionError::refused(error.to_string()))?;
        let (sender, future) = completion();
        cx.spawn_in(window, async move |this, cx| {
            if sender.is_cancelled() {
                return;
            }
            let result = native.await;
            if sender.is_cancelled() {
                return;
            }
            let result = this
                .update_in(cx, |view, _, cx| {
                    view.poll(cx);
                    if sender.is_cancelled() {
                        return Ok(Value::Null);
                    }
                    anyhow::ensure!(
                        view.presentation.visible && view.presentation.generation == presentation,
                        "Server selection changed before copy completed"
                    );
                    anyhow::ensure!(
                        view.runtime.connected()
                            && view.runtime.state().instance_id() == instance
                            && view.runtime.connection_generation() == generation,
                        "Client reconnected before copy completed"
                    );
                    anyhow::ensure!(
                        view.runtime
                            .state()
                            .pane(pane)
                            .is_some_and(
                                |block| block.remote_id == remote && block.session_id == session
                            ),
                        "The copied terminal no longer exists"
                    );
                    let value = result?;
                    match value.get("text") {
                        Some(Value::String(text)) => {
                            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                        }
                        Some(Value::Null) => {}
                        _ => anyhow::bail!("Terminal copy returned malformed text"),
                    }
                    Ok(Value::Null)
                })
                .unwrap_or_else(|error| Err(anyhow::anyhow!(error.to_string())));
            sender.send(result);
        })
        .detach();
        Ok(Execution::Worker(future))
    }

    fn toggle_detached_action(
        &mut self,
        context: &ActionContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Execution, ActionError> {
        let session = if let Some(id) = &context.session_id {
            self.runtime
                .state()
                .sessions()
                .iter()
                .find(|session| session.remote_id() == id)
        } else {
            self.runtime.state().active_session()
        }
        .ok_or_else(|| ActionError::refused("No session is selected"))?
        .id();
        if self.detached_browser == Some(session) {
            self.cancel_detached_browser(window, cx);
            Ok(Execution::Local(Value::Null))
        } else {
            self.open_detached_browser(session, window, cx)
                .map(Execution::Worker)
        }
    }

    fn open_action_rename(
        &mut self,
        action: &NativeAction,
        context: &ActionContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), ActionError> {
        let session = if let Some(id) = &context.session_id {
            self.runtime
                .state()
                .sessions()
                .iter()
                .find(|session| session.remote_id() == id)
        } else {
            self.runtime.state().active_session()
        }
        .ok_or_else(|| ActionError::refused("No session is selected"))?;
        let (target, value) = if *action == NativeAction::RenameSession {
            (
                super::RenameTarget::Session(session.id()),
                session.title().to_owned(),
            )
        } else {
            let tab = if let Some(block) = &context.block_id {
                session.windows().iter().find(|tab| {
                    tab.layers.iter().any(|layer| {
                        layer.blocks.iter().any(|pane| {
                            self.runtime
                                .state()
                                .pane(*pane)
                                .is_some_and(|pane| &pane.remote_id == block)
                        })
                    })
                })
            } else {
                session.active_window()
            }
            .ok_or_else(|| ActionError::refused("No tab is selected"))?;
            (
                super::RenameTarget::Window(tab.id()),
                tab.label().to_owned(),
            )
        };
        self.open_rename(target, value, window, cx);
        Ok(())
    }

    fn action_pane(&self, context: &ActionContext) -> Result<PaneId, ActionError> {
        if let Some(remote) = &context.block_id {
            return self
                .runtime
                .state()
                .pane_ids()
                .into_iter()
                .find(|pane| {
                    self.runtime
                        .state()
                        .pane(*pane)
                        .is_some_and(|block| &block.remote_id == remote)
                })
                .ok_or_else(|| ActionError::refused("The requested block no longer exists"));
        }
        if let Some(remote) = &context.session_id {
            return self
                .runtime
                .state()
                .sessions()
                .iter()
                .find(|session| session.remote_id() == remote)
                .and_then(crate::rex::workspace::SessionView::active_pane)
                .ok_or_else(|| ActionError::refused("The requested session has no focused block"));
        }
        self.active_pane()
            .ok_or_else(|| ActionError::refused("No terminal is focused"))
    }
}

/// Presentation-only actions do not dismiss an explicitly requested overview.
fn native_reveals_terminal(action: &NativeAction) -> bool {
    match action {
        NativeAction::SelectSession { .. }
        | NativeAction::NewSession
        | NativeAction::CloseSession
        | NativeAction::NewWindow
        | NativeAction::CloseWindow
        | NativeAction::NextWindow
        | NativeAction::PreviousWindow
        | NativeAction::GoToWindow { .. }
        | NativeAction::Split { .. }
        | NativeAction::ClosePane
        | NativeAction::FocusPane { .. }
        | NativeAction::ZoomPane
        | NativeAction::NextPane
        | NativeAction::MovePane { .. }
        | NativeAction::SwapPanes { .. }
        | NativeAction::MoveWindow { .. }
        | NativeAction::DetachPane
        | NativeAction::RestorePane { .. }
        | NativeAction::Paste => true,
        NativeAction::TogglePalette
        | NativeAction::Quit
        | NativeAction::FontIncrease
        | NativeAction::FontDecrease
        | NativeAction::FontReset
        | NativeAction::ToggleSidebar
        | NativeAction::Copy
        | NativeAction::ReloadConfig
        | NativeAction::EnterMode { .. }
        | NativeAction::ExitMode
        | NativeAction::ExitAllModes
        | NativeAction::RenameSession
        | NativeAction::RenameWindow
        | NativeAction::ArrangePane
        | NativeAction::ToggleDetached
        | NativeAction::Find
        | NativeAction::FindNext
        | NativeAction::FindPrevious => false,
    }
}
