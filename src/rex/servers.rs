//! Route Lua callbacks only to explicitly named, currently connected servers.
use super::{
    protocol::RemoteError,
    transport::{ControlHandle, PendingCall},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, PoisonError, Weak,
        mpsc::{self, Receiver, SyncSender},
    },
    task::{Context as TaskContext, Poll, Waker},
    thread,
    time::{Duration, Instant},
};

const MAX_SERVERS: usize = 16;
const MAX_CALLS: usize = 16;
const MAX_CALL_BYTES: usize = 4 * 1024 * 1024;
const CALL_TIMEOUT: Duration = Duration::from_secs(9);

#[derive(Clone)]
struct Peer {
    id: String,
    instance: String,
    generation: u64,
    handle: ControlHandle,
    ordered: bool,
    callback_ids: VecDeque<String>,
    callback_sequence: Option<u64>,
}
#[derive(Default)]
struct Registry {
    slots: BTreeMap<String, Option<Peer>>,
    next_id: u64,
    calls: usize,
    bytes: usize,
}
struct Shared {
    registry: Mutex<Registry>,
}

/// One bounded router shared by the application's explicitly configured windows.
#[derive(Clone)]
pub struct ServerRoutes {
    shared: Arc<Shared>,
    jobs: SyncSender<Job>,
}
impl Default for ServerRoutes {
    fn default() -> Self {
        let shared = Arc::new(Shared {
            registry: Mutex::new(Registry::default()),
        });
        let (jobs, receiver) = mpsc::sync_channel(MAX_CALLS);
        // A failed spawn closes the receiver. Admission then returns an explicit
        // error through the originating connection, without leaking a job.
        let _worker = thread::Builder::new()
            .name("rex-server-routes".into())
            .spawn(move || pump(&receiver));
        Self { shared, jobs }
    }
}

/// A name reserved by launch configuration. It never contains a callback URL.
#[derive(Clone)]
pub struct NamedServer {
    name: String,
    routes: ServerRoutes,
}
impl ServerRoutes {
    /// Reserve a unique Lua server name before opening its window.
    ///
    /// # Errors
    /// Rejects duplicate/empty names, control characters and more than 16 names.
    pub fn named(&self, name: impl Into<String>) -> Result<NamedServer> {
        let name = name.into();
        ensure!(
            !name.is_empty()
                && name.len() <= 128
                && name.trim() == name
                && !name.chars().any(char::is_control),
            "invalid Rex server name"
        );
        let mut registry = self
            .shared
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        ensure!(
            !registry.slots.contains_key(&name),
            "duplicate Rex server name"
        );
        ensure!(
            registry.slots.len() < MAX_SERVERS,
            "too many named Rex servers"
        );
        registry.slots.insert(name.clone(), None);
        Ok(NamedServer {
            name,
            routes: self.clone(),
        })
    }
}
impl NamedServer {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Publish one bootstrapped connection. Reconnect receives a fresh route ID.
    ///
    /// # Errors
    /// Rejects missing instance identity or an exhausted registration counter.
    pub fn register(
        &self,
        instance: String,
        generation: u64,
        handle: ControlHandle,
    ) -> Result<ServerLease> {
        ensure!(
            !instance.is_empty() && instance.len() <= 4096,
            "invalid Rex server instance"
        );
        let mut registry = self
            .routes
            .shared
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        registry.next_id = registry
            .next_id
            .checked_add(1)
            .context("Rex route identities exhausted")?;
        let id = format!("rex-rs-route-{}", registry.next_id);
        registry.slots.insert(
            self.name.clone(),
            Some(Peer {
                id: id.clone(),
                instance,
                generation,
                handle,
                ordered: false,
                callback_ids: VecDeque::new(),
                callback_sequence: None,
            }),
        );
        Ok(ServerLease {
            routes: self.routes.clone(),
            name: self.name.clone(),
            id,
        })
    }
}

