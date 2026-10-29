//! Persistent ordered Rex control transport over a local Unix socket.
use super::protocol::{self, MAX_MESSAGE_BYTES, MAX_PENDING_EVENT_BYTES, MAX_PENDING_EVENTS};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    io::ErrorKind,
    os::unix::net::UnixStream,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex, PoisonError,
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    task::{Context as TaskContext, Poll, Waker},
    thread,
    time::{Duration, Instant},
};
use tungstenite::{
    Message, WebSocket,
    client::{IntoClientRequest, client_with_config},
    http::HeaderValue,
    protocol::WebSocketConfig,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const ACTION_TIMEOUT: Duration = Duration::from_secs(32);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);

/// Resolve the local endpoint without launching a server or invoking a CLI.
///
/// # Errors
/// Rejects a relative runtime directory or missing endpoint environment.
pub fn default_endpoint() -> Result<String> {
    if let Ok(endpoint) = std::env::var("REX_RS_ENDPOINT")
        .or_else(|_| std::env::var("REX_RS_REX_ENDPOINT"))
        .or_else(|_| std::env::var("REX_SERVER"))
    {
        ensure!(
            endpoint.starts_with("unix:///"),
            "Rex endpoint must be an absolute unix:// endpoint"
        );
        return Ok(endpoint);
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(
        || PathBuf::from(format!("/run/user/{}", nix::unistd::geteuid())),
        PathBuf::from,
    );
    ensure!(runtime.is_absolute(), "XDG_RUNTIME_DIR must be absolute");
    Ok(format!(
        "unix://{}",
        runtime.join("rex/server.sock").display()
    ))
}

/// Open an uncompressed WebSocket using Unix peer credentials.
///
/// # Errors
/// Reports invalid endpoints, unavailable servers and invalid upgrade replies.
pub fn open_websocket(
    endpoint: &str,
    path: &str,
    subprotocol: &str,
) -> Result<WebSocket<UnixStream>> {
    let socket_path = endpoint
        .strip_prefix("unix://")
        .context("only local unix:// Rex endpoints are supported")?;
    ensure!(
        std::path::Path::new(socket_path).is_absolute(),
        "Rex socket path must be absolute"
    );
    ensure!(
        path.starts_with('/') && !path.contains(['\r', '\n']),
        "invalid Rex WebSocket path"
    );
    let stream = UnixStream::connect(socket_path)
        .with_context(|| format!("connect to Rex at {endpoint}"))?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let mut request = format!("ws://rex{path}").into_client_request()?;
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_str(subprotocol)?,
    );
    let config = WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_MESSAGE_BYTES + 4096)
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let (socket, response) = client_with_config(request, stream, Some(config))
        .map_err(|error| anyhow::anyhow!("Rex WebSocket handshake: {error}"))?;
    ensure!(
        response
            .headers()
            .get("Sec-WebSocket-Protocol")
            .and_then(|value| value.to_str().ok())
            == Some(subprotocol),
        "Rex did not negotiate {subprotocol}"
    );
    Ok(socket)
}

const REQUEST_CAPACITY: usize = 64;
const REPLY_CAPACITY: usize = 8;
const CALLBACK_CAPACITY: usize = 64;
const IO_POLL_INTERVAL: Duration = Duration::from_millis(1);

type CallbackHandler = Arc<dyn Fn(Value) + Send + Sync>;

struct Reply {
    sequence: u64,
    payload: Result<Value>,
}

#[derive(Default)]
struct PendingState {
    reply: Option<Result<Reply>>,
    waiter: Option<Waker>,
}

#[derive(Default)]
struct PendingInner {
    state: Mutex<PendingState>,
    ready: Condvar,
}

impl PendingInner {
    fn finish(&self, reply: Result<Reply>) {
        let waiter = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.reply = Some(reply);
            state.waiter.take()
        };
        self.ready.notify_all();
        if let Some(waiter) = waiter {
            waiter.wake();
        }
    }
}

/// One correlated request. Dropping it never cancels or retries delivery.
pub struct PendingCall {
    inner: Arc<PendingInner>,
}

impl PendingCall {
    /// Wait outside the UI thread for this request's correlated response.
    ///
    /// # Errors
    /// Reports API failures, delivery uncertainty, timeout or disconnection.
    pub fn wait(self) -> Result<Value> {
        self.wait_reply()?.payload
    }

