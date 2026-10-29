//! Resolve Rex bindings before GPUI text delivery, preserving unbound IME commits.
use super::{WorkspaceView, automation::Execution};
use crate::{
    core::{Command, input::KeyInput},
    rex::{
        actions,
        automation::ActionContext,
        keymap::{Keymap, Resolution, Resolver},
    },
};
use gpui::{Context, EntityInputHandler as _, KeyDownEvent, Window};
use serde_json::Value;
use std::{collections::VecDeque, sync::Arc};

const MAX_QUEUED_KEYS: usize = 256;
const MAX_QUEUED_BYTES: usize = 1024 * 1024;

pub(super) enum Work {
    Key(KeyInput, Option<String>),
    Arrange(crate::core::PaneId),
    Browse(crate::core::SessionId),
    Overview(crate::core::SessionId),
    Terminal(crate::core::PaneId, super::TerminalViewEvent),
    Command(crate::core::Command),
    Native(actions::NativeAction),
    Text(String),
    Action {
        name: String,
        args: Value,
        prefix: Vec<String>,
        key: Option<KeyInput>,
    },
}

impl Work {
    fn bytes(&self) -> usize {
        match self {
            Self::Command(_)
            | Self::Native(_)
            | Self::Arrange(_)
            | Self::Browse(_)
            | Self::Overview(_) => 512,
            Self::Terminal(_, event) => match event {
                super::TerminalViewEvent::Key(key) => {
                    key.key.len() + key.text.as_ref().map_or(0, String::len)
                }
                super::TerminalViewEvent::Text(text) => text.len(),
                _ => 64,
            },
            Self::Key(key, physical) => {
                key.key.len()
                    + key.text.as_ref().map_or(0, String::len)
                    + physical.as_ref().map_or(0, String::len)
            }
            Self::Text(text) => text.len(),
            Self::Action {
                name,
                args,
                prefix,
                key,
            } => {
                name.len()
                    + args.to_string().len()
                    + prefix.iter().map(String::len).sum::<usize>()
                    + key.as_ref().map_or(0, |key| {
                        key.key.len() + key.text.as_ref().map_or(0, String::len)
                    })
            }
        }
    }
}

#[derive(Default)]
pub(super) struct Shortcuts {
    resolver: Option<Resolver>,
    pub(super) diagnostics: Vec<String>,
    source: Arc<Value>,
    generation: u64,
    queue: VecDeque<Work>,
    bytes: usize,
    running: bool,
    executing: bool,
    task: Option<gpui::Task<()>>,
}

impl WorkspaceView {
    pub(super) fn load_initial_scripts(&mut self, cx: &mut Context<Self>) {
        if !self.runtime.connected() {
            return;
        }
        let Some(scripts) = self.initial_scripts.take() else {
            return;
        };
        match self.runtime.set_scripts(scripts) {
            Ok(future) => cx
                .spawn(async move |this, cx| {
                    let result = future.await;
                    let _ = this.update(cx, |view, cx| {
                        view.poll(cx);
                        if let Err(error) = result {
                            view.report_error(format!("Load client scripts: {error:#}"), cx);
                        }
                    });
                })
                .detach(),
            Err(error) => self.report_error(format!("Load client scripts: {error:#}"), cx),
        }
    }

