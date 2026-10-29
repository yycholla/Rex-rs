//! Owned action requests and completions shared by the IO worker and desktop.
use super::transport::{ControlHandle, PendingCall};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, PoisonError},
    task::{Context as TaskContext, Poll, Waker},
};

#[derive(Clone, Debug, Default)]
pub struct ActionContext {
    pub session_id: Option<String>,
    pub block_id: Option<String>,
}

/// Validate sender-supplied targets before even a local UI action executes.
///
/// # Errors
/// Rejects missing sessions, foreign blocks, and block IDs without a session.
pub fn validate_context(
    state: &super::workspace::WorkspaceState,
    context: &ActionContext,
) -> Result<()> {
    if let Some(session_id) = &context.session_id {
        ensure!(
            state
                .sessions()
                .iter()
                .any(|session| session.remote_id() == session_id),
            "Rex action session no longer exists"
        );
    }
    if let Some(block_id) = &context.block_id {
        let session_id = context
            .session_id
            .as_ref()
            .context("Rex action block requires a session")?;
        ensure!(
            state.pane_ids().into_iter().any(|pane| state
                .pane(pane)
                .is_some_and(
                    |block| &block.remote_id == block_id && &block.session_id == session_id
                )),
            "Rex action block is not in the requested session"
        );
    }
    Ok(())
}

pub struct ClientActionRequest {
    pub call_id: String,
    pub action: String,
    pub args: Value,
    pub context: ActionContext,
    pub(crate) instance: String,
    pub(crate) generation: u64,
    handle: ControlHandle,
    replied: bool,
}

impl ClientActionRequest {
    pub(crate) fn from_event(
        event: &Value,
        instance: String,
        generation: u64,
        handle: ControlHandle,
    ) -> Result<Self> {
        let payload = &event["payload"]["payload"];
        let call_id = payload["call_id"]
            .as_str()
            .context("Rex action lacks call ID")?;
        let action = payload["action"]
            .as_str()
            .context("Rex action lacks name")?;
        ensure!(
            call_id.len() <= 512 && !call_id.is_empty() && action.len() <= 256,
            "invalid Rex action identity"
        );
        let optional_id = |key| match &payload[key] {
            Value::Null => Ok(None),
            Value::String(id) if !id.is_empty() && id.len() <= 256 => Ok(Some(id.clone())),
            _ => anyhow::bail!("invalid Rex action {key}"),
        };
        Ok(Self {
            call_id: call_id.into(),
            action: action.into(),
            args: payload.get("args").cloned().unwrap_or(Value::Null),
            context: ActionContext {
                session_id: optional_id("session_id")?,
                block_id: optional_id("block_id")?,
            },
            instance,
            generation,
            handle,
            replied: false,
        })
    }

    /// Consuming the request prevents a second reply from this dispatcher.
    ///
    /// # Errors
    /// Reports a disconnected or saturated control connection.
    pub fn reply(mut self, result: std::result::Result<Value, ActionError>) -> Result<PendingCall> {
        let mut params = json!({"call_id":self.call_id});
        match result {
            Ok(value) => params["result"] = value,
            Err(error) => params["error"] = json!({"code":error.code,"message":error.message}),
        }
        self.replied = true;
        self.handle.request("client.reply", params)
    }
}

impl Drop for ClientActionRequest {
    fn drop(&mut self) {
        if !self.replied {
            let _ = self.handle.request(
                "client.reply",
                json!({"call_id":self.call_id,
                "error":{"code":"refused","message":"The desktop action was cancelled"}}),
            );
        }
    }
}

pub struct ActionError {
    pub code: &'static str,
    pub message: String,
}

impl ActionError {
    #[must_use]
    pub fn refused(message: impl Into<String>) -> Self {
        Self {
            code: "refused",
            message: message.into(),
        }
    }
    #[must_use]
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            code: "not_supported",
            message: message.into(),
        }
    }
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: "internal",
            message: message.into(),
        }
    }
}

#[derive(Default)]
struct Completion {
    result: Option<Result<Value>>,
    waiter: Option<Waker>,
    receiver_dropped: bool,
}

pub struct ActionFuture(Arc<Mutex<Completion>>);
pub(crate) struct ActionSender(Option<Arc<Mutex<Completion>>>);

pub(crate) fn completion() -> (ActionSender, ActionFuture) {
    let state = Arc::new(Mutex::new(Completion::default()));
    (ActionSender(Some(Arc::clone(&state))), ActionFuture(state))
}

impl ActionSender {
    /// Advisory cancellation for read-only background work. Mutations still run
    /// once admitted, even when their caller stops waiting for the result.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.as_ref().is_none_or(|state| {
            state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .receiver_dropped
        })
    }

    pub(crate) fn send(mut self, result: Result<Value>) {
        if let Some(state) = self.0.take() {
            complete(&state, result);
        }
    }
}

impl Drop for ActionSender {
    fn drop(&mut self) {
        if let Some(state) = self.0.take() {
            complete(
                &state,
                Err(anyhow::anyhow!(
                    "Rex action was interrupted before completion"
                )),
            );
        }
    }
}

fn complete(state: &Mutex<Completion>, result: Result<Value>) {
    let waiter = {
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        state.result = Some(result);
        state.waiter.take()
    };
    if let Some(waiter) = waiter {
        waiter.wake();
    }
}

impl Future for ActionFuture {
    type Output = Result<Value>;
    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(result) = state.result.take() {
            return Poll::Ready(result);
        }
        state.waiter = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for ActionFuture {
    fn drop(&mut self) {
        let waiter = {
            let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            state.receiver_dropped = true;
            state.waiter.take()
        };
        drop(waiter);
    }
}
