use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant, MissedTickBehavior};

use crate::cli::WorkConfig;
use crate::message::{AcpMessage, ControlAction, ControlMessage, StreamType};
use crate::mqtt_client::{MqttIncomingMessage, MqttRelayClient, RelayTopicLayout};

const SESSION_TTL: Duration = Duration::from_secs(48 * 60 * 60);
const SESSION_CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

pub async fn run(config: WorkConfig) -> Result<()> {
    let topic_layout = RelayTopicLayout::for_node(&config.common.node_id);
    let control_topic = topic_layout.control_topic();
    let mqtt_client_id = format!(
        "amr-work-{}-{:x}",
        sanitize_for_mqtt_client_id(&config.common.node_id),
        std::process::id()
    );

    let (relay, event_loop) = MqttRelayClient::new(
        config.common.broker.as_deref().unwrap(),
        &mqtt_client_id,
        config.common.username.as_deref(),
        config.common.password.as_deref(),
        vec![
            control_topic.clone(),
            topic_layout.session_in_wildcard_topic(),
        ],
    )?;
    relay.subscribe_all().await?;

    let (mqtt_tx, mut mqtt_rx) = mpsc::channel(128);
    let mqtt_task = tokio::spawn({
        let relay = relay.clone();
        async move { relay.start_event_loop(event_loop, mqtt_tx).await }
    });

    let (session_event_tx, mut session_event_rx) = mpsc::channel(128);
    let mut sessions: HashMap<String, SessionHandle> = HashMap::new();

    let mut cleanup_interval = tokio::time::interval(SESSION_CLEANUP_INTERVAL);
    cleanup_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut mqtt_task = mqtt_task;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        tokio::select! {
            maybe_message = mqtt_rx.recv() => {
                match maybe_message {
                    Some(MqttIncomingMessage::Publish { topic, payload }) => {
                        handle_mqtt_message(
                            &topic_layout,
                            &relay,
                            &config.command,
                            &session_event_tx,
                            &mut sessions,
                            &topic,
                            &payload,
                        )
                        .await?;
                    }
                    None => break,
                }
            }
            maybe_event = session_event_rx.recv() => {
                if let Some(event) = maybe_event {
                    if let Some(error) = event.error {
                        tracing::warn!(client_id = %event.client_id, error = %error, "session exited with error");
                    } else {
                        tracing::info!(client_id = %event.client_id, "session exited");
                    }
                    sessions.remove(&event.client_id);
                }
            }
            _ = cleanup_interval.tick() => {
                expire_sessions(&mut sessions, Instant::now(), SESSION_TTL).await?;
            }
            result = &mut mqtt_task => {
                result.context("mqtt task join failed")??;
                break;
            }
            signal = &mut ctrl_c => {
                signal.context("failed to listen for ctrl-c")?;
                tracing::info!("received ctrl-c, shutting down work mode");
                break;
            }
        }
    }

    shutdown_all_sessions(&mut sessions).await?;
    Ok(())
}

async fn handle_mqtt_message(
    topic_layout: &RelayTopicLayout,
    relay: &MqttRelayClient,
    command: &str,
    session_event_tx: &mpsc::Sender<SessionExitEvent>,
    sessions: &mut HashMap<String, SessionHandle>,
    topic: &str,
    payload: &[u8],
) -> Result<()> {
    if topic == topic_layout.control_topic() {
        let message = ControlMessage::from_slice(payload)?;
        handle_control_message(
            topic_layout,
            relay,
            command,
            session_event_tx,
            sessions,
            message,
        )
        .await?;
        return Ok(());
    }

    let Some(client_id) = topic_layout.parse_session_in_topic(topic) else {
        tracing::warn!(topic = %topic, "ignoring publish on unexpected topic");
        return Ok(());
    };

    let message = AcpMessage::from_slice(payload)?;
    let Some(session) = sessions.get_mut(&client_id) else {
        tracing::warn!(client_id = %client_id, seq = message.seq, "ignoring stdin for unknown session");
        return Ok(());
    };

    session.touch();
    let ready_messages = session.drain_ready_messages(message)?;
    for bytes in ready_messages {
        session
            .command_tx
            .send(SessionCommand::Write(bytes))
            .await
            .with_context(|| format!("session {client_id} stdin channel closed"))?;
    }

    Ok(())
}

