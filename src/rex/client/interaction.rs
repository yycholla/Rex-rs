//! Local owner-thread interaction; never sends terminal input or changes Rex state.
use super::{
    ActionFuture, ActionSender, BlockView, Cache, Mailbox, Operation, QueuedOperation,
    TerminalStream, WorkspaceClient, automation, next_operation, record_error,
};
use crate::core::PaneId;
use crate::core::interaction::{
    MAX_SEARCH_QUERY_BYTES, SearchCommand, SelectionCommand, TerminalAnnotations,
};
use anyhow::{Context as _, Result, bail, ensure};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, PoisonError,
        mpsc::{Receiver, TrySendError},
    },
};

#[derive(Clone)]
pub(super) struct SearchIntent {
    query: String,
    generation: u64,
}

pub(super) enum LocalOperation {
    Search(SearchCommand),
    Select(SelectionCommand),
    Copy(ActionSender),
    Link {
        column: u16,
        row: u16,
        viewport_token: u64,
        completion: ActionSender,
    },
}

impl WorkspaceClient {
    #[must_use]
    pub fn annotations(&self, pane: PaneId) -> Option<&Arc<TerminalAnnotations>> {
        self.cache.annotations.get(&pane)
    }

    #[must_use]
    pub fn search_query(&self, pane: PaneId) -> Option<(&str, u64)> {
        self.cache
            .searches
            .get(&pane)
            .filter(|intent| !intent.query.is_empty())
            .map(|intent| (intent.query.as_str(), intent.generation))
    }

    /// Update this client's local history search, including exited terminals.
    ///
    /// # Errors
    /// Rejects unavailable replicas, oversized queries and exhausted query slots.
    pub fn search(&mut self, pane: PaneId, command: SearchCommand) -> Result<()> {
        if !matches!(command, SearchCommand::Cancel { .. }) {
            self.check_local_replica(pane)?;
        }
        if let SearchCommand::Set { query, .. } = &command {
            ensure!(
                query.len() <= MAX_SEARCH_QUERY_BYTES,
                "Search query exceeds 1024 bytes"
            );
        }
        let generation = match &command {
            SearchCommand::Set { generation, .. } | SearchCommand::Cancel { generation } => {
                Some(*generation)
            }
            _ => None,
        };
        if let Some(generation) = generation {
            let mut mailbox = self.mailbox.lock().unwrap_or_else(PoisonError::into_inner);
            let cancel = matches!(command, SearchCommand::Cancel { .. });
            if cancel
                && self.state().pane(pane).is_none()
                && !mailbox.pending_search.contains_key(&pane)
            {
                return Ok(());
            }
            // Cancellation must remain admissible when every pending edit slot
            // is full. The authoritative workspace bounds live blocks to1024.
            let limit = if cancel { 1024 } else { 16 };
            ensure!(
                mailbox.pending_search.contains_key(&pane) || mailbox.pending_search.len() < limit,
                "Too many pending history searches"
            );
            if mailbox.pending_search.get(&pane).is_some_and(|queued| {
                matches!(&queued.operation, Operation::Local(_, LocalOperation::Search(
                    SearchCommand::Set { generation: old, .. } | SearchCommand::Cancel { generation: old }
                )) if *old > generation)
            }) {
                return Ok(());
            }
            mailbox.pending_search.insert(
                pane,
                QueuedOperation {
                    instance: self.state().instance_id().into(),
                    generation: self.cache.connection_generation,
                    operation: Operation::Local(pane, LocalOperation::Search(command)),
                },
            );
            return Ok(());
        }
        self.enqueue(Operation::Local(pane, LocalOperation::Search(command)))
    }

    /// Apply a local selection gesture without process input or size ownership.
    ///
    /// # Errors
    /// Rejects unavailable display replicas and full input queues.
    pub fn select(&mut self, pane: PaneId, command: SelectionCommand) -> Result<()> {
        if matches!(
            command,
            SelectionCommand::Release | SelectionCommand::Cancel | SelectionCommand::Clear
        ) && (self.state().is_retired_pane(pane) || self.state().is_detached(pane))
        {
            return Ok(());
        }
        self.check_local_replica(pane)?;
        if matches!(
            command,
            SelectionCommand::Release | SelectionCommand::Cancel | SelectionCommand::Clear
        ) {
            let queued = QueuedOperation {
                instance: self.state().instance_id().into(),
                generation: self.cache.connection_generation,
                operation: Operation::Local(pane, LocalOperation::Select(command)),
            };
            let mut mailbox = self.mailbox.lock().unwrap_or_else(PoisonError::into_inner);
            if mailbox.pending_focus.is_empty() {
                match self.commands.try_send(queued) {
                    Ok(()) => return Ok(()),
                    Err(TrySendError::Disconnected(_)) => bail!("Rex client worker stopped"),
                    Err(TrySendError::Full(queued)) => {
                        return defer_selection_end(&mut mailbox, queued, pane);
                    }
                }
            }
            return defer_selection_end(&mut mailbox, queued, pane);
        }
        self.enqueue(Operation::Local(pane, LocalOperation::Select(command)))
    }

