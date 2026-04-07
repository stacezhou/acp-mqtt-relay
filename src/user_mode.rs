use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration, Instant, MissedTickBehavior};

use crate::cli::UserConfig;
use crate::message::{AcpMessage, ControlAction, ControlMessage, StreamType};
use crate::mqtt_client::{MqttIncomingMessage, MqttRelayClient, RelayTopicLayout};

const USER_IDLE_GRACE_PERIOD: Duration = Duration::from_secs(1);
const USER_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

pub async fn run(config: UserConfig) -> Result<()> {
    let client_id = generate_client_id();
    let topic_layout = RelayTopicLayout::for_node(&config.common.node_id);
    let control_topic = topic_layout.control_topic();
    let session_in_topic = topic_layout.session_in_topic(&client_id);
    let session_out_topic = topic_layout.session_out_topic(&client_id);
    let mqtt_client_id = format!(
        "amr-user-{}-{client_id}",
        sanitize_for_mqtt_client_id(&config.common.node_id)
    );

    let (relay, event_loop) = MqttRelayClient::new(
        config.common.broker.as_deref().unwrap(),
        &mqtt_client_id,
        config.common.username.as_deref(),
        config.common.password.as_deref(),
        vec![session_out_topic.clone()],
    )?;

    relay.subscribe_all().await?;

    let (mqtt_tx, mut mqtt_rx) = mpsc::channel(64);
    let event_loop_task = tokio::spawn({
        let relay = relay.clone();
        async move { relay.start_event_loop(event_loop, mqtt_tx).await }
    });

    send_control_message(&relay, &control_topic, &client_id, ControlAction::Spawn).await?;

    let heartbeat_task = tokio::spawn({
        let relay = relay.clone();
        let control_topic = control_topic.clone();
        let client_id = client_id.clone();
        async move { heartbeat_loop(relay, control_topic, client_id).await }
    });

    let stdin_task = tokio::spawn({
        let relay = relay.clone();
        let session_in_topic = session_in_topic.clone();
        async move { forward_stdin_to_mqtt(&relay, &session_in_topic).await }
    });

    let mut stdin_task = stdin_task;
    let mut heartbeat_task = heartbeat_task;
    let mut event_loop_task = event_loop_task;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    let mut stdin_closed = false;
    let mut idle_since: Option<Instant> = None;
    let mut next_expected_seq = 1_u64;
    let mut message_buffer: BTreeMap<u64, AcpMessage> = BTreeMap::new();

    let exit_result: Result<()> = loop {
        let deadline = idle_since.map(|started| {
            let elapsed = started.elapsed();
            let wait = USER_IDLE_GRACE_PERIOD.saturating_sub(elapsed);
            sleep(wait)
        });

        tokio::select! {
            result = &mut stdin_task, if !stdin_closed => {
                result.context("stdin task join failed")??;
                stdin_closed = true;
                idle_since = Some(Instant::now());
            }
            result = &mut heartbeat_task => {
                result.context("heartbeat task join failed")??;
                break Ok(());
            }
            maybe_message = mqtt_rx.recv() => {
                match maybe_message {
                    Some(MqttIncomingMessage::Publish { topic, payload }) => {
                        tracing::debug!(topic = %topic, client_id = %client_id, "received MQTT payload");
                        let message = AcpMessage::from_slice(&payload)?;

                        if message.seq < next_expected_seq {
                            tracing::warn!(seq = message.seq, client_id = %client_id, "ignoring duplicate message");
                            continue;
                        }

                        message_buffer.insert(message.seq, message);

                        while let Some(msg) = message_buffer.remove(&next_expected_seq) {
                            tracing::debug!(seq = msg.seq, client_id = %client_id, "processing session message in order");
                            write_message_to_output(&msg).await?;
                            next_expected_seq += 1;
                        }

                        if stdin_closed {
                            idle_since = Some(Instant::now());
                        }
                    }
                    None => break Ok(()),
                }
            }
            result = &mut event_loop_task => {
                result.context("mqtt task join failed")??;
                break Ok(());
            }
            _ = async {
                if let Some(deadline) = deadline {
                    deadline.await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if stdin_closed => {
                tracing::info!(client_id = %client_id, "stdin closed and no MQTT traffic observed during grace period");
                break Ok(());
            }
            signal = &mut ctrl_c => {
                signal.context("failed to listen for ctrl-c")?;
                tracing::info!(client_id = %client_id, "received ctrl-c, shutting down user mode");
                break Ok(());
            }
        }
    };

    let shutdown_result =
        send_control_message(&relay, &control_topic, &client_id, ControlAction::Shutdown).await;
    heartbeat_task.abort();

    if let Err(error) = shutdown_result {
        tracing::warn!(client_id = %client_id, error = %error, "failed to send shutdown control message");
    }

    exit_result
}

async fn forward_stdin_to_mqtt(relay: &MqttRelayClient, session_in_topic: &str) -> Result<()> {
    let mut stdin = io::stdin();
    let mut buffer = [0_u8; 4096];
    let mut seq = 1_u64;

    loop {
        let bytes_read = stdin
            .read(&mut buffer)
            .await
            .context("failed to read stdin")?;

        if bytes_read == 0 {
            tracing::info!("stdin reached EOF");
            break;
        }

        tracing::info!(bytes_read, seq, "captured stdin bytes");
        let payload =
            AcpMessage::new(seq, StreamType::Stdin, &buffer[..bytes_read]).to_json_line()?;
        relay.publish(session_in_topic, payload).await?;
        seq += 1;
    }

    Ok(())
}

async fn write_message_to_output(message: &AcpMessage) -> Result<()> {
    let bytes = message.decode_bytes()?;

    match message.stream {
        StreamType::Stdout | StreamType::Stdin => {
            let mut stdout = io::stdout();
            stdout
                .write_all(&bytes)
                .await
                .context("failed to write to stdout")?;
            stdout.flush().await.context("failed to flush stdout")?;
        }
        StreamType::Stderr => {
            let mut stderr = io::stderr();
            stderr
                .write_all(&bytes)
                .await
                .context("failed to write to stderr")?;
            stderr.flush().await.context("failed to flush stderr")?;
        }
    }

    Ok(())
}

async fn heartbeat_loop(
    relay: MqttRelayClient,
    control_topic: String,
    client_id: String,
) -> Result<()> {
    let mut interval = tokio::time::interval(USER_HEARTBEAT_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    interval.tick().await;

    loop {
        interval.tick().await;
        send_control_message(&relay, &control_topic, &client_id, ControlAction::Heartbeat).await?;
    }
}

async fn send_control_message(
    relay: &MqttRelayClient,
    control_topic: &str,
    client_id: &str,
    action: ControlAction,
) -> Result<()> {
    let payload = ControlMessage::new(client_id, action).to_vec()?;
    relay.publish(control_topic, payload).await
}

fn generate_client_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let pid = std::process::id();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);

    format!("{now_ms:x}-{pid:x}-{counter:x}")
}

fn sanitize_for_mqtt_client_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{generate_client_id, sanitize_for_mqtt_client_id};

    #[test]
    fn generated_client_ids_are_unique() {
        let first = generate_client_id();
        let second = generate_client_id();
        assert_ne!(first, second);
    }

    #[test]
    fn mqtt_client_id_is_sanitized() {
        assert_eq!(sanitize_for_mqtt_client_id("node/id 1"), "node-id-1");
    }
}
