//! Bounded desktop cache; all Rex IO and terminal decoding runs on its worker.
use super::{
    actions::{self, NativeAction},
    automation::{self, ActionContext, ActionFuture, ActionSender, ClientActionRequest},
    diagnostics,
    terminal::TerminalStream,
    transport::{ControlConnection, default_endpoint},
    workspace::{BlockView, SessionView, WorkspaceState},
};
use crate::{
    core::{
        Command, GridSize, PaneId,
        frame::TerminalFrame,
        input::{KeyInput, Modifiers, MouseAction, MouseButton, MouseInput, MouseTracking},
    },
    terminal::PaneStatus,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    task::{Context as TaskContext, Poll, Waker},
    thread,
    time::{Duration, Instant},
};

mod interaction;
use interaction::{LocalOperation, SearchIntent};

const COMMAND_CAPACITY: usize = 64;
const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_SCRIPTS: usize = 32;
const MAX_SCRIPT_BYTES: usize = 512 * 1024;

/// One client-owned Lua source; its name appears only in server diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientScript {
    pub name: String,
    pub source: String,
}

/// Decode failures cannot be repaired by fetching the same snapshot repeatedly.
/// This map belongs to one control connection, so reconnect allows a fresh try.
#[derive(Clone, Copy)]
enum StreamRetry {
    After(Instant),
    Blocked,
}

impl StreamRetry {
    fn ready(self, now: Instant) -> bool {
        match self {
            Self::After(deadline) => now >= deadline,
            Self::Blocked => false,
        }
    }

    const fn is_blocked(self) -> bool {
        matches!(self, Self::Blocked)
    }
}

#[derive(Clone, Default)]
struct Cache {
    state: WorkspaceState,
    frames: BTreeMap<PaneId, Arc<TerminalFrame>>,
    annotations: BTreeMap<PaneId, Arc<crate::core::interaction::TerminalAnnotations>>,
    searches: BTreeMap<PaneId, SearchIntent>,
    statuses: BTreeMap<PaneId, PaneStatus>,
    ready: BTreeSet<PaneId>,
    mouse_tracking: BTreeMap<PaneId, MouseTracking>,
    mouse_buttons: BTreeMap<PaneId, Vec<MouseInput>>,
    connected: bool,
    state_revision: u64,
    connection_generation: u64,
    keymap: Arc<Value>,
    scripts: Arc<Vec<ClientScript>>,
    script_attempt: Option<(String, String)>,
}

#[derive(Default)]
struct Mailbox {
    latest: Option<Cache>,
    errors: VecDeque<String>,
    waiter: Option<Waker>,
    last_logged_error: Option<(String, Instant)>,
    // Retain focus transitions and selection endings when the input queue fills.
    // New input cannot overtake these deferred lifecycle notifications.
    pending_focus: VecDeque<QueuedOperation>,
    pending_search: BTreeMap<PaneId, QueuedOperation>,
    client_actions: VecDeque<ClientActionRequest>,
    action_bytes: usize,
    callback_ids: VecDeque<(u64, String)>,
    worker_action_active: bool,
}

struct Update(Arc<Mutex<Mailbox>>);

impl Future for Update {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        let mut mailbox = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if mailbox.latest.is_some()
            || !mailbox.errors.is_empty()
            || !mailbox.client_actions.is_empty()
        {
            return Poll::Ready(());
        }
        mailbox.waiter = Some(cx.waker().clone());
        Poll::Pending
    }
}

#[derive(Clone, Copy)]
struct WheelInput {
    lines: i32,
    column: u16,
    row: u16,
    modifiers: Modifiers,
}

enum Operation {
    Local(PaneId, LocalOperation),
    SetScripts(Vec<ClientScript>, ActionSender),
    ServerAction(String, Value, ActionContext, ActionSender),
    ActionCommand(Command, ActionSender),
    RefreshInventory(crate::core::SessionId, ActionSender),
    RefreshInsights(crate::core::SessionId, ActionSender),
    NativeAction(NativeAction, ActionContext, ActionSender),
    ActionPaste(PaneId, String, ActionSender),
    Command(Command),
    Key(PaneId, KeyInput),
    KeySequence(PaneId, KeyInput, Vec<String>),
    Text(PaneId, String),
    Paste(PaneId, String),
    Resize(PaneId, GridSize, u32, u32),
    Scroll(PaneId, i32),
    Wheel(PaneId, WheelInput),
    Mouse(PaneId, MouseInput),
    Focus(PaneId, bool),
    Own(PaneId, bool),
}

impl Operation {
    fn pane(&self) -> Option<PaneId> {
        match self {
            Self::Key(pane, _)
            | Self::KeySequence(pane, ..)
            | Self::Text(pane, _)
            | Self::Paste(pane, _)
            | Self::Resize(pane, ..)
            | Self::Scroll(pane, _)
            | Self::Wheel(pane, _)
            | Self::Mouse(pane, _)
            | Self::Focus(pane, _)
            | Self::Own(pane, _) => Some(*pane),
            Self::Command(_)
            | Self::Local(..)
            | Self::NativeAction(..)
            | Self::ActionPaste(..)
            | Self::SetScripts(..)
            | Self::ServerAction(..)
            | Self::ActionCommand(..)
            | Self::RefreshInventory(..)
            | Self::RefreshInsights(..) => None,
        }
    }
}

struct QueuedOperation {
    instance: String,
    generation: u64,
    operation: Operation,
}

/// A nonblocking cached view of the official Rex server. Drop only detaches.
pub struct WorkspaceClient {
    cache: Cache,
    mailbox: Arc<Mutex<Mailbox>>,
    commands: SyncSender<QueuedOperation>,
    stopped: Arc<AtomicBool>,
}

impl Default for WorkspaceClient {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceClient {
    #[must_use]
    pub fn new() -> Self {
        match default_endpoint() {
            Ok(endpoint) => Self::connect(endpoint),
            Err(error) => {
                let client = Self::connect(String::new());
                record_error(&client.mailbox, format!("resolve Rex endpoint: {error:#}"));
                client.stopped.store(true, Ordering::Release);
                client
            }
        }
    }

    #[must_use]
    pub fn connect(endpoint: String) -> Self {
        let (commands, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_mailbox = Arc::clone(&mailbox);
        let worker_stopped = Arc::clone(&stopped);
        let result = thread::Builder::new()
            .name("rex-client".into())
            .spawn(move || {
                worker(&endpoint, &receiver, &worker_mailbox, &worker_stopped);
            });
        let client = Self {
            cache: Cache::default(),
            mailbox,
            commands,
            stopped,
        };
        if let Err(error) = result {
            record_error(&client.mailbox, format!("start Rex client: {error}"));
        }
        client
    }

    #[must_use]
    pub fn state(&self) -> &WorkspaceState {
        &self.cache.state
    }
    #[must_use]
    pub fn frame(&self, pane: PaneId) -> Option<&Arc<TerminalFrame>> {
        self.cache.frames.get(&pane)
    }
    #[must_use]
    pub fn status(&self, pane: PaneId) -> Option<&PaneStatus> {
        self.cache.statuses.get(&pane)
    }
    #[must_use]
    pub const fn connected(&self) -> bool {
        self.cache.connected
    }
    #[must_use]
    pub fn server_id(&self) -> Option<&str> {
        let id = self.cache.state.instance_id();
        (!id.is_empty()).then_some(id)
    }
    #[must_use]
    pub const fn state_revision(&self) -> u64 {
        self.cache.state_revision
    }

    #[must_use]
    pub const fn connection_generation(&self) -> u64 {
        self.cache.connection_generation
    }
    #[must_use]
    pub fn owns_size(&self, pane: PaneId) -> bool {
        self.state()
            .pane(pane)
            .is_some_and(|block| block.owner.as_deref() == Some(self.state().client_id()))
    }
    #[must_use]
    pub fn pane_label(&self, pane: PaneId) -> String {
        self.state()
            .pane(pane)
            .map_or_else(|| format!("Block {pane}"), |block| block.title().to_owned())
    }
    #[must_use]
    pub fn pane_message(&self, pane: PaneId) -> String {
        if self
            .state()
            .pane(pane)
            .is_some_and(|block| !block.is_terminal())
        {
            "This block's application is not supported by this client.".into()
        } else {
            "Waiting for Rex terminal data…".into()
        }
    }

    pub fn wait_for_update(&self) -> impl Future<Output = ()> + Send + 'static + use<> {
        Update(Arc::clone(&self.mailbox))
    }

    /// Consume authoritative state without socket IO.
    ///
    /// # Errors
    /// Returns the next worker error, after installing any received state.
    pub fn poll(&mut self) -> Result<bool> {
        let mut mailbox = self.mailbox.lock().unwrap_or_else(PoisonError::into_inner);
        let changed = mailbox.latest.is_some() || !mailbox.client_actions.is_empty();
        if let Some(cache) = mailbox.latest.take() {
            self.cache = cache;
        }
        if let Some(error) = mailbox.errors.pop_front() {
            bail!("{error}");
        }
        Ok(changed)
    }

    pub fn take_client_actions(&mut self) -> Vec<ClientActionRequest> {
        let mut mailbox = self.mailbox.lock().unwrap_or_else(PoisonError::into_inner);
        mailbox.action_bytes = 0;
        mailbox.client_actions.drain(..).collect()
    }

    #[must_use]
    pub fn action_request_current(&self, request: &ClientActionRequest) -> bool {
        self.connected()
            && request.instance == self.state().instance_id()
            && request.generation == self.cache.connection_generation
    }

    #[must_use]
    pub fn keymap(&self) -> &Value {
        &self.cache.keymap
    }

    #[must_use]
    pub fn keymap_snapshot(&self) -> Arc<Value> {
        Arc::clone(&self.cache.keymap)
    }

    /// Replace client scripts atomically on the server and refresh their keymap.
    /// Accepted sources are retained for a new server/client identity.
    ///
    /// # Errors
    /// Rejects oversized sources, disconnected clients or saturated queues.
    pub fn set_scripts(&mut self, scripts: Vec<ClientScript>) -> Result<ActionFuture> {
        scripts_params(&scripts)?;
        let (sender, future) = automation::completion();
        self.enqueue(Operation::SetScripts(scripts, sender))?;
        Ok(future)
    }

    /// Invoke a Lua/block action through the ordered worker. The full response
    /// includes native `client_actions` for the caller to execute once, in order.
    ///
    /// # Errors
    /// Rejects invalid names/arguments/context, disconnection or saturated queues.
    pub fn perform_server_action(
        &mut self,
        name: String,
        args: Value,
        context: ActionContext,
    ) -> Result<ActionFuture> {
        server_action_params(&name, &args, &context)?;
        automation::validate_context(self.state(), &context)?;
        let (sender, future) = automation::completion();
        self.enqueue(Operation::ServerAction(name, args, context, sender))?;
        Ok(future)
    }

