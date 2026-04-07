use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StreamType {
    Stdin,
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcpMessage {
    pub seq: u64,
    #[serde(rename = "type")]
    pub stream: StreamType,
    pub content: String,
    #[serde(default = "default_encoding")]
    pub encoding: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    Spawn,
    Heartbeat,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlMessage {
    pub client_id: String,
    pub action: ControlAction,
    pub sent_at_ms: u64,
}

fn default_encoding() -> String {
    "base64".to_string()
}

impl AcpMessage {
    pub fn new(seq: u64, stream: StreamType, bytes: &[u8]) -> Self {
        Self {
            seq,
            stream,
            content: STANDARD.encode(bytes),
            encoding: default_encoding(),
        }
    }

    pub fn decode_bytes(&self) -> Result<Vec<u8>> {
        match self.encoding.as_str() {
            "base64" => STANDARD
                .decode(self.content.as_bytes())
                .context("failed to decode base64 content"),
            other => anyhow::bail!("unsupported encoding: {other}"),
        }
    }

    pub fn to_json_line(&self) -> Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec(self).context("failed to serialize message")?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub fn from_slice(slice: &[u8]) -> Result<Self> {
        serde_json::from_slice(slice).context("failed to deserialize ACP message")
    }
}

impl ControlMessage {
    pub fn new(client_id: impl Into<String>, action: ControlAction) -> Self {
        Self {
            client_id: client_id.into(),
            action,
            sent_at_ms: now_unix_ms(),
        }
    }

    pub fn to_vec(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).context("failed to serialize control message")
    }

    pub fn from_slice(slice: &[u8]) -> Result<Self> {
        serde_json::from_slice(slice).context("failed to deserialize control message")
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::{AcpMessage, ControlAction, ControlMessage, StreamType};

    #[test]
    fn round_trip_json() {
        let message = AcpMessage::new(1, StreamType::Stdout, b"hello");
        let encoded = message.to_json_line().expect("json encoding");
        let decoded = AcpMessage::from_slice(encoded.trim_ascii_end()).expect("json decoding");
        assert_eq!(decoded.seq, 1);
        assert_eq!(decoded.stream, StreamType::Stdout);
        assert_eq!(decoded.decode_bytes().expect("base64 decode"), b"hello");
    }

    #[test]
    fn control_message_round_trip_json() {
        let message = ControlMessage::new("client-123", ControlAction::Heartbeat);
        let encoded = message.to_vec().expect("json encoding");
        let decoded = ControlMessage::from_slice(&encoded).expect("json decoding");
        assert_eq!(decoded.client_id, "client-123");
        assert_eq!(decoded.action, ControlAction::Heartbeat);
        assert!(decoded.sent_at_ms > 0);
    }
}
