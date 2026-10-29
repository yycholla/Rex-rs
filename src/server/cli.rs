//! Synchronous automation client. It never creates a server or a replacement shell.
use super::{
    default_socket_path,
    protocol::{
        MAX_INPUT_BYTES, MAX_MESSAGE_BYTES, Operation, PROTOCOL_VERSION, Request, Response,
        Snapshot, read_message, write_message,
    },
};
use crate::core::{Command, Direction, GridSize, PaneId, SessionId, SplitAxis, SplitId, WindowId};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{os::unix::net::UnixStream, path::PathBuf, time::Duration};

const HELP: &str = "rexctl [--socket PATH] [--json] [--take-control] COMMAND [ARGS]\n\
Options precede the command. Connects to an existing local server; never starts it.\n\
  status | list                     Workspace, labels, layout, IDs and process status\n\
  snapshot                          Full versioned JSON response\n\
  capture PANE                      Current terminal screen (text; --json for metadata)\n\
  new-session TITLE                 Create a session and initial terminal\n\
  new-tab SESSION TITLE             Create a tab in a session\n\
  split PANE right|down             Split a terminal\n\
  focus PANE                        Focus a terminal\n\
  focus-direction DIRECTION [--wrap] DIRECTION is left, right, up or down\n\
  zoom PANE                         Toggle shared pane zoom\n\
  resize-split SPLIT RATIO           First-child share in permille, 100..900\n\
  rename-session SESSION TITLE | rename-tab WINDOW TITLE\n\
  close-pane PANE | close-tab WINDOW | close-session SESSION\n\
  input PANE TEXT | paste PANE TEXT  Send exact text; no implicit newline\n\
  scroll PANE LINES                 Positive lines scroll into history\n\
  resize PANE COLS ROWS [CELL_WIDTH CELL_HEIGHT]\n\
  call JSON_OPERATION               Send a typed local protocol operation\n\
Input, paste, scroll and resize acquire terminal control. An existing controller\n\
is respected unless --take-control explicitly requests takeover. This is the\n\
Rex-rs local protocol, not the official Rex protocol.\n";

#[derive(Debug)]
struct Arguments {
    socket: Option<PathBuf>,
    json: bool,
    take_control: bool,
    command: CliCommand,
}

#[derive(Debug)]
enum CliCommand {
    Help,
    Status,
    Snapshot,
    Capture(PaneId),
    Operation(Operation),
}

impl Arguments {
    fn parse(args: &[String]) -> Result<Self> {
        let mut remaining = args;
        let mut socket = None;
        let mut json = false;
        let mut take_control = false;
        while let Some((flag, rest)) = remaining.split_first() {
            match flag.as_str() {
                "--socket" => {
                    ensure!(socket.is_none(), "--socket may only be supplied once");
                    let (path, next) = rest.split_first().context("--socket requires a path")?;
                    let path = PathBuf::from(path);
                    ensure!(path.is_absolute(), "--socket requires an absolute path");
                    socket = Some(path);
                    remaining = next;
                }
                "--json" => {
                    json = true;
                    remaining = rest;
                }
                "--take-control" => {
                    take_control = true;
                    remaining = rest;
                }
                "--help" | "-h" => {
                    ensure!(rest.is_empty(), "--help takes no arguments");
                    remaining = &[];
                    break;
                }
                _ => break,
            }
        }
        Ok(Self {
            socket,
            json,
            take_control,
            command: parse_command(remaining)?,
        })
    }
}

fn parse_command(args: &[String]) -> Result<CliCommand> {
    let Some((name, args)) = args.split_first() else {
        return Ok(CliCommand::Help);
    };
    match name.as_str() {
        "help" => {
            exact(args, 0)?;
            Ok(CliCommand::Help)
        }
        "status" | "list" => {
            exact(args, 0)?;
            Ok(CliCommand::Status)
        }
        "snapshot" => {
            exact(args, 0)?;
            Ok(CliCommand::Snapshot)
        }
        "capture" => {
            exact(args, 1)?;
            Ok(CliCommand::Capture(pane(&args[0])?))
        }
        _ => Ok(CliCommand::Operation(parse_operation(name, args)?)),
    }
}

