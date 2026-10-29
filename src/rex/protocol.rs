//! Bounded envelopes for the Rex control WebSocket protocol.
use anyhow::{Result, ensure};
use serde_json::{Value, json};

/// A server-reported API failure, retained for cross-server callback replies.
#[derive(Debug)]
pub struct RemoteError {
    pub code: String,
    pub message: String,
}
impl std::fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Rex {}: {}", self.code, self.message)
    }
}
impl std::error::Error for RemoteError {}

pub const API_REVISION: u64 = 7;
pub const MAX_MESSAGE_BYTES: usize = 1_048_576;
pub const MAX_PENDING_EVENTS: usize = 256;
pub const MAX_PENDING_EVENT_BYTES: usize = 8 * 1_048_576;

#[must_use]
pub fn hello() -> Value {
    json!({
        "type": "hello", "api_revision": API_REVISION, "api_min_revision": API_REVISION,
        "client": {"kind": "app", "platform": "linux", "name": "rex-rs",
                   "version": env!("CARGO_PKG_VERSION")}
    })
}

/// Build one ordered request with an optional object payload.
///
/// # Errors
/// Rejects non-object arguments and messages exceeding the protocol limit.
pub fn request(id: u64, method: &str, payload: Value) -> Result<Value> {
    ensure!(!method.is_empty(), "Rex method must not be empty");
    ensure!(
        payload.is_null() || payload.is_object(),
        "Rex arguments must be an object"
    );
    let mut value = json!({"type":"request", "id":format!("rex-rs-{id}"), "method":method});
    if !payload.is_null() {
        value["payload"] = payload;
    }
    ensure!(
        serde_json::to_vec(&value)?.len() <= MAX_MESSAGE_BYTES,
        "Rex request exceeds message limit"
    );
    Ok(value)
}

/// Extract a matching response without discarding the server's sequence number.
///
/// # Errors
/// Rejects malformed responses and server-reported operation failures.
pub fn response(value: &Value, expected_id: &str) -> Result<(u64, Value)> {
    ensure!(value["type"] == "response", "expected Rex response");
    ensure!(
        value["id"].as_str() == Some(expected_id),
        "unexpected Rex response ID"
    );
    let sequence = value["seq"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("Rex response lacks sequence"))?;
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        return Err(RemoteError {
            code: error["code"].as_str().unwrap_or("error").to_owned(),
            message: error["message"]
                .as_str()
                .unwrap_or("request failed")
                .to_owned(),
        }
        .into());
    }
    Ok((
        sequence,
        value.get("payload").cloned().unwrap_or(Value::Null),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_keeps_sequence_and_reports_remote_errors() -> Result<()> {
        let (seq, payload) = response(
            &json!({"type":"response","id":"r","seq":8,"payload":{"ok":true}}),
            "r",
        )?;
        assert_eq!(seq, 8);
        assert_eq!(payload["ok"], true);
        assert!(response(&json!({"type":"response","id":"other","seq":8}), "r").is_err());
        assert!(response(&json!({"type":"response","id":"r","seq":8,"error":{"code":"not_found","message":"gone"}}), "r").is_err());
        Ok(())
    }

    #[test]
    fn request_rejects_invalid_and_oversized_arguments() -> Result<()> {
        assert!(request(1, "method", json!([])).is_err());
        assert!(request(1, "method", json!({"text":"x".repeat(MAX_MESSAGE_BYTES)})).is_err());
        let value = request(1, "session.list", Value::Null)?;
        assert!(value.get("payload").is_none());
        assert_eq!(value["id"], "rex-rs-1");
        Ok(())
    }
}