async fn handle_control_message(
    topic_layout: &RelayTopicLayout,
    relay: &MqttRelayClient,
    command: &str,
    session_event_tx: &mpsc::Sender<SessionExitEvent>,
    sessions: &mut HashMap<String, SessionHandle>,
    message: ControlMessage,
) -> Result<()> {
    match message.action {
        ControlAction::Spawn => {
            ensure_session(
                topic_layout,
                relay,
                command,
                session_event_tx,
                sessions,
                &message.client_id,
            )
            .await?;
        }
        ControlAction::Heartbeat => {
            if let Some(session) = sessions.get_mut(&message.client_id) {
                session.touch();
            } else {
                ensure_session(
                    topic_layout,
                    relay,
                    command,
                    session_event_tx,
                    sessions,
                    &message.client_id,
                )
                .await?;
            }
        }
        ControlAction::Shutdown => {
            shutdown_session(sessions, &message.client_id).await?;
        }
    }

    Ok(())
}

async fn ensure_session(
    topic_layout: &RelayTopicLayout,
    relay: &MqttRelayClient,
    command: &str,
    session_event_tx: &mpsc::Sender<SessionExitEvent>,
    sessions: &mut HashMap<String, SessionHandle>,
    client_id: &str,
) -> Result<()> {
    if let Some(session) = sessions.get_mut(client_id) {
        session.touch();
        return Ok(());
    }

    let (command_tx, command_rx) = mpsc::channel(64);
    let out_topic = topic_layout.session_out_topic(client_id);
    let session_client_id = client_id.to_string();
    let session_command = command.to_string();
    let relay = relay.clone();
    let session_event_tx = session_event_tx.clone();

    tokio::spawn(async move {
        let result = session_loop(
            session_client_id.clone(),
            session_command,
            relay,
            out_topic,
            command_rx,
        )
        .await;

        let event = SessionExitEvent {
            client_id: session_client_id,
            error: result.err().map(|error| error.to_string()),
        };

        if session_event_tx.send(event).await.is_err() {
            tracing::debug!("work session exit receiver dropped");
        }
    });

    sessions.insert(client_id.to_string(), SessionHandle::new(command_tx));
    tracing::info!(client_id = %client_id, "spawned work session");
    Ok(())
}

async fn shutdown_session(
    sessions: &mut HashMap<String, SessionHandle>,
    client_id: &str,
) -> Result<()> {
    let Some(session) = sessions.remove(client_id) else {
        tracing::debug!(client_id = %client_id, "shutdown requested for unknown session");
        return Ok(());
    };

    session
        .command_tx
        .send(SessionCommand::Shutdown)
        .await
        .with_context(|| format!("failed to signal shutdown for session {client_id}"))?;

    Ok(())
}

async fn expire_sessions(
    sessions: &mut HashMap<String, SessionHandle>,
    now: Instant,
    ttl: Duration,
) -> Result<()> {
    let expired_client_ids: Vec<String> = sessions
        .iter()
        .filter_map(|(client_id, session)| {
            if now.duration_since(session.last_seen) >= ttl {
                Some(client_id.clone())
            } else {
                None
            }
        })
        .collect();

    for client_id in expired_client_ids {
        tracing::info!(client_id = %client_id, "expiring stale session");
        shutdown_session(sessions, &client_id).await?;
    }

    Ok(())
}

async fn shutdown_all_sessions(sessions: &mut HashMap<String, SessionHandle>) -> Result<()> {
    let client_ids: Vec<String> = sessions.keys().cloned().collect();
    for client_id in client_ids {
        shutdown_session(sessions, &client_id).await?;
    }
    Ok(())
}

async fn session_loop(
    client_id: String,
    command: String,
    relay: MqttRelayClient,
    out_topic: String,
    mut command_rx: mpsc::Receiver<SessionCommand>,
) -> Result<()> {
    let mut child = spawn_command(&command)?;
    let mut child_stdin = child.stdin.take().context("child stdin unavailable")?;
    let child_stdout = child.stdout.take().context("child stdout unavailable")?;
    let child_stderr = child.stderr.take().context("child stderr unavailable")?;

    let out_seq = Arc::new(AtomicU64::new(1));
    let stdout_task = tokio::spawn(stream_child_output(
        relay.clone(),
        out_topic.clone(),
        child_stdout,
        StreamType::Stdout,
        out_seq.clone(),
    ));
    let stderr_task = tokio::spawn(stream_child_output(
        relay.clone(),
        out_topic.clone(),
        child_stderr,
        StreamType::Stderr,
        out_seq,
    ));

    let child_result: Result<()> = loop {
        tokio::select! {
            maybe_command = command_rx.recv() => {
                match maybe_command {
                    Some(SessionCommand::Write(bytes)) => {
                        write_bytes_to_child_stdin(&mut child_stdin, &bytes).await?;
                    }
                    Some(SessionCommand::Shutdown) | None => {
                        tracing::info!(client_id = %client_id, "shutting down child session");
                        terminate_child(&mut child).await?;
                        break Ok(());
                    }
                }
            }
            result = child.wait() => {
                let status = result.context("failed to wait for child")?;
                if status.success() {
                    tracing::info!(client_id = %client_id, ?status, "child process exited");
                    break Ok(());
                }

                anyhow::bail!("child process exited unexpectedly with status {status}");
            }
        }
    };

    drop(child_stdin);

    stdout_task
        .await
        .context("stdout forwarder join failed")??;
    stderr_task
        .await
        .context("stderr forwarder join failed")??;

    child_result
}