    fn wait_reply(self) -> Result<Reply> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(reply) = state.reply.take() {
                return reply;
            }
            state = self
                .inner
                .ready
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl Future for PendingCall {
    type Output = Result<Value>;

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(reply) = state.reply.take() {
            Poll::Ready(reply.and_then(|reply| reply.payload))
        } else {
            state.waiter = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

struct PendingEntry {
    inner: Arc<PendingInner>,
    deadline: Instant,
    is_reply: bool,
}

#[derive(Clone, Copy)]
enum Closing {
    Graceful,
    Resume,
}

#[derive(Default)]
struct ConnectionState {
    next_id: u64,
    pending: BTreeMap<u64, PendingEntry>,
    events: VecDeque<(Value, usize)>,
    event_bytes: usize,
    callback: Option<CallbackHandler>,
    failure: Option<String>,
    closing: Option<Closing>,
}

struct Shared {
    state: Mutex<ConnectionState>,
    event_ready: Condvar,
    message_limit: usize,
}

impl Shared {
    fn fail(&self, reason: &str) {
        let pending = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.failure.is_none() {
                state.failure = Some(reason.to_owned());
            }
            state.callback = None;
            std::mem::take(&mut state.pending)
        };
        self.event_ready.notify_all();
        for (_, pending) in pending {
            pending.inner.finish(Err(anyhow::anyhow!(
                "Rex control connection must be re-established: {reason}; request delivery uncertain; request will not be replayed"
            )));
        }
    }
}

struct WireRequest {
    id: u64,
    text: String,
}

/// A request handle tied to exactly one control connection, never auto-reconnected.
#[derive(Clone)]
pub struct ControlHandle {
    shared: Arc<Shared>,
    requests: SyncSender<WireRequest>,
    replies: SyncSender<WireRequest>,
}

impl ControlHandle {
    /// Queue a request without waiting for its response. Ordinary work has 64
    /// slots; client.reply has eight reserved slots and a priority delivery lane.
    ///
    /// # Errors
    /// Rejects disconnected handles, invalid/oversized requests or a full queue.
    pub fn request(&self, method: &str, params: Value) -> Result<PendingCall> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        ensure!(
            state.failure.is_none() && state.closing.is_none(),
            "Rex control connection must be re-established"
        );
        let is_reply = method == "client.reply";
        let capacity = if is_reply {
            REPLY_CAPACITY
        } else {
            REQUEST_CAPACITY
        };
        let kind = if is_reply {
            "callback reply"
        } else {
            "request"
        };
        ensure!(
            state
                .pending
                .values()
                .filter(|entry| entry.is_reply == is_reply)
                .count()
                < capacity,
            "Rex control {kind} queue is full; request was not sent"
        );
        let id = state.next_id;
        let text = protocol::request(id, method, params)?.to_string();
        ensure!(
            text.len() <= self.shared.message_limit,
            "Rex request exceeds negotiated message limit"
        );
        state.next_id = id.checked_add(1).context("Rex request ID exhausted")?;
        let inner = Arc::new(PendingInner::default());
        state.pending.insert(
            id,
            PendingEntry {
                inner: Arc::clone(&inner),
                deadline: Instant::now() + request_timeout(method),
                is_reply,
            },
        );
        let queue = if is_reply {
            &self.replies
        } else {
            &self.requests
        };
        match queue.try_send(WireRequest { id, text }) {
            Ok(()) => Ok(PendingCall { inner }),
            Err(error) => {
                state.pending.remove(&id);
                match error {
                    TrySendError::Full(_) => {
                        bail!("Rex control {kind} queue is full; request was not sent")
                    }
                    TrySendError::Disconnected(_) => {
                        bail!("Rex control connection must be re-established")
                    }
                }
            }
        }
    }
}

fn request_timeout(method: &str) -> Duration {
    if method == "client.action" {
        ACTION_TIMEOUT
    } else {
        REQUEST_TIMEOUT
    }
}

/// An ordered API facade whose dedicated IO thread can service nested requests.
pub struct ControlConnection {
    welcome: Value,
    handle: ControlHandle,
    interrupt: UnixStream,
    last_response_seq: u64,
}

impl ControlConnection {
    /// Connect and exchange hello/welcome. Bootstrap must be the first API call.
    ///
    /// # Errors
    /// Reports connection, handshake and incompatible API errors.
    pub fn connect(endpoint: &str) -> Result<Self> {
        Self::connect_with_resume(endpoint, None)
    }

