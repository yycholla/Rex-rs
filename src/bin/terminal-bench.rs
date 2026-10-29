//! Bounded diagnostic benchmark for viewport-dependent terminal update costs.
use std::{
    hint::black_box,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use rex_rs::{
    core::{Command, GridSize},
    terminal::{GhosttyTerminal, ShellConfig, TerminalConfig, TerminalWorkspace},
};

const ITERATIONS: usize = 20;

fn main() -> Result<()> {
    println!("grid,clean_snapshot_us,single_row_snapshot_us,full_update_us,input_write_us");
    for (columns, rows) in [(80, 24), (220, 120), (350, 160)] {
        benchmark(GridSize::new(columns, rows)?)?;
    }
    Ok(())
}

fn median(mut operation: impl FnMut() -> Result<()>) -> Result<Duration> {
    let mut timings = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        operation()?;
        timings.push(start.elapsed());
    }
    timings.sort_unstable();
    Ok(timings[ITERATIONS / 2])
}

fn contents(size: GridSize) -> Vec<u8> {
    let line = "x".repeat(usize::from(size.columns().saturating_sub(1)));
    let mut bytes = b"\x1b[H".to_vec();
    for row in 0..size.rows() {
        if row != 0 {
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(line.as_bytes());
    }
    bytes
}

fn benchmark(size: GridSize) -> Result<()> {
    let config = TerminalConfig {
        size,
        ..TerminalConfig::default()
    };
    let mut terminal = GhosttyTerminal::new(config)?;
    let filled = contents(size);
    terminal.feed(&filled);
    black_box(terminal.snapshot()?);
    let clean = median(|| {
        black_box(terminal.snapshot()?);
        Ok(())
    })?;
    let mut alternate = false;
    let dirty = median(|| {
        terminal.feed(if alternate {
            b"\x1b[Hodd"
        } else {
            b"\x1b[Heven"
        });
        alternate = !alternate;
        black_box(terminal.snapshot()?);
        Ok(())
    })?;
    let full = median(|| {
        terminal.feed(b"\x1b[2J");
        terminal.feed(&filled);
        black_box(terminal.snapshot()?);
        Ok(())
    })?;

    let mut workspace = TerminalWorkspace::live(
        config,
        ShellConfig {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "stty -echo; cat >/dev/null".into()],
            cwd: PathBuf::from("/tmp"),
        },
    );
    workspace.apply(Command::CreateSession {
        title: "Terminal benchmark".into(),
    })?;
    let pane = workspace
        .state()
        .active_session()
        .context("benchmark session must exist")?
        .active_pane()
        .context("fixture session must have an attached pane")?;
    workspace.feed(pane, &filled)?;
    let input = median(|| workspace.write_input(pane, b"x"))?;
    println!(
        "{}x{},{:.2},{:.2},{:.2},{:.2}",
        size.columns(),
        size.rows(),
        clean.as_secs_f64() * 1_000_000.,
        dirty.as_secs_f64() * 1_000_000.,
        full.as_secs_f64() * 1_000_000.,
        input.as_secs_f64() * 1_000_000.
    );
    Ok(())
}