fn parse_operation(name: &str, args: &[String]) -> Result<Operation> {
    let operation = match name {
        "new-session" => {
            exact(args, 1)?;
            apply(Command::CreateSession {
                title: title(&args[0])?,
            })
        }
        "new-tab" => {
            exact(args, 2)?;
            apply(Command::CreateWindow {
                session: session(&args[0])?,
                title: title(&args[1])?,
            })
        }
        "split" => {
            exact(args, 2)?;
            apply(Command::SplitPane {
                pane: pane(&args[0])?,
                axis: axis(&args[1])?,
            })
        }
        "focus" => {
            exact(args, 1)?;
            apply(Command::FocusPane(pane(&args[0])?))
        }
        "focus-direction" => focus_direction(args)?,
        "zoom" => {
            exact(args, 1)?;
            apply(Command::ToggleZoom(pane(&args[0])?))
        }
        "resize-split" => {
            exact(args, 2)?;
            let ratio = args[1]
                .parse()
                .context("ratio must be an integer in permille")?;
            ensure!(
                (100..=900).contains(&ratio),
                "ratio must be 100..900 permille"
            );
            apply(Command::ResizeSplit {
                split: split(&args[0])?,
                ratio,
            })
        }
        "rename-session" => {
            exact(args, 2)?;
            apply(Command::RenameSession {
                session: session(&args[0])?,
                title: title(&args[1])?,
            })
        }
        "rename-tab" => {
            exact(args, 2)?;
            apply(Command::RenameWindow {
                window: window(&args[0])?,
                title: title(&args[1])?,
            })
        }
        "close-pane" => {
            exact(args, 1)?;
            apply(Command::ClosePane(pane(&args[0])?))
        }
        "close-tab" => {
            exact(args, 1)?;
            apply(Command::CloseWindow(window(&args[0])?))
        }
        "close-session" => {
            exact(args, 1)?;
            apply(Command::CloseSession(session(&args[0])?))
        }
        "input" | "paste" => text_input(name, args)?,
        "scroll" => {
            exact(args, 2)?;
            Operation::Scroll {
                pane: pane(&args[0])?,
                lines: args[1].parse().context("lines must be a signed integer")?,
            }
        }
        "resize" => resize(args)?,
        "call" => {
            exact(args, 1)?;
            ensure!(
                args[0].len() <= MAX_MESSAGE_BYTES,
                "operation exceeds message limit"
            );
            serde_json::from_str(&args[0]).context("invalid JSON local operation")?
        }
        _ => bail!("unknown command {name:?}; use rexctl --help"),
    };
    Ok(operation)
}

