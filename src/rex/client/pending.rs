//! Keep existing terminal replicas progressing while an ordered Lua RPC is pending.
use super::{
    ActionContext, ActionSender, Cache, ClientScript, ControlConnection, LocalOperation, Mailbox,
    Operation, TerminalStream, automation, interaction, key_payload, prepare_terminal_operation,
    publish, publish_configuration_result, record_error, refresh_all_views, refresh_closed_pane,
    refresh_keymap, script_identity, scripts_params, server_action_params, text_key_payload,
    validate_sequence,
};
use crate::core::PaneId;
use crate::rex::{servers::OrderedLease, transport::PendingCall};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, PoisonError},
    task::{Context as TaskContext, Poll, Waker},
    time::{Duration, Instant},
};

enum Finish {
    ServerAction,
    Scripts(Vec<ClientScript>),
    Key(PaneId),
}

struct OrderedRequest {
    call: PendingCall,
    _lease: Option<OrderedLease>,
}

pub(super) struct PendingWork {
    request: OrderedRequest,
    finish: Finish,
    completion: Option<ActionSender>,
    mailbox: Arc<Mutex<Mailbox>>,
}

impl Drop for PendingWork {
    fn drop(&mut self) {
        self.mailbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .worker_action_active = false;
    }
}

fn request(
    control: &ControlConnection,
    method: &str,
    params: Value,
    mailbox: &Arc<Mutex<Mailbox>>,
) -> Result<OrderedRequest> {
    let lease = super::ordered_lease(mailbox)?;
    // Set before admission: the IO thread can receive a callback immediately.
    mailbox
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .worker_action_active = true;
    match control.handle().request(method, params) {
        Ok(call) => Ok(OrderedRequest {
            call,
            _lease: lease,
        }),
        Err(error) => {
            mailbox
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .worker_action_active = false;
            Err(error)
        }
    }
}

impl PendingWork {
    fn new(
        request: OrderedRequest,
        finish: Finish,
        completion: Option<ActionSender>,
        mailbox: &Arc<Mutex<Mailbox>>,
    ) -> Self {
        mailbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .worker_action_active = true;
        Self {
            request,
            finish,
            completion,
            mailbox: Arc::clone(mailbox),
        }
    }

    pub(super) fn server(
        control: &ControlConnection,
        cache: &Cache,
        name: &str,
        args: &Value,
        context: &ActionContext,
        completion: ActionSender,
        mailbox: &Arc<Mutex<Mailbox>>,
    ) -> Option<Self> {
        let request = automation::validate_context(&cache.state, context)
            .and_then(|()| server_action_params(name, args, context))
            .and_then(|mut params| {
                let lease = mailbox
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .server_lease
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade);
                if let Some(lease) = lease {
                    params["servers"] = lease.advertised();
                }
                request(control, "client.action", params, mailbox)
            });
        match request {
            Ok(request) => Some(Self::new(
                request,
                Finish::ServerAction,
                Some(completion),
                mailbox,
            )),
            Err(error) => {
                completion.send(Err(error));
                None
            }
        }
    }

    pub(super) fn scripts(
        control: &ControlConnection,
        cache: &mut Cache,
        scripts: Vec<ClientScript>,
        completion: ActionSender,
        mailbox: &Arc<Mutex<Mailbox>>,
    ) -> Option<Self> {
        let request = scripts_params(&scripts).and_then(|params| {
            // An admitted script load may mutate state even if its reply is lost.
            cache.script_attempt = Some(script_identity(cache));
            request(control, "client.set_script", params, mailbox)
        });
        match request {
            Ok(request) => Some(Self::new(
                request,
                Finish::Scripts(scripts),
                Some(completion),
                mailbox,
            )),
            Err(error) => {
                completion.send(Err(error));
                None
            }
        }
    }

    pub(super) fn key(
        control: &ControlConnection,
        cache: &mut Cache,
        streams: &mut BTreeMap<PaneId, TerminalStream>,
        operation: Operation,
        mailbox: &Arc<Mutex<Mailbox>>,
    ) -> Result<Option<Self>> {
        let pane = operation.pane().context("key has no pane")?;
        if cache.state.is_retired_pane(pane) || cache.state.is_detached(pane) {
            return Ok(None);
        }
        let (_, changed) = prepare_terminal_operation(cache, streams, &operation)?;
        let block = cache
            .state
            .pane(pane)
            .context("Rex block no longer exists")?;
        let mut params = json!({"session_id":block.session_id,"block_id":block.remote_id});
        match operation {
            Operation::Key(_, key) => params["key"] = key_payload(&key),
            Operation::KeySequence(_, key, prefix) => {
                validate_sequence(&key, &prefix)?;
                params["key"] = key_payload(&key);
                if !prefix.is_empty() {
                    params["prefix"] = json!(prefix);
                }
            }
            Operation::Text(_, text) => params["key"] = text_key_payload(text),
            _ => bail!("unexpected pending key operation"),
        }
        if changed {
            publish(mailbox, cache);
        }
        let request = request(control, "client.key", params, mailbox)?;
        Ok(Some(Self::new(request, Finish::Key(pane), None, mailbox)))
    }

    pub(super) fn poll(&mut self) -> Poll<Result<Value>> {
        Pin::new(&mut self.request.call).poll(&mut TaskContext::from_waker(Waker::noop()))
    }

    pub(super) fn finish(
        mut self,
        result: Result<Value>,
        control: &mut ControlConnection,
        cache: &mut Cache,
    ) {
        let result = result.and_then(|value| {
            match &self.finish {
                Finish::ServerAction => refresh_all_views(control, cache)?,
                Finish::Scripts(scripts) => {
                    cache.scripts = Arc::new(scripts.clone());
                    refresh_keymap(control, cache)?;
                }
                Finish::Key(_) => {}
            }
            Ok(value)
        });
        if let Finish::Key(pane) = self.finish {
            if let Err(error) = result {
                let retired = refresh_closed_pane(control, cache, pane).unwrap_or(false)
                    && (cache.state.is_retired_pane(pane) || cache.state.is_detached(pane));
                if !retired {
                    record_error(&self.mailbox, format!("Rex key: {error:#}"));
                }
            }
        } else {
            publish_configuration_result(cache, &self.mailbox, &result);
            if let Some(completion) = self.completion.take() {
                completion.send(result);
            }
        }
    }
}

/// Only independently admitted local queries and callback copies may bypass the
/// ordered mutation/input queue. They cannot send process input or mutate Rex.
pub(super) fn service_local(
    cache: &mut Cache,
    streams: &mut BTreeMap<PaneId, TerminalStream>,
    mailbox: &Arc<Mutex<Mailbox>>,
) {
    let start = Instant::now();
    for _ in 0..16 {
        let queued = {
            let mut mailbox = mailbox.lock().unwrap_or_else(PoisonError::into_inner);
            mailbox
                .callback_local
                .pop_front()
                .or_else(|| mailbox.pending_search.pop_first().map(|(_, queued)| queued))
        };
        let Some(queued) = queued else {
            break;
        };
        if queued.instance != cache.state.instance_id()
            || queued.generation != cache.connection_generation
        {
            continue;
        }
        if let Operation::Local(pane, operation) = queued.operation {
            let copy = matches!(operation, LocalOperation::Copy(_));
            interaction::execute(cache, streams, pane, operation, mailbox);
            if copy || start.elapsed() >= Duration::from_millis(4) {
                break;
            }
        }
    }
}