    /// Connect with the latest private token. Tokens rotate at each welcome.
    ///
    /// # Errors
    /// Reports connection, handshake and incompatible API errors.
    pub fn connect_with_resume(endpoint: &str, token: Option<&str>) -> Result<Self> {
        let mut socket = open_websocket(endpoint, "/api/control", "rex.control.v1")?;
        let mut hello = protocol::hello();
        if let Some(token) = token {
            hello["reconnect_token"] = Value::String(token.to_owned());
        }
        let hello = hello.to_string();
        ensure!(hello.len() < 512, "Rex hello exceeds protocol limit");
        socket.send(Message::Text(hello.into()))?;
        let welcome =
            read_value(&mut socket, HANDSHAKE_TIMEOUT)?.context("Rex welcome timed out")?;
        ensure!(
            welcome["type"] == "welcome" && welcome["protocol"] == "rex.control.v1",
            "Rex rejected control handshake"
        );
        ensure!(
            welcome["client"]["id"].is_string(),
            "Rex welcome lacks client identity"
        );
        ensure!(
            welcome["api_revision"].as_u64() == Some(protocol::API_REVISION),
            "Rex API revision negotiation failed"
        );
        let limit = welcome["limits"]["max_message_bytes"]
            .as_u64()
            .unwrap_or(MAX_MESSAGE_BYTES as u64);
        ensure!(limit > 0, "Rex advertised an invalid message limit");
        let limit = usize::try_from(limit)
            .unwrap_or(MAX_MESSAGE_BYTES)
            .min(MAX_MESSAGE_BYTES);
        socket.set_config(|config| {
            config.max_message_size = Some(limit);
            config.max_frame_size = Some(limit);
        });
        let interrupt = socket.get_ref().try_clone()?;
        let handle = start_io(socket, limit)?;
        Ok(Self {
            welcome,
            handle,
            interrupt,
            last_response_seq: 0,
        })
    }

    #[must_use]
    pub const fn welcome(&self) -> &Value {
        &self.welcome
    }

    #[must_use]
    pub fn client_id(&self) -> &str {
        self.welcome["client"]["id"].as_str().unwrap_or("")
    }

    /// Private rotating token. Callers must never log it.
    #[must_use]
    pub fn reconnect_token(&self) -> Option<&str> {
        self.welcome["client"]["reconnect_token"].as_str()
    }

    #[must_use]
    pub const fn last_response_seq(&self) -> u64 {
        self.last_response_seq
    }

    #[must_use]
    pub fn handle(&self) -> ControlHandle {
        self.handle.clone()
    }

    /// Install a callback dispatcher. It runs outside the socket owner thread;
    /// enqueue UI work promptly. Native callback envelopes also remain in events.
    pub fn set_callback_handler(&mut self, handler: CallbackHandler) {
        self.handle
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .callback = Some(handler);
    }

    /// Interrupt without a close frame; no queued requests are replayed.
    ///
    /// # Errors
    /// Reports operating-system shutdown errors.
    pub fn disconnect_for_resume(&mut self) -> Result<()> {
        self.handle
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closing = Some(Closing::Resume);
        self.handle
            .shared
            .fail("Rex connection interrupted for resume");
        self.interrupt
            .shutdown(std::net::Shutdown::Both)
            .context("interrupt Rex connection for resume")
    }

    /// Blocking convenience over the same correlated asynchronous request path.
    ///
    /// # Errors
    /// Reports API failures, timeout or uncertain delivery; never retries requests.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let reply = self
            .handle
            .request(method, params)?
            .wait_reply()
            .with_context(|| format!("Rex method {method}"))?;
        self.last_response_seq = reply.sequence;
        reply
            .payload
            .with_context(|| format!("Rex method {method}"))
    }

    /// Drain buffered original event envelopes, preserving their sequence numbers.
    pub fn drain_events(&mut self) -> Vec<Value> {
        let mut state = self
            .handle
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.event_bytes = 0;
        state.events.drain(..).map(|(value, _)| value).collect()
    }

    /// Wait for an event without preventing independent response/callback work.
    ///
    /// # Errors
    /// Reports malformed messages, event overflow or connection loss.
    pub fn poll_event(&mut self, timeout: Duration) -> Result<Option<Value>> {
        let shared = &self.handle.shared;
        let state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut state, _) = shared
            .event_ready
            .wait_timeout_while(state, timeout, |state| {
                state.events.is_empty() && state.failure.is_none() && state.closing.is_none()
            })
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((value, bytes)) = state.events.pop_front() {
            state.event_bytes = state.event_bytes.saturating_sub(bytes);
            return Ok(Some(value));
        }
        ensure!(
            state.failure.is_none() && state.closing.is_none(),
            "Rex control connection must be re-established: {}",
            state.failure.as_deref().unwrap_or("closed")
        );
        Ok(None)
    }
}

impl Drop for ControlConnection {
    fn drop(&mut self) {
        let mut state = self
            .handle
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if state.closing.is_none() {
            state.closing = Some(Closing::Graceful);
        }
    }
}

struct CallbackJob {
    handler: CallbackHandler,
    event: Value,
}