fn exact(args: &[String], count: usize) -> Result<()> {
    ensure!(
        args.len() == count,
        "expected {count} command arguments, received {}",
        args.len()
    );
    Ok(())
}
fn title(value: &str) -> Result<String> {
    ensure!(!value.trim().is_empty(), "title cannot be empty");
    ensure!(
        value.chars().count() <= 128,
        "title exceeds 128 Unicode characters"
    );
    Ok(value.to_owned())
}
fn id(value: &str) -> Result<u64> {
    let id = value.parse().context("ID must be a positive integer")?;
    ensure!(id > 0, "ID must be a positive integer");
    Ok(id)
}
fn pane(value: &str) -> Result<PaneId> {
    PaneId::from_value(id(value)?).context("invalid pane ID")
}
fn session(value: &str) -> Result<SessionId> {
    SessionId::from_value(id(value)?).context("invalid session ID")
}
fn window(value: &str) -> Result<WindowId> {
    WindowId::from_value(id(value)?).context("invalid window ID")
}
fn split(value: &str) -> Result<SplitId> {
    SplitId::from_value(id(value)?).context("invalid split ID")
}
fn apply(command: Command) -> Operation {
    Operation::Apply(command)
}
fn axis(value: &str) -> Result<SplitAxis> {
    match value {
        "right" => Ok(SplitAxis::Horizontal),
        "down" => Ok(SplitAxis::Vertical),
        _ => bail!("split direction must be right or down"),
    }
}
fn focus_direction(args: &[String]) -> Result<Operation> {
    ensure!(
        (1..=2).contains(&args.len()),
        "focus-direction requires DIRECTION [--wrap]"
    );
    let direction = match args[0].as_str() {
        "left" => Direction::Left,
        "right" => Direction::Right,
        "up" => Direction::Up,
        "down" => Direction::Down,
        _ => bail!("direction must be left, right, up or down"),
    };
    ensure!(
        args.len() == 1 || args[1] == "--wrap",
        "expected --wrap after direction"
    );
    Ok(apply(Command::FocusDirection {
        direction,
        wrap: args.len() == 2,
    }))
}
fn text_input(name: &str, args: &[String]) -> Result<Operation> {
    exact(args, 2)?;
    let limit = if name == "paste" {
        MAX_INPUT_BYTES - 12
    } else {
        MAX_INPUT_BYTES
    };
    ensure!(
        args[1].len() <= limit,
        "terminal text exceeds the {limit}-byte limit"
    );
    Ok(if name == "paste" {
        Operation::Paste {
            pane: pane(&args[0])?,
            text: args[1].clone(),
        }
    } else {
        Operation::Input {
            pane: pane(&args[0])?,
            bytes: args[1].as_bytes().to_vec(),
        }
    })
}
fn resize(args: &[String]) -> Result<Operation> {
    ensure!(
        args.len() == 3 || args.len() == 5,
        "resize requires PANE COLS ROWS [CELL_WIDTH CELL_HEIGHT]"
    );
    let size = GridSize::new(
        args[1].parse().context("invalid columns")?,
        args[2].parse().context("invalid rows")?,
    )?;
    let (cell_width, cell_height) = if args.len() == 5 {
        (
            args[3].parse().context("invalid cell width")?,
            args[4].parse().context("invalid cell height")?,
        )
    } else {
        (0, 0)
    };
    ensure!(
        cell_width <= 1024 && cell_height <= 1024,
        "cell dimensions exceed 1024 pixels"
    );
    Ok(Operation::Resize {
        pane: pane(&args[0])?,
        size,
        cell_width,
        cell_height,
    })
}

fn requires_control(operation: &Operation) -> bool {
    matches!(
        operation,
        Operation::Input { .. }
            | Operation::Key { .. }
            | Operation::Paste { .. }
            | Operation::Scroll { .. }
            | Operation::Resize { .. }
    )
}

