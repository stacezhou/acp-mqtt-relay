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
                        tracing::debug!(topic = %topic, "forwarding MQTT payload to stdout");
                        write_incoming_payload(&payload).await?;
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

    loop {
        let bytes_read = stdin
            .read(&mut buffer)
            .await
            .context("failed to read stdin")?;

        if bytes_read == 0 {
            tracing::info!("stdin reached EOF");
            break;
        }

        tracing::info!(bytes_read, "captured stdin bytes");
        let payload = AcpMessage::new(StreamType::Stdin, &buffer[..bytes_read]).to_json_line()?;
        relay.publish(relay.publish_topic(), payload).await?;
    }

    Ok(())
}

async fn write_incoming_payload(payload: &[u8]) -> Result<()> {
    let message = AcpMessage::from_slice(payload)?;
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