fn start_io(socket: WebSocket<UnixStream>, message_limit: usize) -> Result<ControlHandle> {
    let shared = Arc::new(Shared {
        state: Mutex::new(ConnectionState {
            next_id: 1,
            ..ConnectionState::default()
        }),
        event_ready: Condvar::new(),
        message_limit,
    });
    let (requests, receiver) = mpsc::sync_channel(REQUEST_CAPACITY);
    let (replies, reply_receiver) = mpsc::sync_channel(REPLY_CAPACITY);
    let handle = ControlHandle {
        shared: Arc::clone(&shared),
        requests,
        replies,
    };
    let (callbacks, callback_receiver) = mpsc::sync_channel::<CallbackJob>(CALLBACK_CAPACITY);
    thread::Builder::new()
        .name("rex-callbacks".into())
        .spawn(move || {
            while let Ok(job) = callback_receiver.recv() {
                (job.handler)(job.event);
            }
        })
        .context("start Rex callback dispatcher")?;
    let io_handle = handle.clone();
    thread::Builder::new()
        .name("rex-control-io".into())
        .spawn(move || {
            let _guard = IoGuard(Arc::clone(&shared));
            if let Err(error) = io_loop(socket, &receiver, &reply_receiver, &io_handle, &callbacks)
            {
                shared.fail(&format!("{error:#}"));
            }
        })
        .context("start Rex control IO")?;
    Ok(handle)
}

struct IoGuard(Arc<Shared>);
impl Drop for IoGuard {
    fn drop(&mut self) {
        self.0.fail("Rex control IO stopped");
    }
}

fn io_loop(
    mut socket: WebSocket<UnixStream>,
    requests: &Receiver<WireRequest>,
    replies: &Receiver<WireRequest>,
    handle: &ControlHandle,
    callbacks: &SyncSender<CallbackJob>,
) -> Result<()> {
    socket.get_mut().set_write_timeout(Some(REQUEST_TIMEOUT))?;
    loop {
        let closing = {
            let state = handle
                .shared
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            ensure!(state.failure.is_none(), "Rex control connection failed");
            ensure!(
                !state
                    .pending
                    .values()
                    .any(|entry| Instant::now() >= entry.deadline),
                "Rex response timed out; request delivery uncertain"
            );
            state.closing
        };
        if let Some(closing) = closing {
            if matches!(closing, Closing::Graceful) {
                socket
                    .get_mut()
                    .set_write_timeout(Some(Duration::from_millis(1)))?;
                let _ = socket.close(None);
            }
            return Ok(());
        }
        for _ in 0..16 {
            // Replies unblock server-side calls and must retain an independent
            // admission lane even when ordinary requests fill their own queue.
            let Ok(request) = replies.try_recv().or_else(|_| requests.try_recv()) else {
                break;
            };
            let remaining = {
                let state = handle
                    .shared
                    .state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                ensure!(
                    state.closing.is_none() && state.failure.is_none(),
                    "Rex control connection closed"
                );
                state
                    .pending
                    .values()
                    .map(|entry| entry.deadline)
                    .min()
                    .context("Rex request lost its pending response")?
                    .saturating_duration_since(Instant::now())
            };
            ensure!(
                !remaining.is_zero(),
                "Rex response timed out; request delivery uncertain"
            );
            socket.get_mut().set_write_timeout(Some(remaining))?;
            socket
                .send(Message::Text(request.text.into()))
                .with_context(|| {
                    format!(
                        "Rex request {} delivery uncertain; request will not be replayed",
                        request.id
                    )
                })?;
        }
        if let Some(value) = read_value(&mut socket, IO_POLL_INTERVAL)? {
            dispatch_value(handle, callbacks, value)?;
        }
    }
}

fn dispatch_value(
    handle: &ControlHandle,
    callbacks: &SyncSender<CallbackJob>,
    value: Value,
) -> Result<()> {
    match value["type"].as_str() {
        Some("response") => {
            let id_text = value["id"].as_str().context("Rex response lacks ID")?;
            let id = id_text
                .strip_prefix("rex-rs-")
                .context("unexpected Rex response ID")?
                .parse::<u64>()?;
            let sequence = value["seq"]
                .as_u64()
                .context("Rex response lacks sequence")?;
            let entry = handle
                .shared
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pending
                .remove(&id)
                .context("unexpected Rex response ID")?;
            let payload = protocol::response(&value, id_text).map(|(_, value)| value);
            entry.inner.finish(Ok(Reply { sequence, payload }));
        }
        Some("event") => dispatch_event(handle, callbacks, value)?,
        _ => bail!("unexpected Rex control message"),
    }
    Ok(())
}

