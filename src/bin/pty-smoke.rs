//! Real PTY integration checks with bounded waits and deterministic shell scripts.
use anyhow::{Context, Result, ensure};
use rex_rs::{
    core::{Command, GridSize, PaneId, SessionId, WindowId},
    terminal::{PaneStatus, ShellConfig, TerminalConfig, TerminalWorkspace},
};
use std::{
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(8);

fn main() -> Result<()> {
    timed_check("input, resize and exit", check_input_resize_exit)?;
    timed_check("10,000-line final output ordering", check_exit_output_order)?;
    timed_check("hidden tabs and sessions", check_hidden_tabs_and_sessions)?;
    timed_check("failed spawn rollback", check_failed_spawn_is_transactional)?;
    timed_check("background cleanup", check_cleanup)?;
    timed_check(
        "interactive foreground cleanup",
        check_interactive_foreground_cleanup,
    )?;
    println!(
        "PASS: PTY output, input, resize, exit ordering, hidden tabs/sessions, spawn rollback, and process cleanup"
    );
    Ok(())
}

fn timed_check(name: &str, check: impl FnOnce() -> Result<()>) -> Result<()> {
    let start = Instant::now();
    check().with_context(|| format!("PTY smoke: {name}"))?;
    println!("PASS: {name} ({:.2?})", start.elapsed());
    Ok(())
}

fn shell(script: &str) -> ShellConfig {
    ShellConfig {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), script.into()],
        cwd: PathBuf::from("/tmp"),
    }
}

fn open(script: &str) -> Result<(TerminalWorkspace, PaneId)> {
    let mut workspace = TerminalWorkspace::live(TerminalConfig::default(), shell(script));
    workspace.apply(Command::CreateSession {
        title: "PTY smoke".into(),
    })?;
    let pane = workspace
        .state()
        .active_session()
        .context("PTY smoke session must exist")?
        .active_pane()
        .context("fixture session must have an attached pane")?;
    Ok((workspace, pane))
}

fn visible_text(workspace: &TerminalWorkspace, pane: PaneId) -> Result<String> {
    Ok(workspace
        .frame(pane)
        .context("PTY smoke frame must exist")?
        .text_lines()
        .join("\n"))
}

fn wait_for(
    workspace: &mut TerminalWorkspace,
    pane: PaneId,
    predicate: impl Fn(&str, Option<&PaneStatus>) -> bool,
) -> Result<String> {
    wait_for_with_metrics(workspace, pane, predicate).map(|(text, _)| text)
}