struct Connection {
    stream: UnixStream,
    server_id: String,
    revision: u64,
    snapshot: Snapshot,
    has_control: bool,
}
impl Connection {
    fn open(path: &std::path::Path) -> Result<Self> {
        let mut stream = UnixStream::connect(path)
            .with_context(|| format!("connect to existing server {}", path.display()))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        write_message(
            &mut stream,
            &Request {
                version: PROTOCOL_VERSION,
                server_id: None,
                known_revision: None,
                operation: Operation::Snapshot,
            },
        )?;
        let response: Response = read_message(&mut stream)?;
        validate_response(&response)?;
        ensure!(
            response.error.is_none(),
            "attach rejected: {:?}",
            response.error
        );
        let snapshot = response
            .snapshot
            .context("server omitted initial snapshot")?;
        Ok(Self {
            stream,
            server_id: response.server_id,
            revision: response.revision,
            snapshot,
            has_control: response.has_control,
        })
    }
    fn request(&mut self, operation: Operation) -> Result<Response> {
        let request = Request {
            version: PROTOCOL_VERSION,
            server_id: Some(self.server_id.clone()),
            known_revision: Some(self.revision),
            operation,
        };
        write_message(&mut self.stream, &request)
            .context("request delivery may be uncertain; it was not retried")?;
        let mut response: Response =
            read_message(&mut self.stream).context("response lost; request was not retried")?;
        validate_response(&response)?;
        ensure!(
            response.server_id == self.server_id && response.revision >= self.revision,
            "server identity or revision changed during command"
        );
        if let Some(update) = response.update.take() {
            ensure!(
                response.snapshot.is_none(),
                "server sent conflicting full and incremental state"
            );
            response.snapshot = Some(update.apply(Some((&self.snapshot, self.revision)))?);
        }
        if let Some(snapshot) = &response.snapshot {
            self.snapshot = snapshot.clone();
            self.revision = response.revision;
        }
        self.has_control = response.has_control;
        ensure!(
            response.error.is_none(),
            "server rejected operation: {}",
            response.error.as_deref().unwrap_or("unknown error")
        );
        Ok(response)
    }
    fn acquire(&mut self, force: bool) -> Result<()> {
        self.request(Operation::AcquireControl { force })?;
        ensure!(
            self.has_control,
            "another client controls terminal input and viewport; use --take-control for explicit takeover"
        );
        Ok(())
    }
    fn summary(&self) -> Value {
        let sessions: Vec<_> = self.snapshot.state.sessions().iter().map(|session| {
            let windows: Vec<_> = session.windows().iter().map(|window| {
                let panes: Vec<_> = window.layout().panes().into_iter().map(|id| {
                    let runtime = self.snapshot.panes.get(&id);
                    json!({"id":id,"process_id":runtime.and_then(|pane| pane.process_id),"status":runtime.and_then(|pane| pane.status.as_ref()),"size":runtime.map(|pane| pane.frame.size)})
                }).collect();
                json!({"id":window.id(),"title":window.title(),"active_pane":window.active_pane(),"zoomed_pane":window.zoomed_pane(),"layout":window.layout(),"panes":panes})
            }).collect();
            json!({"id":session.id(),"title":session.title(),"active_window":session.active_window().map(crate::core::WorkspaceWindow::id),"windows":windows,"detached_panes":session.detached_panes()})
        }).collect();
        json!({"success":true,"protocol_version":PROTOCOL_VERSION,"server_id":self.server_id,"revision":self.revision,"has_control":self.has_control,"restored_metadata":self.snapshot.restored_metadata,"active_session":self.snapshot.state.active_session().map(crate::core::Session::id),"sessions":sessions})
    }
}
fn validate_response(response: &Response) -> Result<()> {
    ensure!(
        response.version == PROTOCOL_VERSION,
        "unsupported local protocol version"
    );
    ensure!(
        !response.server_id.is_empty() && response.server_id.len() <= 128,
        "invalid server identity"
    );
    if let Some(snapshot) = &response.snapshot {
        snapshot.validate()?;
    }
    Ok(())
}

/// Run an automation request without initializing or launching a server.
///
/// # Errors
/// Returns argument, connection, validation or server rejection errors.
pub fn run_with_args(args: &[String]) -> Result<String> {
    let args = Arguments::parse(args)?;
    if matches!(args.command, CliCommand::Help) {
        return Ok(HELP.into());
    }
    let path = args.socket.map_or_else(default_socket_path, Ok)?;
    let mut connection = Connection::open(&path)?;
    if args.take_control
        || matches!(&args.command, CliCommand::Operation(operation) if requires_control(operation))
    {
        connection.acquire(args.take_control)?;
    }
    match args.command {
        CliCommand::Help => Ok(HELP.into()),
        CliCommand::Status => encode(&connection.summary()),
        CliCommand::Snapshot => encode(
            &json!({"success":true,"version":PROTOCOL_VERSION,"server_id":connection.server_id,"revision":connection.revision,"has_control":connection.has_control,"snapshot":connection.snapshot}),
        ),
        CliCommand::Capture(pane) => {
            let frame = &connection
                .snapshot
                .panes
                .get(&pane)
                .with_context(|| format!("pane {pane} no longer exists"))?
                .frame;
            let text = frame.text_lines().join("\n");
            if args.json {
                encode(
                    &json!({"success":true,"version":PROTOCOL_VERSION,"server_id":connection.server_id,"revision":connection.revision,"pane":pane,"text":text}),
                )
            } else {
                Ok(format!("{text}\n"))
            }
        }
        CliCommand::Operation(operation) => {
            connection.request(operation)?;
            if args.json {
                encode(&connection.summary())
            } else {
                Ok(format!(
                    "accepted server={} revision={}\n",
                    connection.server_id, connection.revision
                ))
            }
        }
    }
}
fn encode(value: &Value) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string_pretty(value)?))
}