/// Removing this lease retires only this exact connection, never a replacement.
pub struct ServerLease {
    routes: ServerRoutes,
    name: String,
    id: String,
}
impl Drop for ServerLease {
    fn drop(&mut self) {
        let mut registry = self
            .routes
            .shared
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if registry
            .slots
            .get(&self.name)
            .and_then(Option::as_ref)
            .is_some_and(|peer| peer.id == self.id)
        {
            registry.slots.insert(self.name.clone(), None);
        }
    }
}
/// Exclusive admission for calls that can wait on callbacks on one connection.
pub struct OrderedLease {
    shared: Weak<Shared>,
    id: String,
}
impl Drop for OrderedLease {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            let mut registry = shared
                .registry
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(peer) = registry
                .slots
                .values_mut()
                .filter_map(Option::as_mut)
                .find(|peer| peer.id == self.id)
            {
                peer.ordered = false;
            }
        }
    }
}
impl ServerLease {
    /// Reserve the connection before admitting an ordered callback-producing call.
    ///
    /// # Errors
    /// A stale or busy connection is rejected before any request is sent.
    pub fn try_ordered(&self) -> Result<OrderedLease> {
        let mut registry = self
            .routes
            .shared
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let peer = registry
            .slots
            .get_mut(&self.name)
            .and_then(Option::as_mut)
            .filter(|peer| peer.id == self.id)
            .context("Rex server connection is stale; request was not sent")?;
        reserve_ordered(peer, &self.routes.shared)
    }

    /// Whether a local or forwarded call currently holds this connection's lane.
    #[must_use]
    pub fn ordered_active(&self) -> bool {
        self.routes
            .shared
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .slots
            .get(&self.name)
            .and_then(Option::as_ref)
            .is_some_and(|peer| peer.id == self.id && peer.ordered)
    }

    /// Snapshot other connected servers for a single `client.action` request.
    #[must_use]
    pub fn advertised(&self) -> Value {
        let registry = self
            .routes
            .shared
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(origin) = registry
            .slots
            .get(&self.name)
            .and_then(Option::as_ref)
            .filter(|peer| peer.id == self.id)
        else {
            return json!([]);
        };
        Value::Array(
            registry
                .slots
                .iter()
                .filter_map(|(name, peer)| {
                    let peer = peer.as_ref()?;
                    (peer.instance != origin.instance).then(|| json!({"name":name,"id":peer.id}))
                })
                .collect(),
        )
    }