fn dispatch_event(
    handle: &ControlHandle,
    callbacks: &SyncSender<CallbackJob>,
    event: Value,
) -> Result<()> {
    let bytes = serde_json::to_vec(&event)?.len();
    let handler = {
        let mut state = handle
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        ensure!(
            state.events.len() < MAX_PENDING_EVENTS
                && state.event_bytes.saturating_add(bytes) <= MAX_PENDING_EVENT_BYTES,
            "Rex event queue exceeded its limit; reconnect and bootstrap"
        );
        state.events.push_back((event.clone(), bytes));
        state.event_bytes += bytes;
        state.callback.clone()
    };
    handle.shared.event_ready.notify_all();
    let payload = &event["payload"];
    let kind = payload["type"].as_str();
    if !matches!(
        kind,
        Some("client_perform_requested" | "client_call_requested")
    ) {
        return Ok(());
    }
    let call_id = payload["payload"]["call_id"]
        .as_str()
        .context("Rex callback lacks call_id")?;
    if kind == Some("client_perform_requested")
        && let Some(handler) = handler
    {
        callbacks
            .try_send(CallbackJob { handler, event })
            .map_err(|_| anyhow::anyhow!("Rex callback queue exceeded its limit"))?;
    } else {
        // Cross-server calls remain unsupported; absent native handlers retain
        // the old prompt rejection. Their acknowledgements use normal IDs.
        let _reply = handle.request("client.reply", serde_json::json!({
            "call_id":call_id,
            "error":{"code":"not_supported","message":"This client does not provide this remote client action"}
        }))?;
    }
    Ok(())
}

