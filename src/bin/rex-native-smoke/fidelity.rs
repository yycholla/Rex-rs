//! Owned decoration/cursor fidelity through real Rex snapshots and subsequent VT.
use super::{terminal_call, wait_cache};
use anyhow::{Context, Result, ensure};
use rex_rs::{
    core::{
        Command, PaneId,
        frame::{CursorStyle, Rgb, TerminalFrame, UnderlineKind},
    },
    rex::{WorkspaceClient, transport::ControlConnection},
};
use serde_json::json;
use std::{fs, path::Path, sync::Arc};

const PROGRAM: &str = r"import os,sys,tty
tty.setraw(0)
log=open(sys.argv[1],'wb',buffering=0)
def out(s):os.write(1,s.encode())
out('\x1b[2J\x1b[H\x1b[2;9;53;5mflags\x1b[0m\r\n')
out('\x1b[58:2::12:34:56m')
for n,c in enumerate('ABCDEF'):out('\x1b[4:%dm%s'%(n,c))
out('\x1b[24mG\x1b[4:2m\x1b[59mH\x1b[0mI\r\n')
out('\x1b]4;42;rgb:12/34/56\x1b\\\x1b[4:3m\x1b[58:5:42mP\x1b[0m\r\n')
out('wide: 界e\u0301\r\nREADY_FIDELITY\x1b[8;4H\x1b[2 q\x1b]12;rgb:ab/cd/ef\x1b\\')
while True:
 c=os.read(0,1)
 if not c:break
 log.write(c)
 if c in b'123456':out('\x1b[%d q'%int(c))
 elif c==b'p':out('\x1b]4;42;rgb:34/56/78\x1b\\')
 elif c==b'h':out('\x1b[?25l')
 elif c==b's':out('\x1b[?25h')
 elif c==b'c':out('\x1b]12;rgb:12/34/56\x1b\\')
 elif c==b'r':out('\x1b]112\x1b\\')
 elif c==b'b':out('\x1b[?12h')
 elif c==b'B':out('\x1b[?12l')
 elif c==b'!':break
";

fn pane(client: &WorkspaceClient, block: &str) -> Option<PaneId> {
    client.state().pane_ids().into_iter().find(|id| {
        client
            .state()
            .pane(*id)
            .is_some_and(|entry| entry.remote_id == block)
    })
}

fn frame(client: &WorkspaceClient, block: &str) -> Option<Arc<TerminalFrame>> {
    pane(client, block)
        .and_then(|id| client.frame(id))
        .map(Arc::clone)
}

fn check_styles(frame: &TerminalFrame) -> Result<()> {
    let style = frame.rows[0][0].style;
    ensure!(
        style.faint && style.strikethrough && style.overline && style.blink,
        "snapshot discarded native SGR decoration flags"
    );
    let kinds = [
        UnderlineKind::None,
        UnderlineKind::Single,
        UnderlineKind::Double,
        UnderlineKind::Curly,
        UnderlineKind::Dotted,
        UnderlineKind::Dashed,
        UnderlineKind::None,
    ];
    for (cell, kind) in frame.rows[1].iter().zip(kinds) {
        ensure!(
            cell.style.underline_kind == kind
                && cell.style.underline == (kind != UnderlineKind::None),
            "snapshot lost underline kind {kind:?}"
        );
        ensure!(
            cell.style.underline_color == Some(Rgb::from_hex(0x000c_2238)),
            "snapshot lost RGB underline color"
        );
    }
    ensure!(
        frame.rows[1][7].style.underline_kind == UnderlineKind::Double
            && frame.rows[1][7].style.underline_color.is_none(),
        "underline color reset lost"
    );
    ensure!(
        frame.rows[2][0].style.underline_kind == UnderlineKind::Curly
            && frame.rows[2][0].style.underline_color == Some(Rgb::from_hex(0x0012_3456)),
        "snapshot lost palette underline color"
    );
    Ok(())
}