    /// Execute a server callback's local copy while its originating Lua request
    /// waits. This separate lane never admits terminal input or server mutations.
    ///
    /// # Errors
    /// Rejects unavailable replicas and full callback queues.
    pub fn copy_callback_selection(&mut self, pane: PaneId) -> Result<ActionFuture> {
        self.check_local_replica(pane)?;
        let (sender, future) = automation::completion();
        let mut mailbox = self.mailbox.lock().unwrap_or_else(PoisonError::into_inner);
        ensure!(
            mailbox.callback_local.len() < 8,
            "Local callback queue is full"
        );
        mailbox.callback_local.push_back(QueuedOperation {
            instance: self.cache.state.instance_id().to_owned(),
            generation: self.cache.connection_generation,
            operation: Operation::Local(pane, LocalOperation::Copy(sender)),
        });
        Ok(future)
    }

    /// Resolve a user-activated OSC 8 hyperlink without process input.
    ///
    /// # Errors
    /// Rejects unavailable replicas and full queues; stale positions fail asynchronously.
    pub fn hyperlink_at(
        &mut self,
        pane: PaneId,
        column: u16,
        row: u16,
        viewport_token: u64,
    ) -> Result<ActionFuture> {
        self.check_local_replica(pane)?;
        let (completion, future) = automation::completion();
        self.enqueue(Operation::Local(
            pane,
            LocalOperation::Link {
                column,
                row,
                viewport_token,
                completion,
            },
        ))?;
        Ok(future)
    }

    /// Format the native selection asynchronously, preserving logical soft wraps.
    ///
    /// # Errors
    /// Rejects unavailable replicas or a full command queue; the future also
    /// reports selections exceeding the native copy work/byte limits.
    pub fn copy_selection(&mut self, pane: PaneId) -> Result<ActionFuture> {
        self.check_local_replica(pane)?;
        let (sender, future) = automation::completion();
        self.enqueue(Operation::Local(pane, LocalOperation::Copy(sender)))?;
        Ok(future)
    }

    fn check_local_replica(&self, pane: PaneId) -> Result<()> {
        ensure!(
            self.connected(),
            "Local history is unavailable while disconnected"
        );
        ensure!(
            self.state().pane(pane).is_some_and(BlockView::is_terminal)
                && !self.state().is_detached(pane)
                && self.frame(pane).is_some(),
            "Local terminal history is unavailable"
        );
        Ok(())
    }
}

pub(super) fn execute(
    cache: &mut Cache,
    streams: &mut BTreeMap<PaneId, TerminalStream>,
    pane: PaneId,
    operation: LocalOperation,
    mailbox: &Mutex<Mailbox>,
) {
    // A preceding layout action may have changed visibility before the next
    // stream poll. Local query/gesture operations must use that current state.
    if let Some(stream) = streams.get_mut(&pane) {
        stream.set_interaction_visible(super::visible_panes(&cache.state).contains(&pane));
    }
    if let LocalOperation::Link {
        column,
        row,
        viewport_token,
        completion,
    } = operation
    {
        if !completion.is_cancelled() {
            let result = streams
                .get_mut(&pane)
                .context("Local terminal history is unavailable")
                .and_then(|stream| stream.hyperlink_at(column, row, viewport_token))
                .map(|uri| json!({"uri":uri}));
            completion.send(result);
        }
        return;
    }
    if let LocalOperation::Copy(completion) = operation {
        if completion.is_cancelled() {
            return;
        }
        let result = streams
            .get(&pane)
            .context("Local terminal history is unavailable")
            .and_then(TerminalStream::copy_selection)
            .map(|text| json!({"text":text}));
        completion.send(result);
        return;
    }
    if cache.state.is_retired_pane(pane) || cache.state.is_detached(pane) {
        cache.searches.remove(&pane);
        return;
    }
    let result = match operation {
        LocalOperation::Search(command) => apply_search(cache, streams, pane, command),
        LocalOperation::Select(command) => streams
            .get_mut(&pane)
            .context("Local terminal history is unavailable")
            .and_then(|stream| stream.select(command)),
        LocalOperation::Copy(_) | LocalOperation::Link { .. } => return,
    };
    if let Err(error) = result {
        record_error(mailbox, format!("Local terminal interaction: {error:#}"));
    }
}

