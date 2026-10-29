use super::*;
use std::{
    io::Read as _,
    os::unix::net::{UnixListener, UnixStream},
    thread,
};
use tungstenite::protocol::Role;

fn replica() -> Result<StreamDecoder> {
    let mut terminal = GhosttyTerminal::new(crate::terminal::TerminalConfig::default())?;
    terminal.feed(b"hello world\r\nhello again");
    let snapshot = terminal.snapshot_bytes()?;
    let mut decoder = StreamDecoder::new(42);
    decoder.push(&snapshot)?;
    Ok(decoder)
}

fn stream(decoder: StreamDecoder) -> Result<(TerminalStream, UnixStream)> {
    let (local, peer) = UnixStream::pair()?;
    Ok((
        TerminalStream {
            socket: WebSocket::from_raw_socket(RexStream::Unix(local), Role::Client, None),
            endpoint: String::new(),
            welcome: Welcome {
                token: [7; 16],
                nonce: 42,
            },
            received: 123,
            decoder,
            resumable: true,
            last_materialized: None,
        },
        peer,
    ))
}

fn select_word(stream: &mut TerminalStream) -> Result<()> {
    let viewport_token = stream.annotations().viewport_token;
    for time_millis in [1000, 1100] {
        stream.select(SelectionCommand::Press {
            column: 2.5,
            row: 0.5,
            time_millis,
            viewport_token,
        })?;
        stream.select(SelectionCommand::Release)?;
    }
    Ok(())
}

#[test]
fn local_selection_publishes_without_frame_or_server_input_after_exit() -> Result<()> {
    let (mut stream, mut peer) = stream(replica()?)?;
    stream.decoder.exit_code = Some(0);
    let frame = Arc::clone(stream.decoder.last_frame.as_ref().context("frame")?);
    let before = stream.annotations();
    select_word(&mut stream)?;
    assert_eq!(stream.copy_selection()?.as_deref(), Some("hello"));
    assert!(stream.poll(Duration::ZERO)?.is_none());
    let after = stream.annotations();
    assert_ne!(after.selection, []);
    assert!(after.revision > before.revision);
    assert!(Arc::ptr_eq(
        &frame,
        stream.decoder.last_frame.as_ref().context("frame")?
    ));
    peer.set_nonblocking(true)?;
    let mut bytes = [0; 8];
    assert_eq!(
        peer.read(&mut bytes)
            .err()
            .context("local actions must not write data")?
            .kind(),
        ErrorKind::WouldBlock
    );
    Ok(())
}

