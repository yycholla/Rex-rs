//! Bounded, local NDJSON adapter over the native Rex client. Never launches a server.
use anyhow::{Context, Result, bail, ensure};
use rex_rs::{
    core::{
        Command, GridSize, PaneId, SplitAxis,
        input::{KeyInput, Modifiers},
    },
    rex::{
        WorkspaceClient,
        automation::ActionError,
        workspace::{ViewLayout, WorkspaceState},
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    future::Future,
    io::{self, BufRead, Write},
    sync::mpsc,
    task::{Context as TaskContext, Poll, Waker},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_LINE: usize = 32 * 1024;
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_REQUESTS: usize = 100_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Request {
    id: String,
    op: String,
    generation: Option<String>,
    session_id: Option<String>,
    window_id: Option<String>,
    block_id: Option<String>,
    text: Option<String>,
    key: Option<String>,
    modifiers: Option<Modifiers>,
    paste: Option<bool>,
    cols: Option<u16>,
    rows: Option<u16>,
    action: Option<String>,
    title: Option<String>,
    confirmed: Option<bool>,
}
impl Request {
    fn parse(line: &[u8]) -> Result<Self> {
        ensure!(line.len() <= MAX_LINE, "request too large");
        let request: Self = serde_json::from_slice(line)?;
        ensure!(
            !request.id.is_empty() && request.id.len() <= 80,
            "invalid request id"
        );
        ensure!(
            ["snapshot", "select", "input", "resize", "command"].contains(&request.op.as_str()),
            "unknown operation"
        );
        for id in [&request.session_id, &request.window_id, &request.block_id]
            .into_iter()
            .flatten()
        {
            ensure!(!id.is_empty() && id.len() <= 512, "invalid target id");
        }
        ensure!(
            request.text.as_ref().is_none_or(|s| s.len() <= 8192),
            "input too large"
        );
        ensure!(
            request
                .title
                .as_ref()
                .is_none_or(|s| s.len() <= 256 && !s.chars().any(char::is_control)),
            "invalid title"
        );
        Ok(request)
    }
}
fn valid_key(key: &str) -> bool {
    (key.len() == 1
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b.is_ascii_punctuation()))
        || [
            "enter",
            "backspace",
            "delete",
            "insert",
            "tab",
            "escape",
            "up",
            "down",
            "left",
            "right",
            "home",
            "end",
            "pageup",
            "pagedown",
            "space",
            "f1",
            "f2",
            "f3",
            "f4",
            "f5",
            "f6",
            "f7",
            "f8",
            "f9",
            "f10",
            "f11",
            "f12",
        ]
        .contains(&key)
}
fn remote(state: &WorkspaceState, pane: PaneId) -> Option<&str> {
    state.remote_pane(pane).ok()
}
fn layout(state: &WorkspaceState, node: &ViewLayout) -> Value {
    match node {
        ViewLayout::Pane(pane) => json!({"type":"pane", "blockId":remote(state,*pane)}),
        ViewLayout::Split {
            axis,
            ratio,
            first,
            second,
            ..
        } => {
            json!({"type":"split","axis":match axis {SplitAxis::Horizontal=>"horizontal",SplitAxis::Vertical=>"vertical"},"ratio":ratio,"first":layout(state,first),"second":layout(state,second)})
        }
    }
}
struct Bridge {
    client: WorkspaceClient,
    incarnation: String,
    errors: Vec<String>,
}
impl Bridge {
    fn generation(&self) -> String {
        format!(
            "{}:{}",
            self.incarnation,
            self.client.connection_generation()
        )
    }
    fn poll(&mut self) {
        if let Err(error) = self.client.poll() {
            eprintln!("Rex bridge update: {error:#}");
            self.errors
                .push("Rex update failed; queued input may not have applied".into());
            self.errors.truncate(16);
        }
        // Never execute externally requested UI actions or export clipboard contents.
        drop(self.client.take_program_clipboard());
        for action in self.client.take_client_actions() {
            let _ = action.reply(Err(ActionError::unsupported(
                "Web bridge does not execute client actions",
            )));
        }
    }
    fn snapshot(&self) -> Value {
        let state = self.client.state();
        let sessions: Vec<_> = state.sessions().iter().map(|s| json!({"id":s.remote_id(),"title":s.title(),"activeWindowId":s.active_window().map(rex_rs::rex::workspace::WindowView::remote_id),"windows":s.windows().iter().map(|w|json!({"id":w.remote_id(),"title":w.title(),"activeBlockId":w.active_pane().and_then(|p|remote(state,p)),"layers":w.layers.iter().map(|l|json!({"id":l.remote_id,"kind":l.kind,"bounds":l.bounds,"layout":l.layout.as_ref().map(|n|layout(state,n))})).collect::<Vec<_>>()})).collect::<Vec<_>>() })).collect();
        let mut frames = serde_json::Map::new();
        let blocks: Vec<_> = state.pane_ids().into_iter().filter_map(|p| {
            let b = state.pane(p)?;
            if let Some(f) = self.client.frame(p) {
                frames.insert(b.remote_id.clone(),json!({"cols":f.size.columns(),"rows":f.size.rows(),"lines":(0..f.rows.len()).map(|r|f.row_runs(r)).collect::<Vec<_>>(),"cursor":f.cursor,"foreground":f.foreground,"background":f.background,"imagesOmitted":!f.images.is_empty()}));
            }
            Some(json!({"id":b.remote_id,"sessionId":b.session_id,"title":b.title(),"owner":b.owner,"terminal":b.is_terminal(),"detached":state.is_detached(p),"status":self.client.status(p).map(|s|format!("{s:?}")),"inputAvailable":self.client.input_available(p),"ownsSize":self.client.owns_size(p)}))
        }).collect();
        json!({"connected":self.client.connected(),"generation":self.generation(),"revision":self.client.state_revision(),"serverId":self.client.server_id(),"activeSessionId":state.active_session().map(rex_rs::rex::workspace::SessionView::remote_id),"sessions":sessions,"blocks":blocks,"frames":frames,"errors":self.errors})
    }
    fn pane(&self, r: &Request) -> Result<PaneId> {
        let id = r.block_id.as_deref().context("blockId required")?;
        self.client
            .state()
            .pane_ids()
            .into_iter()
            .find(|p| remote(self.client.state(), *p) == Some(id))
            .context("block no longer exists")
    }
    fn command(&self, r: &Request) -> Result<Command> {
        let state = self.client.state();
        let session = || {
            state
                .sessions()
                .iter()
                .find(|s| Some(s.remote_id()) == r.session_id.as_deref())
                .map(rex_rs::rex::workspace::SessionView::id)
                .context("session no longer exists")
        };
        let window = || {
            state
                .sessions()
                .iter()
                .flat_map(rex_rs::rex::workspace::SessionView::windows)
                .find(|w| Some(w.remote_id()) == r.window_id.as_deref())
                .map(rex_rs::rex::workspace::WindowView::id)
                .context("window no longer exists")
        };
        let title = || r.title.clone().unwrap_or_default();
        Ok(match r.action.as_deref().context("action required")? {
            "new-session" => Command::CreateSession { title: title() },
            "new-window" => Command::CreateWindow {
                session: session()?,
                title: title(),
            },
            "rename-session" => Command::RenameSession {
                session: session()?,
                title: title(),
            },
            "rename-window" => Command::RenameWindow {
                window: window()?,
                title: title(),
            },
            "close-session" => Command::CloseSession(session()?),
            "close-window" => Command::CloseWindow(window()?),
            "close-block" => Command::ClosePane(self.pane(r)?),
            "detach-block" => Command::DetachPane(self.pane(r)?),
            "restore-block" => Command::RestorePane {
                pane: self.pane(r)?,
                title: title(),
            },
            "split-right" => Command::SplitPane {
                pane: self.pane(r)?,
                axis: SplitAxis::Horizontal,
            },
            "split-down" => Command::SplitPane {
                pane: self.pane(r)?,
                axis: SplitAxis::Vertical,
            },
            _ => bail!("unknown command"),
        })
    }
    fn apply(&mut self, command: Command) -> Result<Value> {
        let mut future = Box::pin(self.client.perform_command(command)?);
        let start = Instant::now();
        loop {
            self.poll();
            if let Poll::Ready(result) = future
                .as_mut()
                .poll(&mut TaskContext::from_waker(Waker::noop()))
            {
                self.poll();
                return result
                    .map(|value| json!({"ack":"applied","value":value}))
                    .context(
                        "uncertain: command completion failed; inspect state before any retry",
                    );
            }
            ensure!(
                start.elapsed() < Duration::from_secs(5),
                "uncertain: command timed out; do not replay"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn handle(&mut self, r: &Request) -> Result<Value> {
        if r.op == "snapshot" {
            let cells: usize = self
                .client
                .state()
                .pane_ids()
                .into_iter()
                .filter_map(|p| self.client.frame(p))
                .map(|f| usize::from(f.size.columns()) * usize::from(f.size.rows()))
                .sum();
            ensure!(cells <= 262_144, "snapshot cell budget exceeded");
            return Ok(self.snapshot());
        }
        ensure!(self.client.connected(), "disconnected");
        ensure!(
            r.generation.as_deref() == Some(self.generation().as_str()),
            "stale generation"
        );
        match r.op.as_str() {
            "select" => {
                let s = self
                    .client
                    .state()
                    .sessions()
                    .iter()
                    .find(|s| Some(s.remote_id()) == r.session_id.as_deref())
                    .context("session no longer exists")?;
                let sid = s.id();
                let wid = r
                    .window_id
                    .as_deref()
                    .map(|id| {
                        s.windows()
                            .iter()
                            .find(|w| w.remote_id() == id)
                            .map(rex_rs::rex::workspace::WindowView::id)
                            .context("window not in session")
                    })
                    .transpose()?;
                self.apply(Command::SelectSession(sid))?;
                if let Some(w) = wid {
                    ensure!(
                        self.client.connected()
                            && r.generation.as_deref() == Some(self.generation().as_str()),
                        "stale generation after session selection; window not selected"
                    );
                    self.apply(Command::SelectWindow(w))?;
                }
                Ok(json!({"ack":"applied"}))
            }
            "input" => {
                let p = self.pane(r)?;
                match (&r.text, &r.key) {
                    (Some(text), None) => {
                        if r.paste == Some(true) {
                            self.client.paste(p, text)?;
                        } else {
                            ensure!(
                                text.chars().count() == 1 && !text.chars().any(char::is_control),
                                "text input requires one printable character; use paste:true for composed text and native keys for controls"
                            );
                            self.client.write_input(p, text.as_bytes())?;
                        }
                    }
                    (None, Some(key)) => {
                        ensure!(r.paste != Some(true), "paste requires text");
                        ensure!(valid_key(key), "unsupported key");
                        self.client.send_key(
                            p,
                            &KeyInput {
                                key: key.clone(),
                                physical_key: None,
                                text: None,
                                modifiers: r.modifiers.unwrap_or_default(),
                                repeat: false,
                            },
                        )?;
                    }
                    _ => bail!("provide exactly one of text or key"),
                }
                Ok(json!({"ack":"queued"}))
            }
            "resize" => {
                let p = self.pane(r)?;
                ensure!(
                    self.client.owns_size(p),
                    "size ownership required; bridge never takes ownership"
                );
                let grid = GridSize::new(
                    r.cols.context("cols required")?,
                    r.rows.context("rows required")?,
                )?;
                self.client.resize(p, grid, 8, 16)?;
                Ok(json!({"ack":"queued"}))
            }
            "command" => {
                ensure!(r.confirmed == Some(true), "explicit confirmation required");
                self.apply(self.command(r)?)
            }
            _ => bail!("unknown operation"),
        }
    }
}
fn read_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let n = buf
            .iter()
            .position(|b| *b == b'\n')
            .map_or(buf.len(), |i| i + 1);
        ensure!(line.len() + n <= MAX_LINE, "request too large");
        let done = buf[n - 1] == b'\n';
        line.extend_from_slice(&buf[..n]);
        reader.consume(n);
        if done {
            return Ok(Some(line));
        }
    }
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 2 && args[0] == "--endpoint" && args[1].len() <= 4096,
        "usage: rex-web-bridge --endpoint <configured endpoint>"
    );
    let incarnation = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let mut bridge = Bridge {
        client: WorkspaceClient::connect(args[1].clone()),
        incarnation,
        errors: Vec::new(),
    };
    let (tx, rx) = mpsc::sync_channel(8);
    thread::spawn(move || {
        let mut input = io::stdin().lock();
        loop {
            match read_line(&mut input) {
                Ok(Some(line)) => {
                    if tx.send(Ok(line)).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        }
    });
    let mut seen = HashSet::new();
    let mut output = io::stdout().lock();
    loop {
        bridge.poll();
        let line = match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(line) => line?,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let exhausted = seen.len() >= MAX_REQUESTS;
        let response = match Request::parse(&line) {
            Ok(r) => {
                let result = if seen.len() >= MAX_REQUESTS {
                    Err(anyhow::anyhow!(
                        "request budget exhausted; reconnect bridge"
                    ))
                } else if !seen.insert(r.id.clone()) {
                    Err(anyhow::anyhow!("duplicate request id; not replayed"))
                } else {
                    bridge.handle(&r)
                };
                match result {
                    Ok(value) => json!({"id":r.id,"ok":true,"result":value}),
                    Err(e) => {
                        json!({"id":r.id,"ok":false,"error":{"code":if e.to_string().starts_with("uncertain:"){"uncertain"}else{"rejected"},"message":e.to_string().chars().take(1024).collect::<String>()}})
                    }
                }
            }
            Err(_) => {
                json!({"id":null,"ok":false,"error":{"code":"invalid_request","message":"Invalid or oversized request"}})
            }
        };
        let encoded = serde_json::to_vec(&response)?;
        if encoded.len() > MAX_RESPONSE {
            writeln!(
                output,
                "{}",
                json!({"id":response["id"],"ok":false,"error":{"code":"response_limit","message":"Snapshot exceeds bridge limit"}})
            )?;
        } else {
            output.write_all(&encoded)?;
            output.write_all(b"\n")?;
        }
        output.flush()?;
        if exhausted {
            break;
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unknown_and_unbounded_requests() {
        assert!(Request::parse(br#"{"id":"1","op":"launch"}"#).is_err());
        assert!(Request::parse(br#"{"id":"1","op":"snapshot","endpoint":"bad"}"#).is_err());
        assert!(Request::parse(&vec![b' '; MAX_LINE + 1]).is_err());
        assert!(Request::parse(br#"{"id":"1","op":"snapshot"}"#).is_ok());
    }
    #[test]
    fn key_names_are_native_not_escape_sequences() {
        for key in ["enter", "up", "backspace", "c", "f12"] {
            assert!(valid_key(key));
        }
        for key in ["\x1b[A", "command", "", "f99"] {
            assert!(!valid_key(key));
        }
    }
    #[test]
    fn input_protocol_accepts_native_modifiers_and_paste() {
        assert!(Request::parse(br#"{"id":"1","op":"input","key":"c","modifiers":{"shift":false,"control":true,"alt":false,"super_key":false}}"#).is_ok());
        assert!(
            Request::parse(br#"{"id":"2","op":"input","text":"line\nline","paste":true}"#).is_ok()
        );
        assert!(
            Request::parse(br#"{"id":"3","op":"input","key":"c","modifiers":{"control":"yes"}}"#)
                .is_err()
        );
    }
    #[test]
    fn bounded_reader_preserves_messages() -> Result<()> {
        let mut input = io::Cursor::new(b"one\ntwo\n");
        assert_eq!(read_line(&mut input)?, Some(b"one\n".to_vec()));
        assert_eq!(read_line(&mut input)?, Some(b"two\n".to_vec()));
        assert_eq!(read_line(&mut input)?, None);
        assert!(read_line(&mut io::Cursor::new(vec![b'x'; MAX_LINE + 1])).is_err());
        Ok(())
    }
}