    pub(super) fn refresh_shortcuts(&mut self, cx: &mut Context<Self>) {
        let generation = self.runtime.connection_generation();
        let source = self.runtime.keymap_snapshot();
        if generation != self.shortcuts.generation || !self.runtime.connected() {
            self.shortcuts.resolver = None;
            self.shortcuts.running = false;
            self.shortcuts.task = None;
            self.shortcuts.queue.clear();
            self.shortcuts.bytes = 0;
            self.shortcuts.generation = generation;
        }
        if !self.runtime.connected() || source.is_null() {
            return;
        }
        if self.shortcuts.resolver.is_some() && Arc::ptr_eq(&source, &self.shortcuts.source) {
            return;
        }
        match Keymap::parse(&source) {
            Ok(mut keymap) => {
                // Native declarations aren't repeated in the server response.
                for action in actions::declarations().as_array().into_iter().flatten() {
                    if let Some(name) = action["name"].as_str() {
                        keymap.set_repeats(name, action["repeats"].as_bool().unwrap_or(false));
                    }
                }
                if let Some(resolver) = &mut self.shortcuts.resolver {
                    resolver.replace(keymap);
                } else {
                    self.shortcuts.resolver = Some(Resolver::new(keymap));
                }
                self.shortcuts.diagnostics = source["diagnostics"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|diagnostic| {
                        let message = diagnostic["message"].as_str()?;
                        let source = diagnostic["source"].as_str().unwrap_or("keymap");
                        Some(format!("{source}: {message}"))
                    })
                    .collect();
                self.shortcuts.source = source;
                cx.notify();
            }
            Err(error) => self.report_error(format!("Cannot apply Rex keymap: {error:#}"), cx),
        }
    }

