//! Real-process checks of the public automation CLI against a local server.
use anyhow::{Context, Result, ensure};
use rex_rs::server::{
    cli,
    protocol::{Operation, PROTOCOL_VERSION, Request, Response, read_message, write_message},
};
use serde_json::Value;
use std::{
    fs,
    os::unix::{fs::DirBuilderExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
const TIMEOUT: Duration = Duration::from_secs(8);

fn main() -> Result<()> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments.first().is_some_and(|arg| arg == "--server") {
        return rex_rs::server::run_default();
    }
    if arguments.first().is_some_and(|arg| arg == "--cli") {
        std::process::exit(cli::entry_with_args(&arguments[1..]));
    }
    let mut server = Fixture::new()?;
    check_layout(&server.path)?;
    check_terminal_and_errors(&server.path)?;
    check_controller_policy(&server.path)?;
    server.stop()?;
    println!(
        "PASS: CLI JSON/errors, Unicode labels, session/tab/split/zoom/ratio commands, capture/input and explicit control takeover"
    );
    Ok(())
}

fn invoke(path: &Path, arguments: &[&str]) -> Result<Output> {
    // Cargo may replace the pathname during a concurrent build. Execute the
    // running fixture image, including when its original inode is unlinked.
    Command::new(format!("/proc/{}/exe", std::process::id()))
        .arg("--cli")
        .arg("--socket")
        .arg(path)
        .arg("--json")
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .context("run CLI subprocess")
}
fn success(path: &Path, arguments: &[&str]) -> Result<Value> {
    let output = invoke(path, arguments)?;
    ensure!(
        output.status.success(),
        "CLI {arguments:?} failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).context("CLI result must be JSON")?;
    ensure!(value["success"] == true, "CLI reported failure: {value}");
    Ok(value)
}
fn field_id(value: &Value) -> Result<String> {
    value
        .as_u64()
        .map(|value| value.to_string())
        .context("missing numeric fixture ID")
}
fn active_pane(path: &Path) -> Result<String> {
    field_id(&success(path, &["status"])?["sessions"][0]["windows"][0]["active_pane"])
}

fn check_layout(path: &Path) -> Result<()> {
    let status = success(path, &["status"])?;
    ensure!(
        status["sessions"].as_array().is_some_and(Vec::is_empty),
        "status initialized an empty server"
    );
    let created = success(path, &["new-session", "CLI 界"])?;
    let session = field_id(&created["sessions"][0]["id"])?;
    let pane = field_id(&created["sessions"][0]["windows"][0]["active_pane"])?;
    let split = success(path, &["split", &pane, "right"])?;
    let split_id = field_id(&split["sessions"][0]["windows"][0]["layout"]["Split"]["id"])?;
    let ratio = success(path, &["resize-split", &split_id, "650"])?;
    ensure!(
        ratio["sessions"][0]["windows"][0]["layout"]["Split"]["ratio"] == 650,
        "resize-split did not change shared ratio"
    );
    let zoom = success(path, &["zoom", &pane])?;
    ensure!(
        zoom["sessions"][0]["windows"][0]["zoomed_pane"]
            .as_u64()
            .map(|id| id.to_string())
            .as_deref()
            == Some(pane.as_str()),
        "zoom was not authoritative"
    );
    success(path, &["zoom", &pane])?;
    success(path, &["focus-direction", "right"])?;
    let new_tab = success(path, &["new-tab", &session, "CLI tab"])?;
    let tab = field_id(&new_tab["sessions"][0]["active_window"])?;
    let renamed = success(path, &["rename-tab", &tab, "Renamed 界"])?;
    ensure!(
        renamed["sessions"][0]["windows"][1]["title"] == "Renamed 界",
        "tab rename did not persist in server state"
    );
    success(path, &["close-tab", &tab])?;
    let raw = success(path, &["snapshot"])?;
    ensure!(
        raw["snapshot"]["state"]["sessions"]
            .as_array()
            .is_some_and(|sessions| sessions.len() == 1),
        "snapshot omitted authoritative workspace"
    );
    Ok(())
}

fn check_terminal_and_errors(path: &Path) -> Result<()> {
    let pane = active_pane(path)?;
    success(
        path,
        &["input", &pane, "stty -echo; printf '%s%s\\n' CLI _READY\n"],
    )?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let captured = success(path, &["capture", &pane])?;
        if captured["text"]
            .as_str()
            .is_some_and(|text| text.contains("CLI_READY"))
        {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "CLI input did not reach the live terminal"
        );
        thread::sleep(Duration::from_millis(20));
    }
    for arguments in [
        vec!["focus", "0"],
        vec!["focus", "999999"],
        vec!["resize-split", "4", "0"],
        vec!["call", "not JSON"],
    ] {
        let output = invoke(path, &arguments)?;
        ensure!(
            !output.status.success(),
            "invalid CLI command succeeded: {arguments:?}"
        );
        let error: Value = serde_json::from_slice(&output.stdout)?;
        ensure!(
            error["success"] == false && error["error"].is_string(),
            "invalid command lacked structured error"
        );
    }
    Ok(())
}

fn check_controller_policy(path: &Path) -> Result<()> {
    let mut owner = UnixStream::connect(path)?;
    owner.set_read_timeout(Some(TIMEOUT))?;
    owner.set_write_timeout(Some(TIMEOUT))?;
    let mut request = Request {
        version: PROTOCOL_VERSION,
        server_id: None,
        known_revision: None,
        operation: Operation::Snapshot,
    };
    write_message(&mut owner, &request)?;
    let response: Response = read_message(&mut owner)?;
    request.server_id = Some(response.server_id);
    request.operation = Operation::AcquireControl { force: true };
    write_message(&mut owner, &request)?;
    let response: Response = read_message(&mut owner)?;
    ensure!(
        response.error.is_none() && response.has_control,
        "fixture failed to acquire control"
    );
    let pane = active_pane(path)?;
    let rejected = invoke(path, &["input", &pane, ""])?;
    ensure!(
        !rejected.status.success(),
        "CLI stole terminal control without explicit takeover"
    );
    success(path, &["capture", &pane])?;
    success(path, &["--take-control", "input", &pane, ""])?;
    request.operation = Operation::Snapshot;
    write_message(&mut owner, &request)?;
    let response: Response = read_message(&mut owner)?;
    ensure!(
        !response.has_control,
        "explicit CLI takeover did not revoke old owner"
    );
    Ok(())
}

struct Fixture {
    directory: PathBuf,
    path: PathBuf,
    child: Child,
}
impl Fixture {
    fn new() -> Result<Self> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let directory =
            std::env::temp_dir().join(format!("rex-cli-smoke-{}-{nonce}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let path = directory.join("server.sock");
        let child = Command::new(format!("/proc/{}/exe", std::process::id()))
            .args(["--server", "--ephemeral", "--socket"])
            .arg(&path)
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let mut fixture = Self {
            directory,
            path,
            child,
        };
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if UnixStream::connect(&fixture.path).is_ok() {
                return Ok(fixture);
            }
            ensure!(
                fixture.child.try_wait()?.is_none(),
                "CLI fixture server exited early"
            );
            ensure!(Instant::now() < deadline, "CLI fixture did not bind");
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn stop(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        success(&self.path, &["call", "\"Shutdown\""])?;
        let deadline = Instant::now() + TIMEOUT;
        while self.child.try_wait()?.is_none() {
            ensure!(Instant::now() < deadline, "CLI fixture server did not stop");
            thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.stop().is_err() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}