fn apply_search(
    cache: &mut Cache,
    streams: &mut BTreeMap<PaneId, TerminalStream>,
    pane: PaneId,
    command: SearchCommand,
) -> Result<()> {
    match &command {
        SearchCommand::Set { query, generation } => {
            if cache
                .searches
                .get(&pane)
                .is_some_and(|old| old.generation > *generation)
            {
                return Ok(());
            }
            ensure!(
                query.is_empty()
                    || cache
                        .searches
                        .get(&pane)
                        .is_some_and(|old| !old.query.is_empty())
                    || cache
                        .searches
                        .values()
                        .filter(|intent| !intent.query.is_empty())
                        .count()
                        < 16,
                "This client supports at most 16 active history searches"
            );
            cache.searches.insert(
                pane,
                SearchIntent {
                    query: query.clone(),
                    generation: *generation,
                },
            );
        }
        SearchCommand::Cancel { generation } => {
            if cache
                .searches
                .get(&pane)
                .is_some_and(|old| old.generation > *generation)
            {
                return Ok(());
            }
            cache.searches.insert(
                pane,
                SearchIntent {
                    query: String::new(),
                    generation: *generation,
                },
            );
        }
        SearchCommand::Next { generation } | SearchCommand::Previous { generation } => {
            if cache
                .searches
                .get(&pane)
                .is_none_or(|intent| intent.generation != *generation || intent.query.is_empty())
            {
                return Ok(());
            }
        }
    }
    if let Some(stream) = streams.get_mut(&pane) {
        stream.search(command)?;
    }
    Ok(())
}

pub(super) fn restore(stream: &mut TerminalStream, cache: &Cache, pane: PaneId) -> Result<()> {
    if let Some(intent) = cache.searches.get(&pane) {
        let command = if intent.query.is_empty() {
            SearchCommand::Cancel {
                generation: intent.generation,
            }
        } else {
            SearchCommand::Set {
                query: intent.query.clone(),
                generation: intent.generation,
            }
        };
        stream.search(command)?;
    }
    Ok(())
}

pub(super) fn retain(cache: &mut Cache, placed: &[PaneId]) {
    cache.searches.retain(|pane, _| placed.contains(pane));
    cache.annotations.retain(|pane, _| placed.contains(pane));
}

pub(super) fn is_query_update(operation: &Operation) -> bool {
    matches!(
        operation,
        Operation::Local(
            _,
            LocalOperation::Search(SearchCommand::Set { .. } | SearchCommand::Cancel { .. })
        )
    )
}

pub(super) fn drain_disconnected(
    cache: &mut Cache,
    commands: &Receiver<QueuedOperation>,
    mailbox: &Mutex<Mailbox>,
) -> bool {
    let mut changed = false;
    while let Some(queued) = next_operation(commands, mailbox) {
        if queued.instance == cache.state.instance_id() && is_query_update(&queued.operation) {
            if let Operation::Local(pane, operation) = queued.operation {
                execute(cache, &mut BTreeMap::new(), pane, operation, mailbox);
                changed = true;
            }
        } else if !matches!(queued.operation, Operation::Local(..)) {
            record_error(
                mailbox,
                "Rex disconnected; queued operation was not sent".into(),
            );
        }
    }
    changed
}

fn defer_selection_end(mailbox: &mut Mailbox, queued: QueuedOperation, pane: PaneId) -> Result<()> {
    if let Some(previous) = mailbox.pending_focus.iter_mut().find(|old| {
        old.instance == queued.instance && old.generation == queued.generation
            && matches!(old.operation, Operation::Local(id, LocalOperation::Select(SelectionCommand::Release | SelectionCommand::Cancel | SelectionCommand::Clear)) if id == pane)
    }) {
        let priority = |operation: &Operation| match operation {
            Operation::Local(_, LocalOperation::Select(SelectionCommand::Clear)) => 3,
            Operation::Local(_, LocalOperation::Select(SelectionCommand::Cancel)) => 2,
            _ => 1,
        };
        if priority(&queued.operation) > priority(&previous.operation) {
            previous.operation = queued.operation;
        }
        return Ok(());
    }
    ensure!(
        mailbox.pending_focus.len() < 32,
        "Rex terminal notification queue is full"
    );
    mailbox.pending_focus.push_back(queued);
    Ok(())
}

#[cfg(test)]
mod tests;
