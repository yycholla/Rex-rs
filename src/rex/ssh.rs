//! SSH-forwarded Unix sockets for Rex servers reachable only over SSH.
//!
//! A tunnel owns one `ssh -L` child. Opening blocks while the remote socket
//! is resolved and the forward becomes ready, so callers run it off the UI
//! thread. Dropping the tunnel stops the child without waiting for it; if this
//! process dies instead, the child's closed stdin ends the session.
use anyhow::{Context, Result, bail, ensure};
use std::{
    io::Read,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use url::Url;

/// Authentication and the forward itself must finish within this time.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// Diagnostics retained from ssh's standard error.
const STDERR_LIMIT: usize = 4096;

static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);

/// An SSH destination, optionally naming the remote Rex socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshTarget {
    user: Option<String>,
    host: String,
    port: Option<u16>,
    socket: Option<String>,
}

/// True when `value` should be read as an SSH address rather than a Rex endpoint.
#[must_use]
pub fn is_ssh_address(value: &str) -> bool {
    let value = value.trim();
    value.starts_with("ssh://") || (!value.contains("://") && value.contains('@'))
}

impl SshTarget {
    /// Parses `ssh://[user@]host[:port][/remote/socket]` or `user@host`.
    ///
    /// # Errors
    /// Rejects other schemes, passwords, queries, fragments and values ssh
    /// would read as options.
    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        ensure!(is_ssh_address(value), "not an SSH address");
        let url = if value.contains("://") {
            Url::parse(value)
        } else {
            Url::parse(&format!("ssh://{value}"))
        }
        .context("invalid SSH address")?;
        ensure!(url.scheme() == "ssh", "not an SSH address");
        ensure!(
            url.password().is_none() && url.query().is_none() && url.fragment().is_none(),
            "SSH addresses cannot contain passwords, queries or fragments"
        );
        let host = url
            .host_str()
            .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
            .filter(|host| !host.is_empty())
            .context("SSH address lacks a host")?
            .to_owned();
        let user = (!url.username().is_empty()).then(|| url.username().to_owned());
        // A leading dash would be read as an ssh option.
        ensure!(
            !host.starts_with('-')
                && user
                    .as_deref()
                    .is_none_or(|user| !user.starts_with('-') && !user.contains('%')),
            "invalid SSH user or host"
        );
        let socket = match url.path() {
            "" | "/" => None,
            path => {
                // `-L` separates its paths with a colon.
                ensure!(
                    !path.contains(['%', ':']) && !path.chars().any(char::is_control),
                    "invalid remote socket path"
                );
                Some(path.to_owned())
            }
        };
        Ok(Self {
            user,
            host,
            port: url.port(),
            socket,
        })
    }

    /// The normalized address, used to identify the server.
    #[must_use]
    pub fn canonical(&self) -> String {
        let user = self
            .user
            .as_ref()
            .map_or_else(String::new, |user| format!("{user}@"));
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let port = self
            .port
            .map_or_else(String::new, |port| format!(":{port}"));
        let socket = self.socket.as_deref().unwrap_or_default();
        format!("ssh://{user}{host}{port}{socket}")
    }

    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// `user@host`, as people type it.
    #[must_use]
    pub fn destination(&self) -> String {
        self.user
            .as_ref()
            .map_or_else(|| self.host.clone(), |user| format!("{user}@{}", self.host))
    }

    /// A non-interactive ssh invocation; host key and password prompts fail
    /// instead of waiting for a terminal that does not exist. `options` must
    /// precede the destination, after which ssh reads only the remote command.
    fn ssh(&self, options: &[&str]) -> Command {
        let mut command = Command::new("ssh");
        command.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
        command.args(options);
        if let Some(port) = self.port {
            command.arg("-p").arg(port.to_string());
        }
        if let Some(user) = &self.user {
            command.arg("-l").arg(user);
        }
        command.arg("--").arg(&self.host);
        command
    }

    /// The remote server socket: as given, else as reported by the remote Rex.
    fn resolve_socket(&self) -> Result<String> {
        if let Some(socket) = &self.socket {
            return Ok(socket.clone());
        }
        // A login shell supplies the user's PATH. Like the Rex CLI, status
        // starts a missing server.
        let output = self
            .ssh(&[])
            .arg("sh -lc 'rex server status'")
            .stdin(Stdio::null())
            .output()
            .context("run ssh")?;
        if let Some(socket) = parse_status_socket(&String::from_utf8_lossy(&output.stdout)) {
            return Ok(socket);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{}",
            ssh_failure(&stderr).unwrap_or_else(|| {
                if stderr.contains("not found") {
                    format!("Rex is not installed on {}", self.destination())
                } else {
                    format!(
                        "Rex did not report a server on {}: {}",
                        self.destination(),
                        last_line(&stderr).unwrap_or("no server status")
                    )
                }
            })
        )
    }
}

