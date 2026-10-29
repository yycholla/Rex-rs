use anyhow::{Result, ensure};
use rex_rs::{
    core::{
        GridSize,
        input::{KeyInput, Modifiers},
    },
    terminal::{GhosttyTerminal, TerminalConfig},
};

fn main() -> Result<()> {
    let mut terminal = GhosttyTerminal::new(TerminalConfig::default())?;
    check_keyboard(&mut terminal)?;
    check_paste(&mut terminal)?;
    check_scrollback()?;
    println!("PASS: terminal-mode key encoding, Unicode, paste sanitization, and scrollback");
    Ok(())
}

fn key(name: &str) -> KeyInput {
    KeyInput {
        key: name.into(),
        text: None,
        modifiers: Modifiers::default(),
        repeat: false,
    }
}

fn check_keyboard(terminal: &mut GhosttyTerminal) -> Result<()> {
    let simple: [(&str, &[u8]); 7] = [
        ("enter", b"\r"),
        ("backspace", b"\x7f"),
        ("tab", b"\t"),
        ("escape", b"\x1b"),
        ("up", b"\x1b[A"),
        ("f1", b"\x1bOP"),
        ("space", b" "),
    ];
    for (name, expected) in simple {
        ensure!(
            terminal.encode_key(&key(name))? == expected,
            "incorrect encoding for {name}"
        );
    }
    let mut control = key("c");
    control.modifiers.control = true;
    ensure!(
        terminal.encode_key(&control)? == b"\x03",
        "Ctrl+C must generate ETX"
    );
    // Some platforms report the transformed control byte as text. It must not
    // be passed through as printable text to Ghostty's key encoder.
    control.text = Some("\x03".into());
    ensure!(terminal.encode_key(&control)? == b"\x03");
    let mut alt = key("x");
    alt.modifiers.alt = true;
    ensure!(
        terminal.encode_key(&alt)? == b"\x1bx",
        "Alt+X must honor escape-prefix mode"
    );
    let mut shifted = key("tab");
    shifted.modifiers.shift = true;
    ensure!(terminal.encode_key(&shifted)? == b"\x1b[Z");
    let mut unicode = key("界");
    unicode.text = Some("界".into());
    ensure!(terminal.encode_key(&unicode)? == "界".as_bytes());
    terminal.feed(b"\x1b[?1h");
    ensure!(
        terminal.encode_key(&key("up"))? == b"\x1bOA",
        "application cursor mode must change arrows"
    );
    terminal.feed(b"\x1b[?1l");
    ensure!(terminal.encode_key(&key("up"))? == b"\x1b[A");
    let mut control_i = key("i");
    control_i.modifiers.control = true;
    // Ghostty intentionally follows fixterms even before Kitty negotiation:
    // upstream src/input/key_encode.zig's "legacy: fixterm awkward letters"
    // test distinguishes Ctrl+I from the literal Tab key checked above.
    ensure!(terminal.encode_key(&control_i)? == b"\x1b[105;5u");
    terminal.feed(b"\x1b[>3u");
    control_i.repeat = true;
    ensure!(
        terminal.encode_key(&control_i)? == b"\x1b[105;5:2u",
        "Kitty report-events mode must identify repeated key events"
    );
    terminal.feed(b"\x1b[<u");
    ensure!(terminal.encode_key(&control_i)? == b"\x1b[105;5u");
    ensure!(
        terminal.encode_key(&key("f26")).is_err(),
        "unsupported keys must report an error"
    );
    Ok(())
}

fn check_paste(terminal: &mut GhosttyTerminal) -> Result<()> {
    ensure!(terminal.encode_paste("first\nsecond")? == b"first\rsecond");
    terminal.feed(b"\x1b[?2004h");
    ensure!(terminal.encode_paste("first\nsecond")? == b"\x1b[200~first\nsecond\x1b[201~");
    let encoded = terminal.encode_paste("before\x1b[201~after")?;
    ensure!(
        encoded
            .windows(6)
            .filter(|window| *window == b"\x1b[201~")
            .count()
            == 1,
        "paste data must not be able to terminate bracketed paste early"
    );
    terminal.feed(b"\x1b[?2004l");
    ensure!(terminal.encode_paste("界")? == "界".as_bytes());
    Ok(())
}

fn check_scrollback() -> Result<()> {
    let mut terminal = GhosttyTerminal::new(TerminalConfig {
        size: GridSize::new(80, 2)?,
        ..TerminalConfig::default()
    })?;
    terminal.feed(b"one\r\ntwo\r\nthree\r\nfour");
    ensure!(
        terminal
            .snapshot()?
            .text_lines()
            .first()
            .map(String::as_str)
            == Some("three")
    );
    terminal.scroll_lines(1)?;
    ensure!(
        terminal
            .snapshot()?
            .text_lines()
            .first()
            .map(String::as_str)
            == Some("two")
    );
    terminal.scroll_lines(i32::MAX)?;
    ensure!(
        terminal
            .snapshot()?
            .text_lines()
            .first()
            .map(String::as_str)
            == Some("one")
    );
    terminal.scroll_lines(-i32::MAX)?;
    ensure!(
        terminal
            .snapshot()?
            .text_lines()
            .first()
            .map(String::as_str)
            == Some("three")
    );
    terminal.scroll_lines(1)?;
    terminal.scroll_to_bottom();
    ensure!(
        terminal
            .snapshot()?
            .text_lines()
            .first()
            .map(String::as_str)
            == Some("three")
    );
    Ok(())
}
