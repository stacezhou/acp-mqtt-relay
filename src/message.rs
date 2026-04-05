use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StreamType {
    Stdin,
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcpMessage {
    #[serde(rename = "type")]
    pub stream: StreamType,
    pub content: String,
    #[serde(default = "default_encoding")]
    pub encoding: String,
}

fn default_encoding() -> String {
    "base64".to_string()
}

impl AcpMessage {
    pub fn new(stream: StreamType, bytes: &[u8]) -> Self {
        Self {
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

#[cfg(test)]
mod tests {
    use super::{AcpMessage, StreamType};

    #[test]
    fn round_trip_json() {
        let message = AcpMessage::new(StreamType::Stdout, b"hello");
        let encoded = message.to_json_line().expect("json encoding");
        let decoded = AcpMessage::from_slice(encoded.trim_ascii_end()).expect("json decoding");
        assert_eq!(decoded.stream, StreamType::Stdout);
        assert_eq!(decoded.decode_bytes().expect("base64 decode"), b"hello");
    }
}