pub(super) fn check(control: &mut ControlConnection, endpoint: &str) -> Result<()> {
    let socket = Path::new(endpoint.strip_prefix("unix://").context("fixture socket")?);
    let script = socket.with_file_name("fidelity-program.py");
    let received = socket.with_file_name("fidelity-input.bin");
    fs::write(&script, PROGRAM)?;
    let created = control.call("session.create", json!({"label":"Terminal fidelity fixture",
        "initial_windows":[{"layout":{"block":{"flavor":"com.superlogical.terminal.shell","options":{
        "shell":"none","command":["/usr/bin/python3",script,received],
        "size":{"columns":40,"rows":10},"exit":{"on_completion":false}
    }}}}]}))?;
    let session = created["session_id"].as_str().context("fidelity session")?;
    let block = created["initial_windows"][0]["block_ids"][0]
        .as_str()
        .context("fidelity block")?;
    let mut client = WorkspaceClient::connect(endpoint.into());
    wait_cache(&mut client, |client| pane(client, block).is_some())?;
    let session_id = client
        .state()
        .sessions()
        .iter()
        .find(|entry| entry.remote_id() == session)
        .context("fidelity session alias")?
        .id();
    client.apply(Command::SelectSession(session_id))?;
    wait_cache(&mut client, |client| {
        frame(client, block).is_some_and(|frame| {
            frame
                .text_lines()
                .iter()
                .any(|line| line.contains("READY_FIDELITY"))
        })
    })?;
    let initial = frame(&client, block).context("fidelity snapshot")?;
    check_styles(&initial)?;
    let cursor = initial.cursor.context("snapshot cursor")?;
    ensure!(
        cursor.style == CursorStyle::Block
            && !cursor.blinking
            && cursor.color == Some(Rgb::from_hex(0x00ab_cdef)),
        "snapshot cursor presentation lost"
    );
    check_stream(control, &mut client, session, block, &initial)?;
    drop(client);
    control.call("session.destroy", json!({"session_id":session}))?;
    ensure!(
        fs::read(&received)? == b"123456phscbBr",
        "unexpected input/effects in fidelity child"
    );
    fs::remove_file(script)?;
    fs::remove_file(received)?;
    Ok(())
}

fn check_stream(
    control: &mut ControlConnection,
    client: &mut WorkspaceClient,
    session: &str,
    block: &str,
    initial: &TerminalFrame,
) -> Result<()> {
    let send = |control: &mut ControlConnection, text: &str| {
        terminal_call(
            control,
            session,
            block,
            "com.superlogical.terminal.write",
            json!({"data":text}),
        )
    };
    for (key, style, blinking) in [
        ("1", CursorStyle::Block, true),
        ("2", CursorStyle::Block, false),
        ("3", CursorStyle::Underline, true),
        ("4", CursorStyle::Underline, false),
        ("5", CursorStyle::Bar, true),
        ("6", CursorStyle::Bar, false),
    ] {
        send(control, key)?;
        wait_cache(client, |client| {
            frame(client, block)
                .and_then(|frame| frame.cursor)
                .is_some_and(|cursor| cursor.style == style && cursor.blinking == blinking)
        })?;
        let current = frame(client, block).context("style-only cursor frame")?;
        ensure!(
            initial
                .rows
                .iter()
                .zip(&current.rows)
                .all(|(a, b)| Arc::ptr_eq(a, b)),
            "cursor presentation-only VT rebuilt cell rows"
        );
    }
    send(control, "p")?;
    wait_cache(client, |client| {
        frame(client, block).is_some_and(|frame| {
            frame.rows[2][0].style.underline_color == Some(Rgb::from_hex(0x0034_5678))
        })
    })?;
    send(control, "h")?;
    wait_cache(client, |client| {
        frame(client, block).is_some_and(|frame| frame.cursor.is_none())
    })?;
    send(control, "s")?;
    wait_cache(client, |client| {
        frame(client, block).is_some_and(|frame| frame.cursor.is_some())
    })?;
    send(control, "c")?;
    wait_cache(client, |client| {
        frame(client, block)
            .and_then(|frame| frame.cursor)
            .is_some_and(|cursor| cursor.color == Some(Rgb::from_hex(0x0012_3456)))
    })?;
    send(control, "b")?;
    wait_cache(client, |client| {
        frame(client, block)
            .and_then(|frame| frame.cursor)
            .is_some_and(|cursor| cursor.blinking)
    })?;
    send(control, "B")?;
    wait_cache(client, |client| {
        frame(client, block)
            .and_then(|frame| frame.cursor)
            .is_some_and(|cursor| !cursor.blinking)
    })?;
    send(control, "r")?;
    wait_cache(client, |client| {
        frame(client, block)
            .and_then(|frame| frame.cursor)
            .is_some_and(|cursor| cursor.color.is_none())
    })?;
    Ok(())
}
