use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::mpsc;

use crate::cli::WorkConfig;
use crate::message::{AcpMessage, StreamType};
use crate::mqtt_client::{MqttIncomingMessage, MqttRelayClient, RelayRole};

pub async fn run(config: WorkConfig) -> Result<()> {
    let (relay, event_loop) = MqttRelayClient::new(
        config.common.broker.as_deref().unwrap(),
        &config.common.node_id,
        RelayRole::Work,
        config.common.username.as_deref(),
        config.common.password.as_deref(),
    )?;
    relay.subscribe().await?;

    let mut child = spawn_command(&config.command)?;
    let mut child_stdin = child.stdin.take().context("child stdin unavailable")?;
    let child_stdout = child.stdout.take().context("child stdout unavailable")?;
    let child_stderr = child.stderr.take().context("child stderr unavailable")?;

    let (mqtt_tx, mut mqtt_rx) = mpsc::channel(64);
    let mqtt_task = tokio::spawn({
        let relay = relay.clone();
        async move { relay.start_event_loop(event_loop, mqtt_tx).await }
    });

    let stdin_task = tokio::spawn(async move {
        let mut next_expected_seq = 1_u64;
        let mut message_buffer: BTreeMap<u64, AcpMessage> = BTreeMap::new();

        while let Some(message) = mqtt_rx.recv().await {
            match message {
                MqttIncomingMessage::Publish { topic, payload } => {
                    tracing::debug!(topic = %topic, "received MQTT payload for child stdin");
                    let msg = AcpMessage::from_slice(&payload)?;

                    if msg.seq < next_expected_seq {
                        tracing::warn!(seq = msg.seq, "ignoring duplicate stdin message");
                        continue;
                    }

                    message_buffer.insert(msg.seq, msg);

                    while let Some(ordered_msg) = message_buffer.remove(&next_expected_seq) {
                        write_message_to_child_stdin(&mut child_stdin, &ordered_msg).await?;
                        next_expected_seq += 1;
                    }
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    });

    let out_seq = Arc::new(AtomicU64::new(1));

    let stdout_task = tokio::spawn(stream_child_output(
        relay.clone(),
        child_stdout,
        StreamType::Stdout,
        out_seq.clone(),
    ));
    let stderr_task = tokio::spawn(stream_child_output(
        relay.clone(),
        child_stderr,
        StreamType::Stderr,
        out_seq.clone(),
    ));
    let monitor_task = tokio::spawn(async move {
        let status = child.wait().await.context("failed to wait for child")?;
        if status.success() {
            tracing::info!(?status, "child process exited");
            Ok(())
        } else {
            anyhow::bail!("child process exited unexpectedly with status {status}");
        }
    });

    tokio::select! {
        result = stdin_task => result.context("work stdin task join failed")??,
        result = stdout_task => result.context("work stdout task join failed")??,
        result = stderr_task => result.context("work stderr task join failed")??,
        result = monitor_task => result.context("child monitor join failed")??,
        result = mqtt_task => result.context("mqtt task join failed")??,
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for ctrl-c")?;
            tracing::info!("received ctrl-c, shutting down work mode");
        }
    }

    Ok(())
}

fn spawn_command(command: &str) -> Result<tokio::process::Child> {
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut command_builder = Command::new("cmd");
        command_builder.arg("/C").arg(command);
        command_builder
    };

    #[cfg(not(target_os = "windows"))]
    let mut cmd = {
        let mut command_builder = Command::new("sh");
        command_builder.arg("-lc").arg(command);
        command_builder
    };

    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    cmd.spawn()
        .with_context(|| format!("failed to spawn child command: {command}"))
}

async fn write_message_to_child_stdin(stdin: &mut ChildStdin, message: &AcpMessage) -> Result<()> {
    let bytes = message.decode_bytes()?;

    if message.stream != StreamType::Stdin {
        tracing::debug!(stream = ?message.stream, "ignoring non-stdin payload for child");
        return Ok(());
    }

    tracing::info!(
        seq = message.seq,
        bytes = bytes.len(),
        "writing bytes to child stdin"
    );
    stdin
        .write_all(&bytes)
        .await
        .context("failed to write to child stdin")?;
    stdin.flush().await.context("failed to flush child stdin")?;
    Ok(())
}

async fn stream_child_output<R>(
    relay: MqttRelayClient,
    mut reader: R,
    stream: StreamType,
    seq_counter: Arc<AtomicU64>,
) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0_u8; 4096];

    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .await
            .context("failed to read child stream")?;

        if bytes_read == 0 {
            tracing::info!(?stream, "child stream closed");
            return Ok(());
        }

        let seq = seq_counter.fetch_add(1, Ordering::SeqCst);
        tracing::info!(?stream, bytes_read, seq, "captured child output");
        let payload = AcpMessage::new(seq, stream, &buffer[..bytes_read]).to_json_line()?;
        relay.publish(relay.publish_topic(), payload).await?;
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use tokio::io::AsyncReadExt;

    use super::spawn_command;

    #[tokio::test]
    async fn command_emits_stdout() -> Result<()> {
        let mut child = spawn_command("printf 'hello world\\n'")?;
        let mut stdout = child.stdout.take().expect("stdout");

        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).await?;
        let status = child.wait().await?;
        assert!(status.success());
        assert_eq!(buf, b"hello world\n");
        Ok(())
    }

    #[tokio::test]
    async fn command_emits_stderr() -> Result<()> {
        let mut child = spawn_command("printf 'error\\n' >&2")?;
        let mut stderr = child.stderr.take().expect("stderr");

        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).await?;
        let status = child.wait().await?;
        assert!(status.success());
        assert_eq!(buf, b"error\n");
        Ok(())
    }
}
