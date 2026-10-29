//! Queue/lifecycle behavior without native terminals, sockets, or user sessions.
use super::*;
use crate::core::frame::{Cell, Rgb};
use crate::{
    core::{GridSize, frame::TerminalFrame},
    rex::{client::COMMAND_CAPACITY, workspace::WorkspaceState},
};
use std::{
    future::Future as _,
    pin::Pin,
    sync::{atomic::AtomicBool, mpsc},
    task::{Context as TaskContext, Poll, Waker},
};

fn fixture() -> Result<(WorkspaceClient, Receiver<QueuedOperation>, Vec<PaneId>)> {
    let windows: Vec<_> = (0..20)
        .map(|index| {
            let block = format!("block:{index}");
            json!({"window_id":format!("window:{index}"),"label":"Shell","focused_block_id":block,
            "layers":[{"layer_id":format!("layer:{index}"),"kind":"tiled","bounds":[0,0,1,1],
                "layout":{"block_id":block,"rect":[0,0,1,1]},
                "blocks":[{"block_id":block,"creator_name":"com.superlogical.terminal"}]}]})
        })
        .collect();
    let view = json!({"session_id":"session:a","label":"Work","revision":1,
        "active_window_id":"window:0","windows":windows});
    let mut state = WorkspaceState::default();
    state.apply_bootstrap(&json!({"server_status":{"instance_id":"instance:a"},
        "client_id":"client:a","attach_all":{"sessions":[{"session_id":"session:a"}]},
        "attached_sessions":[{"view":view}]}))?;
    let panes = state.placed_pane_ids();
    ensure!(
        panes.len() == 20,
        "fixture did not retain valid terminal panes"
    );
    let frame = Arc::new(TerminalFrame {
        images: Arc::default(),
        graphics_warning: None,
        size: GridSize::new(1, 1)?,
        rows: vec![Arc::new(vec![Cell::default()])],
        cursor: None,
        foreground: Rgb::default(),
        background: Rgb::default(),
    });
    let (commands, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
    let client = WorkspaceClient {
        cache: Cache {
            state,
            connected: true,
            connection_generation: 7,
            frames: panes
                .iter()
                .map(|pane| (*pane, Arc::clone(&frame)))
                .collect(),
            ..Cache::default()
        },
        mailbox: Arc::new(Mutex::new(Mailbox::default())),
        commands,
        stopped: Arc::new(AtomicBool::new(false)),
    };
    Ok((client, receiver, panes))
}

fn set(query: &str, generation: u64) -> SearchCommand {
    SearchCommand::Set {
        query: query.into(),
        generation,
    }
}

fn apply_queued(client: &mut WorkspaceClient, queued: QueuedOperation) -> Result<()> {
    let Operation::Local(pane, operation) = queued.operation else {
        anyhow::bail!("expected local terminal operation");
    };
    execute(
        &mut client.cache,
        &mut BTreeMap::new(),
        pane,
        operation,
        &client.mailbox,
    );
    Ok(())
}

#[test]
fn newer_cancel_preempts_saturated_input_and_old_query_cannot_resurrect_it() -> Result<()> {
    let (mut client, receiver, panes) = fixture()?;
    let pane = panes[0];
    for index in 0..COMMAND_CAPACITY {
        client.enqueue(Operation::Text(pane, index.to_string()))?;
    }
    client.search(pane, set("first", 10))?;
    client.search(pane, set("replacement", 11))?;
    client.search(pane, SearchCommand::Cancel { generation: 12 })?;
    client.search(pane, set("stale", 11))?;
    assert_eq!(
        client
            .mailbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending_search
            .len(),
        1
    );
    let queued = next_operation(&receiver, &client.mailbox).context("priority cancel")?;
    assert!(
        matches!(&queued.operation,Operation::Local(id,LocalOperation::Search(SearchCommand::Cancel { generation:12 })) if *id==pane)
    );
    apply_queued(&mut client, queued)?;
    assert_eq!(client.search_query(pane), None);
    apply_search(
        &mut client.cache,
        &mut BTreeMap::new(),
        pane,
        set("old delivery", 10),
    )?;
    assert_eq!(client.cache.searches[&pane].query, "");
    assert_eq!(client.cache.searches[&pane].generation, 12);
    for index in 0..COMMAND_CAPACITY {
        let queued = next_operation(&receiver, &client.mailbox).context("accepted input")?;
        assert!(
            matches!(queued.operation,Operation::Text(id,text) if id==pane && text==index.to_string())
        );
    }
    assert!(next_operation(&receiver, &client.mailbox).is_none());
    Ok(())
}

#[test]
fn active_query_limit_survives_pending_drain_and_allows_replacement_and_cancel() -> Result<()> {
    let (mut client, receiver, panes) = fixture()?;
    for pane in &panes[..16] {
        client.search(*pane, set("find", 1))?;
        let queued = next_operation(&receiver, &client.mailbox).context("queued search")?;
        apply_queued(&mut client, queued)?;
    }
    assert!(
        client
            .mailbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending_search
            .is_empty()
    );
    assert!(
        apply_search(
            &mut client.cache,
            &mut BTreeMap::new(),
            panes[16],
            set("seventeenth", 1)
        )
        .is_err()
    );
    assert!(!client.cache.searches.contains_key(&panes[16]));
    apply_search(
        &mut client.cache,
        &mut BTreeMap::new(),
        panes[0],
        set("replacement", 2),
    )?;
    assert_eq!(client.search_query(panes[0]), Some(("replacement", 2)));
    apply_search(
        &mut client.cache,
        &mut BTreeMap::new(),
        panes[0],
        SearchCommand::Cancel { generation: 3 },
    )?;
    apply_search(
        &mut client.cache,
        &mut BTreeMap::new(),
        panes[16],
        set("new slot", 1),
    )?;
    apply_search(
        &mut client.cache,
        &mut BTreeMap::new(),
        panes[0],
        set("stale replacement", 2),
    )?;
    assert_eq!(client.search_query(panes[0]), None);
    assert_eq!(
        client
            .cache
            .searches
            .values()
            .filter(|intent| !intent.query.is_empty())
            .count(),
        16
    );
    Ok(())
}

fn press() -> SelectionCommand {
    SelectionCommand::Press {
        column: 0.25,
        row: 0.5,
        time_millis: 1000,
        viewport_token: 1,
    }
}

fn drag() -> SelectionCommand {
    SelectionCommand::Drag {
        column: 0.75,
        row: -1.,
        rectangle: false,
        viewport_token: 1,
    }
}

#[test]
fn selection_end_tail_follows_accepted_press_and_drag_and_blocks_overtaking() -> Result<()> {
    for end in [
        SelectionCommand::Release,
        SelectionCommand::Cancel,
        SelectionCommand::Clear,
    ] {
        let (mut client, receiver, panes) = fixture()?;
        let pane = panes[0];
        client.select(pane, press())?;
        client.select(pane, drag())?;
        for _ in 2..COMMAND_CAPACITY {
            client.enqueue(Operation::Text(pane, "accepted".into()))?;
        }
        client.select(pane, end)?;
        let first = next_operation(&receiver, &client.mailbox).context("press")?;
        assert!(
            matches!(first.operation,Operation::Local(id,LocalOperation::Select(command)) if id==pane && command==press())
        );
        // Even with one normal slot free, a new gesture cannot overtake its end.
        assert!(client.select(pane, press()).is_err());
        let second = next_operation(&receiver, &client.mailbox).context("drag")?;
        assert!(
            matches!(second.operation,Operation::Local(id,LocalOperation::Select(command)) if id==pane && command==drag())
        );
        for _ in 2..COMMAND_CAPACITY {
            assert!(matches!(
                next_operation(&receiver, &client.mailbox)
                    .context("accepted input")?
                    .operation,
                Operation::Text(..)
            ));
        }
        let final_event =
            next_operation(&receiver, &client.mailbox).context("reliable gesture end")?;
        assert!(
            matches!(final_event.operation,Operation::Local(id,LocalOperation::Select(command)) if id==pane && command==end)
        );
        assert!(next_operation(&receiver, &client.mailbox).is_none());
        client.select(pane, press())?;
    }
    Ok(())
}

#[test]
fn accepted_clear_is_not_lost_to_later_deferred_release() -> Result<()> {
    let (mut client, receiver, panes) = fixture()?;
    let pane = panes[0];
    for _ in 0..COMMAND_CAPACITY {
        client.enqueue(Operation::Text(pane, "accepted".into()))?;
    }
    client.select(pane, SelectionCommand::Clear)?;
    client.select(pane, SelectionCommand::Release)?;
    for _ in 0..COMMAND_CAPACITY {
        receiver.try_recv()?;
    }
    let queued = next_operation(&receiver, &client.mailbox).context("selection clear")?;
    assert!(
        matches!(queued.operation,Operation::Local(id,LocalOperation::Select(SelectionCommand::Clear)) if id==pane)
    );
    assert!(next_operation(&receiver, &client.mailbox).is_none());
    Ok(())
}

#[test]
fn disconnected_drain_preserves_same_instance_query_intent_but_drops_other_work() -> Result<()> {
    let (mut client, receiver, panes) = fixture()?;
    let pane = panes[0];
    let queued = |operation| QueuedOperation {
        instance: "instance:a".into(),
        generation: 6,
        operation: Operation::Local(pane, operation),
    };
    client
        .commands
        .try_send(queued(LocalOperation::Search(set("resume intent", 10))))?;
    client
        .commands
        .try_send(queued(LocalOperation::Select(press())))?;
    client
        .commands
        .try_send(queued(LocalOperation::Search(SearchCommand::Next {
            generation: 10,
        })))?;
    let (sender, mut copy) = automation::completion();
    client
        .commands
        .try_send(queued(LocalOperation::Copy(sender)))?;
    client.commands.try_send(QueuedOperation {
        instance: "instance:foreign".into(),
        generation: 7,
        operation: Operation::Local(pane, LocalOperation::Search(set("wrong server", 99))),
    })?;
    client.cache.connected = false;
    assert!(drain_disconnected(
        &mut client.cache,
        &receiver,
        &client.mailbox
    ));
    assert_eq!(client.search_query(pane), Some(("resume intent", 10)));
    let mut context = TaskContext::from_waker(Waker::noop());
    assert!(matches!(
        Pin::new(&mut copy).poll(&mut context),
        Poll::Ready(Err(_))
    ));
    assert!(client.select(pane, press()).is_err());
    assert!(
        client
            .search(pane, SearchCommand::Next { generation: 10 })
            .is_err()
    );
    assert!(client.copy_selection(pane).is_err());
    assert!(next_operation(&receiver, &client.mailbox).is_none());
    Ok(())
}

#[test]
fn cancel_while_disconnected_replaces_owned_intent_and_fences_old_delivery() -> Result<()> {
    let (mut client, receiver, panes) = fixture()?;
    let pane = panes[0];
    apply_search(
        &mut client.cache,
        &mut BTreeMap::new(),
        pane,
        set("retained", 10),
    )?;
    client.cache.connected = false;
    client.search(pane, SearchCommand::Cancel { generation: 11 })?;
    assert!(drain_disconnected(
        &mut client.cache,
        &receiver,
        &client.mailbox
    ));
    assert_eq!(client.search_query(pane), None);
    assert_eq!(client.cache.searches[&pane].generation, 11);
    client.cache.connected = true;
    client.cache.connection_generation += 1;
    apply_search(
        &mut client.cache,
        &mut BTreeMap::new(),
        pane,
        set("late previous attachment", 10),
    )?;
    assert_eq!(client.search_query(pane), None);
    client.search(pane, set("fresh attachment", 12))?;
    let queued = next_operation(&receiver, &client.mailbox).context("new query")?;
    assert_eq!(queued.generation, 8);
    apply_queued(&mut client, queued)?;
    assert_eq!(client.search_query(pane), Some(("fresh attachment", 12)));
    Ok(())
}

#[test]
fn cancellation_remains_admissible_when_all_sixteen_pending_edit_slots_are_full() -> Result<()> {
    let (mut client, receiver, panes) = fixture()?;
    let active = panes[16];
    apply_search(
        &mut client.cache,
        &mut BTreeMap::new(),
        active,
        set("already active", 1),
    )?;
    for pane in &panes[..16] {
        client.search(*pane, set("pending edit", 1))?;
    }
    assert!(client.search(panes[17], set("edit overflow", 1)).is_err());
    client.search(active, SearchCommand::Cancel { generation: 2 })?;
    assert_eq!(
        client
            .mailbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending_search
            .len(),
        17
    );
    while let Some(queued) = next_operation(&receiver, &client.mailbox) {
        apply_queued(&mut client, queued)?;
    }
    assert_eq!(client.search_query(active), None);
    assert_eq!(client.cache.searches[&active].generation, 2);
    apply_search(
        &mut client.cache,
        &mut BTreeMap::new(),
        active,
        set("late old edit", 1),
    )?;
    assert_eq!(client.search_query(active), None);
    Ok(())
}