    /// Handle a cross-server callback; return false for native or other events.
    ///
    /// # Errors
    /// Reports malformed callbacks or inability to acknowledge them on the origin.
    pub fn route_event(&self, event: &Value) -> Result<bool> {
        if event["payload"]["type"] != "client_call_requested" {
            return Ok(false);
        }
        let payload = &event["payload"]["payload"];
        let call_id = payload["call_id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 256)
            .context("invalid Rex callback ID")?;
        let origin = {
            let mut registry = self
                .routes
                .shared
                .registry
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let origin = registry
                .slots
                .get_mut(&self.name)
                .and_then(Option::as_mut)
                .filter(|peer| peer.id == self.id)
                .context("Rex callback origin is disconnected")?;
            // Delivery retries must not replay a mutation, including after its
            // timeout reply while the original destination request still drains.
            let sequence = event["seq"].as_u64();
            if sequence
                .is_some_and(|value| origin.callback_sequence.is_some_and(|last| value <= last))
                || origin.callback_ids.iter().any(|id| id == call_id)
            {
                return Ok(true);
            }
            if let Some(sequence) = sequence {
                origin.callback_sequence = Some(sequence);
            }
            if origin.callback_ids.len() == 256 {
                origin.callback_ids.pop_front();
            }
            origin.callback_ids.push_back(call_id.to_owned());
            origin.clone()
        };
        if let Err(error) = self.admit(&origin, call_id, payload) {
            reply(&origin.handle, call_id, Err(error))?;
        }
        Ok(true)
    }

    fn admit(&self, origin: &Peer, call_id: &str, payload: &Value) -> Result<()> {
        let route = payload["server"]
            .as_str()
            .context("Rex callback lacks server ID")?;
        let method = payload["method"]
            .as_str()
            .filter(|value| {
                !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
            })
            .context("invalid Rex callback method")?;
        let args = payload.get("args").cloned().unwrap_or(Value::Null);
        ensure!(
            args.is_null() || args.is_object(),
            "invalid Rex callback arguments"
        );
        let bytes = serde_json::to_vec(payload)?.len();
        let mut registry = self
            .routes
            .shared
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        ensure!(
            registry.calls < MAX_CALLS && registry.bytes.saturating_add(bytes) <= MAX_CALL_BYTES,
            "Rex server callback queue is full; request was not sent"
        );
        let target = registry
            .slots
            .values_mut()
            .filter_map(Option::as_mut)
            .find(|peer| peer.id == route && peer.instance != origin.instance)
            .context("Rex callback server is unknown or disconnected")?;
        let ordered = reserve_ordered(target, &self.routes.shared)?;
        let target = target.clone();
        registry.calls += 1;
        registry.bytes += bytes;
        drop(registry);
        let job = Job {
            origin: origin.clone(),
            target,
            call_id: call_id.to_owned(),
            method: method.to_owned(),
            args,
            request: None,
            response: None,
            acknowledgement: None,
            reply_done: false,
            request_done: false,
            ordered: Some(ordered),
            deadline: Instant::now() + CALL_TIMEOUT,
            permit: Permit {
                shared: Arc::downgrade(&self.routes.shared),
                bytes,
            },
        };
        self.routes
            .jobs
            .try_send(job)
            .map_err(|_| anyhow::anyhow!("Rex server router unavailable; request was not sent"))
    }
}
fn reserve_ordered(peer: &mut Peer, shared: &Arc<Shared>) -> Result<OrderedLease> {
    ensure!(
        !peer.ordered,
        "Rex destination is busy; request was not sent"
    );
    peer.ordered = true;
    Ok(OrderedLease {
        shared: Arc::downgrade(shared),
        id: peer.id.clone(),
    })
}
struct Permit {
    shared: Weak<Shared>,
    bytes: usize,
}
impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            let mut registry = shared
                .registry
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            registry.calls = registry.calls.saturating_sub(1);
            registry.bytes = registry.bytes.saturating_sub(self.bytes);
        }
    }
}
struct Job {
    origin: Peer,
    target: Peer,
    call_id: String,
    method: String,
    args: Value,
    request: Option<PendingCall>,
    response: Option<Value>,
    acknowledgement: Option<PendingCall>,
    reply_done: bool,
    request_done: bool,
    ordered: Option<OrderedLease>,
    deadline: Instant,
    permit: Permit,
}
impl Job {
    fn current(&self) -> bool {
        self.permit.shared.upgrade().is_some_and(|shared| {
            let registry = shared
                .registry
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            [&self.origin, &self.target].iter().all(|expected| {
                registry
                    .slots
                    .values()
                    .filter_map(Option::as_ref)
                    .any(|peer| peer.id == expected.id && peer.generation == expected.generation)
            })
        })
    }
    fn poll(&mut self) -> bool {
        if self.response.is_none() {
            if !self.current() {
                self.respond(Err(anyhow::anyhow!(
                    "Rex server connection changed; request was not retried"
                )));
            } else if Instant::now() >= self.deadline {
                self.respond(Err(anyhow::anyhow!(
                    "Rex server callback timed out; delivery may be uncertain and was not retried"
                )));
            } else if self.request.is_none() {
                match self.send_request() {
                    Ok(request) => self.request = Some(request),
                    Err(error) => self.respond(Err(error)),
                }
            }
        }
        if !self.request_done {
            if let Some(request) = &mut self.request {
                if let Poll::Ready(result) =
                    Pin::new(request).poll(&mut TaskContext::from_waker(Waker::noop()))
                {
                    self.request_done = true;
                    self.ordered = None;
                    if self.response.is_none() {
                        self.respond(result);
                    }
                }
            } else if self.response.is_some() {
                self.request_done = true;
                self.ordered = None;
            }
        }
        self.poll_reply();
        // A timed-out mutation is not cancelled. Keep its lane reserved and
        // drain the exact PendingCall until the transport settles it (<=32s).
        self.reply_done && self.request_done
    }

    fn send_request(&mut self) -> Result<PendingCall> {
        let shared = self
            .permit
            .shared
            .upgrade()
            .context("Rex server router closed; request was not sent")?;
        let registry = shared
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        ensure!(
            [&self.origin, &self.target].iter().all(|expected| registry
                .slots
                .values()
                .filter_map(Option::as_ref)
                .any(|peer| peer.id == expected.id && peer.generation == expected.generation)),
            "Rex server connection changed; request was not sent"
        );
        // Registration and admission share this lock. A reconnect cannot swap
        // the peer between validation and queueing on the captured handle.
        self.target.handle.request(&self.method, self.args.take())
    }

    fn respond(&mut self, result: Result<Value>) {
        self.response = Some(reply_payload(&self.call_id, result));
    }

