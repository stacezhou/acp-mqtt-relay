use std::collections::BTreeMap;
use anyhow::{Context, Result};
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration, Instant};

use crate::cli::UserConfig;
use crate::message::{AcpMessage, StreamType};
use crate::mqtt_client::{MqttIncomingMessage, MqttRelayClient, RelayRole};

pub async fn run(config: UserConfig) -> Result<()> {
    let (relay, event_loop) = MqttRelayClient::new(
        &config.common.broker,
        &config.common.node_id,
        RelayRole::User,
        config.common.username.as_deref(),
        config.common.password.as_deref(),
    )?;

    relay.subscribe().await?;

    let (mqtt_tx, mut mqtt_rx) = mpsc::channel(64);
    let event_loop_task = tokio::spawn({
        let relay = relay.clone();
        async move { relay.start_event_loop(event_loop, mqtt_tx).await }
    });

    let stdin_task = tokio::spawn({
        let relay = relay.clone();
        async move { forward_stdin_to_mqtt(&relay).await }
    });

    let mut stdin_task = stdin_task;
    let mut event_loop_task = event_loop_task;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    let mut stdin_closed = false;
    let mut idle_since: Option<Instant> = None;

    let mut next_expected_seq = 1_u64;
    let mut message_buffer: BTreeMap<u64, AcpMessage> = BTreeMap::new();

    loop {
        let deadline = idle_since.map(|started| {
            let elapsed = started.elapsed();
            let wait = Duration::from_secs(1).saturating_sub(elapsed);
            sleep(wait)
        });

        tokio::select! {
            result = &mut stdin_task, if !stdin_closed => {
                result.context("stdin task join failed")??;
                stdin_closed = true;
                idle_since = Some(Instant::now());
            }
            maybe_message = mqtt_rx.recv() => {
                match maybe_message {
                    Some(MqttIncomingMessage::Publish { topic, payload }) => {
                        tracing::debug!(topic = %topic, "received MQTT payload");
                        let message = AcpMessage::from_slice(&payload)?;
                        
                        if message.seq < next_expected_seq {
                            tracing::warn!(seq = message.seq, "ignoring duplicate message");
                            continue;
                        }

                        message_buffer.insert(message.seq, message);

                        // Process all contiguous messages starting from next_expected_seq
                        while let Some(msg) = message_buffer.remove(&next_expected_seq) {
                            tracing::debug!(seq = msg.seq, "processing message in order");
                            write_message_to_output(&msg).await?;
                            next_expected_seq += 1;
                        }

                        if stdin_closed {
                            idle_since = Some(Instant::now());
                        }
                    }
                    None => return Ok(()),
                }
            }
            result = &mut event_loop_task => {
                result.context("mqtt task join failed")??;
                return Ok(());
            }
            _ = async {
                if let Some(deadline) = deadline {
                    deadline.await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if stdin_closed => {
                tracing::info!("stdin closed and no MQTT traffic observed during grace period");
                return Ok(());
            }
            signal = &mut ctrl_c => {
                signal.context("failed to listen for ctrl-c")?;
                tracing::info!("received ctrl-c, shutting down user mode");
                return Ok(());
            }
        }
    }
}

async fn forward_stdin_to_mqtt(relay: &MqttRelayClient) -> Result<()> {
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
        let payload = AcpMessage::new(seq, StreamType::Stdin, &buffer[..bytes_read]).to_json_line()?;
        relay.publish(relay.publish_topic(), payload).await?;
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