fn parse_status_socket(status: &str) -> Option<String> {
    status
        .lines()
        .find_map(|line| line.trim().strip_prefix("endpoint: unix://"))
        .filter(|path| path.starts_with('/'))
        .map(str::to_owned)
}

fn last_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).rfind(|line| !line.is_empty())
}

/// Explains ssh's own failures, which come before any remote command runs.
fn ssh_failure(stderr: &str) -> Option<String> {
    if stderr.contains("Host key verification failed") {
        return Some(
            "Host key not trusted. Connect once with ssh in a terminal to verify it.".into(),
        );
    }
    if stderr.contains("Permission denied") {
        return Some("SSH key authentication was refused".into());
    }
    last_line(stderr)
        .filter(|line| line.starts_with("ssh:"))
        .map(str::to_owned)
}

fn tunnel_directory() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("rex-rs");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&base)
        .context("create tunnel directory")?;
    Ok(base)
}

/// A running SSH forward from a private local socket to a remote Rex server.
pub struct SshTunnel {
    child: Option<Child>,
    /// Held open for the tunnel's lifetime; see `LIFELINE`.
    _lifeline: Option<ChildStdin>,
    local: PathBuf,
}

/// Remote command keeping the session alive until our end of its stdin closes,
/// which also happens when this process exits without dropping the tunnel.
const LIFELINE: &str = "cat >/dev/null";

impl SshTunnel {
    /// Resolve the remote socket, start the forward and wait until it listens.
    ///
    /// # Errors
    /// Reports ssh failures such as an untrusted host key or refused key, a
    /// missing remote Rex, or a forward that is not ready in time.
    pub fn open(target: &SshTarget) -> Result<Self> {
        let remote = target.resolve_socket()?;
        let name: String = target
            .host
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .take(32)
            .collect();
        let local = tunnel_directory()?.join(format!(
            "{name}-{}-{}.sock",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&local);
        let forward = format!("{}:{remote}", local.display());
        let mut child = target
            .ssh(&[
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "StreamLocalBindUnlink=yes",
                "-o",
                "ServerAliveInterval=30",
                "-L",
                &forward,
            ])
            .arg(LIFELINE)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("start ssh")?;
        let stderr = drain(&mut child);
        let lifeline = child.stdin.take();
        let mut tunnel = Self {
            child: Some(child),
            _lifeline: lifeline,
            local,
        };
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if let Some(child) = &mut tunnel.child
                && child.try_wait().context("wait for ssh")?.is_some()
            {
                tunnel.child = None;
                let stderr = stderr.lock().unwrap_or_else(PoisonError::into_inner);
                bail!(
                    "{}",
                    ssh_failure(&stderr).unwrap_or_else(|| format!(
                        "SSH forward failed: {}",
                        last_line(&stderr).unwrap_or("ssh exited")
                    ))
                );
            }
            if tunnel.local.exists() {
                return Ok(tunnel);
            }
            ensure!(
                Instant::now() < deadline,
                "SSH forward was not ready in time"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// The local Rex endpoint served by this forward.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("unix://{}", self.local.display())
    }
}

/// Keep the tail of ssh's diagnostics without letting a full pipe stall it.
fn drain(child: &mut Child) -> Arc<Mutex<String>> {
    let buffer = Arc::new(Mutex::new(String::new()));
    if let Some(mut stderr) = child.stderr.take() {
        let sink = Arc::clone(&buffer);
        let _ = thread::Builder::new()
            .name("ssh-stderr".into())
            .spawn(move || {
                let mut chunk = [0; 512];
                while let Ok(read @ 1..) = stderr.read(&mut chunk) {
                    let mut text = sink.lock().unwrap_or_else(PoisonError::into_inner);
                    text.push_str(&String::from_utf8_lossy(&chunk[..read]));
                    if text.len() > STDERR_LIMIT {
                        let mut cut = text.len() - STDERR_LIMIT;
                        while !text.is_char_boundary(cut) {
                            cut += 1;
                        }
                        text.drain(..cut);
                    }
                }
            });
    }
    buffer
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            // Reap off the caller's thread, which may be the UI thread.
            let _ = thread::Builder::new()
                .name("ssh-reap".into())
                .spawn(move || child.wait());
        }
        let _ = std::fs::remove_file(&self.local);
    }
}