fn spawn_command(command: &str) -> Result<Child> {
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

async fn terminate_child(child: &mut Child) -> Result<()> {
    if child
        .try_wait()
        .context("failed to query child status")?
        .is_some()
    {
        return Ok(());
    }

    child.kill().await.context("failed to kill child process")?;
    Ok(())
}

async fn write_bytes_to_child_stdin(stdin: &mut ChildStdin, bytes: &[u8]) -> Result<()> {
    tracing::info!(bytes = bytes.len(), "writing bytes to child stdin");
    stdin
        .write_all(bytes)
        .await
        .context("failed to write to child stdin")?;
    stdin.flush().await.context("failed to flush child stdin")?;
    Ok(())
}

async fn stream_child_output<R>(
    relay: MqttRelayClient,
    out_topic: String,
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
        relay.publish(&out_topic, payload).await?;
    }
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

struct SessionHandle {
    command_tx: mpsc::Sender<SessionCommand>,
    next_expected_seq: u64,
    message_buffer: BTreeMap<u64, AcpMessage>,
    last_seen: Instant,
}

impl SessionHandle {
    fn new(command_tx: mpsc::Sender<SessionCommand>) -> Self {
        Self {
            command_tx,
            next_expected_seq: 1,
            message_buffer: BTreeMap::new(),
            last_seen: Instant::now(),
        }
    }

    fn touch(&mut self) {
        self.last_seen = Instant::now();
    }

    fn drain_ready_messages(&mut self, message: AcpMessage) -> Result<Vec<Vec<u8>>> {
        if message.stream != StreamType::Stdin {
            tracing::debug!(stream = ?message.stream, "ignoring non-stdin payload for child");
            return Ok(Vec::new());
        }

        if message.seq < self.next_expected_seq {
            tracing::warn!(seq = message.seq, "ignoring duplicate stdin message");
            return Ok(Vec::new());
        }

        self.message_buffer.insert(message.seq, message);

        let mut ready_messages = Vec::new();
        while let Some(message) = self.message_buffer.remove(&self.next_expected_seq) {
            ready_messages.push(message.decode_bytes()?);
            self.next_expected_seq += 1;
        }

        Ok(ready_messages)
    }
}

enum SessionCommand {
    Write(Vec<u8>),
    Shutdown,
}

struct SessionExitEvent {
    client_id: String,
    error: Option<String>,
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use tokio::io::AsyncReadExt;
    use tokio::sync::mpsc;
    use tokio::time::{Duration, Instant};

    use crate::message::{AcpMessage, StreamType};

    use super::{sanitize_for_mqtt_client_id, spawn_command, SessionHandle};

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

    #[test]
    fn session_buffer_reorders_messages() {
        let (tx, _rx) = mpsc::channel(1);
        let mut session = SessionHandle::new(tx);

        let first = AcpMessage::new(2, StreamType::Stdin, b"world");
        let second = AcpMessage::new(1, StreamType::Stdin, b"hello ");

        assert!(session.drain_ready_messages(first).unwrap().is_empty());
        let ready = session.drain_ready_messages(second).unwrap();
        assert_eq!(ready, vec![b"hello ".to_vec(), b"world".to_vec()]);
    }

    #[test]
    fn mqtt_client_id_is_sanitized() {
        assert_eq!(sanitize_for_mqtt_client_id("node/id 1"), "node-id-1");
    }

    #[test]
    fn session_ttl_check_uses_last_seen() {
        let (tx, _rx) = mpsc::channel(1);
        let mut session = SessionHandle::new(tx);
        session.last_seen = Instant::now() - Duration::from_secs(5);
        assert!(Instant::now().duration_since(session.last_seen) >= Duration::from_secs(5));
    }
}