    /// Refresh block metadata even when no layout revision changed.
    ///
    /// # Errors
    /// Rejects stale sessions, disconnection and full command queues.
    pub fn refresh_inventory(&mut self, session: crate::core::SessionId) -> Result<ActionFuture> {
        ensure!(
            self.state()
                .sessions()
                .iter()
                .any(|entry| entry.id() == session),
            "Rex session no longer exists"
        );
        let (sender, future) = automation::completion();
        self.enqueue(Operation::RefreshInventory(session, sender))?;
        Ok(future)
    }

    /// Query attached clients and terminal processes for a session overview.
    /// The future resolves to a value for `insights::SessionInsights::from_value`.
    ///
    /// # Errors
    /// Rejects stale sessions, disconnection and full command queues.
    pub fn refresh_insights(&mut self, session: crate::core::SessionId) -> Result<ActionFuture> {
        ensure!(
            self.state()
                .sessions()
                .iter()
                .any(|entry| entry.id() == session),
            "Rex session no longer exists"
        );
        let (sender, future) = automation::completion();
        self.enqueue(Operation::RefreshInsights(session, sender))?;
        Ok(future)
    }

    /// Execute through the same ordered worker as terminal keys and layout commands.
    ///
    /// # Errors
    /// Rejects disconnected clients or saturated queues.
    pub fn perform_native_action(
        &mut self,
        action: NativeAction,
        context: ActionContext,
    ) -> Result<ActionFuture> {
        let (sender, future) = automation::completion();
        self.enqueue(Operation::NativeAction(action, context, sender))?;
        Ok(future)
    }

    fn enqueue(&self, operation: Operation) -> Result<()> {
        ensure!(self.connected(), "Rex is disconnected");
        let mailbox = self.mailbox.lock().unwrap_or_else(PoisonError::into_inner);
        ensure!(
            mailbox.pending_focus.is_empty(),
            "Rex focus update is pending; try again"
        );
        let queued = QueuedOperation {
            instance: self.state().instance_id().into(),
            generation: self.cache.connection_generation,
            operation,
        };
        match self.commands.try_send(queued) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => bail!("Rex command queue is full; try again"),
            Err(TrySendError::Disconnected(_)) => bail!("Rex client worker stopped"),
        }
    }

    /// # Errors
    /// Rejects disconnected clients, stale targets and full queues.
    pub fn apply(&mut self, command: Command) -> Result<()> {
        validate_command(self.state(), &command)?;
        self.enqueue(Operation::Command(command))
    }

    /// Run a workspace command and resolve after authoritative state publication.
    ///
    /// # Errors
    /// Rejects stale/invalid commands, disconnection or saturated queues.
    pub fn perform_command(&mut self, command: Command) -> Result<ActionFuture> {
        validate_command(self.state(), &command)?;
        let (sender, future) = automation::completion();
        self.enqueue(Operation::ActionCommand(command, sender))?;
        Ok(future)
    }

    /// # Errors
    /// Rejects disconnected clients or full queues.
    pub fn send_key(&mut self, pane: PaneId, key: &KeyInput) -> Result<()> {
        self.check_terminal_input(pane)?;
        ensure!(
            key.key.len() <= 128
                && key
                    .text
                    .as_ref()
                    .is_none_or(|text| text.len() <= MAX_INPUT_BYTES),
            "Rex key exceeds input limit"
        );
        self.enqueue(Operation::Key(pane, key.clone()))
    }

    /// Forward the final key and explicit prefix of a Lua-bound key sequence.
    ///
    /// # Errors
    /// Rejects invalid/oversized keys or prefixes and unavailable terminals.
    pub fn send_key_sequence(
        &mut self,
        pane: PaneId,
        key: &KeyInput,
        prefix: Vec<String>,
    ) -> Result<()> {
        validate_sequence(key, &prefix)?;
        self.check_terminal_input(pane)?;
        self.enqueue(Operation::KeySequence(pane, key.clone(), prefix))
    }
    /// # Errors
    /// Rejects invalid UTF-8, oversized text or unavailable connections.
    pub fn write_input(&mut self, pane: PaneId, bytes: &[u8]) -> Result<()> {
        self.check_terminal_input(pane)?;
        ensure!(bytes.len() <= MAX_INPUT_BYTES, "Rex input exceeds 64 KiB");
        self.enqueue(Operation::Text(pane, std::str::from_utf8(bytes)?.into()))
    }
    /// # Errors
    /// Rejects oversized paste or unavailable connections.
    pub fn paste(&mut self, pane: PaneId, text: &str) -> Result<()> {
        self.check_terminal_input(pane)?;
        ensure!(text.len() <= MAX_INPUT_BYTES, "Rex paste exceeds 64 KiB");
        self.enqueue(Operation::Paste(pane, text.into()))
    }

    /// # Errors
    /// Rejects unavailable terminals, oversized clipboard data and full queues.
    pub fn paste_action(&mut self, pane: PaneId, text: String) -> Result<ActionFuture> {
        self.check_terminal_input(pane)?;
        ensure!(text.len() <= MAX_INPUT_BYTES, "Rex paste exceeds 64 KiB");
        let (sender, future) = automation::completion();
        self.enqueue(Operation::ActionPaste(pane, text, sender))?;
        Ok(future)
    }

    /// Whether this pane's application currently requests mouse tracking.
    #[must_use]
    pub fn mouse_reporting(&self, pane: PaneId) -> bool {
        self.mouse_tracking(pane) != MouseTracking::None
    }

    /// The mouse events currently requested by this ready terminal application.
    #[must_use]
    pub fn mouse_tracking(&self, pane: PaneId) -> MouseTracking {
        if self.input_available(pane) {
            self.cache
                .mouse_tracking
                .get(&pane)
                .copied()
                .unwrap_or(MouseTracking::None)
        } else {
            MouseTracking::None
        }
    }

    /// Input becomes available only after this connection restores the live stream.
    #[must_use]
    pub fn input_available(&self, pane: PaneId) -> bool {
        terminal_input_available(&self.cache, pane)
    }

    /// Send application mouse input; Shift reserves the pointer for local selection.
    ///
    /// # Errors
    /// Rejects unavailable terminals, disconnected clients or full queues.
    pub fn mouse(&mut self, pane: PaneId, mouse: MouseInput) -> Result<()> {
        if mouse.modifiers.shift && mouse.action != MouseAction::Release {
            return Ok(());
        }
        self.check_terminal_input(pane)?;
        self.enqueue(Operation::Mouse(pane, mouse))
    }

    /// Notify the terminal application of focus changes; Rex filters mode 1004.
    ///
    /// # Errors
    /// Rejects unavailable terminals, disconnected clients or full queues.
    pub fn focus(&mut self, pane: PaneId, focused: bool) -> Result<()> {
        self.check_terminal_input(pane)?;
        let mut mailbox = self.mailbox.lock().unwrap_or_else(PoisonError::into_inner);
        let queued = QueuedOperation {
            instance: self.state().instance_id().into(),
            generation: self.cache.connection_generation,
            operation: Operation::Focus(pane, focused),
        };
        if mailbox.pending_focus.is_empty() {
            match self.commands.try_send(queued) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(_)) => bail!("Rex client worker stopped"),
                Err(TrySendError::Full(queued)) => return defer_focus(&mut mailbox, queued),
            }
        }
        defer_focus(&mut mailbox, queued)
    }

    fn check_terminal_input(&self, pane: PaneId) -> Result<()> {
        ensure!(
            terminal_input_available(&self.cache, pane),
            "Rex terminal data is unavailable; input was not sent"
        );
        Ok(())
    }
    /// # Errors
    /// Rejects disconnected clients or full queues.
    pub fn resize(&mut self, pane: PaneId, size: GridSize, width: u32, height: u32) -> Result<()> {
        ensure!(
            usize::from(size.columns()) * usize::from(size.rows()) <= 65_536,
            "Rex viewport exceeds 65,536 cells"
        );
        ensure!(
            width > 0 && height > 0 && width <= 65535 && height <= 65535,
            "invalid Rex cell size"
        );
        self.enqueue(Operation::Resize(pane, size, width, height))
    }
    /// # Errors
    /// Rejects disconnected clients or full queues.
    pub fn scroll(&mut self, pane: PaneId, lines: i32) -> Result<()> {
        self.enqueue(Operation::Scroll(pane, lines))
    }
    /// Route wheel input to terminal applications or this client's local history.
    ///
    /// # Errors
    /// Rejects disconnected clients or full command queues.
    pub fn wheel(
        &mut self,
        pane: PaneId,
        lines: i32,
        column: u16,
        row: u16,
        modifiers: Modifiers,
    ) -> Result<()> {
        if lines == 0 {
            return Ok(());
        }
        self.enqueue(Operation::Wheel(
            pane,
            WheelInput {
                lines: lines.clamp(-1024, 1024),
                column,
                row,
                modifiers,
            },
        ))
    }

    /// # Errors
    /// Rejects disconnected clients or full queues.
    pub fn set_size_ownership(&mut self, pane: PaneId, take: bool) -> Result<()> {
        self.enqueue(Operation::Own(pane, take))
    }
}

fn defer_focus(mailbox: &mut Mailbox, queued: QueuedOperation) -> Result<()> {
    let Operation::Focus(pane, _) = queued.operation else {
        bail!("only terminal focus may use the deferred focus queue");
    };
    mailbox
        .pending_focus
        .retain(|previous| !matches!(previous.operation, Operation::Focus(id, _) if id == pane));
    ensure!(
        mailbox.pending_focus.len() < 32,
        "Rex terminal notification queue is full"
    );
    mailbox.pending_focus.push_back(queued);
    Ok(())
}

fn next_operation(
    commands: &Receiver<QueuedOperation>,
    mailbox: &Mutex<Mailbox>,
) -> Option<QueuedOperation> {
    if let Some((_, query)) = mailbox
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .pending_search
        .pop_first()
    {
        return Some(query);
    }
    // Deferred focus follows everything already accepted by the normal queue.
    // enqueue() rejects new input until this tail has been consumed.
    commands.try_recv().ok().or_else(|| {
        mailbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending_focus
            .pop_front()
    })
}

