use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rtrb::Producer;
use sendspin::protocol::client::{AudioChunk, WsSender};
use sendspin::protocol::messages::{
    AudioFormatSpec, ClientCommand, ClientGoodbye, ClientHello, ClientState, ControllerCommand,
    DeviceInfo, GoodbyeReason, Message, PlayerFormatRequest, PlayerState, PlayerV1Support,
    StreamRequestFormat,
};
use sendspin::ProtocolClient;
use tokio::runtime::Builder as TokioRuntimeBuilder;
use tokio::sync::mpsc::{error::TryRecvError, UnboundedReceiver};

use crate::config::{default_client_name, normalize_client_name, normalize_server_url};
use crate::constants::{
    BUFFER_CAPACITY_BYTES, EFFECTIVE_PLUGIN_VERSION, PREFERRED_PCM_BIT_DEPTH, PRODUCT_NAME,
};
use crate::state::{
    ActiveStream, ConnectionState, GroupPlaybackState, SharedState, SyncState, TimestampedChunk,
    WorkerCommand,
};

pub(crate) fn network_thread_main(
    shared: Arc<SharedState>,
    mut chunk_producer: Producer<TimestampedChunk>,
    mut command_rx: UnboundedReceiver<WorkerCommand>,
    server_url: String,
    client_name: String,
    client_id: String,
) {
    let runtime = TokioRuntimeBuilder::new_current_thread()
        .enable_all()
        .build();

    let Ok(runtime) = runtime else {
        shared.set_connection_state(ConnectionState::Error);
        return;
    };

    runtime.block_on(async move {
        let mut reconnect_delay = Duration::from_secs(1);
        let mut active_server_url = normalize_server_url(&server_url);
        let mut active_client_name =
            normalize_client_name(&client_name).unwrap_or_else(default_client_name);
        let mut pending_controller_command: Option<String> = None;
        shared.set_configured_server_url(active_server_url.clone().unwrap_or_default());
        shared.set_configured_client_name(active_client_name.clone());

        'outer: loop {
            loop {
                match command_rx.try_recv() {
                    Ok(WorkerCommand::Shutdown) => break 'outer,
                    Ok(WorkerCommand::SetServerUrl(new_url)) => {
                        if let Some(normalized) = normalize_server_url(&new_url) {
                            active_server_url = Some(normalized.clone());
                            shared.set_configured_server_url(normalized);
                            reconnect_delay = Duration::from_millis(100);
                        }
                    }
                    Ok(WorkerCommand::SetClientName(new_client_name)) => {
                        if let Some(normalized) = normalize_client_name(&new_client_name) {
                            active_client_name = normalized.clone();
                            shared.set_configured_client_name(normalized);
                            reconnect_delay = Duration::from_millis(100);
                        }
                    }
                    Ok(WorkerCommand::SendControllerCommand(command)) => {
                        pending_controller_command = Some(command);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break 'outer,
                }
            }

            if active_server_url.is_none() {
                shared.set_connection_state(ConnectionState::Disconnected);
                shared.set_now_playing(None, None);
                match wait_reconnect_or_command(&mut command_rx, Duration::from_secs(30)).await {
                    ReconnectAction::Stop => break,
                    ReconnectAction::Continue => continue,
                    ReconnectAction::UpdateServerUrl(new_url) => {
                        active_server_url = Some(new_url.clone());
                        shared.set_configured_server_url(new_url);
                        reconnect_delay = Duration::from_millis(100);
                        continue;
                    }
                    ReconnectAction::UpdateClientName(new_client_name) => {
                        active_client_name = new_client_name.clone();
                        shared.set_configured_client_name(new_client_name);
                        reconnect_delay = Duration::from_millis(100);
                        continue;
                    }
                    ReconnectAction::QueueControllerCommand(command) => {
                        pending_controller_command = Some(command);
                        continue;
                    }
                }
            }

            shared.set_connection_state(ConnectionState::Connecting);
            shared.set_group_playback_state(GroupPlaybackState::Unknown);
            shared.set_now_playing(None, None);

            let host_sample_rate_hz = shared.host_sample_rate_hz.load(Ordering::Relaxed).max(44_100);
            let player_support = build_player_support(host_sample_rate_hz);
            let hello = build_client_hello(&client_id, &active_client_name, player_support);

            let Some(connect_url) = active_server_url.as_ref() else {
                continue;
            };
            let client = match ProtocolClient::connect(connect_url, hello).await {
                Ok(client) => client,
                Err(_) => {
                    shared.set_connection_state(ConnectionState::Error);
                    match wait_reconnect_or_command(&mut command_rx, reconnect_delay).await {
                        ReconnectAction::Stop => break,
                        ReconnectAction::Continue => {
                            reconnect_delay = (reconnect_delay * 2).min(Duration::from_secs(30));
                            continue;
                        }
                        ReconnectAction::UpdateServerUrl(new_url) => {
                            active_server_url = Some(new_url.clone());
                            shared.set_configured_server_url(new_url);
                            reconnect_delay = Duration::from_millis(100);
                            continue;
                        }
                        ReconnectAction::UpdateClientName(new_client_name) => {
                            active_client_name = new_client_name.clone();
                            shared.set_configured_client_name(new_client_name);
                            reconnect_delay = Duration::from_millis(100);
                            continue;
                        }
                        ReconnectAction::QueueControllerCommand(command) => {
                            pending_controller_command = Some(command);
                            reconnect_delay = Duration::from_millis(100);
                            continue;
                        }
                    }
                }
            };

            reconnect_delay = Duration::from_secs(1);
            shared.set_connection_state(ConnectionState::Connected);
            shared.mark_state_dirty();

            let (mut message_rx, mut audio_rx, clock_sync, ws_tx, _guard) = client.split();

            let mut active_stream: Option<ActiveStream> = None;
            let mut state_tick = tokio::time::interval(Duration::from_millis(200));
            let mut last_requested_sample_rate = shared.host_sample_rate_hz.load(Ordering::Relaxed);

            let _ = send_client_state(&ws_tx, &shared).await;
            if let Some(command) = pending_controller_command.take() {
                let _ = send_controller_command(&ws_tx, &command).await;
            }

            let mut reconnect = true;
            let mut reconnect_immediately = false;

            loop {
                tokio::select! {
                    maybe_cmd = command_rx.recv() => {
                        match maybe_cmd {
                            Some(WorkerCommand::Shutdown) => {
                                let _ = send_goodbye(&ws_tx, GoodbyeReason::Restart).await;
                                reconnect = false;
                                break;
                            }
                            Some(WorkerCommand::SetServerUrl(new_url)) => {
                                if let Some(normalized) = normalize_server_url(&new_url) {
                                    if active_server_url.as_ref() != Some(&normalized) {
                                        let _ = send_goodbye(&ws_tx, GoodbyeReason::Restart).await;
                                        active_server_url = Some(normalized.clone());
                                        shared.set_configured_server_url(normalized);
                                        reconnect_immediately = true;
                                        break;
                                    }
                                }
                            }
                            Some(WorkerCommand::SetClientName(new_client_name)) => {
                                if let Some(normalized) = normalize_client_name(&new_client_name) {
                                    if normalized != active_client_name {
                                        let _ = send_goodbye(&ws_tx, GoodbyeReason::Restart).await;
                                        active_client_name = normalized.clone();
                                        shared.set_configured_client_name(normalized);
                                        reconnect_immediately = true;
                                        break;
                                    }
                                }
                            }
                            Some(WorkerCommand::SendControllerCommand(command)) => {
                                let _ = send_controller_command(&ws_tx, &command).await;
                            }
                            None => {
                                reconnect = false;
                                break;
                            }
                        }
                    }
                    maybe_message = message_rx.recv() => {
                        let Some(message) = maybe_message else {
                            break;
                        };

                        handle_message(message, &shared, &ws_tx, &mut active_stream).await;
                    }
                    maybe_chunk = audio_rx.recv() => {
                        let Some(chunk) = maybe_chunk else {
                            break;
                        };

                        if let Some(stream) = active_stream.as_ref() {
                            if let Some(decoded_chunk) = decode_audio_chunk(&chunk, stream, &clock_sync) {
                                if chunk_producer.push(decoded_chunk).is_err() {
                                    shared.record_queue_overflow();
                                    shared.set_sync_state(SyncState::Error);
                                }
                            } else {
                                shared.record_decode_error();
                            }
                        }
                    }
                    _ = state_tick.tick() => {
                        if shared.take_state_dirty() {
                            let _ = send_client_state(&ws_tx, &shared).await;
                        }

                        let current_host_rate = shared.host_sample_rate_hz.load(Ordering::Relaxed);
                        if current_host_rate > 0 && current_host_rate != last_requested_sample_rate {
                            let _ = request_pcm_format(&ws_tx, current_host_rate).await;
                            last_requested_sample_rate = current_host_rate;
                        }
                    }
                }
            }

            shared.set_connection_state(ConnectionState::Disconnected);
            shared.set_group_playback_state(GroupPlaybackState::Unknown);
            shared.set_now_playing(None, None);
            shared.set_stream_active(false);
            shared.request_clear();

            if !reconnect {
                break;
            }

            if reconnect_immediately {
                reconnect_delay = Duration::from_millis(100);
                continue;
            }

            match wait_reconnect_or_command(&mut command_rx, reconnect_delay).await {
                ReconnectAction::Stop => break,
                ReconnectAction::Continue => {
                    reconnect_delay = (reconnect_delay * 2).min(Duration::from_secs(30));
                }
                ReconnectAction::UpdateServerUrl(new_url) => {
                    active_server_url = Some(new_url.clone());
                    shared.set_configured_server_url(new_url);
                    reconnect_delay = Duration::from_millis(100);
                }
                ReconnectAction::UpdateClientName(new_client_name) => {
                    active_client_name = new_client_name.clone();
                    shared.set_configured_client_name(new_client_name);
                    reconnect_delay = Duration::from_millis(100);
                }
                ReconnectAction::QueueControllerCommand(command) => {
                    pending_controller_command = Some(command);
                    reconnect_delay = Duration::from_millis(100);
                }
            }
        }
    });
}