fn read_value(socket: &mut WebSocket<UnixStream>, timeout: Duration) -> Result<Option<Value>> {
    if timeout.is_zero() {
        return Ok(None);
    }
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        socket.get_mut().set_read_timeout(Some(remaining))?;
        match socket.read() {
            Ok(Message::Text(text)) => {
                let value: Value =
                    serde_json::from_str(&text).context("decode Rex control message")?;
                ensure!(value.is_object(), "Rex control envelope must be an object");
                let kind = value["type"]
                    .as_str()
                    .context("Rex control message lacks type")?;
                if matches!(kind, "welcome" | "response" | "event") {
                    return Ok(Some(value));
                }
            }
            Ok(Message::Ping(_) | Message::Pong(_)) => socket.flush()?,
            Ok(Message::Close(_)) => bail!("Rex closed the control connection"),
            Ok(_) => bail!("Rex control connection sent a non-JSON message"),
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error).context("read Rex control connection"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, net::UnixListener},
        path::Path,
        thread,
        time::SystemTime,
    };
    use tungstenite::{
        accept_hdr,
        handshake::server::{Request, Response},
    };

    #[test]
    fn server_actions_allow_the_documented_lua_window_without_lengthening_keys() {
        assert_eq!(request_timeout("client.action"), Duration::from_secs(32));
        assert_eq!(request_timeout("client.key"), Duration::from_secs(8));
        assert_eq!(request_timeout("client.reply"), Duration::from_secs(8));
        assert_eq!(request_timeout("client.set_script"), Duration::from_secs(8));
    }

    fn mock_connection(
        peer: impl FnOnce(WebSocket<UnixStream>) -> Result<()> + Send + 'static,
    ) -> Result<(ControlConnection, thread::JoinHandle<Result<()>>, PathBuf)> {
        let suffix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("rex-rs-multiplex-{}-{suffix}", std::process::id()));
        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let path = directory.join("server.sock");
        let listener = UnixListener::bind(&path)?;
        let worker = thread::spawn(move || {
            let (stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(Duration::from_secs(3)))?;
            #[expect(
                clippy::result_large_err,
                reason = "Tungstenite's handshake callback owns its HTTP response."
            )]
            let mut socket = accept_hdr(stream, |_: &Request, mut response: Response| {
                response.headers_mut().insert(
                    "Sec-WebSocket-Protocol",
                    HeaderValue::from_static("rex.control.v1"),
                );
                Ok(response)
            })?;
            let hello = socket.read()?;
            ensure!(
                hello.to_text()?.contains("\"type\":\"hello\""),
                "missing hello"
            );
            socket.send(Message::Text(json!({"type":"welcome","protocol":"rex.control.v1","api_revision":7,"client":{"id":"client:multiplex","reconnect_token":"private-test-token"}}).to_string().into()))?;
            peer(socket)
        });
        let connection = ControlConnection::connect(&format!("unix://{}", path.display()))?;
        Ok((connection, worker, directory))
    }

    fn mock_request(socket: &mut WebSocket<UnixStream>) -> Result<Value> {
        Ok(serde_json::from_str(socket.read()?.to_text()?)?)
    }

    fn mock_reply(
        socket: &mut WebSocket<UnixStream>,
        request: &Value,
        sequence: u64,
        value: Value,
    ) -> Result<()> {
        let mut response = json!({"type":"response","id":request["id"],"seq":sequence});
        response["payload"] = value;
        socket.send(Message::Text(response.to_string().into()))?;
        Ok(())
    }

    fn finish_mock(worker: thread::JoinHandle<Result<()>>, directory: &Path) -> Result<()> {
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("mock Rex peer panicked"))??;
        fs::remove_file(directory.join("server.sock"))?;
        fs::remove_dir(directory)?;
        Ok(())
    }

    #[test]
    fn native_callback_can_complete_nested_rpc_while_outer_call_waits() -> Result<()> {
        let (mut connection, peer, directory) = mock_connection(|mut socket| {
            let bootstrap = mock_request(&mut socket)?;
            mock_reply(&mut socket, &bootstrap, 0, json!({}))?;
            let outer = mock_request(&mut socket)?;
            assert_eq!(outer["method"], "client.perform");
            socket.send(Message::Text(json!({"type":"event","seq":2,"payload":{"type":"client_perform_requested","payload":{"call_id":"nested-call","action":"session.select"}}}).to_string().into()))?;
            let nested = mock_request(&mut socket)?;
            assert_eq!(nested["method"], "session.view");
            mock_reply(&mut socket, &nested, 2, json!({"nested":true}))?;
            let reply = mock_request(&mut socket)?;
            assert_eq!(reply["method"], "client.reply");
            assert_eq!(reply["payload"]["call_id"], "nested-call");
            mock_reply(&mut socket, &reply, 2, json!({}))?;
            // Completion order does not make request-start watermarks monotonic.
            mock_reply(&mut socket, &outer, 1, json!({"outer":true}))
        })?;
        let handle = connection.handle();
        let (finished, completion) = mpsc::sync_channel(1);
        connection.set_callback_handler(Arc::new(move |event| {
            let result = (|| -> Result<()> {
                let nested = handle.request("session.view", json!({}))?.wait()?;
                ensure!(nested["nested"] == true, "wrong nested response");
                handle
                    .request(
                        "client.reply",
                        json!({"call_id":event["payload"]["payload"]["call_id"],"result":{}}),
                    )?
                    .wait()?;
                Ok(())
            })();
            let _ = finished.send(result);
        }));
        connection.call("session.bootstrap", json!({}))?;
        assert_eq!(connection.call("client.perform", json!({}))?["outer"], true);
        completion.recv_timeout(Duration::from_secs(3))??;
        assert_eq!(connection.last_response_seq(), 1);
        let events = connection.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["seq"], 2);
        assert_eq!(events[0]["payload"]["type"], "client_perform_requested");
        finish_mock(peer, &directory)
    }

    #[test]
    fn asynchronous_calls_match_reversed_response_ids() -> Result<()> {
        fn require_future<T: Future<Output = Result<Value>> + Send>(_: &T) {}
        let (mut connection, peer, directory) = mock_connection(|mut socket| {
            let bootstrap = mock_request(&mut socket)?;
            mock_reply(&mut socket, &bootstrap, 0, json!({}))?;
            let first = mock_request(&mut socket)?;
            let second = mock_request(&mut socket)?;
            mock_reply(&mut socket, &second, 2, json!({"value":"second"}))?;
            mock_reply(&mut socket, &first, 1, json!({"value":"first"}))
        })?;
        connection.call("session.bootstrap", json!({}))?;
        let first = connection.handle().request("first", Value::Null)?;
        let second = connection.handle().request("second", Value::Null)?;
        require_future(&first);
        assert_eq!(first.wait()?["value"], "first");
        assert_eq!(second.wait()?["value"], "second");
        finish_mock(peer, &directory)
    }

    #[test]
    fn bounded_admission_rejects_unsent_work_and_failure_completes_waiters() -> Result<()> {
        let shared = Arc::new(Shared {
            state: Mutex::new(ConnectionState {
                next_id: 1,
                ..ConnectionState::default()
            }),
            event_ready: Condvar::new(),
            message_limit: 256,
        });
        let (requests, _receiver) = mpsc::sync_channel(REQUEST_CAPACITY);
        let (replies, _reply_receiver) = mpsc::sync_channel(REPLY_CAPACITY);
        let handle = ControlHandle {
            shared: Arc::clone(&shared),
            requests,
            replies,
        };
        assert!(
            handle
                .request("oversized", json!({"value":"x".repeat(256)}))
                .is_err()
        );
        let pending: Vec<_> = (0..REQUEST_CAPACITY)
            .map(|_| handle.request("pending", Value::Null))
            .collect::<Result<_>>()?;
        let error = handle
            .request("overflow", Value::Null)
            .err()
            .context("accepted overflowing request")?;
        assert!(error.to_string().contains("queue is full"));
        assert_eq!(
            shared
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pending
                .len(),
            REQUEST_CAPACITY
        );
        let replies: Vec<_> = (0..REPLY_CAPACITY)
            .map(|_| handle.request("client.reply", json!({"call_id":"capacity-test"})))
            .collect::<Result<_>>()?;
        let error = handle
            .request("client.reply", json!({"call_id":"overflow"}))
            .err()
            .context("accepted overflowing callback reply")?;
        assert!(error.to_string().contains("callback reply queue is full"));
        assert_eq!(
            shared
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pending
                .len(),
            REQUEST_CAPACITY + REPLY_CAPACITY
        );
        let (callbacks, _callback_receiver) = mpsc::sync_channel(CALLBACK_CAPACITY);
        for seq in 0..MAX_PENDING_EVENTS {
            dispatch_event(&handle, &callbacks, json!({"type":"event","seq":seq}))?;
        }
        assert!(
            dispatch_event(
                &handle,
                &callbacks,
                json!({"type":"event","seq":MAX_PENDING_EVENTS})
            )
            .is_err()
        );
        shared.fail("test disconnect");
        for request in pending.into_iter().chain(replies) {
            assert!(request.wait().is_err());
        }
        assert!(handle.request("stale-handle", Value::Null).is_err());
        Ok(())
    }

    #[test]
    fn saturated_normal_requests_still_deliver_callback_reply() -> Result<()> {
        let (mut connection, peer, directory) = mock_connection(|mut socket| {
            let bootstrap = mock_request(&mut socket)?;
            mock_reply(&mut socket, &bootstrap, 0, json!({}))?;
            let mut ordinary = Vec::new();
            let mut replied = false;
            for _ in 0..=REQUEST_CAPACITY {
                let request = mock_request(&mut socket)?;
                if request["method"] == "client.reply" {
                    assert_eq!(request["payload"]["call_id"], "capacity-test");
                    mock_reply(&mut socket, &request, 1, json!({"replied":true}))?;
                    replied = true;
                } else {
                    ordinary.push(request);
                }
            }
            assert!(
                replied,
                "saturated ordinary calls starved the callback reply"
            );
            assert_eq!(ordinary.len(), REQUEST_CAPACITY);
            for request in ordinary {
                mock_reply(&mut socket, &request, 1, json!({}))?;
            }
            Ok(())
        })?;
        connection.call("session.bootstrap", json!({}))?;
        let handle = connection.handle();
        let ordinary: Vec<_> = (0..REQUEST_CAPACITY)
            .map(|_| handle.request("pending", Value::Null))
            .collect::<Result<_>>()?;
        assert!(handle.request("normal-overflow", Value::Null).is_err());
        let reply = handle.request(
            "client.reply",
            json!({"call_id":"capacity-test","result":{}}),
        )?;
        assert_eq!(reply.wait()?["replied"], true);
        for pending in ordinary {
            pending.wait()?;
        }
        finish_mock(peer, &directory)
    }

    #[test]
    fn pending_future_wakes_and_returns_its_payload() -> Result<()> {
        struct WakeCount(std::sync::atomic::AtomicUsize);
        impl std::task::Wake for WakeCount {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let inner = Arc::new(PendingInner::default());
        let mut pending = Box::pin(PendingCall {
            inner: Arc::clone(&inner),
        });
        let count = Arc::new(WakeCount(std::sync::atomic::AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&count));
        let mut context = TaskContext::from_waker(&waker);
        assert!(pending.as_mut().poll(&mut context).is_pending());
        inner.finish(Ok(Reply {
            sequence: 7,
            payload: Ok(json!({"ok":true})),
        }));
        assert_eq!(count.0.load(std::sync::atomic::Ordering::Relaxed), 1);
        let Poll::Ready(result) = pending.as_mut().poll(&mut context) else {
            bail!("future did not complete");
        };
        assert_eq!(result?["ok"], true);
        Ok(())
    }

    #[test]
    fn deadline_and_explicit_resume_disconnect_invalidate_old_handles() -> Result<()> {
        for expire in [false, true] {
            let (mut connection, peer, directory) = mock_connection(|mut socket| {
                let bootstrap = mock_request(&mut socket)?;
                mock_reply(&mut socket, &bootstrap, 0, json!({}))?;
                // Keep unanswered requests pending until the client interrupts.
                loop {
                    match socket.read() {
                        Ok(Message::Close(_)) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
                Ok(())
            })?;
            connection.call("session.bootstrap", json!({}))?;
            let handle = connection.handle();
            let pending = handle.request("must-not-replay", Value::Null)?;
            if expire {
                let mut state = handle
                    .shared
                    .state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                for entry in state.pending.values_mut() {
                    entry.deadline = Instant::now();
                }
            } else {
                connection.disconnect_for_resume()?;
            }
            let error = pending
                .wait()
                .err()
                .context("interrupted request completed")?;
            if expire {
                assert!(error.to_string().contains("timed out"));
            }
            assert!(handle.request("stale-handle", Value::Null).is_err());
            finish_mock(peer, &directory)?;
        }
        Ok(())
    }

    #[test]
    fn bootstrap_preserves_interleaved_events_and_remote_errors_keep_connection() -> Result<()> {
        let suffix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "rex-rs-control-test-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let path = directory.join("server.sock");
        let listener = UnixListener::bind(&path)?;
        let server = thread::spawn(move || -> Result<()> {
            let (stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(Duration::from_secs(3)))?;
            #[expect(
                clippy::result_large_err,
                reason = "Tungstenite requires this callback to return its HTTP error response by value."
            )]
            let mut socket = accept_hdr(stream, |request: &Request, mut response: Response| {
                assert_eq!(request.uri().path(), "/api/control");
                response.headers_mut().insert(
                    "Sec-WebSocket-Protocol",
                    HeaderValue::from_static("rex.control.v1"),
                );
                Ok(response)
            })?;
            let hello = socket.read()?;
            assert!(hello.to_text()?.contains("\"type\":\"hello\""));
            socket.send(Message::Text(json!({"type":"welcome","protocol":"rex.control.v1","api_revision":7,"client":{"id":"client:test"},"limits":{"max_message_bytes":MAX_MESSAGE_BYTES}}).to_string().into()))?;
            let request: Value = serde_json::from_str(socket.read()?.to_text()?)?;
            assert_eq!(request["method"], "session.bootstrap");
            socket.send(Message::Text(
                json!({"type":"event","seq":9,"event":"session_view_changed"})
                    .to_string()
                    .into(),
            ))?;
            socket.send(Message::Text(json!({"type":"event","seq":10,"payload":{"type":"client_perform_requested","payload":{"call_id":"callback-1","action":"unknown"}}}).to_string().into()))?;
            let callback: Value = serde_json::from_str(socket.read()?.to_text()?)?;
            assert_eq!(callback["method"], "client.reply");
            assert_eq!(callback["payload"]["call_id"], "callback-1");
            assert_eq!(callback["payload"]["error"]["code"], "not_supported");
            socket.send(Message::Text(
                json!({"type":"response","id":request["id"],"seq":8,"payload":{"sessions":[]}})
                    .to_string()
                    .into(),
            ))?;
            // The callback acknowledgement is deliberately later than the
            // original response, so the next call must match it separately.
            socket.send(Message::Text(
                json!({"type":"response","id":callback["id"],"seq":10,"payload":{},"error":null})
                    .to_string()
                    .into(),
            ))?;
            let request: Value = serde_json::from_str(socket.read()?.to_text()?)?;
            socket.send(Message::Text(json!({"type":"response","id":request["id"],"seq":9,"error":{"code":"not_found","message":"gone"}}).to_string().into()))?;
            let request: Value = serde_json::from_str(socket.read()?.to_text()?)?;
            socket.send(Message::Text(
                json!({"type":"response","id":request["id"],"seq":10,"payload":{"ok":true}})
                    .to_string()
                    .into(),
            ))?;
            Ok(())
        });
        let mut client = ControlConnection::connect(&format!("unix://{}", path.display()))?;
        let bootstrap = client.call("session.bootstrap", json!({"event_scope":"session"}))?;
        assert_eq!(bootstrap["sessions"], json!([]));
        assert_eq!(client.client_id(), "client:test");
        assert_eq!(client.last_response_seq(), 8);
        assert_eq!(client.drain_events()[0]["seq"], 9);
        assert!(client.call("missing", Value::Null).is_err());
        assert_eq!(client.call("server.status", Value::Null)?["ok"], true);
        server
            .join()
            .map_err(|_| anyhow::anyhow!("mock Rex peer panicked"))??;
        assert!(
            client
                .call("session.create", json!({"label":"must-not-replay"}))
                .is_err()
        );
        let error = client
            .call("session.create", json!({"label":"must-not-replay"}))
            .err()
            .context("poisoned connection unexpectedly accepted a request")?;
        assert!(error.to_string().contains("must be re-established"));
        fs::remove_file(path)?;
        fs::remove_dir(directory)?;
        Ok(())
    }

    #[test]
    fn unsupported_endpoints_fail_before_connecting() {
        assert!(
            open_websocket("https://example.invalid", "/api/control", "rex.control.v1").is_err()
        );
        assert!(open_websocket("unix://relative.sock", "/api/control", "rex.control.v1").is_err());
    }
}
