//! Low-volume development diagnostics. Never log protocol payloads or credentials.
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

pub(crate) fn write(level: &str, component: &str, message: &str) {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    eprintln!(
        "{}.{:03} {level} rex-rs::{component} {}",
        timestamp.as_secs(),
        timestamp.subsec_millis(),
        message.escape_debug()
    );
}

/// Shared mutation diagnostics for core-command and native-action entry points.
/// Captures only method/identity metadata, never labels, terminal data or secrets.
pub(crate) struct WorkspaceCall {
    method: String,
    session: String,
    window: String,
    started: Instant,
}

impl WorkspaceCall {
    pub(crate) fn new(method: &str, params: &Value) -> Self {
        Self {
            method: method.to_owned(),
            session: params["session_id"].as_str().unwrap_or("-").to_owned(),
            window: params["window_id"].as_str().unwrap_or("-").to_owned(),
            started: Instant::now(),
        }
    }

    pub(crate) fn applied(&self, response: &Value) {
        write(
            "INFO",
            "client",
            &format!(
                "workspace applied method={} session={} window={} revision={} elapsed_ms={}",
                self.method,
                response["session_id"].as_str().unwrap_or(&self.session),
                response["window_id"].as_str().unwrap_or(&self.window),
                response["revision"]
                    .as_u64()
                    .map_or_else(|| "-".into(), |revision| revision.to_string()),
                self.started.elapsed().as_millis(),
            ),
        );
    }
}