/// Execute command-line arguments and print a single result or structured error.
#[must_use]
pub fn entry_with_args(args: &[String]) -> i32 {
    match run_with_args(args) {
        Ok(output) => {
            print!("{output}");
            0
        }
        Err(error) => {
            if args
                .iter()
                .take_while(|arg| !is_command(arg))
                .any(|arg| arg == "--json")
            {
                println!("{}", json!({"success":false,"error":format!("{error:#}")}));
            } else {
                eprintln!("rexctl: {error:#}");
            }
            1
        }
    }
}
fn is_command(argument: &str) -> bool {
    matches!(
        argument,
        "help"
            | "status"
            | "list"
            | "snapshot"
            | "capture"
            | "new-session"
            | "new-tab"
            | "split"
            | "focus"
            | "focus-direction"
            | "zoom"
            | "resize-split"
            | "rename-session"
            | "rename-tab"
            | "close-pane"
            | "close-tab"
            | "close-session"
            | "input"
            | "paste"
            | "scroll"
            | "resize"
            | "call"
    )
}

/// CLI binary entry point. A successful enqueue is never reported as success;
/// this synchronous tool waits for the authoritative server acknowledgement.
#[must_use]
pub fn main_entry() -> i32 {
    entry_with_args(&std::env::args().skip(1).collect::<Vec<_>>())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Parser fixtures fail immediately if a valid command is rejected."
)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }
    #[test]
    fn text_is_exact_and_unicode_titles_are_bounded_by_characters() {
        let parsed = Arguments::parse(&args(&["input", "3", "echo x\n"])).unwrap();
        assert!(
            matches!(parsed.command,CliCommand::Operation(Operation::Input{bytes,..}) if bytes == b"echo x\n")
        );
        assert!(title(&"界".repeat(128)).is_ok());
        assert!(title(&"界".repeat(129)).is_err());
    }
    #[test]
    fn invalid_ids_bounds_and_ambiguous_extra_arguments_are_rejected() {
        for fixture in [
            vec!["focus", "0"],
            vec!["resize", "3", "0", "24"],
            vec!["resize-split", "4", "99"],
            vec!["focus-direction", "left", "--other"],
            vec!["split", "3", "left"],
            vec!["status", "unexpected"],
            vec!["--socket", "relative", "status"],
        ] {
            assert!(Arguments::parse(&args(&fixture)).is_err(), "{fixture:?}");
        }
        assert!(
            Arguments::parse(&args(&["input", "3", &"x".repeat(MAX_INPUT_BYTES + 1)])).is_err()
        );
    }
    #[test]
    fn global_options_do_not_consume_terminal_text_as_flags() {
        let parsed =
            Arguments::parse(&args(&["--json", "--take-control", "paste", "3", "--json"])).unwrap();
        assert!(parsed.json && parsed.take_control);
        assert!(
            matches!(parsed.command,CliCommand::Operation(Operation::Paste{text,..}) if text == "--json")
        );
        assert!(matches!(
            Arguments::parse(&args(&["--help"])).unwrap().command,
            CliCommand::Help
        ));
    }
}