    pub(super) fn shortcut_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.presentation.visible {
            return;
        }
        if self.offline_shortcut(event, window, cx) {
            return;
        }
        if self.palette.is_some() {
            if !self.palette_is_composing(cx)
                && self.overlay_action_key(
                    event,
                    &["client.palette.toggle", "client.quit"],
                    window,
                    cx,
                )
            {
                return;
            }
            if self.palette_key(event, window, cx) {
                return;
            }
        }
        if self.find.is_some() {
            if !self.find_is_composing(cx)
                && self.overlay_action_key(
                    event,
                    &["client.find.toggle", "client.palette.toggle", "client.quit"],
                    window,
                    cx,
                )
            {
                return;
            }
            if self.find_key(event, window, cx) {
                return;
            }
        }
        if self.detached_key(event, window, cx) {
            return;
        }
        if self.arrangement_key(event, window, cx) {
            return;
        }
        if self.rename.is_some() || !self.runtime.connected() {
            return;
        }
        // Composition belongs to GPUI's input handler, including dead keys.
        if self
            .active_pane()
            .and_then(|id| self.panes.get(&id))
            .is_some_and(|pane| pane.view.read(cx).is_composing())
        {
            return;
        }
        let (key, physical) = crate::ui::platform_input::shortcut_input(event);
        if self.shortcuts.running {
            self.queue_shortcut(Work::Key(key, physical), window, cx);
            cx.stop_propagation();
            return;
        }
        match self.resolve_shortcut(&key, physical.as_deref()) {
            Resolution::Passthrough => {}
            Resolution::Pending | Resolution::Consumed => {
                cx.stop_propagation();
                cx.notify();
            }
            Resolution::Action(action) => {
                self.queue_shortcut(
                    Work::Action {
                        name: action.name,
                        args: action.args,
                        prefix: action.prefix,
                        key: Some(key),
                    },
                    window,
                    cx,
                );
                cx.stop_propagation();
            }
        }
    }

    fn offline_shortcut(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.runtime.connected() && self.shortcuts.resolver.is_some() {
            return false;
        }
        let key = &event.keystroke;
        if !key.modifiers.control
            || !key.modifiers.shift
            || key.modifiers.alt
            || key.modifiers.platform
        {
            return false;
        }
        match key.key.as_str() {
            "q" => cx.quit(),
            "p" => {
                if let Err(error) = self.toggle_palette(window, cx) {
                    self.report_error(error.message, cx);
                }
            }
            _ => return false,
        }
        window.prevent_default();
        cx.stop_propagation();
        true
    }

    pub(super) fn submit_palette_action(
        &mut self,
        name: String,
        args: Value,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if !self.runtime.connected() {
            if name == "client.quit" {
                cx.quit();
            } else {
                self.report_error("Reconnect before running this command".into(), cx);
            }
            return;
        }
        let work = Work::Action {
            name,
            args,
            prefix: Vec::new(),
            key: None,
        };
        // Enter is a consequence of an already consumed queued key. Its selected
        // command precedes any later buffered typing, as with rename submission.
        if self.shortcuts.running {
            self.push_shortcut(work, true, cx);
        } else {
            self.queue_shortcut(work, window, cx);
        }
    }

    fn resolve_shortcut(&mut self, key: &KeyInput, physical: Option<&str>) -> Resolution {
        let Some(mut resolver) = self.shortcuts.resolver.take() else {
            return Resolution::Passthrough;
        };
        let candidate = spelling(key);
        let candidates = shortcut_candidates(&candidate, physical);
        let result = resolver.resolve(&candidates, key.repeat, |name, args| {
            self.shortcut_available(name, args)
        });
        self.shortcuts.resolver = Some(resolver);
        result
    }

    fn overlay_action_key(
        &mut self,
        event: &KeyDownEvent,
        allowed: &[&str],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (key, physical) = crate::ui::platform_input::shortcut_input(event);
        let candidate = spelling(&key);
        let Some(mut preview) = self.shortcuts.resolver.clone() else {
            return false;
        };
        let candidates = shortcut_candidates(&candidate, physical.as_deref());
        let resolved = preview.resolve(&candidates, key.repeat, |name, args| {
            self.shortcut_available(name, args)
        });
        if !matches!(resolved, Resolution::Action(ref action) if allowed.contains(&action.name.as_str()))
        {
            return false;
        }
        if let Resolution::Action(action) = self.resolve_shortcut(&key, physical.as_deref()) {
            self.queue_shortcut(
                Work::Action {
                    name: action.name,
                    args: action.args,
                    prefix: action.prefix,
                    key: Some(key),
                },
                window,
                cx,
            );
            cx.stop_propagation();
            return true;
        }
        false
    }

    pub(super) fn shortcut_available(&self, name: &str, args: &Value) -> bool {
        if let Ok(action) = actions::parse(name, args) {
            return native_available(
                &action,
                self.runtime.state(),
                &self.shortcuts.source["modes"],
            );
        }
        self.runtime.keymap()["actions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|action| {
                action["name"] == name
                    && (action["kind"] == "lua"
                        || (action["kind"] == "block"
                            && self
                                .active_pane()
                                .and_then(|pane| self.runtime.state().pane(pane))
                                .is_some_and(|pane| {
                                    name.strip_prefix(&pane.creator_name)
                                        .is_some_and(|suffix| suffix.starts_with('.'))
                                })))
            })
    }

    pub(super) fn defer_terminal_input(
        &mut self,
        pane: crate::core::PaneId,
        event: &super::TerminalViewEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.shortcuts.running || self.shortcuts.executing {
            return false;
        }
        // Programmatic focus after revealing a terminal can acknowledge the
        // pane already selected in this snapshot. Do not replay that redundant
        // acknowledgement after a newer queued navigation has selected another.
        if matches!(event, super::TerminalViewEvent::Focused) && self.active_pane() == Some(pane) {
            return false;
        }
        let work = match event {
            super::TerminalViewEvent::Key(key) => Work::Key(key.clone(), None),
            super::TerminalViewEvent::Text(text) => Work::Text(text.clone()),
            super::TerminalViewEvent::Resized { .. }
            | super::TerminalViewEvent::FocusChanged(_) => return false,
            _ => Work::Terminal(pane, event.clone()),
        };
        self.push_shortcut(work, false, cx);
        true
    }

    pub(super) fn submit_command(
        &mut self,
        command: Command,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.queue_shortcut(Work::Command(command), window, cx);
    }

    /// Keep overview selection in the same lane as navigation and typed input.
    /// Setting it eagerly would let an earlier queued action dismiss the intent.
    pub(super) fn request_overview(
        &mut self,
        session: crate::core::SessionId,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.queue_shortcut(Work::Overview(session), window, cx);
    }

    pub(super) fn defer_browser(
        &mut self,
        session: crate::core::SessionId,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.shortcuts.running || self.shortcuts.executing {
            return false;
        }
        self.push_shortcut(Work::Browse(session), false, cx);
        true
    }

    pub(super) fn defer_arrangement(
        &mut self,
        pane: crate::core::PaneId,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.shortcuts.running || self.shortcuts.executing {
            return false;
        }
        self.push_shortcut(Work::Arrange(pane), false, cx);
        true
    }

    pub(super) fn defer_rename_command(
        &mut self,
        command: &crate::core::Command,
        cx: &mut Context<Self>,
    ) -> Option<bool> {
        if !self.shortcuts.running || self.shortcuts.executing {
            return None;
        }
        // Submission is a consequence of the buffered Enter already consumed;
        // perform it before subsequent keyboard input.
        Some(self.push_shortcut(Work::Command(command.clone()), true, cx))
    }

    pub(super) fn defer_command(
        &mut self,
        command: &crate::core::Command,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.shortcuts.running || self.shortcuts.executing {
            return false;
        }
        self.push_shortcut(Work::Command(command.clone()), false, cx);
        true
    }

    pub(super) fn defer_native_action(
        &mut self,
        action: &actions::NativeAction,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.shortcuts.running || self.shortcuts.executing {
            return false;
        }
        self.push_shortcut(Work::Native(action.clone()), false, cx);
        true
    }

    pub(super) fn clear_queued_shortcuts(&mut self) {
        self.shortcuts.queue.clear();
        self.shortcuts.bytes = 0;
    }

    fn push_shortcut(&mut self, work: Work, first: bool, cx: &mut Context<Self>) -> bool {
        if !self.presentation.visible {
            return false;
        }
        let bytes = work.bytes();
        if self.shortcuts.queue.len() >= MAX_QUEUED_KEYS
            || self.shortcuts.bytes.saturating_add(bytes) > MAX_QUEUED_BYTES
        {
            self.report_error(
                "Shortcut input queue is full; input was not sent".into(),
                cx,
            );
            return false;
        }
        self.shortcuts.bytes += bytes;
        if first {
            self.shortcuts.queue.push_front(work);
        } else {
            self.shortcuts.queue.push_back(work);
        }
        true
    }

    fn queue_shortcut(&mut self, work: Work, window: &Window, cx: &mut Context<Self>) {
        self.push_shortcut(work, false, cx);
        if self.shortcuts.running {
            return;
        }
        self.shortcuts.running = true;
        let generation = self.runtime.connection_generation();
        self.shortcuts.task = Some(cx.spawn_in(window, async move |this, cx| {
            let mut waiting_since = None;
            loop {
                let waiting = this
                    .update(cx, |view, cx| {
                        view.poll(cx);
                        view.shortcut_input_waiting()
                    })
                    .unwrap_or(false);
                if waiting {
                    let since = waiting_since.get_or_insert_with(std::time::Instant::now);
                    if since.elapsed() >= std::time::Duration::from_secs(8) {
                        let _ = this.update(cx, |view, cx| {
                            view.shortcuts.running = false;
                            view.shortcuts.queue.clear();
                            view.shortcuts.bytes = 0;
                            view.report_error(
                                "Terminal input did not become ready; queued input was not sent"
                                    .into(),
                                cx,
                            );
                        });
                        return;
                    }
                    gpui::Timer::after(std::time::Duration::from_millis(5)).await;
                    continue;
                }
                waiting_since = None;
                // Remote callbacks have their own reply lane. Let an earlier
                // callback finish before a local action can wait on server Lua.
                let busy = this
                    .update(cx, |view, _| view.action_running)
                    .unwrap_or(false);
                if busy {
                    gpui::Timer::after(std::time::Duration::from_millis(5)).await;
                    continue;
                }
                let next = this.update_in(cx, |view, window, cx| {
                    view.poll(cx);
                    if !view.presentation.visible
                        || !view.runtime.connected()
                        || view.runtime.connection_generation() != generation
                    {
                        view.shortcuts.running = false;
                        view.shortcuts.queue.clear();
                        view.shortcuts.bytes = 0;
                        return None;
                    }
                    let Some(work) = view.shortcuts.queue.pop_front() else {
                        view.shortcuts.running = false;
                        return None;
                    };
                    view.shortcuts.bytes = view.shortcuts.bytes.saturating_sub(work.bytes());
                    view.shortcuts.executing = true;
                    let result = view.perform_shortcut(work, window, cx);
                    view.shortcuts.executing = false;
                    Some((result, view.presentation.generation))
                });
                let Ok(Some((result, presentation))) = next else {
                    return;
                };
                match result {
                    Ok(Some((future, returned))) => {
                        let result = future.await;
                        let _ = this.update_in(cx, |view, _window, cx| {
                            view.poll(cx);
                            if !view.presentation.visible
                                || view.presentation.generation != presentation
                                || generation != view.runtime.connection_generation()
                                || !view.runtime.connected()
                            {
                                return;
                            }
                            view.complete_shortcut(result, returned, cx);
                        });
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = this.update(cx, |view, cx| view.report_error(error, cx));
                    }
                }
            }
        }));
    }

    fn complete_shortcut(
        &mut self,
        result: anyhow::Result<Value>,
        returned: bool,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(value) if returned => {
                if let Some(actions) = value["client_actions"].as_array() {
                    for action in actions.iter().rev() {
                        if let Some(name) = action["action"].as_str()
                            && actions::parse(name, &action["args"]).is_ok()
                        {
                            // Unknown queued actions are ignored by protocol.
                            self.push_shortcut(
                                Work::Action {
                                    name: name.into(),
                                    args: action["args"].clone(),
                                    prefix: Vec::new(),
                                    key: None,
                                },
                                true,
                                cx,
                            );
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(error) => self.report_error(format!("{error:#}"), cx),
        }
    }

    fn replay_modal_input(
        &mut self,
        work: &Work,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.replay_palette_input(work, window, cx) {
            return true;
        }
        if self.detached_browser.is_some() {
            match work {
                Work::Key(key, _) => {
                    if key.key == "escape" {
                        self.cancel_detached_browser(window, cx);
                    }
                    return true;
                }
                Work::Text(_) => return true,
                _ => {}
            }
        }
        if self.arranging.is_some() {
            match &work {
                Work::Key(key, _) => {
                    if key.key == "escape" {
                        self.cancel_arrangement(window, cx);
                    }
                    return true;
                }
                Work::Text(_) => return true,
                _ => {}
            }
        }
        let Some(dialog) = &self.rename else {
            return false;
        };
        match work {
            Work::Key(key, _) => dialog
                .input
                .update(cx, |input, cx| input.replay_key(key, window, cx)),
            Work::Text(text) => dialog.input.update(cx, |input, cx| {
                input.replace_text_in_range(None, text, window, cx);
            }),
            _ => return false,
        }
        true
    }

    fn shortcut_input_waiting(&self) -> bool {
        self.presentation.visible
            && self.rename.is_none()
            && self.palette.is_none()
            && self.arranging.is_none()
            && self.detached_browser.is_none()
            && matches!(
                self.shortcuts.queue.front(),
                Some(Work::Key(_, _) | Work::Text(_))
            )
            && self.active_pane().is_some_and(|pane| {
                !self.runtime.input_available(pane)
                    && !matches!(
                        self.runtime.status(pane),
                        Some(
                            crate::terminal::PaneStatus::Failed { .. }
                                | crate::terminal::PaneStatus::Exited { .. }
                        )
                    )
            })
    }

    fn start_overview(
        &mut self,
        session: crate::core::SessionId,
        cx: &mut Context<Self>,
    ) -> Result<crate::rex::automation::ActionFuture, String> {
        let command = Command::SelectSession(session);
        self.runtime
            .state()
            .command(&command)
            .map_err(|error| error.to_string())?;
        // Only change presentation after the selection was admitted. The caller
        // awaits completion before following buffered input resolves.
        let future = self
            .runtime
            .perform_command(command)
            .map_err(|error| error.to_string())?;
        self.overview = Some(session);
        self.focus_pending = true;
        cx.notify();
        Ok(future)
    }

    fn start_command(
        &mut self,
        command: Command,
        cx: &mut Context<Self>,
    ) -> Result<crate::rex::automation::ActionFuture, String> {
        self.runtime
            .state()
            .command(&command)
            .map_err(|error| error.to_string())?;
        if command_reveals_terminal(&command, self.runtime.state()) {
            self.reveal_terminal(cx);
        }
        self.runtime
            .perform_command(command)
            .map_err(|error| error.to_string())
    }

    fn perform_shortcut(
        &mut self,
        work: Work,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Option<(crate::rex::automation::ActionFuture, bool)>, String> {
        if self.replay_modal_input(&work, window, cx) {
            return Ok(None);
        }
        let work = match work {
            Work::Overview(session) => {
                return self
                    .start_overview(session, cx)
                    .map(|future| Some((future, false)));
            }
            Work::Browse(session) => {
                return self
                    .open_detached_browser(session, window, cx)
                    .map(|future| Some((future, false)))
                    .map_err(|error| error.message);
            }
            Work::Arrange(pane) => {
                self.open_arrangement(pane, window, cx);
                return Ok(None);
            }
            Work::Command(command) => {
                return self
                    .start_command(command, cx)
                    .map(|future| Some((future, false)));
            }
            Work::Native(action) => return self.shortcut_native(action, window, cx),
            Work::Terminal(pane, super::TerminalViewEvent::Focused) => {
                if self.active_pane() == Some(pane) {
                    return Ok(None);
                }
                return self
                    .runtime
                    .perform_command(crate::core::Command::FocusPane(pane))
                    .map(|future| Some((future, false)))
                    .map_err(|error| error.to_string());
            }
            Work::Terminal(pane, event) => {
                self.terminal_event(pane, &event, cx);
                return Ok(None);
            }
            Work::Key(key, physical) => match self.resolve_shortcut(&key, physical.as_deref()) {
                Resolution::Action(action) => Work::Action {
                    name: action.name,
                    args: action.args,
                    prefix: action.prefix,
                    key: Some(key),
                },
                Resolution::Pending | Resolution::Consumed => return Ok(None),
                Resolution::Passthrough => {
                    // Unbound typing in an overview has no terminal recipient,
                    // whether it arrived live or waited behind a pure UI action.
                    if self.overview_session().is_some() {
                        return Ok(None);
                    }
                    if let Some(pane) = self.active_pane() {
                        self.restore_viewport(pane, cx)
                            .map_err(|error| error.to_string())?;
                        self.runtime
                            .send_key(pane, &key)
                            .map_err(|error| error.to_string())?;
                    }
                    return Ok(None);
                }
            },
            Work::Text(text) => {
                if self.overview_session().is_some() {
                    return Ok(None);
                }
                if let Some(pane) = self.active_pane() {
                    self.restore_viewport(pane, cx)
                        .map_err(|error| error.to_string())?;
                    self.runtime
                        .write_input(pane, text.as_bytes())
                        .map_err(|error| error.to_string())?;
                }
                return Ok(None);
            }
            action @ Work::Action { .. } => action,
        };
        self.perform_bound_shortcut(work, window, cx)
    }

    fn perform_bound_shortcut(
        &mut self,
        work: Work,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Option<(crate::rex::automation::ActionFuture, bool)>, String> {
        let Work::Action {
            name,
            args,
            prefix,
            key,
        } = work
        else {
            return Ok(None);
        };
        if name == "client.key.forward" {
            if let (Some(pane), Some(key)) = (self.active_pane(), key) {
                self.reveal_terminal(cx);
                self.restore_viewport(pane, cx)
                    .map_err(|error| error.to_string())?;
                self.runtime
                    .send_key_sequence(pane, &key, prefix)
                    .map_err(|error| error.to_string())?;
            }
            return Ok(None);
        }
        if let Ok(action) = actions::parse(&name, &args) {
            return self.shortcut_native(action, window, cx);
        }
        let context = self.current_action_context();
        self.runtime
            .perform_server_action(name, args, context)
            .map(|future| Some((future, true)))
            .map_err(|error| error.to_string())
    }

    fn shortcut_native(
        &mut self,
        action: actions::NativeAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Option<(crate::rex::automation::ActionFuture, bool)>, String> {
        self.execute_native_action(action, &ActionContext::default(), window, cx)
            .map(|execution| match execution {
                Execution::Local(_) => None,
                Execution::Worker(future) => Some((future, false)),
            })
            .map_err(|error| error.message)
    }

    fn current_action_context(&self) -> ActionContext {
        ActionContext {
            session_id: self
                .runtime
                .state()
                .active_session()
                .map(|session| session.remote_id().into()),
            block_id: self
                .active_pane()
                .and_then(|pane| self.runtime.state().pane(pane))
                .map(|pane| pane.remote_id.clone()),
        }
    }
}

fn command_reveals_terminal(
    command: &Command,
    state: &crate::rex::workspace::WorkspaceState,
) -> bool {
    match command {
        Command::RenameSession { .. } | Command::RenameWindow { .. } => false,
        // Closing a detached resource from its browser does not expose a hidden
        // terminal. Restoring or placing that resource does.
        Command::ClosePane(pane) => !state.is_detached(*pane),
        _ => true,
    }
}

fn spelling(key: &KeyInput) -> String {
    let mut result = String::new();
    for (active, name) in [
        (key.modifiers.control, "ctrl+"),
        (key.modifiers.alt, "alt+"),
        (key.modifiers.shift, "shift+"),
        (key.modifiers.super_key, "cmd+"),
    ] {
        if active {
            result.push_str(name);
        }
    }
    result.push_str(&key.key.to_lowercase());
    result
}

impl Shortcuts {
    pub(super) fn active_mode(&self) -> Option<&str> {
        self.resolver.as_ref().and_then(Resolver::active_mode)
    }

    pub(super) fn resolver_mut(
        &mut self,
    ) -> Result<&mut Resolver, crate::rex::automation::ActionError> {
        self.resolver
            .as_mut()
            .ok_or_else(|| crate::rex::automation::ActionError::refused("The keymap is not ready"))
    }
}

fn shortcut_candidates<'a>(logical: &'a str, physical: Option<&'a str>) -> Vec<&'a str> {
    std::iter::once(logical).chain(physical).collect()
}

fn native_available(
    action: &actions::NativeAction,
    state: &crate::rex::workspace::WorkspaceState,
    modes: &Value,
) -> bool {
    use actions::NativeAction;
    let session = state.active_session();
    match action {
        NativeAction::SelectSession {
            session_id,
            window_id,
        } => state.sessions().iter().any(|session| {
            session.remote_id() == session_id
                && window_id.as_ref().is_none_or(|id| {
                    session
                        .windows()
                        .iter()
                        .any(|window| window.remote_id() == id)
                })
        }),
        NativeAction::NewSession
        | NativeAction::TogglePalette
        | NativeAction::Quit
        | NativeAction::FontIncrease
        | NativeAction::FontDecrease
        | NativeAction::FontReset
        | NativeAction::ToggleSidebar
        | NativeAction::ReloadConfig
        | NativeAction::ExitMode
        | NativeAction::ExitAllModes => true,
        NativeAction::EnterMode { name, .. } => modes
            .as_array()
            .into_iter()
            .flatten()
            .any(|mode| mode["name"] == *name),
        NativeAction::CloseSession
        | NativeAction::NewWindow
        | NativeAction::RenameSession
        | NativeAction::ToggleDetached => session.is_some(),
        NativeAction::NextWindow | NativeAction::PreviousWindow => {
            session.is_some_and(|session| !session.windows().is_empty())
        }
        NativeAction::GoToWindow { index } => {
            *index != 0
                && session.is_some_and(|session| {
                    i64::from(*index).unsigned_abs() <= session.windows().len() as u64
                })
        }
        NativeAction::CloseWindow | NativeAction::RenameWindow => session
            .and_then(crate::rex::workspace::SessionView::active_window)
            .is_some(),
        NativeAction::MovePane { .. }
        | NativeAction::SwapPanes { .. }
        | NativeAction::MoveWindow { .. } => management_available(action, state),
        NativeAction::RestorePane { block_id } => state.pane_ids().into_iter().any(|pane| {
            state.is_detached(pane)
                && state.pane(pane).is_some_and(|block| {
                    &block.remote_id == block_id
                        && session.is_some_and(|session| block.session_id == session.remote_id())
                })
        }),
        NativeAction::DetachPane
        | NativeAction::Find
        | NativeAction::FindNext
        | NativeAction::FindPrevious
        | NativeAction::ArrangePane
        | NativeAction::NextPane
        | NativeAction::Split { .. }
        | NativeAction::ClosePane
        | NativeAction::FocusPane { .. }
        | NativeAction::ZoomPane
        | NativeAction::Copy
        | NativeAction::Paste => session
            .and_then(crate::rex::workspace::SessionView::active_pane)
            .is_some(),
    }
}

fn management_available(
    action: &actions::NativeAction,
    state: &crate::rex::workspace::WorkspaceState,
) -> bool {
    use actions::NativeAction;
    let Some(session) = state.active_session() else {
        return false;
    };
    let find_pane = |remote: &str| {
        state.pane_ids().into_iter().find(|pane| {
            state
                .pane(*pane)
                .is_some_and(|pane| pane.remote_id == remote)
        })
    };
    let command = match action {
        NativeAction::MovePane {
            anchor_block_id,
            direction,
            ratio,
        } => {
            let (Some(pane), Some(anchor)) = (session.active_pane(), find_pane(anchor_block_id))
            else {
                return false;
            };
            Command::MovePane {
                pane,
                anchor,
                axis: match direction {
                    crate::core::Direction::Left | crate::core::Direction::Right => {
                        crate::core::SplitAxis::Horizontal
                    }
                    _ => crate::core::SplitAxis::Vertical,
                },
                before: matches!(
                    direction,
                    crate::core::Direction::Left | crate::core::Direction::Up
                ),
                ratio: *ratio,
            }
        }
        NativeAction::SwapPanes { other_block_id } => {
            let (Some(pane), Some(other)) = (session.active_pane(), find_pane(other_block_id))
            else {
                return false;
            };
            Command::SwapPanes { pane, other }
        }
        NativeAction::MoveWindow { before_window_id } => {
            let Some(window) = session.active_window() else {
                return false;
            };
            let before = if let Some(remote) = before_window_id {
                let Some(before) = session
                    .windows()
                    .iter()
                    .find(|window| window.remote_id() == remote)
                else {
                    return false;
                };
                Some(before.id())
            } else {
                None
            };
            Command::MoveWindow {
                window: window.id(),
                before,
            }
        }
        _ => return false,
    };
    state.command(&command).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_window_keeps_navigation_and_window_actions_available() -> anyhow::Result<()> {
        let mut state = crate::rex::workspace::WorkspaceState::default();
        state.apply_view(&json!({"session_id":"session:empty","label":"Empty","revision":1,"active_window_id":"window:empty","windows":[{"window_id":"window:empty","label":"Empty","layers":[]},{"window_id":"window:other","label":"Other","layers":[]}]}))?;
        let modes = Value::Null;
        for action in [
            actions::NativeAction::NextWindow,
            actions::NativeAction::PreviousWindow,
            actions::NativeAction::GoToWindow { index: -1 },
            actions::NativeAction::CloseWindow,
            actions::NativeAction::RenameWindow,
        ] {
            assert!(native_available(&action, &state, &modes));
        }
        assert!(!native_available(
            &actions::NativeAction::ClosePane,
            &state,
            &modes
        ));
        assert!(!native_available(
            &actions::NativeAction::GoToWindow { index: 3 },
            &state,
            &modes
        ));
        assert!(!native_available(
            &actions::NativeAction::SelectSession {
                session_id: "session:empty".into(),
                window_id: Some("window:foreign".into())
            },
            &state,
            &modes
        ));
        Ok(())
    }
}