#[test]
fn hidden_search_completes_without_materializing_and_fresh_snapshot_rebuilds_intent() -> Result<()>
{
    let (mut stream, _peer) = stream(replica()?)?;
    stream.decoder.exit_code = Some(0);
    select_word(&mut stream)?;
    stream.poll(Duration::ZERO)?;
    let frame = Arc::clone(stream.decoder.last_frame.as_ref().context("frame")?);
    let token = stream.annotations().viewport_token;
    stream.search(SearchCommand::Set {
        query: "hello".into(),
        generation: 9,
    })?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        stream.poll_with_frame(Duration::ZERO, false)?;
        if stream
            .annotations()
            .search
            .as_ref()
            .is_some_and(|search| search.complete)
        {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "hidden search did not complete at publication cadence"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let annotations = stream.annotations();
    let search = annotations.search.as_ref().context("search")?;
    assert!(search.complete);
    assert_eq!(search.total, 2);
    assert_eq!(search.generation, 9);
    assert_eq!(annotations.viewport_token, token);
    assert!(Arc::ptr_eq(
        &frame,
        stream.decoder.last_frame.as_ref().context("hidden frame")?
    ));
    assert_eq!(stream.copy_selection()?.as_deref(), Some("hello"));
    assert_eq!(search.matches, []);
    assert_eq!(search.selected_match, []);
    assert_eq!(annotations.selection, []);
    // A completed query and unchanged cell frame still regain full annotations
    // when revealed, with no output byte needed to trigger publication.
    assert!(stream.poll_with_frame(Duration::ZERO, true)?.is_none());
    let revealed = stream.annotations();
    assert_ne!(
        revealed.search.as_ref().context("revealed search")?.matches,
        []
    );
    assert_ne!(revealed.selection, []);
    assert_eq!(revealed.viewport_token, token);
    assert!(Arc::ptr_eq(
        &frame,
        stream
            .decoder
            .last_frame
            .as_ref()
            .context("revealed frame")?
    ));

    let snapshot = stream
        .decoder
        .terminal
        .as_mut()
        .context("terminal")?
        .snapshot_bytes()?;
    let mut fresh = StreamDecoder::new(43);
    fresh.search(SearchCommand::Set {
        query: "hello".into(),
        generation: 9,
    })?;
    fresh.push(&snapshot)?;
    assert_eq!(
        fresh
            .annotations
            .search
            .as_ref()
            .context("restored search")?
            .generation,
        9
    );
    assert_ne!(fresh.annotations.viewport_token, token);
    assert_eq!(fresh.annotations.selection, []);
    assert_eq!(
        fresh
            .terminal
            .as_ref()
            .context("fresh terminal")?
            .copy_selection()?,
        None
    );
    fresh.search(SearchCommand::Cancel { generation: 10 })?;
    fresh.search(SearchCommand::Set {
        query: "stale".into(),
        generation: 9,
    })?;
    assert!(fresh.annotations.search.is_none());
    Ok(())
}

#[test]
fn exact_data_resume_retains_native_search_selection_and_owned_annotations() -> Result<()> {
    let (mut stream, _old_peer) = stream(replica()?)?;
    select_word(&mut stream)?;
    stream.decoder.materialize()?;
    stream.search(SearchCommand::Set {
        query: "hello".into(),
        generation: 4,
    })?;
    let before = stream.annotations();
    let path = std::env::temp_dir().join(format!("rex-stream-resume-{}.sock", std::process::id()));
    let listener = UnixListener::bind(&path)?;
    stream.endpoint = format!("unix://{}", path.display());
    let worker = thread::spawn(move || -> Result<()> {
        let (socket, _) = listener.accept()?;
        socket.set_read_timeout(Some(Duration::from_secs(3)))?;
        #[expect(
            clippy::result_large_err,
            reason = "Tungstenite requires this callback to return its HTTP error response by value."
        )]
        let mut websocket = tungstenite::accept_hdr(
            socket,
            |_request: &tungstenite::handshake::server::Request,
             mut response: tungstenite::handshake::server::Response| {
                response.headers_mut().insert(
                    "Sec-WebSocket-Protocol",
                    tungstenite::http::HeaderValue::from_static(SUBPROTOCOL),
                );
                Ok(response)
            },
        )?;
        let mut expected = vec![2];
        expected.extend_from_slice(&[7; 16]);
        expected.extend_from_slice(&123_u64.to_le_bytes());
        assert_eq!(websocket.read()?, Message::Binary(expected.into()));
        let mut welcome = vec![0; 32];
        welcome[0] = 0x81;
        welcome[1..17].fill(8);
        welcome[24..32].copy_from_slice(&42_u64.to_le_bytes());
        websocket.send(Message::Binary(welcome.into()))?;
        assert!(matches!(websocket.read()?, Message::Close(_)));
        Ok(())
    });
    let result = stream.resume();
    let _ = std::fs::remove_file(path);
    result?;
    assert!(Arc::ptr_eq(&before, &stream.annotations()));
    assert_eq!(stream.copy_selection()?.as_deref(), Some("hello"));
    assert_eq!(
        stream.decoder.search_intent,
        Some(SearchCommand::Set {
            query: "hello".into(),
            generation: 4
        })
    );
    drop(stream);
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("resume server panicked"))??;
    Ok(())
}

#[test]
fn dense_hidden_snapshot_search_skips_highlight_extraction_until_reveal() -> Result<()> {
    let mut source = GhosttyTerminal::new(crate::terminal::TerminalConfig::default())?;
    for _ in 0..120 {
        source.feed(
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n",
        );
    }
    let snapshot = source.snapshot_bytes()?;
    let mut decoder = StreamDecoder::new(42);
    decoder.set_interaction_visible(false);
    decoder.search(SearchCommand::Set {
        query: "a".into(),
        generation: 1,
    })?;
    decoder.push_deferred(&snapshot)?;
    decoder.exit_code = Some(0);
    let (mut stream, _peer) = stream(decoder)?;
    // Hidden installation and local commands must inherit the same cheap path.
    stream.select(SelectionCommand::Clear)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        stream.poll_with_frame(Duration::ZERO, false)?;
        if stream
            .annotations()
            .search
            .as_ref()
            .is_some_and(|search| search.complete)
        {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "dense hidden search did not complete"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let hidden = stream.annotations();
    let search = hidden.search.as_ref().context("dense hidden search")?;
    assert_eq!(search.total, 9600);
    assert_eq!(search.matches, []);
    assert_eq!(hidden.selection, []);
    assert!(stream.decoder.last_frame.is_none());
    assert_eq!(
        stream
            .decoder
            .terminal
            .as_ref()
            .context("hidden terminal")?
            .interaction_highlight_extractions(),
        0
    );
    let frame = stream
        .poll_with_frame(Duration::ZERO, true)?
        .context("reveal must materialize without further VT")?;
    let revealed = stream.annotations();
    assert_ne!(revealed.viewport_token, 0);
    assert_ne!(
        revealed
            .search
            .as_ref()
            .context("revealed dense search")?
            .matches,
        []
    );
    assert!(
        stream
            .decoder
            .terminal
            .as_ref()
            .context("revealed terminal")?
            .interaction_highlight_extractions()
            > 0
    );
    assert_eq!(frame.size, crate::terminal::TerminalConfig::default().size);
    Ok(())
}
