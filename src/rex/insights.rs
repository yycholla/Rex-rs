//! Read-only session activity: attached clients and each terminal's process.
//!
//! Rex reports these through `session.list_clients` and the terminal block's
//! `process` method. Neither is part of the layout stream, so the desktop
//! requests them on demand for the session overview.
use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use super::{
    automation::ActionSender,
    transport::{ControlHandle, PendingCall},
    workspace::WorkspaceState,
};
use crate::core::SessionId;
use anyhow::{Context as _, Result};
use serde_json::{Map, Value, json};

/// Bounds the per-refresh `process` calls so a large session cannot hold the
/// ordered worker that also carries terminal input.
const MAX_PROCESS_QUERIES: usize = 32;

/// Interactive shells: a pane whose foreground program is one of these is idle.
const SHELLS: &[&str] = &[
    "bash", "csh", "dash", "elvish", "fish", "ksh", "nu", "pwsh", "sh", "tcsh", "xonsh", "zsh",
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionInsights {
    pub clients: Vec<ClientSummary>,
    /// Activity by remote block ID.
    pub blocks: BTreeMap<String, BlockActivity>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientSummary {
    pub id: String,
    /// Application name and version, such as "rex-rs 0.1.0".
    pub name: String,
    /// Client kind reported by Rex, such as "app" or "cli".
    pub kind: String,
    /// Where the client connects from, such as "uid 1000" or a tailnet peer.
    pub principal: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockActivity {
    /// Foreground program name, absent once the process has exited.
    pub program: Option<String>,
    /// A program other than an interactive shell holds the foreground.
    pub busy: bool,
    /// Working directory of the foreground program, with the home prefix as `~`.
    pub cwd: Option<String>,
    pub last_exit: Option<i64>,
}

impl BlockActivity {
    #[must_use]
    pub fn failed(&self) -> bool {
        self.program.is_none() && self.last_exit.is_some_and(|code| code != 0)
    }
}

impl SessionInsights {
    /// Parses the completed refresh value. Unknown or malformed entries are
    /// skipped so one odd block cannot hide the rest of the overview.
    #[must_use]
    pub fn from_value(value: &Value) -> Self {
        let clients = value
            .pointer("/clients/clients")
            .and_then(Value::as_array)
            .map(|clients| clients.iter().filter_map(client_summary).collect())
            .unwrap_or_default();
        let blocks = value
            .get("processes")
            .and_then(Value::as_object)
            .map(|processes| {
                processes
                    .iter()
                    .map(|(id, process)| (id.clone(), block_activity(process)))
                    .collect()
            })
            .unwrap_or_default();
        Self { clients, blocks }
    }

    #[must_use]
    pub fn activity(&self, remote_id: &str) -> Option<&BlockActivity> {
        self.blocks.get(remote_id)
    }
}

fn client_summary(value: &Value) -> Option<ClientSummary> {
    let text = |pointer: &str| value.pointer(pointer).and_then(Value::as_str);
    let id = text("/client_id")?.to_owned();
    let name = match (text("/info/name"), text("/info/version")) {
        (Some(name), Some(version)) => format!("{name} {version}"),
        (Some(name), None) => name.to_owned(),
        _ => "Unknown client".to_owned(),
    };
    Some(ClientSummary {
        id,
        name,
        kind: text("/info/kind").unwrap_or_default().to_owned(),
        principal: text("/principal/display").unwrap_or_default().to_owned(),
    })
}

fn block_activity(value: &Value) -> BlockActivity {
    let foreground = value.get("foreground").filter(|entry| entry.is_object());
    let child_pid = value.pointer("/child/pid").and_then(Value::as_i64);
    let program = foreground
        .and_then(|entry| entry.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let shell = program
        .as_deref()
        .is_some_and(|name| SHELLS.contains(&name));
    let foreground_pid = foreground
        .and_then(|entry| entry.get("pid"))
        .and_then(Value::as_i64);
    // A job inside the shell, or a command block that is not a shell itself.
    let busy = program.is_some() && (foreground_pid != child_pid || !shell);
    let home = value.get("home").and_then(Value::as_str);
    let cwd = foreground
        .and_then(|entry| entry.get("cwd"))
        .and_then(Value::as_str)
        .map(|cwd| abbreviate_home(cwd, home));
    BlockActivity {
        program,
        busy,
        cwd,
        last_exit: value
            .pointer("/last_exit/exit_code")
            .and_then(Value::as_i64),
    }
}

fn abbreviate_home(path: &str, home: Option<&str>) -> String {
    match home.and_then(|home| path.strip_prefix(home)) {
        Some("") => "~".to_owned(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_owned(),
    }
}

/// One bounded, connection-local metadata lane. Terminal commands never wait for it.
#[derive(Default)]
pub(crate) struct RefreshQueue {
    jobs: VecDeque<Refresh>,
}

const MAX_REFRESHES: usize = 4;

struct Refresh {
    session: SessionId,
    job: Job,
    completion: Option<ActionSender>,
}

impl RefreshQueue {
    pub(crate) fn enqueue(
        &mut self,
        state: &WorkspaceState,
        session: SessionId,
        completion: ActionSender,
    ) {
        if completion.is_cancelled() {
            return;
        }
        if self.jobs.len() >= MAX_REFRESHES {
            completion.send(Err(anyhow::anyhow!("Rex insights refresh queue is full")));
            return;
        }
        match Job::new(state, session) {
            Ok(job) => self.jobs.push_back(Refresh {
                session,
                job,
                completion: Some(completion),
            }),
            Err(error) => completion.send(Err(error)),
        }
    }

    /// Poll once and admit at most one metadata request per worker turn.
    pub(crate) fn service(&mut self, handle: &ControlHandle, state: &WorkspaceState) {
        // Cancel queued work promptly too. An already admitted request must drain:
        // dropping PendingCall does not cancel its transport admission.
        self.jobs.retain_mut(|refresh| {
            let cancelled = refresh
                .completion
                .as_ref()
                .is_some_and(ActionSender::is_cancelled)
                || !state.sessions().iter().any(|session| {
                    session.id() == refresh.session && session.remote_id() == refresh.job.session_id
                });
            if cancelled {
                if let Some(sender) = refresh.completion.take() {
                    sender.send(Err(anyhow::anyhow!("Rex insights refresh was cancelled")));
                }
                refresh.job.cancelled = true;
            }
            !refresh.job.cancelled || refresh.job.pending.is_some()
        });
        let Some(refresh) = self.jobs.front_mut() else {
            return;
        };
        if let Poll::Ready(result) = refresh
            .job
            .advance(|method, params| handle.request(method, params))
            && let Some(mut completed) = self.jobs.pop_front()
            && let Some(sender) = completed.completion.take()
        {
            sender.send(result);
        }
    }
}

struct Job<F = PendingCall> {
    session_id: String,
    blocks: VecDeque<String>,
    clients: Option<Value>,
    processes: Map<String, Value>,
    pending: Option<(Query, F)>,
    cancelled: bool,
}

enum Query {
    Clients,
    Process(String),
}

impl<F> Job<F> {
    fn new(state: &WorkspaceState, session: SessionId) -> Result<Self> {
        let view = state
            .sessions()
            .iter()
            .find(|entry| entry.id() == session)
            .context("Rex session no longer exists")?;
        let placed = view
            .windows()
            .iter()
            .filter_map(super::workspace::WindowView::layout)
            .flat_map(super::workspace::ViewLayout::panes)
            .filter_map(|pane| state.pane(pane));
        let blocks = placed
            .chain(state.detached_blocks(session))
            .filter(|block| block.is_terminal())
            .map(|block| block.remote_id.clone())
            .take(MAX_PROCESS_QUERIES)
            .collect();
        Ok(Self {
            session_id: view.remote_id().to_owned(),
            blocks,
            clients: None,
            processes: Map::new(),
            pending: None,
            cancelled: false,
        })
    }
}

impl<F: Future<Output = Result<Value>> + Unpin> Job<F> {
    fn advance(&mut self, request: impl FnOnce(&str, Value) -> Result<F>) -> Poll<Result<Value>> {
        if let Some((_, pending)) = &mut self.pending {
            let mut context = Context::from_waker(Waker::noop());
            let Poll::Ready(result) = Pin::new(pending).poll(&mut context) else {
                return Poll::Pending;
            };
            let Some((query, _)) = self.pending.take() else {
                return Poll::Ready(Err(anyhow::anyhow!("missing polled metadata request")));
            };
            if !self.cancelled {
                self.accept(query, result)?;
            }
        }
        if self.cancelled {
            return Poll::Ready(Err(anyhow::anyhow!("Rex insights refresh was cancelled")));
        }
        let (query, method, params) = if self.clients.is_none() {
            (
                Query::Clients,
                "session.list_clients",
                json!({"session_id":self.session_id}),
            )
        } else if let Some(block) = self.blocks.pop_front() {
            let params = json!({"session_id":self.session_id,"block_id":block,"method":"process"});
            (Query::Process(block), "block.method", params)
        } else {
            return Poll::Ready(Ok(
                json!({"clients":self.clients.take(),"processes":std::mem::take(&mut self.processes)}),
            ));
        };
        match request(method, params) {
            Ok(pending) => self.pending = Some((query, pending)),
            Err(error) => {
                self.accept(query, Err(error))?;
            }
        }
        Poll::Pending
    }

    fn accept(&mut self, query: Query, result: Result<Value>) -> Result<()> {
        match query {
            Query::Clients => self.clients = Some(result?),
            Query::Process(block) => {
                // Preserve the original policy: a closed or failed block is omitted.
                if let Ok(process) = result {
                    self.processes.insert(block, process);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    type Slot = Arc<Mutex<Option<Result<Value>>>>;

    struct FakeCall(Slot);

    impl Future for FakeCall {
        type Output = Result<Value>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .map_or(Poll::Pending, Poll::Ready)
        }
    }

    #[derive(Default)]
    struct FakeControl {
        requests: Vec<(String, Value, Slot)>,
    }

    impl FakeControl {
        fn request(&mut self, method: &str, params: Value) -> Result<FakeCall> {
            anyhow::ensure!(self.requests.len() < 64, "fake control queue is full");
            let slot = Arc::new(Mutex::new(None));
            self.requests
                .push((method.to_owned(), params, Arc::clone(&slot)));
            Ok(FakeCall(slot))
        }
        fn finish(&self, index: usize, value: Result<Value>) {
            *self.requests[index]
                .2
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(value);
        }
    }

    fn test_state() -> Result<(WorkspaceState, SessionId)> {
        let mut state = WorkspaceState::default();
        state.apply_view(
            &json!({"session_id":"session:a","label":"Work","revision":1,"windows":[]}),
        )?;
        let blocks: Vec<_> = (0..40)
            .map(|index| {
                json!({"block_id":format!("block:{index:02}"),"label":format!("Block {index}"),
            "creator_name":"com.superlogical.terminal","placement":"detached"})
            })
            .collect();
        state.apply_block_list("session:a", &json!({"blocks":blocks}))?;
        let session = state.sessions().first().context("test session")?.id();
        Ok((state, session))
    }

    #[test]
    fn delayed_metadata_yields_and_cancelled_request_drains_without_more_queries() -> Result<()> {
        let (state, session) = test_state()?;
        let mut job = Job::<FakeCall>::new(&state, session)?;
        let mut control = FakeControl::default();
        assert!(
            job.advance(|method, params| control.request(method, params))
                .is_pending()
        );
        for _ in 0..20 {
            assert!(
                job.advance(|method, params| control.request(method, params))
                    .is_pending()
            );
        }
        assert_eq!(control.requests.len(), 1);
        let mut input = control.request("session.key", json!({"text":"x"}))?;
        control.finish(1, Ok(json!({"written":true})));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(
            Pin::new(&mut input).poll(&mut context),
            Poll::Ready(Ok(_))
        ));
        // Cancellation cannot unsend this one RPC; keep draining it without
        // admitting more metadata or blocking the independently delivered key.
        job.cancelled = true;
        assert!(
            job.advance(|method, params| control.request(method, params))
                .is_pending()
        );
        control.finish(0, Ok(json!({"clients":[]})));
        assert!(matches!(
            job.advance(|method, params| control.request(method, params)),
            Poll::Ready(Err(_))
        ));
        assert_eq!(control.requests.len(), 2);
        Ok(())
    }

    #[test]
    fn refresh_returns_complete_original_shape_for_all_thirty_two_selected_blocks() -> Result<()> {
        let (state, session) = test_state()?;
        let mut job = Job::<FakeCall>::new(&state, session)?;
        assert_eq!(job.blocks.len(), MAX_PROCESS_QUERIES);
        let expected: Vec<_> = job.blocks.iter().cloned().collect();
        let mut control = FakeControl::default();
        assert!(
            job.advance(|method, params| control.request(method, params))
                .is_pending()
        );
        control.finish(0, Ok(json!({"clients":[{"client_id":"client:a"}]})));
        for (index, block) in expected.iter().enumerate() {
            assert!(
                job.advance(|method, params| control.request(method, params))
                    .is_pending()
            );
            assert_eq!(control.requests.len(), index + 2);
            assert_eq!(control.requests[index + 1].1["block_id"], *block);
            control.finish(index + 1, Ok(json!({"foreground":{"name":block}})));
        }
        let Poll::Ready(result) = job.advance(|method, params| control.request(method, params))
        else {
            anyhow::bail!("complete refresh still pending");
        };
        let result = result?;
        assert_eq!(result["clients"]["clients"][0]["client_id"], "client:a");
        assert_eq!(
            result["processes"]
                .as_object()
                .context("process object")?
                .len(),
            32
        );
        assert_eq!(control.requests.len(), 33);
        Ok(())
    }

    #[test]
    fn refresh_queue_rejects_overload_and_drop_completes_disconnected_waiters() -> Result<()> {
        let (state, session) = test_state()?;
        let mut queue = RefreshQueue::default();
        let mut futures = Vec::new();
        for _ in 0..=MAX_REFRESHES {
            let (sender, future) = super::super::automation::completion();
            queue.enqueue(&state, session, sender);
            futures.push(future);
        }
        assert_eq!(queue.jobs.len(), MAX_REFRESHES);
        let mut context = Context::from_waker(Waker::noop());
        let mut overload = futures.pop().context("overload future")?;
        assert!(matches!(
            Pin::new(&mut overload).poll(&mut context),
            Poll::Ready(Err(_))
        ));
        drop(queue);
        for mut future in futures {
            assert!(matches!(
                Pin::new(&mut future).poll(&mut context),
                Poll::Ready(Err(_))
            ));
        }
        Ok(())
    }

    #[test]
    fn dropped_receiver_cancels_before_any_metadata_admission() -> Result<()> {
        let (state, session) = test_state()?;
        let mut queue = RefreshQueue::default();
        let (sender, future) = super::super::automation::completion();
        drop(future);
        queue.enqueue(&state, session, sender);
        assert!(queue.jobs.is_empty());
        Ok(())
    }

    #[test]
    fn failed_clients_abort_but_failed_process_is_omitted() -> Result<()> {
        let (state, session) = test_state()?;
        let mut job = Job::<FakeCall>::new(&state, session)?;
        let mut control = FakeControl::default();
        assert!(
            job.advance(|method, params| control.request(method, params))
                .is_pending()
        );
        control.finish(0, Err(anyhow::anyhow!("clients unavailable")));
        assert!(matches!(
            job.advance(|method, params| control.request(method, params)),
            Poll::Ready(Err(_))
        ));
        let mut job = Job::<FakeCall>::new(&state, session)?;
        job.blocks.truncate(1);
        assert!(
            job.advance(|method, params| control.request(method, params))
                .is_pending()
        );
        control.finish(1, Ok(json!({"clients":[]})));
        assert!(
            job.advance(|method, params| control.request(method, params))
                .is_pending()
        );
        control.finish(2, Err(anyhow::anyhow!("closed block")));
        let Poll::Ready(result) = job.advance(|method, params| control.request(method, params))
        else {
            anyhow::bail!("failed process did not finish refresh");
        };
        assert_eq!(result?["processes"], json!({}));
        Ok(())
    }

    fn process(child: Option<(i64, &str)>, foreground: Option<(i64, &str)>) -> Value {
        let entry =
            |(pid, name): (i64, &str)| json!({"pid":pid,"name":name,"cwd":"/home/me/src/rex"});
        json!({
            "last_exit": null,
            "child": child.map(entry),
            "foreground": foreground.map(entry),
            "home": "/home/me",
        })
    }

    #[test]
    fn idle_shell_is_not_busy_and_cwd_uses_home_shorthand() {
        let activity = block_activity(&process(Some((10, "fish")), Some((10, "fish"))));
        assert_eq!(activity.program.as_deref(), Some("fish"));
        assert!(!activity.busy);
        assert_eq!(activity.cwd.as_deref(), Some("~/src/rex"));
    }

    #[test]
    fn job_in_shell_and_command_block_are_busy() {
        assert!(block_activity(&process(Some((10, "zsh")), Some((11, "nvim")))).busy);
        assert!(block_activity(&process(Some((12, "sleep")), Some((12, "sleep")))).busy);
    }

    #[test]
    fn exited_block_reports_failure_only_for_nonzero_codes() {
        let exited = |code| json!({"last_exit":{"exit_code":code,"error":"exit"},"home":"/h"});
        let failed = block_activity(&exited(3));
        assert_eq!(failed.program, None);
        assert_eq!(failed.last_exit, Some(3));
        assert!(failed.failed());
        assert!(!block_activity(&exited(0)).failed());
    }

    #[test]
    fn home_prefix_must_end_at_a_path_boundary() {
        assert_eq!(abbreviate_home("/home/me", Some("/home/me")), "~");
        assert_eq!(
            abbreviate_home("/home/meadow", Some("/home/me")),
            "/home/meadow"
        );
        assert_eq!(abbreviate_home("/tmp", None), "/tmp");
    }

    #[test]
    fn insights_skip_malformed_clients() {
        let value = json!({
            "clients": {"clients": [
                {"client_id":"client:a","principal":{"display":"uid 1000"},
                 "info":{"kind":"app","name":"rex-rs","version":"0.1.0"}},
                {"principal":{"display":"missing id"}},
            ]},
            "processes": {"block:a": process(Some((1, "bash")), Some((1, "bash")))},
        });
        let insights = SessionInsights::from_value(&value);
        assert_eq!(insights.clients.len(), 1);
        assert_eq!(insights.clients[0].name, "rex-rs 0.1.0");
        assert_eq!(insights.clients[0].principal, "uid 1000");
        assert!(
            insights
                .activity("block:a")
                .is_some_and(|block| !block.busy)
        );
    }
}
