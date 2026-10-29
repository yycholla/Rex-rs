use anyhow::{Context, Result, ensure};
use rex_rs::{
    core::{Command, GridSize, SplitAxis},
    terminal::{GhosttyTerminal, TerminalConfig, TerminalWorkspace},
};
use std::sync::Arc;

fn main() -> Result<()> {
    let mut terminal = GhosttyTerminal::new(TerminalConfig {
        size: GridSize::new(80, 12)?,
        ..Default::default()
    })?;
    terminal.feed(b"Ghostty is alive\r\n\x1b[32mgreen text\x1b[0m\r\n");
    terminal.feed(b"old text\r\x1b[2Kreplacement\r\n");
    let frame = terminal.snapshot()?;
    let lines = frame.text_lines();
    ensure!(lines.iter().any(|line| line == "Ghostty is alive"));
    ensure!(lines.iter().any(|line| line == "green text"));
    ensure!(lines.iter().any(|line| line == "replacement"));
    ensure!(!lines.iter().any(|line| line.contains("old text")));
    ensure!(
        frame.rows[1][0].foreground != frame.foreground,
        "SGR color must survive the frame boundary"
    );
    terminal.resize(GridSize::new(100, 16)?, 8, 22)?;
    ensure!(terminal.snapshot()?.size == GridSize::new(100, 16)?);
    terminal.feed(b"\x1b[6n");
    ensure!(
        !terminal.take_pending_output().is_empty(),
        "device queries need PTY write-back"
    );
    ensure!(
        terminal.take_pending_output().is_empty(),
        "write-back drains once"
    );
    for line in lines.iter().filter(|line| !line.is_empty()) {
        println!("{line}");
    }
    check_workspace_lifetimes()?;
    println!("PASS: VT parsing, colors, erase-line, resizing, PTY responses, and pane lifetimes");
    Ok(())
}

fn check_workspace_lifetimes() -> Result<()> {
    let mut workspace = TerminalWorkspace::new(TerminalConfig::default());
    workspace.apply(Command::CreateSession {
        title: "Smoke".into(),
    })?;
    let first = workspace
        .state()
        .active_session()
        .context("new workspace must have an active session")?
        .active_pane()
        .context("fixture session must have an attached pane")?;
    workspace.feed(first, b"retained contents")?;
    let retained = Arc::clone(
        workspace
            .frame(first)
            .context("new pane must have a frame")?,
    );
    workspace.apply(Command::SplitPane {
        pane: first,
        axis: SplitAxis::Horizontal,
    })?;
    let second = workspace
        .state()
        .active_session()
        .context("splitting must preserve the active session")?
        .active_pane()
        .context("fixture session must have an attached pane")?;
    ensure!(
        Arc::ptr_eq(
            &retained,
            workspace
                .frame(first)
                .context("splitting must preserve the first pane's frame")?
        ),
        "splitting must preserve existing terminal resources"
    );
    workspace.apply(Command::ClosePane(second))?;
    ensure!(
        workspace.frame(second).is_none(),
        "closing a pane must release its runtime"
    );
    let surviving_frame = workspace
        .frame(first)
        .context("closing the second pane must preserve the first pane's frame")?;
    ensure!(surviving_frame.text_lines()[0] == "retained contents");
    Ok(())
}
