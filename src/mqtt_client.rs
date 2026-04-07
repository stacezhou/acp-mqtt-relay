use std::time::Duration;

use anyhow::{Context, Result};
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, MqttOptions, Outgoing, QoS};
use tokio::sync::mpsc;
use url::Url;

#[derive(Debug, Clone)]
pub struct RelayTopicLayout {
    node_id: String,
}

impl RelayTopicLayout {
    pub fn for_node(node_id: &str) -> Self {
        Self {
            node_id: node_id.to_string(),
        }
    }

    pub fn control_topic(&self) -> String {
        format!("acp/{}/control", self.node_id)
    }

    pub fn session_in_topic(&self, client_id: &str) -> String {
        format!("acp/{}/{client_id}/in", self.node_id)
    }

    pub fn session_out_topic(&self, client_id: &str) -> String {
        format!("acp/{}/{client_id}/out", self.node_id)
    }

    pub fn session_in_wildcard_topic(&self) -> String {
        format!("acp/{}/+/in", self.node_id)
    }

    pub fn parse_session_in_topic(&self, topic: &str) -> Option<String> {
        let prefix = format!("acp/{}/", self.node_id);
        let suffix = "/in";

        if !topic.starts_with(&prefix) || !topic.ends_with(suffix) {
            return None;
        }

        let client_id = &topic[prefix.len()..topic.len() - suffix.len()];
        if client_id.is_empty() || client_id.contains('/') {
            return None;
        }

        Some(client_id.to_string())
    }
}

#[derive(Debug)]
pub enum MqttIncomingMessage {
    Publish { topic: String, payload: Vec<u8> },
}

#[derive(Clone)]
pub struct MqttRelayClient {
    client: AsyncClient,
    subscriptions: Vec<String>,
}

impl MqttRelayClient {
    pub fn new(
        broker: &str,
        mqtt_client_id: &str,
        username: Option<&str>,
        password: Option<&str>,
        subscriptions: Vec<String>,
    ) -> Result<(Self, EventLoop)> {
        let (host, port) = parse_broker(broker)?;

        let mut options = MqttOptions::new(mqtt_client_id, host, port);
        options.set_keep_alive(Duration::from_secs(10));
        options.set_clean_session(false);

        if let Some(username) = username {
            options.set_credentials(username, password.unwrap_or_default());
        }

        let (client, event_loop) = AsyncClient::new(options, 64);
        Ok((
            Self {
                client,
                subscriptions,
            },
            event_loop,
        ))
    }

    pub async fn subscribe_all(&self) -> Result<()> {
        for topic in &self.subscriptions {
            self.client
                .subscribe(topic, QoS::AtLeastOnce)
                .await
                .with_context(|| format!("failed to subscribe MQTT topic {topic}"))?;
        }

        Ok(())
    }

    pub async fn publish(&self, topic: &str, payload: Vec<u8>) -> Result<()> {
        self.client
            .publish(topic, QoS::AtLeastOnce, false, payload)
            .await
            .with_context(|| format!("failed to publish MQTT topic {topic}"))
    }

    pub async fn start_event_loop(
        &self,
        mut event_loop: EventLoop,
        tx: mpsc::Sender<MqttIncomingMessage>,
    ) -> Result<()> {
        loop {
            match event_loop.poll().await {
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    tracing::info!(subscriptions = ?self.subscriptions, "mqtt connected");
                    self.subscribe_all()
                        .await
                        .context("failed to subscribe after connection")?;
                }
                Ok(Event::Incoming(Incoming::Publish(publish))) => {
                    tracing::info!(
                        topic = %publish.topic,
                        payload_len = publish.payload.len(),
                        "mqtt publish received"
                    );
                    if tx
                        .send(MqttIncomingMessage::Publish {
                            topic: publish.topic,
                            payload: publish.payload.to_vec(),
                        })
                        .await
                        .is_err()
                    {
                        tracing::info!("mqtt receiver dropped, stopping event loop");
                        return Ok(());
                    }
                }
                Ok(Event::Outgoing(Outgoing::PingReq)) => {
                    tracing::debug!("mqtt ping request sent");
                }
                Ok(Event::Outgoing(Outgoing::PingResp)) => {
                    tracing::debug!("mqtt ping response received");
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "mqtt event loop error, retrying");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
}

fn parse_broker(input: &str) -> Result<(String, u16)> {
    let normalized = if input.contains("://") {
        input.to_string()
    } else {
        format!("mqtt://{input}")
    };

    let url = Url::parse(&normalized).with_context(|| format!("invalid broker url: {input}"))?;
    let host = url
        .host_str()
        .context("broker url is missing host")?
        .to_string();
    let port = url.port().unwrap_or(1883);
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::{parse_broker, RelayTopicLayout};

    #[test]
    fn parses_broker_without_scheme() {
        let (host, port) = parse_broker("localhost").expect("broker parsing");
        assert_eq!(host, "localhost");
        assert_eq!(port, 1883);
    }

    #[test]
    fn derives_session_topics() {
        let topics = RelayTopicLayout::for_node("test-node");
        assert_eq!(topics.control_topic(), "acp/test-node/control");
        assert_eq!(
            topics.session_in_topic("client-a"),
            "acp/test-node/client-a/in"
        );
        assert_eq!(
            topics.session_out_topic("client-a"),
            "acp/test-node/client-a/out"
        );
        assert_eq!(topics.session_in_wildcard_topic(), "acp/test-node/+/in");
    }

    #[test]
    fn parses_client_id_from_session_input_topic() {
        let topics = RelayTopicLayout::for_node("test-node");
        assert_eq!(
            topics.parse_session_in_topic("acp/test-node/client-a/in"),
            Some("client-a".to_string())
        );
        assert_eq!(topics.parse_session_in_topic("acp/test-node/control"), None);
        assert_eq!(
            topics.parse_session_in_topic("acp/test-node/client-a/out"),
            None
        );
    }
}