fn build_client_hello(
    client_id: &str,
    client_name: &str,
    player_support: PlayerV1Support,
) -> ClientHello {
    ClientHello {
        client_id: client_id.to_string(),
        name: client_name.to_string(),
        version: 1,
        supported_roles: vec![
            "player@v1".to_string(),
            "controller@v1".to_string(),
            "metadata@v1".to_string(),
        ],
        device_info: Some(DeviceInfo {
            product_name: Some(PRODUCT_NAME.to_string()),
            manufacturer: Some("Sendspin".to_string()),
            software_version: Some(EFFECTIVE_PLUGIN_VERSION.to_string()),
        }),
        player_v1_support: Some(player_support),
        artwork_v1_support: None,
        visualizer_v1_support: None,
    }
}

async fn handle_message(
    message: Message,
    shared: &Arc<SharedState>,
    ws_tx: &WsSender,
    active_stream: &mut Option<ActiveStream>,
) {
    match message {
        Message::StreamStart(stream_start) => {
            let Some(player) = stream_start.player else {
                return;
            };

            let host_sample_rate_hz = shared
                .host_sample_rate_hz
                .load(Ordering::Relaxed)
                .max(44_100);

            if player.codec != "pcm"
                || player.sample_rate != host_sample_rate_hz
                || !matches!(player.channels, 1 | 2)
                || !matches!(player.bit_depth, 16 | 24)
            {
                let _ = request_pcm_format(ws_tx, host_sample_rate_hz).await;
                *active_stream = None;
                shared.set_stream_active(false);
                shared.request_clear();
                return;
            }

            *active_stream = Some(ActiveStream {
                codec: player.codec,
                sample_rate_hz: player.sample_rate,
                channels: player.channels,
                bit_depth: player.bit_depth,
            });

            shared.set_stream_active(true);
            shared.set_group_playback_state(GroupPlaybackState::Playing);
            shared.request_clear();
            shared.set_sync_state(SyncState::Synchronized);
        }
        Message::StreamClear(_) => {
            shared.request_clear();
            shared.set_sync_state(SyncState::Synchronized);
        }
        Message::StreamEnd(_) => {
            *active_stream = None;
            shared.set_stream_active(false);
            shared.set_group_playback_state(GroupPlaybackState::Stopped);
            shared.request_clear();
            shared.set_sync_state(SyncState::Synchronized);
        }
        Message::ServerState(server_state) => {
            if let Some(metadata) = server_state.metadata {
                shared.set_now_playing(metadata.artist.as_deref(), metadata.title.as_deref());
            }
        }
        Message::GroupUpdate(group_update) => {
            if let Some(playback_state) = group_update.playback_state {
                let mapped_state = match playback_state {
                    sendspin::protocol::messages::PlaybackState::Playing => {
                        GroupPlaybackState::Playing
                    }
                    sendspin::protocol::messages::PlaybackState::Stopped => {
                        GroupPlaybackState::Stopped
                    }
                };
                shared.set_group_playback_state(mapped_state);
            }
        }
        Message::ServerCommand(server_command) => {
            if let Some(player_command) = server_command.player {
                match player_command.command.as_str() {
                    "volume" => {
                        if let Some(volume) = player_command.volume {
                            shared
                                .remote_volume
                                .store(volume.min(100), Ordering::Relaxed);
                            shared.mark_state_dirty();
                        }
                    }
                    "mute" => {
                        if let Some(muted) = player_command.mute {
                            shared.remote_muted.store(muted, Ordering::Relaxed);
                            shared.mark_state_dirty();
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn decode_audio_chunk(
    chunk: &AudioChunk,
    stream: &ActiveStream,
    clock_sync: &Arc<parking_lot::Mutex<sendspin::sync::ClockSync>>,
) -> Option<TimestampedChunk> {
    if stream.codec != "pcm" || !matches!(stream.channels, 1 | 2) {
        return None;
    }

    let local_play_time_us = clock_sync
        .lock()
        .server_to_client_micros(chunk.timestamp)
        .unwrap_or_else(|| unix_time_micros().saturating_add(150_000));

    let channels = usize::from(stream.channels);
    let decoded = decode_pcm_samples(chunk.data.as_ref(), channels, stream.bit_depth)?;

    if decoded.is_empty() {
        return None;
    }

    let frame_count = decoded.len() / channels;
    if frame_count == 0 {
        return None;
    }

    Some(TimestampedChunk {
        local_play_time_us,
        sample_rate_hz: stream.sample_rate_hz,
        channels,
        frame_count,
        samples: decoded,
    })
}

pub(crate) fn decode_pcm_samples(data: &[u8], channels: usize, bit_depth: u8) -> Option<Vec<f32>> {
    match bit_depth {
        16 => {
            if !data.len().is_multiple_of(2) {
                return None;
            }
            let mut samples = Vec::with_capacity(data.len() / 2);
            for bytes in data.chunks_exact(2) {
                let value = i16::from_le_bytes([bytes[0], bytes[1]]);
                samples.push(f32::from(value) / 32768.0);
            }
            if !samples.len().is_multiple_of(channels) {
                return None;
            }
            Some(samples)
        }
        24 => {
            if !data.len().is_multiple_of(3) {
                return None;
            }
            let mut samples = Vec::with_capacity(data.len() / 3);
            for bytes in data.chunks_exact(3) {
                let sign_extended = (((bytes[2] as i32) << 24)
                    | ((bytes[1] as i32) << 16)
                    | ((bytes[0] as i32) << 8))
                    >> 8;
                samples.push((sign_extended as f32) / 8_388_608.0);
            }
            if !samples.len().is_multiple_of(channels) {
                return None;
            }
            Some(samples)
        }
        32 => {
            if !data.len().is_multiple_of(4) {
                return None;
            }
            let mut samples = Vec::with_capacity(data.len() / 4);
            for bytes in data.chunks_exact(4) {
                // Sendspin PCM bit_depth=32 is signed integer PCM, not float PCM.
                let value = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                samples.push((value as f32) / 2_147_483_648.0);
            }
            if !samples.len().is_multiple_of(channels) {
                return None;
            }
            Some(samples)
        }
        _ => None,
    }
}

fn build_player_support(host_sample_rate_hz: u32) -> PlayerV1Support {
    let preferred_sample_rate = host_sample_rate_hz.max(44_100);

    PlayerV1Support {
        // Advertise exactly one format so the server cannot choose anything else.
        supported_formats: vec![AudioFormatSpec {
            codec: "pcm".to_string(),
            channels: 2,
            sample_rate: preferred_sample_rate,
            bit_depth: PREFERRED_PCM_BIT_DEPTH,
        }],
        buffer_capacity: BUFFER_CAPACITY_BYTES,
        supported_commands: vec!["volume".to_string(), "mute".to_string()],
    }
}

async fn request_pcm_format(
    ws_tx: &WsSender,
    sample_rate_hz: u32,
) -> Result<(), sendspin::error::Error> {
    let request = Message::StreamRequestFormat(StreamRequestFormat {
        player: Some(PlayerFormatRequest {
            codec: Some("pcm".to_string()),
            channels: Some(2),
            sample_rate: Some(sample_rate_hz),
            bit_depth: Some(PREFERRED_PCM_BIT_DEPTH),
        }),
        artwork: None,
    });

    ws_tx.send_message(request).await
}

async fn send_client_state(
    ws_tx: &WsSender,
    shared: &SharedState,
) -> Result<(), sendspin::error::Error> {
    let message = Message::ClientState(ClientState {
        player: Some(PlayerState {
            state: shared.sync_state().as_protocol(),
            volume: Some(shared.remote_volume.load(Ordering::Relaxed).min(100)),
            muted: Some(shared.remote_muted.load(Ordering::Relaxed)),
        }),
    });

    ws_tx.send_message(message).await
}

async fn send_controller_command(
    ws_tx: &WsSender,
    command: &str,
) -> Result<(), sendspin::error::Error> {
    let message = Message::ClientCommand(ClientCommand {
        controller: Some(ControllerCommand {
            command: command.to_string(),
            volume: None,
            mute: None,
        }),
    });

    ws_tx.send_message(message).await
}

async fn send_goodbye(
    ws_tx: &WsSender,
    reason: GoodbyeReason,
) -> Result<(), sendspin::error::Error> {
    ws_tx
        .send_message(Message::ClientGoodbye(ClientGoodbye { reason }))
        .await
}

enum ReconnectAction {
    Continue,
    Stop,
    UpdateServerUrl(String),
    UpdateClientName(String),
    QueueControllerCommand(String),
}

async fn wait_reconnect_or_command(
    command_rx: &mut UnboundedReceiver<WorkerCommand>,
    delay: Duration,
) -> ReconnectAction {
    tokio::select! {
        _ = tokio::time::sleep(delay) => ReconnectAction::Continue,
        maybe_command = command_rx.recv() => {
            match maybe_command {
                Some(WorkerCommand::Shutdown) | None => ReconnectAction::Stop,
                Some(WorkerCommand::SetServerUrl(new_url)) => {
                    match normalize_server_url(&new_url) {
                        Some(normalized) => ReconnectAction::UpdateServerUrl(normalized),
                        None => ReconnectAction::Continue,
                    }
                }
                Some(WorkerCommand::SetClientName(new_client_name)) => {
                    match normalize_client_name(&new_client_name) {
                        Some(normalized) => ReconnectAction::UpdateClientName(normalized),
                        None => ReconnectAction::Continue,
                    }
                }
                Some(WorkerCommand::SendControllerCommand(command)) => {
                    ReconnectAction::QueueControllerCommand(command)
                }
            }
        }
    }
}

fn unix_time_micros() -> i64 {
    let Ok(duration_since_epoch) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return 0;
    };

    duration_since_epoch.as_micros() as i64
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use rtrb::RingBuffer;
    use tokio::sync::mpsc::unbounded_channel;
    use uuid::Uuid;

    use super::{
        build_client_hello, build_player_support, decode_pcm_samples, network_thread_main,
        ProtocolClient,
    };
    use crate::state::{ConnectionState, SharedState, TimestampedChunk, WorkerCommand};

    #[test]
    fn decode_pcm_16() {
        let samples = decode_pcm_samples(&[0x00, 0x00, 0xff, 0x7f], 1, 16).expect("decode");
        assert_eq!(samples.len(), 2);
        assert!(samples[0].abs() < 0.0001);
        assert!(samples[1] > 0.9);
    }

    #[test]
    fn decode_pcm_24_rejects_misaligned_bytes() {
        assert!(decode_pcm_samples(&[1, 2], 2, 24).is_none());
    }

    #[test]
    fn decode_pcm_rejects_frame_mismatch() {
        let bytes = [0_u8; 6];
        assert!(decode_pcm_samples(&bytes, 4, 16).is_none());
    }

    #[test]
    #[ignore = "requires a running Sendspin server (set SENDSPIN_SMOKE_SERVER_URL)"]
    fn live_smoke_connects_to_sendspin_server() {
        let server_url = std::env::var("SENDSPIN_SMOKE_SERVER_URL")
            .unwrap_or_else(|_| "ws://127.0.0.1:8927/sendspin".to_string());

        let shared = Arc::new(SharedState::new());
        let (chunk_producer, _chunk_consumer) = RingBuffer::<TimestampedChunk>::new(16);
        let (command_tx, command_rx) = unbounded_channel();
        let shared_for_thread = Arc::clone(&shared);
        let client_id = Uuid::new_v4().to_string();

        let join_handle = thread::spawn(move || {
            network_thread_main(
                shared_for_thread,
                chunk_producer,
                command_rx,
                server_url,
                "Sendspin VST Smoke".to_string(),
                client_id,
            );
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if shared.connection_state() == ConnectionState::Connected {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }

        assert_eq!(
            shared.connection_state(),
            ConnectionState::Connected,
            "worker never reached connected state"
        );

        assert!(
            command_tx.send(WorkerCommand::Shutdown).is_ok(),
            "failed to send worker shutdown command"
        );
        assert!(join_handle.join().is_ok(), "failed joining network worker");
        assert!(
            !shared.stream_active.load(Ordering::Relaxed),
            "stream should be inactive after shutdown"
        );
    }

    #[test]
    #[ignore = "requires a running Sendspin server (set SENDSPIN_SMOKE_SERVER_URL)"]
    fn live_protocol_client_connects() {
        let server_url = std::env::var("SENDSPIN_SMOKE_SERVER_URL")
            .unwrap_or_else(|_| "ws://127.0.0.1:8927/sendspin".to_string());
        let client_id = Uuid::new_v4().to_string();

        let hello = build_client_hello(
            &client_id,
            "Sendspin VST Handshake Test",
            build_player_support(48_000),
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build runtime");

        let result = runtime.block_on(async { ProtocolClient::connect(&server_url, hello).await });
        if let Err(err) = result {
            panic!("ProtocolClient::connect failed: {err}");
        }
    }
}