    fn poll_reply(&mut self) {
        if self.reply_done {
            return;
        }
        let Some(payload) = &self.response else {
            return;
        };
        if let Some(acknowledgement) = &mut self.acknowledgement {
            if Pin::new(acknowledgement)
                .poll(&mut TaskContext::from_waker(Waker::noop()))
                .is_ready()
            {
                self.reply_done = true;
            }
        } else if Instant::now() >= self.deadline + Duration::from_millis(500) {
            self.reply_done = true;
        } else if let Ok(pending) = self.origin.handle.request("client.reply", payload.clone()) {
            // Only unadmitted replies are retried. A PendingCall is retained so
            // eight reserved transport slots backpressure simultaneous results.
            self.acknowledgement = Some(pending);
        }
    }
}

fn pump(receiver: &Receiver<Job>) {
    let mut pending = Vec::new();
    loop {
        if pending.is_empty() {
            match receiver.recv() {
                Ok(job) => pending.push(job),
                Err(_) => return,
            }
        }
        while pending.len() < MAX_CALLS {
            match receiver.try_recv() {
                Ok(job) => pending.push(job),
                Err(_) => break,
            }
        }
        let mut index = 0;
        while index < pending.len() {
            if pending[index].poll() {
                pending.swap_remove(index);
            } else {
                index += 1;
            }
        }
        if !pending.is_empty() {
            thread::sleep(Duration::from_millis(1));
        }
    }
}
fn reply(origin: &ControlHandle, call_id: &str, result: Result<Value>) -> Result<()> {
    let payload = reply_payload(call_id, result);
    let _pending = origin.request("client.reply", payload)?;
    Ok(())
}
fn reply_payload(call_id: &str, result: Result<Value>) -> Value {
    let payload = match result {
        Ok(value) => json!({"call_id":call_id,"result":value}),
        Err(error) => {
            let (code, message) = error.downcast_ref::<RemoteError>().map_or_else(
                || ("refused".to_owned(), format!("{error:#}")),
                |remote| (remote.code.clone(), remote.message.clone()),
            );
            json!({"call_id":call_id,"error":{"code":code,"message":message}})
        }
    };
    if payload.to_string().len() > super::protocol::MAX_MESSAGE_BYTES - 512 {
        json!({"call_id":call_id,"error":{"code":"refused","message":"Rex destination response exceeds the callback reply limit"}})
    } else {
        payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rex::transport::ControlConnection;
    use std::{net::TcpListener, sync::mpsc::RecvTimeoutError};
    use tungstenite::{
        Message, accept_hdr,
        handshake::server::{Request, Response},
        http::HeaderValue,
    };

    struct Mock {
        connection: Option<ControlConnection>,
        requests: Receiver<Value>,
        thread: Option<thread::JoinHandle<Result<()>>>,
    }
    impl Drop for Mock {
        fn drop(&mut self) {
            self.connection.take();
            if let Some(thread) = self.thread.take() {
                let _result = thread.join();
            }
        }
    }
    impl Mock {
        fn new() -> Result<Self> {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let address = listener.local_addr()?;
            let (send, requests) = mpsc::channel();
            let thread = thread::spawn(move || -> Result<()> {
                let (stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(1)))?;
                #[expect(
                    clippy::result_large_err,
                    reason = "Tungstenite's handshake callback owns the HTTP response."
                )]
                let mut socket = accept_hdr(stream, |_: &Request, mut response: Response| {
                    response.headers_mut().insert(
                        "Sec-WebSocket-Protocol",
                        HeaderValue::from_static("rex.control.v1"),
                    );
                    Ok(response)
                })?;
                let _hello = socket.read()?;
                socket.send(Message::Text(json!({"type":"welcome","protocol":"rex.control.v1","api_revision":7,"client":{"id":"client:mock"}}).to_string().into()))?;
                let mut sequence = 0;
                while let Ok(Message::Text(text)) = socket.read() {
                    let value: Value = serde_json::from_str(&text)?;
                    send.send(value.clone())?;
                    sequence += 1;
                    if value["method"] == "hold" {
                        continue;
                    }
                    let response = if value["method"] == "fail" {
                        json!({"type":"response","id":value["id"],"seq":sequence,"error":{"code":"not_found","message":"missing destination"}})
                    } else {
                        json!({"type":"response","id":value["id"],"seq":sequence,"payload":{"method":value["method"],"args":value["payload"]}})
                    };
                    socket.send(Message::Text(response.to_string().into()))?;
                }
                Ok(())
            });
            let connection = ControlConnection::connect(&format!("http://{address}"))?;
            Ok(Self {
                connection: Some(connection),
                requests,
                thread: Some(thread),
            })
        }
        fn handle(&self) -> Result<ControlHandle> {
            Ok(self
                .connection
                .as_ref()
                .context("mock disconnected")?
                .handle())
        }
        fn next(&self) -> Result<Value> {
            self.requests
                .recv_timeout(Duration::from_secs(2))
                .context("mock did not receive request")
        }
    }
    fn event(id: &str, method: &str) -> Value {
        json!({"payload":{"type":"client_call_requested","payload":{"call_id":format!("call:{method}"),"server":id,"method":method,"args":{"value":7}}}})
    }
    fn advertised_id(lease: &ServerLease) -> Result<String> {
        lease.advertised()[0]["id"]
            .as_str()
            .map(str::to_owned)
            .context("missing peer")
    }

    #[test]
    fn oversized_result_becomes_a_small_explicit_error() {
        let payload = reply_payload(
            "call:large",
            Ok(json!({"data":"x".repeat(super::super::protocol::MAX_MESSAGE_BYTES)})),
        );
        assert_eq!(payload["call_id"], "call:large");
        assert_eq!(payload["error"]["code"], "refused");
        assert!(payload.get("result").is_none());
        assert!(payload.to_string().len() < 512);
    }

    #[test]
    fn named_registry_rejects_invalid_duplicate_and_excess_names() -> Result<()> {
        let routes = ServerRoutes::default();
        for name in ["", " leading", "trailing ", "line\nbreak"] {
            assert!(routes.named(name).is_err());
        }
        let _first = routes.named("peer")?;
        assert!(routes.named("peer").is_err());
        for index in 1..MAX_SERVERS {
            let _name = routes.named(format!("peer{index}"))?;
        }
        assert!(routes.named("overflow").is_err());
        Ok(())
    }

    #[test]
    fn routing_preserves_origin_result_error_and_rejects_stale_routes() -> Result<()> {
        let routes = ServerRoutes::default();
        let origin = Mock::new()?;
        let target = Mock::new()?;
        let origin_name = routes.named("origin")?;
        let target_name = routes.named("peer")?;
        let origin_lease = origin_name.register("instance:a".into(), 1, origin.handle()?)?;
        let target_lease = target_name.register("instance:b".into(), 1, target.handle()?)?;
        let id = advertised_id(&origin_lease)?;
        assert_eq!(origin_lease.advertised()[0]["name"], "peer");
        assert!(
            !origin_lease.route_event(&json!({"payload":{"type":"client_perform_requested"}}))?
        );
        assert!(origin_lease.route_event(&event(&id, "server.status"))?);
        assert_eq!(target.next()?["method"], "server.status");
        let reply = origin.next()?;
        assert_eq!(reply["method"], "client.reply");
        assert_eq!(reply["payload"]["call_id"], "call:server.status");
        assert_eq!(reply["payload"]["result"]["args"], json!({"value":7}));
        assert!(origin_lease.route_event(&event(&id, "server.status"))?);
        assert!(matches!(
            target.requests.recv_timeout(Duration::from_millis(30)),
            Err(RecvTimeoutError::Timeout)
        ));
        assert!(origin.requests.try_recv().is_err());
        assert!(origin_lease.route_event(&event(&id, "fail"))?);
        let _target = target.next()?;
        let reply = origin.next()?;
        assert_eq!(
            reply["payload"]["error"],
            json!({"code":"not_found","message":"missing destination"})
        );
        let replacement = target_name.register("instance:b".into(), 2, target.handle()?)?;
        drop(target_lease);
        assert_ne!(advertised_id(&origin_lease)?, id);
        origin_lease.route_event(&event(&id, "must_not_send"))?;
        assert!(
            origin.next()?["payload"]["error"]["message"]
                .as_str()
                .is_some_and(|text| text.contains("unknown or disconnected"))
        );
        assert!(matches!(
            target.requests.recv_timeout(Duration::from_millis(30)),
            Err(RecvTimeoutError::Timeout)
        ));
        drop(replacement);
        assert_eq!(origin_lease.advertised(), json!([]));
        Ok(())
    }

    #[test]
    fn busy_and_oversized_routes_are_refused_before_destination_send() -> Result<()> {
        let routes = ServerRoutes::default();
        let origin = Mock::new()?;
        let target = Mock::new()?;
        let origin_ase =
            routes
                .named("origin")?
                .register("instance:a".into(), 1, origin.handle()?)?;
        let target_lease =
            routes
                .named("target")?
                .register("instance:b".into(), 1, target.handle()?)?;
        let id = advertised_id(&origin_ase)?;
        let guard = target_lease.try_ordered()?;
        assert!(target_lease.ordered_active());
        assert!(target_lease.try_ordered().is_err());
        origin_ase.route_event(&event(&id, "must_not_send"))?;
        assert!(
            origin.next()?["payload"]["error"]["message"]
                .as_str()
                .is_some_and(|text| text.contains("busy"))
        );
        drop(guard);
        assert!(!target_lease.ordered_active());
        let mut oversized = event(&id, "large_must_not_send");
        oversized["payload"]["payload"]["args"] = json!({"data":"x".repeat(MAX_CALL_BYTES)});
        origin_ase.route_event(&oversized)?;
        assert!(origin.next()?["payload"]["error"].is_object());
        assert!(matches!(
            target.requests.recv_timeout(Duration::from_millis(30)),
            Err(RecvTimeoutError::Timeout)
        ));
        // Different endpoints to the same running server must not be advertised.
        let _alias = routes
            .named("alias")?
            .register("instance:a".into(), 1, origin.handle()?)?;
        assert_eq!(origin_ase.advertised().as_array().map(Vec::len), Some(1));
        Ok(())
    }

    #[test]
    fn disconnect_during_forward_releases_lane_without_replaying() -> Result<()> {
        let routes = ServerRoutes::default();
        let origin = Mock::new()?;
        let mut target = Mock::new()?;
        let origin_lease =
            routes
                .named("origin")?
                .register("instance:a".into(), 1, origin.handle()?)?;
        let target_lease =
            routes
                .named("target")?
                .register("instance:b".into(), 1, target.handle()?)?;
        let _origin_guard = origin_lease.try_ordered()?;
        origin_lease.route_event(&event(&advertised_id(&origin_lease)?, "hold"))?;
        assert_eq!(target.next()?["method"], "hold");
        assert!(target_lease.try_ordered().is_err());
        // The target action cannot route back into the waiting origin's lane.
        target_lease.route_event(&event(&advertised_id(&target_lease)?, "cycle"))?;
        let refused = target.next()?;
        assert_eq!(refused["method"], "client.reply");
        assert!(
            refused["payload"]["error"]["message"]
                .as_str()
                .is_some_and(|text| text.contains("busy"))
        );
        target
            .connection
            .as_mut()
            .context("missing target")?
            .disconnect_for_resume()?;
        assert!(origin.next()?["payload"]["error"].is_object());
        let deadline = Instant::now() + Duration::from_secs(1);
        while target_lease.ordered_active() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(!target_lease.ordered_active());
        assert!(target.requests.try_recv().is_err());
        Ok(())
    }
    #[test]
    fn expired_route_reports_uncertainty_but_retains_lane_until_destination_settles() -> Result<()>
    {
        // Drive the pump manually to cross the deadline without a nine-second
        // sleep, while the real mock transport retains an unanswered request.
        let shared = Arc::new(Shared {
            registry: Mutex::new(Registry::default()),
        });
        let (jobs, receiver) = mpsc::sync_channel(MAX_CALLS);
        let routes = ServerRoutes { shared, jobs };
        let origin = Mock::new()?;
        let mut target = Mock::new()?;
        let origin_lease =
            routes
                .named("origin")?
                .register("instance:a".into(), 1, origin.handle()?)?;
        let target_lease =
            routes
                .named("target")?
                .register("instance:b".into(), 1, target.handle()?)?;
        origin_lease.route_event(&event(&advertised_id(&origin_lease)?, "hold"))?;
        let mut job = receiver.recv_timeout(Duration::from_secs(1))?;
        assert!(!job.poll());
        assert_eq!(target.next()?["method"], "hold");
        job.deadline = Instant::now();
        assert!(!job.poll());
        let acknowledgement = origin.next()?;
        assert!(
            acknowledgement["payload"]["error"]["message"]
                .as_str()
                .is_some_and(|text| text.contains("uncertain"))
        );
        assert!(target_lease.ordered_active());
        assert!(target_lease.try_ordered().is_err());
        target
            .connection
            .as_mut()
            .context("missing target")?
            .disconnect_for_resume()?;
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut complete = job.poll();
        while !complete && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
            complete = job.poll();
        }
        assert!(complete);
        assert!(!target_lease.ordered_active());
        assert!(target.requests.try_recv().is_err());
        Ok(())
    }
}