fn wait_for_with_metrics(
    workspace: &mut TerminalWorkspace,
    pane: PaneId,
    predicate: impl Fn(&str, Option<&PaneStatus>) -> bool,
) -> Result<(String, Duration)> {
    let deadline = Instant::now() + TIMEOUT;
    let mut max_poll = Duration::ZERO;
    loop {
        let poll_start = Instant::now();
        workspace.poll()?;
        let poll_time = poll_start.elapsed();
        max_poll = max_poll.max(poll_time);
        ensure!(
            poll_time < Duration::from_millis(100),
            "PTY poll blocked the application thread for {poll_time:?}"
        );
        let text = visible_text(workspace, pane)?;
        ensure!(
            Instant::now() < deadline,
            "PTY smoke timed out; status {:?}, screen {text:?}",
            workspace.status(pane)
        );
        if predicate(&text, workspace.status(pane)) {
            return Ok((text, max_poll));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn check_input_resize_exit() -> Result<()> {
    let (mut workspace, pane) = open(
        "stty -echo; printf 'READY\\n'; IFS= read -r value; printf 'INPUT:%s\\n' \"$value\"; stty size; exit 7",
    )?;
    wait_for(&mut workspace, pane, |text, _| text.contains("READY"))?;
    workspace.resize(pane, GridSize::new(100, 32)?, 8, 22)?;
    workspace.write_input(pane, b"hello from Rex\n")?;
    let text = wait_for(&mut workspace, pane, |text, status| {
        text.contains("INPUT:hello from Rex")
            && text.contains("32 100")
            && matches!(status, Some(PaneStatus::Exited { code: 7 }))
    })?;
    ensure!(
        !text.contains("INPUT:hello from Rex\nhello from Rex"),
        "stty echo configuration should be honored"
    );
    Ok(())
}

fn check_cleanup() -> Result<()> {
    let (mut workspace, pane) =
        open("sleep 30 & child=$!; printf 'CHILD:%s:DONE\\n' \"$child\"; wait")?;
    let leader = workspace
        .process_id(pane)
        .context("PTY child must have a PID")?;
    let text = wait_for(&mut workspace, pane, |text, _| text.contains(":DONE"))?;
    let descendant: u32 = text
        .lines()
        .find_map(|line| {
            line.strip_prefix("CHILD:")
                .and_then(|value| value.strip_suffix(":DONE"))
        })
        .context("PTY script must report its child PID")?
        .trim()
        .parse()?;
    let start = Instant::now();
    workspace.apply(Command::ClosePane(pane))?;
    ensure!(
        start.elapsed() < Duration::from_secs(1),
        "closing a pane must not wait for its child"
    );
    wait_for_cleanup(leader, descendant)
}

fn wait_for_cleanup(leader: u32, descendant: u32) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if !process_exists(leader) && !process_running(descendant) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "closing pane did not reap leader {leader} and stop descendant {descendant}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn marker_pid(text: &str, marker: &str) -> Option<u32> {
    text.lines().find_map(|line| {
        line.strip_prefix(marker)?
            .strip_suffix(":DONE")?
            .trim()
            .parse()
            .ok()
    })
}

fn check_interactive_foreground_cleanup() -> Result<()> {
    let mut workspace = TerminalWorkspace::live(
        TerminalConfig::default(),
        ShellConfig {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-i".into()],
            cwd: PathBuf::from("/tmp"),
        },
    );
    workspace.apply(Command::CreateSession {
        title: "Interactive cleanup".into(),
    })?;
    let (_, _, pane) = active_ids(&workspace)?;
    let leader = workspace
        .process_id(pane)
        .context("interactive shell must have a PID")?;
    workspace.write_input(pane, b"stty -echo; printf '\\nINTERACTIVE-READY\\n'\n")?;
    wait_for(&mut workspace, pane, |text, _| {
        text.lines().any(|line| line == "INTERACTIVE-READY")
    })?;
    workspace.write_input(
        pane,
        b"/bin/sh -c 'trap \"\" HUP; printf \"\\nFOREGROUND:%s:DONE\\n\" \"$$\"; exec sleep 30'\n",
    )?;
    let text = wait_for(&mut workspace, pane, |text, _| {
        marker_pid(text, "FOREGROUND:").is_some()
    })?;
    let foreground =
        marker_pid(&text, "FOREGROUND:").context("foreground job must report its PID")?;
    let group = nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(i32::try_from(foreground)?)))?;
    ensure!(
        group.as_raw() == i32::try_from(foreground)?,
        "interactive job must have a separate foreground process group"
    );
    ensure!(foreground != leader);
    let start = Instant::now();
    workspace.apply(Command::ClosePane(pane))?;
    ensure!(
        start.elapsed() < Duration::from_secs(1),
        "closing an interactive pane must not wait for its job"
    );
    wait_for_cleanup(leader, foreground)
}

fn check_exit_output_order() -> Result<()> {
    let (mut workspace, pane) = open(
        "i=0; while [ \"$i\" -lt 10000 ]; do printf 'row %s\\n' \"$i\"; i=$((i + 1)); done; printf 'FINAL-MARKER\\n'; exit 0",
    )?;
    let (text, max_poll) = wait_for_with_metrics(&mut workspace, pane, |_, status| {
        matches!(status, Some(PaneStatus::Exited { code: 0 }))
    })?;
    println!("  10,000-line output maximum poll: {max_poll:.2?}");
    ensure!(
        text.contains("FINAL-MARKER"),
        "exit status must follow the final output frame"
    );
    Ok(())
}

fn active_ids(workspace: &TerminalWorkspace) -> Result<(SessionId, WindowId, PaneId)> {
    let session = workspace
        .state()
        .active_session()
        .context("smoke session must exist")?;
    Ok((
        session.id(),
        session
            .active_window()
            .context("smoke window must exist")?
            .id(),
        session.active_pane().context("smoke pane must exist")?,
    ))
}

fn check_hidden_tabs_and_sessions() -> Result<()> {
    let (mut workspace, first) = open(
        "stty -echo; printf 'WAITING\\n'; IFS= read -r value; printf 'GOT:%s\\n' \"$value\"; exit 9",
    )?;
    wait_for(&mut workspace, first, |text, _| text.contains("WAITING"))?;
    let (session, first_window, _) = active_ids(&workspace)?;
    let first_process = workspace
        .process_id(first)
        .context("first tab must have a process")?;
    workspace.apply(Command::CreateWindow {
        session,
        title: "Second tab".into(),
    })?;
    let (_, second_window, second) = active_ids(&workspace)?;
    wait_for(&mut workspace, second, |text, _| text.contains("WAITING"))?;
    workspace.apply(Command::SelectWindow(first_window))?;
    ensure!(
        workspace.process_id(first) == Some(first_process),
        "selecting a hidden tab must not restart its process"
    );
    ensure!(matches!(workspace.status(first), Some(PaneStatus::Running)));
    workspace.apply(Command::SelectWindow(second_window))?;
    workspace.write_input(first, b"hidden tab\n")?;
    wait_for(&mut workspace, first, |text, status| {
        text.contains("GOT:hidden tab") && matches!(status, Some(PaneStatus::Exited { code: 9 }))
    })?;
    ensure!(
        active_ids(&workspace)?.2 == second,
        "hidden tab output must not steal focus"
    );
    let retained = Arc::clone(
        workspace
            .frame(first)
            .context("hidden tab frame must exist")?,
    );
    workspace.apply(Command::SelectWindow(first_window))?;
    ensure!(
        Arc::ptr_eq(
            &retained,
            workspace
                .frame(first)
                .context("selected frame must exist")?
        ),
        "switching tabs must retain terminal output"
    );
    ensure!(matches!(
        workspace.status(first),
        Some(PaneStatus::Exited { code: 9 })
    ));
    workspace.apply(Command::SelectWindow(second_window))?;
    check_hidden_session(&mut workspace, session, second)?;
    ensure!(workspace.process_id(first) == Some(first_process));
    ensure!(visible_text(&workspace, first)?.contains("GOT:hidden tab"));
    Ok(())
}