#[cfg(test)]
mod tests {
    use super::{SshTarget, is_ssh_address, parse_status_socket, ssh_failure};

    #[test]
    fn addresses_accept_urls_and_shorthand_but_never_ssh_options() -> anyhow::Result<()> {
        let short = SshTarget::parse(" dev@192.0.2.10 ")?;
        assert_eq!(short.canonical(), "ssh://dev@192.0.2.10");
        assert_eq!(short.destination(), "dev@192.0.2.10");
        let full = SshTarget::parse("ssh://dev@host:2222/run/user/1000/rex/server.sock")?;
        assert_eq!(
            full.canonical(),
            "ssh://dev@host:2222/run/user/1000/rex/server.sock"
        );
        assert_eq!(SshTarget::parse("ssh://[::1]")?.canonical(), "ssh://[::1]");
        assert!(SshTarget::parse("ssh://-oProxyCommand=x@host").is_err());
        assert!(SshTarget::parse("ssh://dev:secret@host").is_err());
        assert!(SshTarget::parse("ssh://dev@host/a:b").is_err());
        assert!(SshTarget::parse("https://host").is_err());
        assert!(!is_ssh_address("unix:///tmp/rex.sock"));
        assert!(!is_ssh_address("https://user@host"));
        Ok(())
    }

    #[test]
    fn options_precede_the_destination() -> anyhow::Result<()> {
        let command = SshTarget::parse("ssh://dev@host:2222")?.ssh(&["-N", "-L", "a:b"]);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy())
            .collect();
        let separator = args.iter().position(|arg| arg == "--");
        assert_eq!(separator, Some(args.len() - 2), "{args:?}");
        assert_eq!(args.last().map(AsRef::as_ref), Some("host"));
        assert!(args.iter().position(|arg| arg == "-N") < separator);
        Ok(())
    }

    #[test]
    fn remote_socket_comes_from_rex_status() {
        let status = "you: uid 1000 over unix\nendpoint: unix:///run/user/1000/rex/server.sock\n";
        assert_eq!(
            parse_status_socket(status).as_deref(),
            Some("/run/user/1000/rex/server.sock")
        );
        assert_eq!(
            parse_status_socket("Error: rex server is not running"),
            None
        );
    }

    #[test]
    fn ssh_failures_explain_how_to_recover() {
        assert!(
            ssh_failure("Host key verification failed.\r\n")
                .is_some_and(|message| message.contains("Connect once"))
        );
        assert_eq!(
            ssh_failure("ssh: connect to host h port 22: Connection refused\n").as_deref(),
            Some("ssh: connect to host h port 22: Connection refused")
        );
        assert_eq!(ssh_failure("bash: rex: command not found"), None);
    }
}