impl Drop for WorkspaceClient {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

fn publish(mailbox: &Mutex<Mailbox>, cache: &Cache) {
    let waiter = {
        let mut mailbox = mailbox.lock().unwrap_or_else(PoisonError::into_inner);
        mailbox.latest = Some(cache.clone());
        mailbox.waiter.take()
    };
    if let Some(waiter) = waiter {
        waiter.wake();
    }
}

fn record_error(mailbox: &Mutex<Mailbox>, error: String) {
    let (waiter, log_error) = {
        let mut mailbox = mailbox.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        let log_error = if mailbox
            .last_logged_error
            .as_ref()
            .is_none_or(|(previous, time)| {
                previous != &error || now.duration_since(*time) >= Duration::from_secs(5)
            }) {
            mailbox.last_logged_error = Some((error.clone(), now));
            Some(error.clone())
        } else {
            None
        };
        if mailbox.errors.back() != Some(&error) {
            if mailbox.errors.len() == 16 {
                mailbox.errors.pop_front();
            }
            mailbox.errors.push_back(error);
        }
        (mailbox.waiter.take(), log_error)
    };
    if let Some(waiter) = waiter {
        waiter.wake();
    }
    // A slow stderr consumer must never hold the UI cache lock.
    if let Some(error) = log_error {
        diagnostics::write("ERROR", "client", &error);
    }
}

fn modifier_names(modifiers: Modifiers) -> String {
    let mut mods = Vec::new();
    if modifiers.shift {
        mods.push("shift");
    }
    if modifiers.control {
        mods.push("ctrl");
    }
    if modifiers.alt {
        mods.push("alt");
    }
    if modifiers.super_key {
        mods.push("super");
    }
    mods.join("+")
}

fn key_payload(key: &KeyInput) -> Value {
    let mods = modifier_names(key.modifiers);
    let character = key
        .key
        .chars()
        .next()
        .filter(|_| key.key.chars().count() == 1);
    let physical = character.map_or_else(
        || {
            match key.key.as_str() {
                "up" => "arrow_up",
                "down" => "arrow_down",
                "left" => "arrow_left",
                "right" => "arrow_right",
                "pageup" => "page_up",
                "pagedown" => "page_down",
                "backspace" => "backspace",
                "enter" => "enter",
                "escape" => "escape",
                "tab" => "tab",
                "space" => "space",
                "home" => "home",
                "end" => "end",
                "delete" => "delete",
                "insert" => "insert",
                name if name.starts_with('f')
                    && name[1..]
                        .parse::<u8>()
                        .is_ok_and(|number| (1..=25).contains(&number)) =>
                {
                    name
                }
                _ => "unidentified",
            }
            .to_owned()
        },
        |character| {
            if character.is_ascii_alphabetic() {
                format!("key_{}", character.to_ascii_lowercase())
            } else if character.is_ascii_digit() {
                format!("digit_{character}")
            } else {
                match character {
                    '`' => "backquote",
                    '\\' => "backslash",
                    '[' => "bracket_left",
                    ']' => "bracket_right",
                    ',' => "comma",
                    '=' => "equal",
                    '-' => "minus",
                    '.' => "period",
                    '\'' => "quote",
                    ';' => "semicolon",
                    '/' => "slash",
                    ' ' => "space",
                    _ => "unidentified",
                }
                .to_owned()
            }
        },
    );
    json!({"key":physical,"mods":mods,"consumed_mods":"", "text":key.text.as_deref().unwrap_or(""),
        "unshifted_codepoint":character.map_or(0, |character| u32::from(character.to_ascii_lowercase())),
        "composing":false,"action":if key.repeat {"repeat"} else {"press"}})
}

// The worker owns both the persistent control connection and terminal decoders.
fn worker(
    endpoint: &str,
    commands: &Receiver<QueuedOperation>,
    mailbox: &Arc<Mutex<Mailbox>>,
    stopped: &AtomicBool,
) {
    diagnostics::write("INFO", "client", &format!("connecting endpoint={endpoint}"));
    let mut cache = Cache::default();
    let mut token = None;
    while !stopped.load(Ordering::Acquire) {
        let result = connected_worker(endpoint, commands, mailbox, stopped, &mut cache, &mut token);
        if cache.connected {
            diagnostics::write(
                "INFO",
                "client",
                &format!("disconnected endpoint={endpoint}"),
            );
        }
        cache.connected = false;
        cache.annotations.clear();
        publish(mailbox, &cache);
        if let Err(error) = result {
            record_error(
                mailbox,
                format!("Rex connection endpoint={endpoint}: {error:#}"),
            );
        }
        // Only owned local query intent survives a connection. Process input
        // and mutations are never replayed after a new attachment.
        if interaction::drain_disconnected(&mut cache, commands, mailbox) {
            publish(mailbox, &cache);
        }
        for _ in 0..10 {
            if stopped.load(Ordering::Acquire) {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

fn connected_worker(
    endpoint: &str,
    commands: &Receiver<QueuedOperation>,
    mailbox: &Arc<Mutex<Mailbox>>,
    stopped: &AtomicBool,
    cache: &mut Cache,
    token: &mut Option<String>,
) -> Result<()> {
    let mut control = ControlConnection::connect_with_resume(endpoint, token.as_deref())?;
    cache.ready.clear();
    cache.mouse_tracking.clear();
    cache.keymap = Arc::new(Value::Null);
    *token = control.reconnect_token().map(str::to_owned);
    let bootstrap = control
        .call("session.bootstrap", json!({"event_scope":"all"}))
        .context("bootstrap Rex sessions")?;
    let bootstrap_seq = control.last_response_seq();
    let old_instance = cache.state.instance_id().to_owned();
    cache.state.apply_bootstrap(&bootstrap)?;
    if old_instance != cache.state.instance_id() {
        cache.frames.clear();
        cache.annotations.clear();
        cache.searches.clear();
        cache.statuses.clear();
        cache.mouse_buttons.clear();
    }
    let mut attached = BTreeSet::new();
    attach_sessions(&mut control, cache, &mut attached)?;
    release_reconnected_buttons(&mut control, cache)?;
    cache.connection_generation = cache
        .connection_generation
        .checked_add(1)
        .context("Rex connection generation exhausted")?;
    cache.connected = true;
    diagnostics::write(
        "INFO",
        "client",
        &format!(
            "connected endpoint={endpoint} instance={} sessions={}",
            cache.state.instance_id(),
            cache.state.sessions().len()
        ),
    );
    cache.state_revision = cache.state_revision.wrapping_add(1);
    publish(mailbox, cache);
    register_client_actions(&mut control, cache, mailbox)?;
    let mut sequence = bootstrap_seq;
    let mut streams = BTreeMap::new();
    let mut retry_streams = BTreeMap::new();
    let mut stream_cursor = 0;
    let mut fetched_titles = BTreeMap::new();
    let mut inventories = super::inventory::RefreshQueue::default();
    let mut insights = super::insights::RefreshQueue::default();
    while !stopped.load(Ordering::Acquire) {
        let completion = process_commands(
            &mut control,
            cache,
            &mut streams,
            commands,
            mailbox,
            &mut insights,
        );
        let mut state_changed = refresh_events(
            &mut control,
            cache,
            &mut attached,
            &mut sequence,
            &fetched_titles,
        )?;
        if let Some(session) = inventories.next_session(&cache.state) {
            match super::inventory::refresh(&mut control, &mut cache.state, &session) {
                Ok(changed) => state_changed |= changed,
                Err(error) => {
                    record_error(mailbox, format!("Refresh Rex block inventory: {error:#}"));
                }
            }
        }
        state_changed |=
            refresh_terminal_titles(&mut control, cache, &mut fetched_titles, mailbox)?;
        if state_changed {
            cache.state_revision = cache.state_revision.wrapping_add(1);
        }
        let stream_changed = service_streams(
            endpoint,
            &mut control,
            cache,
            &mut streams,
            &mut retry_streams,
            &mut stream_cursor,
            mailbox,
        )?;
        if state_changed || stream_changed {
            publish(mailbox, cache);
        }
        insights.service(&control.handle(), &cache.state);
        if let Some((sender, result)) = completion {
            sender.send(result);
        }
    }
    Ok(())
}

fn process_commands(
    control: &mut ControlConnection,
    cache: &mut Cache,
    streams: &mut BTreeMap<PaneId, TerminalStream>,
    commands: &Receiver<QueuedOperation>,
    mailbox: &Arc<Mutex<Mailbox>>,
    insights: &mut super::insights::RefreshQueue,
) -> Option<(ActionSender, Result<Value>)> {
    let command_start = Instant::now();
    for _ in 0..16 {
        let Some(queued) = next_operation(commands, mailbox) else {
            break;
        };
        if queued.instance != cache.state.instance_id()
            || (queued.generation != cache.connection_generation
                && !interaction::is_query_update(&queued.operation))
        {
            if !matches!(queued.operation, Operation::Local(..)) {
                record_error(
                    mailbox,
                    "Rex instance changed; stale operation was not sent".into(),
                );
            }
            continue;
        }
        let operation = match queued.operation {
            Operation::Local(pane, operation) => {
                let copy = matches!(operation, LocalOperation::Copy(_));
                interaction::execute(cache, streams, pane, operation, mailbox);
                if copy || command_start.elapsed() >= Duration::from_millis(4) {
                    break;
                }
                continue;
            }
            Operation::RefreshInventory(session, completion) => {
                let result = refresh_requested_inventory(control, cache, session);
                publish_configuration_result(cache, mailbox, &result);
                return Some((completion, result));
            }
            Operation::RefreshInsights(session, completion) => {
                insights.enqueue(&cache.state, session, completion);
                continue;
            }
            Operation::SetScripts(scripts, completion) => {
                let result = install_scripts(control, cache, scripts, mailbox);
                publish_configuration_result(cache, mailbox, &result);
                return Some((completion, result));
            }
            Operation::ServerAction(name, args, context, completion) => {
                let result = execute_server_action(control, cache, &name, &args, &context, mailbox);
                publish_configuration_result(cache, mailbox, &result);
                return Some((completion, result));
            }
            Operation::ActionCommand(command, completion) => {
                let result = execute_action_command(control, cache, &command);
                publish_configuration_result(cache, mailbox, &result);
                return Some((completion, result));
            }
            operation => operation,
        };
        if let Operation::NativeAction(action, context, completion) = operation {
            let result = execute_native_action(control, cache, &action, &context, mailbox);
            return Some((completion, result));
        }
        if let Operation::ActionPaste(pane, text, completion) = operation {
            let result = perform(control, cache, streams, Operation::Paste(pane, text));
            if result.as_ref().is_ok_and(|changed| *changed) {
                publish(mailbox, cache);
            }
            completion.send(result.map(|_| Value::Null));
            if command_start.elapsed() >= Duration::from_millis(4) {
                break;
            }
            continue;
        }
        let workspace_command = matches!(&operation, Operation::Command(_));
        match perform(control, cache, streams, operation) {
            Ok(changed) => {
                if changed {
                    if workspace_command {
                        cache.state_revision = cache.state_revision.wrapping_add(1);
                    }
                    publish(mailbox, cache);
                }
            }
            Err(error) => record_error(mailbox, format!("{error:#}")),
        }
        if command_start.elapsed() >= Duration::from_millis(4) {
            break;
        }
    }
    None
}

fn validate_command(state: &WorkspaceState, command: &Command) -> Result<()> {
    match command {
        Command::CreateSession { title }
        | Command::RenameSession { title, .. }
        | Command::CreateWindow { title, .. }
        | Command::RenameWindow { title, .. } => ensure!(
            title.len() <= 512 && title.chars().count() <= 128,
            "Rex labels are limited to 128 characters"
        ),
        _ => {}
    }
    state.command(command)?;
    Ok(())
}

fn refresh_requested_inventory(
    control: &mut ControlConnection,
    cache: &mut Cache,
    session: crate::core::SessionId,
) -> Result<Value> {
    let id = cache
        .state
        .sessions()
        .iter()
        .find(|entry| entry.id() == session)
        .context("Rex session no longer exists")?
        .remote_id()
        .to_owned();
    super::inventory::refresh(control, &mut cache.state, &id)?;
    Ok(Value::Null)
}

fn scripts_params(scripts: &[ClientScript]) -> Result<Value> {
    ensure!(
        scripts.len() <= MAX_SCRIPTS,
        "Rex client scripts exceed the source count limit"
    );
    let mut total = 0_usize;
    let mut values = Vec::with_capacity(scripts.len());
    for script in scripts {
        ensure!(
            script.name.len() <= 512 && !script.name.contains(['\0', '\r', '\n']),
            "invalid Rex client script name"
        );
        total = total.saturating_add(script.source.len());
        ensure!(
            total <= MAX_SCRIPT_BYTES,
            "Rex client scripts exceed 512 KiB"
        );
        values.push(json!({"name":script.name,"source":script.source}));
    }
    let params = json!({"scripts":values});
    ensure!(
        params.to_string().len() <= super::protocol::MAX_MESSAGE_BYTES - 4096,
        "Rex client scripts exceed the encoded message limit"
    );
    Ok(params)
}

fn server_action_params(name: &str, args: &Value, context: &ActionContext) -> Result<Value> {
    ensure!(
        !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control),
        "invalid Rex server action name"
    );
    ensure!(
        args.is_null() || args.is_object(),
        "Rex action arguments must be an object"
    );
    let mut params = json!({"action":name,"origin":"key"});
    if !args.is_null() {
        params["args"] = args.clone();
    }
    if let Some(session) = &context.session_id {
        params["session_id"] = json!(session);
    }
    if let Some(block) = &context.block_id {
        params["block_id"] = json!(block);
    }
    ensure!(
        params.to_string().len() <= super::protocol::MAX_MESSAGE_BYTES - 4096,
        "Rex server action exceeds the encoded message limit"
    );
    Ok(params)
}

fn validate_sequence(key: &KeyInput, prefix: &[String]) -> Result<()> {
    ensure!(
        key.key.len() <= 128
            && key
                .text
                .as_ref()
                .is_none_or(|text| text.len() <= MAX_INPUT_BYTES),
        "Rex key exceeds input limit"
    );
    ensure!(
        prefix.len() <= 16
            && prefix.iter().all(|key| !key.is_empty()
                && key.len() <= 128
                && !key.chars().any(char::is_control)),
        "Rex key sequence prefix exceeds client limits"
    );
    Ok(())
}

struct WorkerRpcGuard<'a>(&'a Mutex<Mailbox>);
impl Drop for WorkerRpcGuard<'_> {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .worker_action_active = false;
    }
}

fn guarded_worker_rpc(
    control: &mut ControlConnection,
    mailbox: &Mutex<Mailbox>,
    method: &str,
    params: Value,
) -> Result<Value> {
    mailbox
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .worker_action_active = true;
    let _guard = WorkerRpcGuard(mailbox);
    control.call(method, params)
}

fn action_needs_worker(name: &str, args: &Value) -> bool {
    actions::parse(name, args).is_ok_and(|action| {
        !matches!(
            action,
            NativeAction::FontIncrease
                | NativeAction::FontDecrease
                | NativeAction::FontReset
                | NativeAction::ToggleSidebar
                | NativeAction::EnterMode { .. }
                | NativeAction::ExitMode
                | NativeAction::ExitAllModes
                | NativeAction::RenameSession
                | NativeAction::RenameWindow
                | NativeAction::ArrangePane
                | NativeAction::Find
                | NativeAction::FindNext
                | NativeAction::FindPrevious
        )
    })
}

fn refresh_keymap(control: &mut ControlConnection, cache: &mut Cache) -> Result<()> {
    cache.keymap = Arc::new(control.call(
        "client.keymap",
        json!({
            "actions":actions::declarations(),"defaults":actions::defaults()
        }),
    )?);
    Ok(())
}

fn script_identity(cache: &Cache) -> (String, String) {
    (
        cache.state.instance_id().to_owned(),
        cache.state.client_id().to_owned(),
    )
}

fn install_scripts(
    control: &mut ControlConnection,
    cache: &mut Cache,
    scripts: Vec<ClientScript>,
    mailbox: &Mutex<Mailbox>,
) -> Result<Value> {
    let params = scripts_params(&scripts)?;
    // Source code may execute mutations while loading. Do not automatically
    // repeat an uncertain load for this same retained client identity.
    cache.script_attempt = Some(script_identity(cache));
    let result = guarded_worker_rpc(control, mailbox, "client.set_script", params)?;
    cache.scripts = Arc::new(scripts);
    refresh_keymap(control, cache)?;
    Ok(result)
}

fn restore_scripts(
    control: &mut ControlConnection,
    cache: &mut Cache,
    mailbox: &Mutex<Mailbox>,
) -> Result<()> {
    let identity = script_identity(cache);
    if cache.script_attempt.as_ref() == Some(&identity) {
        return Ok(());
    }
    cache.script_attempt = Some(identity);
    if cache.scripts.is_empty() {
        return Ok(());
    }
    let params = scripts_params(&cache.scripts)?;
    match guarded_worker_rpc(control, mailbox, "client.set_script", params) {
        Ok(_) => refresh_keymap(control, cache)?,
        Err(error) => record_error(mailbox, format!("restore Rex client scripts: {error:#}")),
    }
    Ok(())
}

fn execute_server_action(
    control: &mut ControlConnection,
    cache: &mut Cache,
    name: &str,
    args: &Value,
    context: &ActionContext,
    mailbox: &Mutex<Mailbox>,
) -> Result<Value> {
    automation::validate_context(&cache.state, context)?;
    let params = server_action_params(name, args, context)?;
    let response = guarded_worker_rpc(control, mailbox, "client.action", params)?;
    refresh_all_views(control, cache)?;
    Ok(response)
}

fn refresh_all_views(control: &mut ControlConnection, cache: &mut Cache) -> Result<()> {
    cache
        .state
        .update_sessions(&control.call("session.list", json!({}))?)?;
    // Lua can change any session; fetch every authoritative view before resolving
    // the action future, without guessing affected targets from its name.
    attach_sessions(control, cache, &mut BTreeSet::new())?;
    Ok(())
}

fn execute_action_command(
    control: &mut ControlConnection,
    cache: &mut Cache,
    command: &Command,
) -> Result<Value> {
    validate_command(&cache.state, command)?;
    perform_workspace(control, cache, command)?;
    if !matches!(command, Command::SelectSession(_)) {
        refresh_all_views(control, cache)?;
    }
    Ok(Value::Null)
}

fn publish_configuration_result(
    cache: &mut Cache,
    mailbox: &Mutex<Mailbox>,
    result: &Result<Value>,
) {
    cache.state_revision = cache.state_revision.wrapping_add(1);
    publish(mailbox, cache);
    if let Err(error) = result {
        record_error(mailbox, format!("Rex configuration action: {error:#}"));
    }
}

fn register_client_actions(
    control: &mut ControlConnection,
    cache: &mut Cache,
    mailbox: &Arc<Mutex<Mailbox>>,
) -> Result<()> {
    let instance = cache.state.instance_id().to_owned();
    let generation = cache.connection_generation;
    let handle = control.handle();
    let callback_mailbox = Arc::clone(mailbox);
    control.set_callback_handler(Arc::new(move |event| {
        queue_client_action(&callback_mailbox, &event, &instance, generation, &handle);
    }));
    cache.keymap = Arc::new(control.call(
        "client.keymap",
        json!({
            "actions":actions::declarations(), "defaults":actions::defaults()
        }),
    )?);
    restore_scripts(control, cache, mailbox)?;
    publish(mailbox, cache);
    Ok(())
}

fn queue_client_action(
    mailbox: &Mutex<Mailbox>,
    event: &Value,
    instance: &str,
    generation: u64,
    handle: &super::transport::ControlHandle,
) {
    let mut mailbox = mailbox.lock().unwrap_or_else(PoisonError::into_inner);
    let call_id = event["payload"]["payload"]["call_id"]
        .as_str()
        .unwrap_or("");
    if mailbox
        .callback_ids
        .iter()
        .any(|(seen, id)| *seen == generation && id == call_id)
    {
        return;
    }
    let result =
        ClientActionRequest::from_event(event, instance.into(), generation, handle.clone());
    let bytes = event.to_string().len();
    let admitted = mailbox.client_actions.len() < 64
        && mailbox.action_bytes.saturating_add(bytes) <= super::protocol::MAX_PENDING_EVENT_BYTES;
    match result {
        Ok(request)
            if mailbox.worker_action_active
                && action_needs_worker(&request.action, &request.args) =>
        {
            drop(mailbox);
            let _ = request.reply(Err(automation::ActionError::refused(
                "A server action is waiting for this client; nested worker actions are unavailable",
            )));
        }
        Ok(request) if admitted => {
            mailbox
                .callback_ids
                .push_back((generation, request.call_id.clone()));
            if mailbox.callback_ids.len() > 256 {
                mailbox.callback_ids.pop_front();
            }
            mailbox.client_actions.push_back(request);
            mailbox.action_bytes += bytes;
            let waiter = mailbox.waiter.take();
            drop(mailbox);
            if let Some(waiter) = waiter {
                waiter.wake();
            }
        }
        Ok(request) => {
            drop(mailbox);
            let _ = request.reply(Err(automation::ActionError::refused(
                "Client action queue is full",
            )));
        }
        Err(_) => {
            drop(mailbox);
            if !call_id.is_empty() {
                let _ = handle.request(
                    "client.reply",
                    json!({"call_id":call_id,
                    "error":{"code":"refused","message":"Client action is invalid"}}),
                );
            }
        }
    }
}

fn execute_native_action(
    control: &mut ControlConnection,
    cache: &mut Cache,
    action: &NativeAction,
    context: &ActionContext,
    mailbox: &Arc<Mutex<Mailbox>>,
) -> Result<Value> {
    let reload = action == &NativeAction::ReloadConfig;
    let result = super::action_worker::execute(control, &mut cache.state, action, context)
        .and_then(|value| {
            if reload {
                register_client_actions(control, cache, mailbox)?;
            }
            Ok(value)
        });
    cache.state_revision = cache.state_revision.wrapping_add(1);
    publish(mailbox, cache);
    if let Err(error) = &result {
        record_error(mailbox, format!("Rex client action: {error:#}"));
    }
    result
}

fn refresh_events(
    control: &mut ControlConnection,
    cache: &mut Cache,
    attached: &mut BTreeSet<String>,
    sequence: &mut u64,
    title_sequences: &BTreeMap<PaneId, u64>,
) -> Result<bool> {
    let mut state_changed = false;
    let mut list_changed = false;
    let mut keymap_changed = false;
    let mut refresh = BTreeSet::new();
    let mut events = control.drain_events();
    if let Some(event) = control.poll_event(Duration::from_millis(2))? {
        events.push(event);
    }
    for event in events {
        let seq = event["seq"].as_u64().context("Rex event lacks sequence")?;
        if seq <= *sequence {
            continue;
        }
        ensure!(
            seq == *sequence + 1,
            "Rex event sequence gap; reconnect and bootstrap"
        );
        *sequence = seq;
        let event = &event["payload"];
        match event["type"].as_str() {
            Some("keymap_changed") => keymap_changed = true,
            Some("session_created" | "session_destroyed" | "session_moved") => {
                list_changed = true;
            }
            Some("session_event") => {
                state_changed |= apply_session_event(
                    cache,
                    &event["payload"],
                    &mut refresh,
                    seq,
                    title_sequences,
                )?;
            }
            _ => {}
        }
    }
    if keymap_changed {
        cache.keymap = Arc::new(control.call(
            "client.keymap",
            json!({
                "actions":actions::declarations(),"defaults":actions::defaults()
            }),
        )?);
        state_changed = true;
    }
    if list_changed {
        let sessions = control.call("session.list", json!({}))?;
        state_changed |= cache.state.update_sessions(&sessions)?;
        state_changed |= attach_sessions(control, cache, attached)?;
    }
    let mut snapshots = Vec::new();
    for id in refresh {
        if !cache
            .state
            .sessions()
            .iter()
            .any(|session| session.remote_id() == id)
        {
            continue;
        }
        let view = control.call("session.view", json!({"session_id":id}))?;
        let inventory = control.call("session.list_blocks", json!({"session_id":id}))?;
        snapshots.push((view, inventory));
    }
    if !snapshots.is_empty() {
        state_changed |= cache.state.apply_snapshots(&snapshots)?;
    }
    Ok(state_changed)
}

fn apply_session_event(
    cache: &mut Cache,
    payload: &Value,
    refresh: &mut BTreeSet<String>,
    sequence: u64,
    title_sequences: &BTreeMap<PaneId, u64>,
) -> Result<bool> {
    let event = &payload["event"];
    if event["type"] == "block_closed" {
        if let Some(id) = payload["session_id"].as_str() {
            refresh.insert(id.to_owned());
        }
    } else if event["type"] == "session_view_changed" {
        let id = payload["session_id"]
            .as_str()
            .context("Rex event lacks session ID")?;
        let revision = event["payload"]["revision"]
            .as_u64()
            .context("Rex view event lacks revision")?;
        if cache
            .state
            .sessions()
            .iter()
            .find(|session| session.remote_id() == id)
            .is_none_or(|session| session.revision().is_none_or(|current| revision > current))
        {
            refresh.insert(id.to_owned());
        }
    } else if event["type"] == "block_event" && event["payload"]["name"] == "title_changed" {
        let block = &event["payload"];
        if let Some(pane) = cache.state.pane_ids().into_iter().find(|pane| {
            cache.state.pane(*pane).is_some_and(|entry| {
                entry.is_terminal() && Some(entry.remote_id.as_str()) == block["block_id"].as_str()
            })
        }) {
            if title_sequences
                .get(&pane)
                .is_some_and(|baseline| sequence <= *baseline)
            {
                return Ok(false);
            }
            return Ok(apply_terminal_title(
                cache,
                pane,
                block["payload"]["title"].as_str().unwrap_or(""),
            ));
        }
    }
    Ok(false)
}

fn apply_terminal_title(cache: &mut Cache, pane: PaneId, title: &str) -> bool {
    match cache.state.update_terminal_title(pane, title) {
        Ok(changed) => changed,
        Err(error) => {
            super::diagnostics::write(
                "ERROR",
                "client",
                &format!("Rex terminal title pane={pane}: {error:#}; ignoring title metadata"),
            );
            false
        }
    }
}

fn refresh_terminal_titles(
    control: &mut ControlConnection,
    cache: &mut Cache,
    fetched: &mut BTreeMap<PaneId, u64>,
    mailbox: &Mutex<Mailbox>,
) -> Result<bool> {
    let ids = cache.state.pane_ids();
    fetched.retain(|pane, _| ids.contains(pane));
    let pending: Vec<_> = ids
        .into_iter()
        .filter(|pane| {
            !fetched.contains_key(pane)
                && cache.state.pane(*pane).is_some_and(BlockView::is_terminal)
        })
        .take(2)
        .collect();
    let mut changed = false;
    for pane in pending {
        let Some(block) = cache.state.pane(pane) else {
            continue;
        };
        let result = control.call(
            "com.superlogical.terminal.title",
            json!({"session_id":block.session_id,"block_id":block.remote_id,"args":{}}),
        );
        fetched.insert(pane, control.last_response_seq());
        match result {
            Ok(value) => {
                changed |= apply_terminal_title(cache, pane, value["title"].as_str().unwrap_or(""));
            }
            Err(error) => {
                // Another client may close a short-lived block between its
                // view and this metadata request. Confirm removal before
                // suppressing the error; live blocks still report failures.
                if refresh_closed_pane(control, cache, pane)? && cache.state.pane(pane).is_none() {
                    changed = true;
                } else {
                    record_error(
                        mailbox,
                        format!("read Rex terminal title pane={pane}: {error:#}"),
                    );
                }
            }
        }
    }
    Ok(changed)
}

fn attach_sessions(
    control: &mut ControlConnection,
    cache: &mut Cache,
    attached: &mut BTreeSet<String>,
) -> Result<bool> {
    let mut changed = false;
    let ids: Vec<_> = cache
        .state
        .sessions()
        .iter()
        .map(|session| session.remote_id().to_owned())
        .collect();
    attached.retain(|id| ids.contains(id));
    let mut snapshots = Vec::new();
    for id in ids {
        if attached.insert(id.clone()) {
            let result = control.call("session.attach", json!({"session_id":id}))?;
            let list = control.call("session.list_blocks", json!({"session_id":id}))?;
            snapshots.push((result["view"].clone(), list));
        }
    }
    if !snapshots.is_empty() {
        changed |= cache.state.apply_snapshots(&snapshots)?;
    }
    Ok(changed)
}

fn perform_workspace(
    control: &mut ControlConnection,
    cache: &mut Cache,
    command: &Command,
) -> Result<bool> {
    if let Command::RestorePane { pane, title } = command {
        super::restore::execute(control, &mut cache.state, *pane, title)?;
        return Ok(true);
    }
    if let Command::SelectSession(id) = command {
        cache.state.select_session(*id)?;
        return Ok(true);
    }
    if let Some(call) = cache.state.command(command)? {
        let diagnostic = diagnostics::WorkspaceCall::new(call.method, &call.params);
        let result = control.call(call.method, call.params)?;
        diagnostic.applied(&result);
        let selected = match command {
            Command::CreateSession { .. } => {
                let remote = result["session_id"]
                    .as_str()
                    .context("Rex create response lacks session ID")?;
                let sessions = control.call("session.list", json!({}))?;
                cache.state.update_sessions(&sessions)?;
                let attached = control.call("session.attach", json!({"session_id":remote}))?;
                cache.state.apply_view(&attached["view"])?;
                cache
                    .state
                    .sessions()
                    .iter()
                    .find(|session| session.remote_id() == remote)
                    .map(SessionView::id)
            }
            Command::SelectWindow(window) => cache
                .state
                .sessions()
                .iter()
                .find(|session| session.windows().iter().any(|entry| entry.id() == *window))
                .map(SessionView::id),
            _ => None,
        };
        if let Some(session) = selected {
            cache.state.select_session(session)?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn accepts_mouse(tracking: MouseTracking, mouse: MouseInput) -> bool {
    match tracking {
        MouseTracking::None => false,
        MouseTracking::Press => mouse.action != MouseAction::Move,
        MouseTracking::Drag => {
            mouse.action != MouseAction::Move || mouse.button != MouseButton::None
        }
        MouseTracking::Any => true,
    }
}

fn perform_wheel(
    control: &mut ControlConnection,
    cache: &mut Cache,
    streams: &mut BTreeMap<PaneId, TerminalStream>,
    pane: PaneId,
    wheel: WheelInput,
) -> Result<bool> {
    let block = cache
        .state
        .pane(pane)
        .context("Rex block no longer exists")?;

    let stream = streams
        .get_mut(&pane)
        .context("Rex terminal data is not attached")?;
    if wheel.modifiers.shift
        || stream.exit_code().is_some()
        || (!stream.uses_mouse_reporting()? && !stream.alternate_screen()?)
    {
        if let Some(frame) = stream.scroll(wheel.lines)? {
            cache.frames.insert(pane, frame);
            return Ok(true);
        }
        return Ok(false);
    }
    ensure!(
        terminal_input_available(cache, pane) && stream.is_ready(),
        "Rex terminal data is unavailable; wheel input was not sent"
    );
    let mut params = json!({"session_id":block.session_id,"block_id":block.remote_id});
    params["event"] = json!({"type":"wheel","payload":{"x":wheel.column,"y":wheel.row,"delta_y":-f64::from(wheel.lines),"delta_x":0,"mods":modifier_names(wheel.modifiers)}});
    control.call("block.event", params)?;
    Ok(false)
}

fn perform_mouse(
    control: &mut ControlConnection,
    cache: &mut Cache,
    pane: PaneId,
    mouse: MouseInput,
    stream: &TerminalStream,
    mut params: Value,
) -> Result<bool> {
    let releasing_held = mouse.action == MouseAction::Release
        && cache
            .mouse_buttons
            .get(&pane)
            .is_some_and(|buttons| buttons.iter().any(|held| held.button == mouse.button));
    if (mouse.modifiers.shift && !releasing_held) || !accepts_mouse(stream.mouse_tracking()?, mouse)
    {
        return Ok(false);
    }
    let action = match mouse.action {
        MouseAction::Press => "press",
        MouseAction::Release => "release",
        MouseAction::Move => "motion",
    };
    let button = match mouse.button {
        MouseButton::None => "none",
        MouseButton::Left => "left",
        MouseButton::Middle => "middle",
        MouseButton::Right => "right",
    };
    params["event"] = json!({"type":"mouse","payload":{
        "action":action,"button":button,"x":mouse.column,"y":mouse.row,
        "mods":modifier_names(mouse.modifiers)
    }});
    control.call("block.event", params)?;
    let buttons = cache.mouse_buttons.entry(pane).or_default();
    match mouse.action {
        MouseAction::Press if mouse.button != MouseButton::None => {
            if !buttons.iter().any(|held| held.button == mouse.button) {
                buttons.push(mouse);
            }
        }
        MouseAction::Release => buttons.retain(|held| held.button != mouse.button),
        MouseAction::Move => {
            for held in buttons {
                held.column = mouse.column;
                held.row = mouse.row;
                held.modifiers = mouse.modifiers;
            }
        }
        MouseAction::Press => {}
    }
    Ok(false)
}

/// End acknowledged gestures even while their display stream is unavailable.
fn release_mouse_buttons(
    control: &mut ControlConnection,
    cache: &mut Cache,
    pane: PaneId,
    params: &Value,
) -> Result<()> {
    // Remove before sending: a failed transport must not replay an uncertain
    // cleanup release after reconnecting.
    for held in cache.mouse_buttons.remove(&pane).unwrap_or_default() {
        let button = match held.button {
            MouseButton::None => "none",
            MouseButton::Left => "left",
            MouseButton::Middle => "middle",
            MouseButton::Right => "right",
        };
        let modifiers = Modifiers {
            shift: false,
            ..held.modifiers
        };
        control.call(
            "block.event",
            json!({
                "session_id":params["session_id"],"block_id":params["block_id"],
                "event":{"type":"mouse","payload":{
                    "action":"release","button":button,"x":held.column,"y":held.row,
                    "mods":modifier_names(modifiers)
                }}
            }),
        )?;
    }
    Ok(())
}

/// Bootstrap/attachment confirms old block IDs before any cleanup is sent.
fn release_reconnected_buttons(control: &mut ControlConnection, cache: &mut Cache) -> Result<()> {
    let panes: Vec<_> = cache.mouse_buttons.keys().copied().collect();
    for pane in panes {
        if let Some(block) = cache.state.pane(pane) {
            let params = json!({"session_id":block.session_id,"block_id":block.remote_id});
            release_mouse_buttons(control, cache, pane, &params)?;
        } else {
            cache.mouse_buttons.remove(&pane);
        }
    }
    Ok(())
}

/// A fresh decoder ends gestures; an exact data resume does not.
fn retire_terminal_interaction(
    control: &mut ControlConnection,
    cache: &mut Cache,
    pane: PaneId,
) -> Result<()> {
    let result = if let Some(block) = cache.state.pane(pane) {
        let params = json!({"session_id":block.session_id,"block_id":block.remote_id});
        perform_focus(control, cache, pane, false, params).map(|_| ())
    } else {
        cache.mouse_buttons.remove(&pane);
        Ok(())
    };
    cache.ready.remove(&pane);
    cache.mouse_tracking.remove(&pane);
    result
}

fn perform_focus(
    control: &mut ControlConnection,
    cache: &mut Cache,
    pane: PaneId,
    focused: bool,
    mut params: Value,
) -> Result<bool> {
    if !focused {
        release_mouse_buttons(control, cache, pane, &params)?;
    }
    params["event"] = json!({"type":"focus","payload":{"action":if focused {"in"} else {"out"}}});
    control.call("block.event", params)?;
    Ok(false)
}

fn text_key_payload(text: String) -> Value {
    let key = KeyInput {
        key: if text.chars().count() == 1 {
            text.clone()
        } else {
            "unidentified".into()
        },
        text: Some(text),
        modifiers: crate::core::input::Modifiers::default(),
        repeat: false,
    };
    key_payload(&key)
}

fn prepare_terminal_operation(
    cache: &mut Cache,
    streams: &mut BTreeMap<PaneId, TerminalStream>,
    operation: &Operation,
) -> Result<(PaneId, bool)> {
    let pane = operation
        .pane()
        .context("unexpected Rex workspace operation")?;
    ensure!(
        cache
            .state
            .pane(pane)
            .context("Rex block no longer exists")?
            .is_terminal(),
        "this Rex block is not a terminal"
    );
    let mut viewport_changed = false;
    if matches!(operation, Operation::Mouse(_, mouse) if mouse.action == MouseAction::Press)
        && let Some(stream) = streams.get_mut(&pane)
    {
        stream.select(crate::core::interaction::SelectionCommand::Clear)?;
    }
    if matches!(
        operation,
        Operation::Key(..)
            | Operation::KeySequence(..)
            | Operation::Text(..)
            | Operation::Paste(..)
            | Operation::Mouse(..)
            | Operation::Focus(..)
    ) {
        ensure!(
            terminal_input_available(cache, pane)
                && streams.get(&pane).is_some_and(TerminalStream::is_ready),
            "Rex terminal data is unavailable or still loading; input was not sent"
        );
    }
    if matches!(
        operation,
        Operation::Key(..)
            | Operation::KeySequence(..)
            | Operation::Text(..)
            | Operation::Paste(..)
    ) && let Some(stream) = streams.get_mut(&pane)
    {
        stream.select(crate::core::interaction::SelectionCommand::Clear)?;
        if let Some(frame) = stream.scroll_to_bottom()? {
            cache.frames.insert(pane, frame);
            viewport_changed = true;
        }
    }
    Ok((pane, viewport_changed))
}

fn perform(
    control: &mut ControlConnection,
    cache: &mut Cache,
    streams: &mut BTreeMap<PaneId, TerminalStream>,
    operation: Operation,
) -> Result<bool> {
    let target = match &operation {
        Operation::Command(Command::FocusPane(pane)) => Some((*pane, true)),
        Operation::Command(Command::ClosePane(pane)) => Some((*pane, false)),
        _ => operation.pane().map(|pane| (pane, true)),
    };
    let retired = |state: &WorkspaceState, pane, display_only| {
        state.is_retired_pane(pane) || (display_only && state.is_detached(pane))
    };
    if target.is_some_and(|(pane, display)| retired(&cache.state, pane, display)) {
        return Ok(false);
    }
    match perform_active(control, cache, streams, operation) {
        Err(error) => {
            // Control events and UI notifications can race a close. Ignore a
            // failed tail only after authoritative membership confirms removal;
            // unknown targets and failures of still-live blocks remain errors.
            if let Some((pane, display)) = target
                && refresh_closed_pane(control, cache, pane).unwrap_or(false)
                && retired(&cache.state, pane, display)
            {
                Ok(true)
            } else {
                Err(error)
            }
        }
        result => result,
    }
}

fn perform_active(
    control: &mut ControlConnection,
    cache: &mut Cache,
    streams: &mut BTreeMap<PaneId, TerminalStream>,
    operation: Operation,
) -> Result<bool> {
    if let Operation::Wheel(pane, wheel) = operation {
        return perform_wheel(control, cache, streams, pane, wheel);
    }
    if let Operation::Command(command) = operation {
        return perform_workspace(control, cache, &command);
    }
    let (pane, viewport_changed) = prepare_terminal_operation(cache, streams, &operation)?;
    let block = cache
        .state
        .pane(pane)
        .context("Rex block no longer exists")?;
    let mut params = json!({"session_id":block.session_id,"block_id":block.remote_id});
    let method = match operation {
        Operation::Key(_, key) => {
            params["key"] = key_payload(&key);
            "client.key"
        }
        Operation::KeySequence(_, key, prefix) => {
            validate_sequence(&key, &prefix)?;
            params["key"] = key_payload(&key);
            if !prefix.is_empty() {
                params["prefix"] = json!(prefix);
            }
            "client.key"
        }
        Operation::Text(_, text) => {
            params["key"] = text_key_payload(text);
            "client.key"
        }
        Operation::Paste(_, text) => {
            params["event"] = json!({"type":"paste","payload":{"text":text}});
            "block.event"
        }
        Operation::Mouse(_, mouse) => {
            let stream = streams
                .get(&pane)
                .context("Rex terminal data is not attached")?;
            return perform_mouse(control, cache, pane, mouse, stream, params);
        }
        Operation::Focus(_, focused) => {
            return perform_focus(control, cache, pane, focused, params);
        }
        Operation::Resize(_, size, width, height) => {
            params["args"] = json!({"columns":size.columns(),"rows":size.rows(),"cell_width_px":width,"cell_height_px":height});
            "com.superlogical.terminal.resize"
        }
        Operation::Own(_, take) => {
            params = json!({"session_id":block.session_id,"block_ids":[block.remote_id]});
            if take {
                params["mode"] = json!("take");
                "session.own_block"
            } else {
                "session.disown_block"
            }
        }
        Operation::Wheel(..) => bail!("unexpected Rex wheel operation"),
        Operation::Scroll(_, lines) => {
            let stream = streams
                .get_mut(&pane)
                .context("Rex terminal data is not attached")?;
            if let Some(frame) = stream.scroll(lines)? {
                cache.frames.insert(pane, frame);
            }
            return Ok(true);
        }
        Operation::Command(_)
        | Operation::Local(..)
        | Operation::NativeAction(..)
        | Operation::ActionPaste(..)
        | Operation::SetScripts(..)
        | Operation::ServerAction(..)
        | Operation::ActionCommand(..)
        | Operation::RefreshInventory(..)
        | Operation::RefreshInsights(..) => {
            bail!("unexpected Rex workspace operation")
        }
    };
    control.call(method, params)?;
    Ok(viewport_changed)
}

fn visible_panes(state: &WorkspaceState) -> BTreeSet<PaneId> {
    state
        .active_session()
        .and_then(SessionView::active_window)
        .map(|window| {
            window.zoomed_pane().map_or_else(
                || {
                    window
                        .layers
                        .iter()
                        .flat_map(|layer| layer.blocks.iter().copied())
                        .collect()
                },
                |pane| BTreeSet::from([pane]),
            )
        })
        .unwrap_or_default()
}

fn service_streams(
    endpoint: &str,
    control: &mut ControlConnection,
    cache: &mut Cache,
    streams: &mut BTreeMap<PaneId, TerminalStream>,
    retry: &mut BTreeMap<PaneId, StreamRetry>,
    cursor: &mut usize,
    mailbox: &Mutex<Mailbox>,
) -> Result<bool> {
    let previous_counts = (cache.frames.len(), cache.ready.len(), cache.statuses.len());
    let ids = cache.state.placed_pane_ids();
    interaction::retain(cache, &ids);
    let retiring: Vec<_> = streams
        .keys()
        .copied()
        .filter(|pane| !ids.contains(pane))
        .collect();
    for pane in retiring {
        if cache.state.is_detached(pane) {
            if let Err(error) = retire_terminal_interaction(control, cache, pane) {
                diagnostics::write(
                    "WARN",
                    "terminal",
                    &format!("retire detached interaction pane={pane}: {error:#}"),
                );
            }
        } else {
            cache.mouse_buttons.remove(&pane);
        }
    }
    streams.retain(|pane, _| ids.contains(pane));
    retry.retain(|pane, _| ids.contains(pane));
    cache.frames.retain(|pane, _| ids.contains(pane));
    cache.statuses.retain(|pane, _| ids.contains(pane));
    cache.ready.retain(|pane| ids.contains(pane));
    cache.mouse_tracking.retain(|pane, _| ids.contains(pane));
    cache.mouse_buttons.retain(|pane, _| ids.contains(pane));
    let mut ids: Vec<_> = ids
        .into_iter()
        .filter(|id| cache.state.pane(*id).is_some_and(BlockView::is_terminal))
        .collect();
    let active = cache
        .state
        .active_session()
        .and_then(SessionView::active_pane);
    let visible = visible_panes(&cache.state);
    ids.sort_by_key(|pane| (Some(*pane) != active, !visible.contains(pane)));
    // Bound both concurrent decoders and initial snapshot allocations.
    let admitted: BTreeSet<_> = ids.iter().take(16).copied().collect();
    streams.retain(|pane, _| admitted.contains(pane));
    let old_frame_count = cache.frames.len();
    retain_admitted_presentations(cache, &admitted);
    let mut changed = old_frame_count != cache.frames.len()
        || previous_counts != (cache.frames.len(), cache.ready.len(), cache.statuses.len());
    for pane in ids.iter().skip(16) {
        if retry
            .get(pane)
            .copied()
            .is_some_and(StreamRetry::is_blocked)
        {
            continue;
        }
        let status = PaneStatus::Failed {
            message: "This client supports at most 16 terminal streams at once".into(),
        };
        if cache.statuses.get(pane) != Some(&status) {
            cache.statuses.insert(*pane, status);
            changed = true;
        }
    }
    // One handshake per tick, so a large workspace does not starve existing streams.
    if let Some(pane) = ids.iter().take(16).copied().find(|pane| {
        !streams.contains_key(pane)
            && retry
                .get(pane)
                .is_none_or(|retry| retry.ready(Instant::now()))
    }) {
        let result = open_terminal(endpoint, control, &cache.state, pane);
        match result {
            Ok(mut stream) => {
                interaction::restore(&mut stream, cache, pane)?;
                streams.insert(pane, stream);
                cache.statuses.insert(pane, PaneStatus::Running);
                changed = true;
            }
            Err(error) => {
                retry.insert(
                    pane,
                    StreamRetry::After(Instant::now() + Duration::from_secs(2)),
                );
                let message = format!("Rex terminal: {error:#}");
                cache.statuses.insert(
                    pane,
                    PaneStatus::Failed {
                        message: message.clone(),
                    },
                );
                record_error(mailbox, message);
                changed = true;
            }
        }
    }
    changed |= poll_streams(control, cache, streams, retry, cursor, mailbox, &visible)?;
    Ok(changed)
}

fn retain_admitted_presentations(cache: &mut Cache, admitted: &BTreeSet<PaneId>) {
    cache.ready.retain(|pane| admitted.contains(pane));
    cache
        .mouse_tracking
        .retain(|pane, _| admitted.contains(pane));
    cache
        .mouse_buttons
        .retain(|pane, _| admitted.contains(pane));
    cache.frames.retain(|pane, _| admitted.contains(pane));
    cache.annotations.retain(|pane, _| admitted.contains(pane));
}

fn open_terminal(
    endpoint: &str,
    control: &mut ControlConnection,
    state: &WorkspaceState,
    pane: PaneId,
) -> Result<TerminalStream> {
    let block = state.pane(pane).context("Rex terminal disappeared")?;
    let response = control.call(
        "data.connect",
        json!({"session_id":block.session_id,"block_id":block.remote_id,
        "data_ref_id":"terminal","subprotocol":"rex.data.terminal.v1"}),
    )?;
    let stream = TerminalStream::connect(
        endpoint,
        response["ticket"]
            .as_str()
            .context("Rex data ticket missing")?,
    )
    .with_context(|| format!("open Rex terminal data block={}", block.remote_id))?;
    diagnostics::write(
        "INFO",
        "terminal",
        &format!("data connected pane={pane} block={}", block.remote_id),
    );
    Ok(stream)
}

fn poll_streams(
    control: &mut ControlConnection,
    cache: &mut Cache,
    streams: &mut BTreeMap<PaneId, TerminalStream>,
    retry: &mut BTreeMap<PaneId, StreamRetry>,
    cursor: &mut usize,
    mailbox: &Mutex<Mailbox>,
    visible: &BTreeSet<PaneId>,
) -> Result<bool> {
    let mut changed = false;
    let previous_revision = cache.state_revision;
    let panes: Vec<_> = streams.keys().copied().collect();
    let start = Instant::now();
    let initial_cursor = *cursor;
    for offset in 0..panes.len() {
        let index = (initial_cursor + offset) % panes.len();
        let pane = panes[index];
        let stream = streams.get_mut(&pane).context("missing Rex data stream")?;
        let result = stream
            .poll_with_frame(Duration::ZERO, visible.contains(&pane))
            .map(|frame| (frame, stream.annotations()));
        match result {
            Ok((frame, annotations)) => {
                if cache
                    .annotations
                    .get(&pane)
                    .is_none_or(|old| !Arc::ptr_eq(old, &annotations))
                {
                    cache.annotations.insert(pane, annotations);
                    changed = true;
                }
                let ready = stream.is_ready();
                if ready {
                    changed |= cache.ready.insert(pane);
                } else {
                    changed |= cache.ready.remove(&pane);
                }
                let tracking = if ready {
                    stream.mouse_tracking()?
                } else {
                    MouseTracking::None
                };
                let was_tracking = cache
                    .mouse_tracking
                    .get(&pane)
                    .copied()
                    .unwrap_or(MouseTracking::None);
                if tracking == MouseTracking::None {
                    cache.mouse_tracking.remove(&pane);
                    cache.mouse_buttons.remove(&pane);
                } else {
                    cache.mouse_tracking.insert(pane, tracking);
                }
                changed |= tracking != was_tracking;
                if let Some(frame) = frame {
                    cache.frames.insert(pane, frame);
                    changed = true;
                }
                let status =
                    stream
                        .exit_code()
                        .map_or(PaneStatus::Running, |code| PaneStatus::Exited {
                            code: u32::try_from(code).unwrap_or(1),
                        });
                if cache.statuses.get(&pane) != Some(&status) {
                    if let PaneStatus::Exited { code } = &status {
                        diagnostics::write(
                            "INFO",
                            "terminal",
                            &format!("program exited pane={pane} code={code}"),
                        );
                    }
                    cache.statuses.insert(pane, status);
                    changed = true;
                }
            }
            Err(error) => {
                if recover_terminal_stream(control, cache, pane, stream, retry, mailbox, &error)? {
                    streams.remove(&pane);
                    cache.annotations.remove(&pane);
                    changed = true;
                }
            }
        }
        *cursor = index.wrapping_add(1);
        if start.elapsed() >= Duration::from_millis(4) {
            break;
        }
    }
    Ok(changed || cache.state_revision != previous_revision)
}

fn recover_terminal_stream(
    control: &mut ControlConnection,
    cache: &mut Cache,
    pane: PaneId,
    stream: &mut TerminalStream,
    retry: &mut BTreeMap<PaneId, StreamRetry>,
    mailbox: &Mutex<Mailbox>,
    error: &anyhow::Error,
) -> Result<bool> {
    if refresh_closed_pane(control, cache, pane)? {
        cache.ready.remove(&pane);
        cache.mouse_tracking.remove(&pane);
        cache.mouse_buttons.remove(&pane);
        retry.remove(&pane);
        return Ok(true);
    }
    if stream.is_decode_failed() {
        retire_terminal_interaction(control, cache, pane)?;
        block_terminal(cache, retry, pane, error, mailbox);
        return Ok(true);
    }
    if let Err(resume_error) = stream.resume() {
        retire_terminal_interaction(control, cache, pane)?;
        retry.insert(
            pane,
            StreamRetry::After(Instant::now() + Duration::from_secs(1)),
        );
        let message = format!(
            "Rex terminal pane={pane}: {error:#}; resume failed: {resume_error:#}; requesting fresh snapshot"
        );
        cache.statuses.insert(
            pane,
            PaneStatus::Failed {
                message: message.clone(),
            },
        );
        record_error(mailbox, message);
        return Ok(true);
    }
    // Exact byte-count resume keeps the same decoder, nonce and live gesture.
    // Publishing a synthetic readiness/mode transition would make the UI forget
    // a held button and suppress its matching release.
    diagnostics::write("INFO", "terminal", &format!("data resumed pane={pane}"));
    Ok(false)
}

/// Data sockets and control events arrive independently. Confirm membership
/// before resuming: closing a tab can close its data socket before its view event.
fn refresh_closed_pane(
    control: &mut ControlConnection,
    cache: &mut Cache,
    pane: PaneId,
) -> Result<bool> {
    let Some(block) = cache.state.pane(pane) else {
        return Ok(true);
    };
    let session = block.session_id.clone();
    let refreshed = match super::inventory::refresh(control, &mut cache.state, &session) {
        Ok(changed) => changed,
        Err(_) => match control.call("session.list", json!({})) {
            Ok(sessions) => cache.state.update_sessions(&sessions)?,
            Err(_) => false,
        },
    };
    if refreshed {
        cache.state_revision = cache.state_revision.wrapping_add(1);
    }
    if cache.state.placed_pane_ids().contains(&pane) {
        return Ok(false);
    }
    if cache.state.is_detached(pane)
        && let Err(error) = retire_terminal_interaction(control, cache, pane)
    {
        diagnostics::write(
            "WARN",
            "terminal",
            &format!("retire detached interaction pane={pane}: {error:#}"),
        );
    }
    cache.frames.remove(&pane);
    cache.statuses.remove(&pane);
    diagnostics::write(
        "INFO",
        "terminal",
        &format!("data closed for removed pane={pane}"),
    );
    Ok(true)
}

fn block_terminal(
    cache: &mut Cache,
    retry: &mut BTreeMap<PaneId, StreamRetry>,
    pane: PaneId,
    error: &anyhow::Error,
    mailbox: &Mutex<Mailbox>,
) {
    retry.insert(pane, StreamRetry::Blocked);
    let message = format!(
        "Rex terminal pane={pane} could not decode server state: {error:#}. Automatic retries stopped for this terminal. Update the client or server for snapshot compatibility, then reconnect the desktop to try again."
    );
    // Keep the last successfully displayed frame. Other streams remain live.
    cache.statuses.insert(
        pane,
        PaneStatus::Failed {
            message: message.clone(),
        },
    );
    record_error(mailbox, message);
}

fn terminal_input_available(cache: &Cache, pane: PaneId) -> bool {
    cache.connected
        && cache.state.pane(pane).is_some_and(BlockView::is_terminal)
        && !cache.state.is_detached(pane)
        && cache.ready.contains(&pane)
        && cache.frames.contains_key(&pane)
        && !matches!(
            cache.statuses.get(&pane),
            Some(PaneStatus::Failed { .. } | PaneStatus::Exited { .. })
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::input::Modifiers;

    #[test]
    fn script_sources_are_bounded_before_queue_admission() -> Result<()> {
        assert_eq!(scripts_params(&[])?["scripts"], json!([]));
        let script = ClientScript {
            name: "client.lua".into(),
            source: "rex.bind('ctrl+g', function() return true end)".into(),
        };
        assert_eq!(
            scripts_params(std::slice::from_ref(&script))?["scripts"][0]["name"],
            "client.lua"
        );
        assert!(scripts_params(&vec![script.clone(); MAX_SCRIPTS + 1]).is_err());
        assert!(
            scripts_params(&[ClientScript {
                name: "bad\nname".into(),
                ..script.clone()
            }])
            .is_err()
        );
        assert!(
            scripts_params(&[ClientScript {
                source: "x".repeat(MAX_SCRIPT_BYTES + 1),
                ..script.clone()
            }])
            .is_err()
        );
        // JSON escaping must respect the wire bound as well as source bytes.
        assert!(
            scripts_params(&[ClientScript {
                source: "\0".repeat(MAX_SCRIPT_BYTES),
                ..script
            }])
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn server_actions_preserve_explicit_context_and_optional_arguments() -> Result<()> {
        let context = ActionContext {
            session_id: Some("session:a".into()),
            block_id: Some("block:a".into()),
        };
        let params = server_action_params("custom_action", &Value::Null, &context)?;
        assert_eq!(params["session_id"], "session:a");
        assert_eq!(params["block_id"], "block:a");
        assert!(params.get("args").is_none());
        assert!(server_action_params("", &json!({}), &context).is_err());
        assert!(server_action_params("custom_action", &json!([]), &context).is_err());
        assert!(
            server_action_params(
                "custom_action",
                &json!({"text":"x".repeat(super::super::protocol::MAX_MESSAGE_BYTES)}),
                &context
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn configuration_operations_share_the_ordered_command_queue() -> Result<()> {
        let (commands, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let mut client = WorkspaceClient {
            cache: Cache {
                connected: true,
                connection_generation: 7,
                ..Cache::default()
            },
            mailbox: Arc::new(Mutex::new(Mailbox::default())),
            commands,
            stopped: Arc::new(AtomicBool::new(false)),
        };
        let _scripts = client.set_scripts(vec![])?;
        let _action = client.perform_server_action(
            "custom_action".into(),
            Value::Null,
            ActionContext::default(),
        )?;
        let _command = client.perform_command(Command::CreateSession {
            title: "Ordered".into(),
        })?;
        let first = receiver.try_recv()?;
        let second = receiver.try_recv()?;
        let third = receiver.try_recv()?;
        assert_eq!(first.generation, 7);
        assert_eq!(second.generation, 7);
        assert!(matches!(first.operation, Operation::SetScripts(..)));
        assert!(matches!(second.operation, Operation::ServerAction(..)));
        assert!(matches!(third.operation, Operation::ActionCommand(..)));
        assert!(
            client
                .perform_command(Command::CreateSession {
                    title: "x".repeat(129)
                })
                .is_err()
        );
        assert!(receiver.try_recv().is_err());
        Ok(())
    }

    #[test]
    fn key_sequences_and_busy_callback_classification_are_explicit() -> Result<()> {
        let key = KeyInput {
            key: "x".into(),
            text: Some("x".into()),
            modifiers: Modifiers::default(),
            repeat: false,
        };
        validate_sequence(&key, &["ctrl+b".into()])?;
        assert!(validate_sequence(&key, &[String::new()]).is_err());
        assert!(validate_sequence(&key, &vec!["x".into(); 17]).is_err());
        assert!(action_needs_worker(
            "pane.split",
            &json!({"direction":"right"})
        ));
        assert!(action_needs_worker("client.paste", &Value::Null));
        assert!(!action_needs_worker("client.font.increase", &Value::Null));
        assert!(action_needs_worker("client.copy", &Value::Null));
        assert!(!action_needs_worker("unimplemented-action", &Value::Null));
        Ok(())
    }

    #[test]
    fn deferred_focus_follows_accepted_input_and_coalesces_latest_state() -> Result<()> {
        let pane = PaneId::from_value(1).context("pane")?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let operation = |operation| QueuedOperation {
            instance: "test".into(),
            generation: 1,
            operation,
        };
        sender
            .try_send(operation(Operation::Text(pane, "before".into())))
            .map_err(|_| anyhow::anyhow!("fill queue"))?;
        let mut mailbox = Mailbox::default();
        defer_focus(&mut mailbox, operation(Operation::Focus(pane, true)))?;
        defer_focus(&mut mailbox, operation(Operation::Focus(pane, false)))?;
        let mailbox = Mutex::new(mailbox);
        assert!(
            matches!(next_operation(&receiver, &mailbox).map(|q| q.operation),
            Some(Operation::Text(_, text)) if text == "before")
        );
        assert!(
            matches!(next_operation(&receiver, &mailbox).map(|q| q.operation),
            Some(Operation::Focus(id, false)) if id == pane)
        );
        assert!(next_operation(&receiver, &mailbox).is_none());
        Ok(())
    }

    #[test]
    fn unsupported_title_metadata_does_not_disconnect_client() -> Result<()> {
        for (creator, title) in [
            ("custom.creator", "Not a terminal".to_owned()),
            ("com.superlogical.terminal", "x".repeat(16_385)),
        ] {
            let mut cache = Cache {
                connected: true,
                ..Cache::default()
            };
            cache.state.apply_view(&json!({
                "session_id":"session:a","label":"Work","revision":1,
                "active_window_id":"window:a","windows":[{
                    "window_id":"window:a","label":"Shell","focused_block_id":"block:a",
                    "layers":[{
                        "layer_id":"layer:a","kind":"tiled","bounds":[0,0,1,1],
                        "layout":{"block_id":"block:a","rect":[0,0,1,1]},
                        "blocks":[{"block_id":"block:a","creator_name":creator}]
                    }]
                }]
            }))?;
            let mut refresh = BTreeSet::new();
            assert!(!apply_session_event(
                &mut cache,
                &json!({"session_id":"session:a","event":{
                    "type":"block_event","payload":{
                        "block_id":"block:a","name":"title_changed","payload":{"title":title}
                    }
                }}),
                &mut refresh,
                1,
                &BTreeMap::new(),
            )?);
            assert!(cache.connected);
            assert!(refresh.is_empty());
            let pane = cache.state.pane_ids()[0];
            assert_eq!(cache.state.pane(pane).context("pane")?.terminal_title, "");
        }
        Ok(())
    }

    #[test]
    fn keys_preserve_lua_modifiers_and_unicode() {
        let value = key_payload(&KeyInput {
            key: "n".into(),
            text: None,
            modifiers: Modifiers {
                control: true,
                ..Modifiers::default()
            },
            repeat: false,
        });
        assert_eq!(value["key"], "key_n");
        assert_eq!(value["mods"], "ctrl");
        assert_eq!(value["unshifted_codepoint"], 110);
        let value = key_payload(&KeyInput {
            key: "unidentified".into(),
            text: Some("日本語".into()),
            modifiers: Modifiers::default(),
            repeat: false,
        });
        assert_eq!(value["text"], "日本語");
        assert_eq!(value["key"], "unidentified");
    }

    #[test]
    fn recoverable_failure_retries_at_its_deadline() -> Result<()> {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(2);
        let retry = StreamRetry::After(deadline);
        assert!(!retry.ready(now));
        assert!(
            !retry.ready(
                deadline
                    .checked_sub(Duration::from_nanos(1))
                    .context("time before deadline")?
            )
        );
        assert!(retry.ready(deadline));
        assert!(!retry.is_blocked());
        Ok(())
    }

    #[test]
    fn exited_or_removed_terminal_keeps_frame_but_rejects_process_input() -> Result<()> {
        use crate::core::frame::{Cell, Rgb};

        let state = terminal_state()?;
        let pane = state.pane_ids()[0];
        let mut cache = Cache {
            connected: true,
            state,
            ..Cache::default()
        };
        cache.frames.insert(
            pane,
            Arc::new(TerminalFrame {
                size: GridSize::new(1, 1)?,
                rows: vec![Arc::new(vec![Cell::default()])],
                cursor: None,
                foreground: Rgb::default(),
                background: Rgb::default(),
            }),
        );
        cache.statuses.insert(pane, PaneStatus::Running);
        assert!(!terminal_input_available(&cache, pane));
        cache.ready.insert(pane);
        assert!(terminal_input_available(&cache, pane));
        // A reconnect may retain the old screen while its new stream is loading.
        cache.ready.clear();
        assert!(!terminal_input_available(&cache, pane));
        cache.ready.insert(pane);
        cache.statuses.insert(pane, PaneStatus::Exited { code: 0 });
        assert!(!terminal_input_available(&cache, pane));
        assert!(cache.frames.contains_key(&pane));
        cache.statuses.insert(pane, PaneStatus::Running);
        cache.state.apply_snapshots(&[(
            json!({"session_id":"session:a","label":"Work","revision":2,"windows":[]}),
            json!({"blocks":[]}),
        )])?;
        assert!(cache.state.is_retired_pane(pane));
        assert!(!terminal_input_available(&cache, pane));
        assert!(cache.frames.contains_key(&pane));
        Ok(())
    }

    fn terminal_state() -> Result<WorkspaceState> {
        let mut state = WorkspaceState::default();
        state.apply_view(&json!({
            "session_id":"session:a","label":"Work","revision":1,
            "active_window_id":"window:a","windows":[{
                "window_id":"window:a","label":"Shell","focused_block_id":"block:a",
                "layers":[{
                    "layer_id":"layer:a","kind":"tiled","bounds":[0,0,1,1],
                    "layout":{"rect":[0,0,1,1],"split":{
                        "split_id":"split:a","direction":"horizontal","ratio":0.5,
                        "before":{"block_id":"block:a","rect":[0,0,0.5,1]},
                        "after":{"block_id":"block:b","rect":[0.5,0,0.5,1]}
                    }},
                    "blocks":[
                        {"block_id":"block:a","creator_name":"com.superlogical.terminal"},
                        {"block_id":"block:b","creator_name":"com.superlogical.terminal"}
                    ]
                }]
            }]
        }))?;
        Ok(state)
    }

    #[test]
    fn decode_failure_preserves_last_frame_and_does_not_block_other_panes() -> Result<()> {
        use crate::core::frame::{Cell, Rgb};

        let state = terminal_state()?;
        let panes = state.pane_ids();
        let failed = panes[0];
        let healthy = panes[1];
        let frame = Arc::new(TerminalFrame {
            size: GridSize::new(1, 1)?,
            rows: vec![Arc::new(vec![Cell::default()])],
            cursor: None,
            foreground: Rgb::default(),
            background: Rgb::default(),
        });
        let mut cache = Cache {
            connected: true,
            state,
            ..Cache::default()
        };
        cache.frames.insert(failed, Arc::clone(&frame));
        cache.frames.insert(healthy, Arc::clone(&frame));
        cache.statuses.insert(healthy, PaneStatus::Running);
        cache.ready.extend([failed, healthy]);
        assert!(terminal_input_available(&cache, failed));
        assert!(terminal_input_available(&cache, healthy));
        let mailbox = Mutex::new(Mailbox::default());
        let mut retries = BTreeMap::new();
        block_terminal(
            &mut cache,
            &mut retries,
            failed,
            &anyhow::anyhow!("unsupported snapshot state"),
            &mailbox,
        );

        assert!(Arc::ptr_eq(
            cache.frames.get(&failed).context("retained frame")?,
            &frame
        ));
        assert!(
            matches!(cache.statuses.get(&failed), Some(PaneStatus::Failed { message }) if message.contains("Automatic retries stopped") && message.contains("reconnect"))
        );
        assert_eq!(cache.statuses.get(&healthy), Some(&PaneStatus::Running));
        assert!(!terminal_input_available(&cache, failed));
        assert!(terminal_input_available(&cache, healthy));
        let later = Instant::now() + Duration::from_secs(24 * 60 * 60);
        assert!(!retries.get(&failed).context("blocked retry")?.ready(later));
        assert!(retries.get(&healthy).is_none_or(|retry| retry.ready(later)));
        assert_eq!(
            mailbox
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .errors
                .len(),
            1
        );
        Ok(())
    }
}