fn check_hidden_session(
    workspace: &mut TerminalWorkspace,
    session: SessionId,
    pane: PaneId,
) -> Result<()> {
    let process = workspace
        .process_id(pane)
        .context("second tab must have a process")?;
    workspace.apply(Command::CreateSession {
        title: "Other session".into(),
    })?;
    let other = active_ids(workspace)?.0;
    workspace.write_input(pane, b"hidden session\n")?;
    wait_for(workspace, pane, |text, status| {
        text.contains("GOT:hidden session")
            && matches!(status, Some(PaneStatus::Exited { code: 9 }))
    })?;
    ensure!(
        active_ids(workspace)?.0 == other,
        "hidden session output must not steal focus"
    );
    workspace.apply(Command::SelectSession(session))?;
    ensure!(
        active_ids(workspace)?.2 == pane,
        "session selection must restore its selected tab"
    );
    ensure!(
        workspace.process_id(pane) == Some(process),
        "switching sessions must not replace terminal resources"
    );
    ensure!(visible_text(workspace, pane)?.contains("GOT:hidden session"));
    ensure!(matches!(
        workspace.status(pane),
        Some(PaneStatus::Exited { code: 9 })
    ));
    Ok(())
}

struct ShellFixture {
    directory: PathBuf,
}

impl ShellFixture {
    fn new() -> Result<Self> {
        let directory = std::env::temp_dir().join(format!("rex-pty-smoke-{}", std::process::id()));
        std::fs::create_dir(&directory).context("create shell fixture directory")?;
        let fixture = Self { directory };
        std::os::unix::fs::symlink("/bin/sh", fixture.program())
            .context("create shell fixture symlink")?;
        Ok(fixture)
    }
    fn program(&self) -> PathBuf {
        self.directory.join("shell")
    }
}

impl Drop for ShellFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn check_failed_spawn_is_transactional() -> Result<()> {
    let fixture = ShellFixture::new()?;
    let config = ShellConfig {
        program: fixture.program(),
        ..shell("stty -echo; printf 'READY\\n'; IFS= read -r value; exit 0")
    };
    let mut workspace = TerminalWorkspace::live(TerminalConfig::default(), config);
    workspace.apply(Command::CreateSession {
        title: "Surviving session".into(),
    })?;
    let (session, _, pane) = active_ids(&workspace)?;
    wait_for(&mut workspace, pane, |text, _| text.contains("READY"))?;
    let original_state = workspace.state().clone();
    let original_frame = Arc::clone(
        workspace
            .frame(pane)
            .context("surviving frame must exist")?,
    );
    let original_process = workspace.process_id(pane);
    // The running shell has already exec'd /bin/sh; removing only its launch
    // path makes the next launch fail without affecting that existing process.
    std::fs::remove_file(fixture.program())?;
    ensure!(
        workspace
            .apply(Command::CreateWindow {
                session,
                title: "Cannot spawn".into()
            })
            .is_err()
    );
    ensure!(
        workspace.state() == &original_state,
        "failed spawn must leave the workspace unchanged"
    );
    ensure!(
        workspace.state().pane_ids() == [pane],
        "failed spawn must not add runtime identities"
    );
    ensure!(
        workspace.process_id(pane) == original_process,
        "failed spawn must preserve existing processes"
    );
    ensure!(
        Arc::ptr_eq(
            &original_frame,
            workspace
                .frame(pane)
                .context("original frame must survive")?
        ),
        "failed spawn must preserve existing frames"
    );
    workspace.write_input(pane, b"finish\n")?;
    wait_for(&mut workspace, pane, |_, status| {
        matches!(status, Some(PaneStatus::Exited { code: 0 }))
    })?;
    Ok(())
}

fn process_exists(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

fn process_running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        // A killed descendant can briefly remain a zombie until its new parent
        // reaps it. It is no longer running or holding the PTY in that state.
        stat.rsplit_once(") ")
            .is_some_and(|(_, tail)| !tail.starts_with('Z'))
    })
}
