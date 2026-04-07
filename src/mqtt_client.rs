use std::time::Duration;

use anyhow::{Context, Result};
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, MqttOptions, Outgoing, QoS};
use tokio::sync::mpsc;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayRole {
    User,
    Work,
}

#[derive(Debug, Clone)]
pub struct RelayTopics {
    pub inbound: String,
    pub outbound: String,
}

impl RelayTopics {
    pub fn for_node(node_id: &str) -> Self {
        Self {
            inbound: format!("acp/{node_id}/in"),
            outbound: format!("acp/{node_id}/out"),
        }
    }
}

#[derive(Debug)]
pub enum MqttIncomingMessage {
    Publish { topic: String, payload: Vec<u8> },
}

#[derive(Clone)]
pub struct MqttRelayClient {
    client: AsyncClient,
    pub topics: RelayTopics,
    role: RelayRole,
}

impl MqttRelayClient {
    pub fn new(
        broker: &str,
        node_id: &str,
        role: RelayRole,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<(Self, EventLoop)> {
        let topics = RelayTopics::for_node(node_id);
        let (host, port) = parse_broker(broker)?;
        let client_id = format!("amr-{}-{node_id}", role.as_str());

        let mut options = MqttOptions::new(client_id, host, port);
        options.set_keep_alive(Duration::from_secs(10));
        options.set_clean_session(false);

        if let Some(username) = username {
            options.set_credentials(username, password.unwrap_or_default());
        }

        let (client, event_loop) = AsyncClient::new(options, 64);
        Ok((
            Self {
                client,
                topics,
                role,
            },
            event_loop,
        ))
    }

    pub async fn subscribe(&self) -> Result<()> {
        let topic = self.subscribe_topic();
        self.client
            .subscribe(topic, QoS::AtLeastOnce)
            .await
            .context("failed to subscribe MQTT topic")
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
        let subscription_topic = self.subscribe_topic().to_string();

        loop {
            match event_loop.poll().await {
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    tracing::info!(topic = %subscription_topic, "mqtt connected");
                    self.client
                        .subscribe(subscription_topic.clone(), QoS::AtLeastOnce)
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

    pub fn publish_topic(&self) -> &str {
        match self.role {
            RelayRole::User => &self.topics.inbound,
            RelayRole::Work => &self.topics.outbound,
        }
    }

    pub fn subscribe_topic(&self) -> &str {
        match self.role {
            RelayRole::User => &self.topics.outbound,
            RelayRole::Work => &self.topics.inbound,
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

impl RelayRole {
    fn as_str(self) -> &'static str {
        match self {
            RelayRole::User => "user",
            RelayRole::Work => "work",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_broker, RelayTopics};

    #[test]
    fn parses_broker_without_scheme() {
        let (host, port) = parse_broker("localhost").expect("broker parsing");
        assert_eq!(host, "localhost");
        assert_eq!(port, 1883);
    }

    #[test]
    fn derives_topics() {
        let topics = RelayTopics::for_node("test-node");
        assert_eq!(topics.inbound, "acp/test-node/in");
        assert_eq!(topics.outbound, "acp/test-node/out");
    }
}
