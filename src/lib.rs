use std::num::NonZeroU32;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nih_plug::prelude::*;
use nih_plug_egui::{create_egui_editor, egui, widgets, EguiState};
use parking_lot::Mutex;
use rtrb::{Consumer, Producer, RingBuffer};
use tokio::sync::mpsc::unbounded_channel;

mod config;
mod constants;
mod mdns;
mod network;
mod state;

use config::{
    default_client_name, load_or_create_client_id, normalize_client_name, normalize_server_url,
};
use constants::{
    CHUNK_QUEUE_CAPACITY, EFFECTIVE_PLUGIN_VERSION, PIPELINE_OFFSET_MAX_MS, PIPELINE_OFFSET_MIN_MS,
    PIPELINE_OFFSET_STEP_MS, RECOVERY_SYNC_THRESHOLD_BLOCKS, RENDER_CLOCK_REANCHOR_THRESHOLD_US,
    TIMING_JITTER_TOLERANCE_US, UNDERRUN_ERROR_THRESHOLD_BLOCKS,
};
use mdns::mdns_thread_main;
use network::network_thread_main;
use state::{
    GroupPlaybackState, MdnsWorker, NetworkWorker, SharedState, SyncState, TimestampedChunk,
};

#[derive(Debug)]
struct ActiveChunk {
    chunk: TimestampedChunk,
    next_frame_index: usize,
    started: bool,
}

#[derive(Debug, Clone, Copy)]
struct TimeAnchor {
    instant: Instant,
    unix_micros: i64,
}

impl TimeAnchor {
    fn capture_now() -> Self {
        Self {
            instant: Instant::now(),
            unix_micros: unix_time_micros(),
        }
    }

    fn instant_to_unix_micros(self, instant: Instant) -> i64 {
        if instant >= self.instant {
            let delta = instant.duration_since(self.instant).as_micros() as i64;
            self.unix_micros.saturating_add(delta)
        } else {
            let delta = self.instant.duration_since(instant).as_micros() as i64;
            self.unix_micros.saturating_sub(delta)
        }
    }
}

#[derive(Params)]
struct SendspinVst3Params {
    #[persist = "editor-state"]
    editor_state: Arc<EguiState>,

    #[persist = "client-name"]
    client_name: Arc<Mutex<String>>,

    #[persist = "server-url"]
    server_url: Arc<Mutex<String>>,

    #[id = "volume"]
    volume: FloatParam,

    #[id = "mute"]
    mute: BoolParam,

    #[id = "pipeline_offset_ms"]
    pipeline_offset_ms: IntParam,
}

impl Default for SendspinVst3Params {
    fn default() -> Self {
        Self {
            editor_state: EguiState::from_size(560, 320),
            client_name: Arc::new(Mutex::new(default_client_name())),
            server_url: Arc::new(Mutex::new(String::new())),
            volume: FloatParam::new("Volume", 1.0, FloatRange::Linear { min: 0.0, max: 1.0 })
                .with_unit(""),
            mute: BoolParam::new("Mute", false),
            pipeline_offset_ms: IntParam::new(
                "Pipeline Offset (ms)",
                0,
                IntRange::Linear {
                    min: PIPELINE_OFFSET_MIN_MS,
                    max: PIPELINE_OFFSET_MAX_MS,
                },
            )
            .with_unit(" ms"),
        }
    }
}

pub struct SendspinVst3 {
    params: Arc<SendspinVst3Params>,
    shared: Arc<SharedState>,
    chunk_consumer: Consumer<TimestampedChunk>,
    chunk_producer: Option<Producer<TimestampedChunk>>,
    active_chunk: Option<ActiveChunk>,
    worker: Option<NetworkWorker>,
    mdns_worker: Option<MdnsWorker>,
    sample_rate_hz: u32,
    render_cursor_us: Option<i64>,
    time_anchor: TimeAnchor,
    underrun_streak_blocks: u32,
    recovery_streak_blocks: u32,
}

impl Default for SendspinVst3 {
    fn default() -> Self {
        let (chunk_producer, chunk_consumer) =
            RingBuffer::<TimestampedChunk>::new(CHUNK_QUEUE_CAPACITY);
        Self {
            params: Arc::new(SendspinVst3Params::default()),
            shared: Arc::new(SharedState::new()),
            chunk_consumer,
            chunk_producer: Some(chunk_producer),
            active_chunk: None,
            worker: None,
            mdns_worker: None,
            sample_rate_hz: 48_000,
            render_cursor_us: None,
            time_anchor: TimeAnchor::capture_now(),
            underrun_streak_blocks: 0,
            recovery_streak_blocks: 0,
        }
    }
}

impl Drop for SendspinVst3 {
    fn drop(&mut self) {
        if let Some(mut mdns_worker) = self.mdns_worker.take() {
            mdns_worker.shutdown();
        }
        if let Some(mut worker) = self.worker.take() {
            worker.shutdown();
        }
    }
}

impl SendspinVst3 {
    fn start_mdns_worker_if_needed(&mut self) {
        if self.mdns_worker.is_some() {
            return;
        }

        let shared = Arc::clone(&self.shared);
        let (command_tx, command_rx) = mpsc::channel();
        self.shared.set_mdns_command_tx(command_tx.clone());

        let join_handle = thread::spawn(move || {
            mdns_thread_main(shared, command_rx);
        });

        self.mdns_worker = Some(MdnsWorker::new(command_tx, join_handle));
    }

    fn start_worker_if_needed(&mut self) {
        if self.worker.is_some() {
            return;
        }

        let Some(chunk_producer) = self.chunk_producer.take() else {
            return;
        };

        let shared = Arc::clone(&self.shared);
        let server_url = self.shared.configured_server_url();
        let client_name = self.shared.configured_client_name();
        let client_id = load_or_create_client_id();
        let (command_tx, command_rx) = unbounded_channel();
        self.shared.set_worker_command_tx(command_tx.clone());

        let join_handle = thread::spawn(move || {
            network_thread_main(
                shared,
                chunk_producer,
                command_rx,
                server_url,
                client_name,
                client_id,
            );
        });

        self.worker = Some(NetworkWorker::new(command_tx, join_handle));
    }

    fn clear_audio_queue(&mut self) {
        self.active_chunk = None;
        self.render_cursor_us = None;
        self.underrun_streak_blocks = 0;
        self.recovery_streak_blocks = 0;
        while self.chunk_consumer.pop().is_ok() {}
    }

    fn next_frame(&mut self, sample_time_us: i64, host_sample_rate_hz: u32) -> Option<(f32, f32)> {
        let host_sample_rate_hz = host_sample_rate_hz.max(1);

        loop {
            if self.active_chunk.is_none() {
                match self.chunk_consumer.pop() {
                    Ok(next_chunk) => {
                        self.active_chunk = Some(ActiveChunk {
                            chunk: next_chunk,
                            next_frame_index: 0,
                            started: false,
                        });
                        continue;
                    }
                    Err(_) => return None,
                }
            }

            let drop_active = {
                let Some(active) = self.active_chunk.as_mut() else {
                    continue;
                };

                if active.chunk.sample_rate_hz != host_sample_rate_hz {
                    self.shared.record_late_chunk();
                    true
                } else {
                    let mut drop_for_late_start = false;
                    if !active.started {
                        let delta_us = sample_time_us - active.chunk.local_play_time_us;
                        if delta_us < -TIMING_JITTER_TOLERANCE_US {
                            return None;
                        }

                        let adjusted_delta_us = if delta_us.abs() <= TIMING_JITTER_TOLERANCE_US {
                            0
                        } else {
                            delta_us.max(0)
                        };
                        let skipped_frames = ((adjusted_delta_us * i64::from(host_sample_rate_hz))
                            / 1_000_000) as usize;
                        if skipped_frames >= active.chunk.frame_count {
                            self.shared.record_late_chunk();
                            drop_for_late_start = true;
                        } else {
                            active.next_frame_index = skipped_frames;
                            active.started = true;
                        }
                    }

                    if drop_for_late_start || active.next_frame_index >= active.chunk.frame_count {
                        true
                    } else {
                        let base = active.next_frame_index * active.chunk.channels;
                        let left = active.chunk.samples[base];
                        let right = if active.chunk.channels > 1 {
                            active.chunk.samples[base + 1]
                        } else {
                            left
                        };
                        active.next_frame_index += 1;
                        return Some((left, right));
                    }
                }
            };

            if drop_active {
                self.active_chunk = None;
                continue;
            }
        }
    }

    fn update_sync_state_for_block(&mut self, received_any_frame: bool) {
        if self.shared.stream_active.load(Ordering::Relaxed) {
            if received_any_frame {
                self.underrun_streak_blocks = 0;
                self.recovery_streak_blocks = self.recovery_streak_blocks.saturating_add(1);

                if self.shared.sync_state() == SyncState::Error {
                    if self.recovery_streak_blocks >= RECOVERY_SYNC_THRESHOLD_BLOCKS {
                        self.shared.set_sync_state(SyncState::Synchronized);
                        self.recovery_streak_blocks = 0;
                    }
                } else {
                    self.shared.set_sync_state(SyncState::Synchronized);
                }
            } else {
                self.recovery_streak_blocks = 0;
                self.underrun_streak_blocks = self.underrun_streak_blocks.saturating_add(1);
                if self.underrun_streak_blocks >= UNDERRUN_ERROR_THRESHOLD_BLOCKS {
                    self.shared.set_sync_state(SyncState::Error);
                }
            }
        } else {
            self.underrun_streak_blocks = 0;
            self.recovery_streak_blocks = 0;
        }
    }
}

#[derive(Debug, Default)]
struct EditorUiState {
    client_name_input: String,
    client_name_initialized: bool,
    use_custom_url: bool,
    custom_url_input: String,
    custom_url_initialized: bool,
    last_message: String,
}

fn apply_client_name_selection(
    params: &Arc<SendspinVst3Params>,
    shared: &Arc<SharedState>,
    raw_name: &str,
) -> Result<String, &'static str> {
    let normalized = normalize_client_name(raw_name).ok_or("Client name cannot be empty")?;
    *params.client_name.lock() = normalized.clone();
    shared.request_client_name_switch(normalized.clone());
    Ok(normalized)
}

fn apply_server_url_selection(
    params: &Arc<SendspinVst3Params>,
    shared: &Arc<SharedState>,
    raw_url: &str,
) -> Result<String, &'static str> {
    let normalized = normalize_server_url(raw_url).ok_or("Invalid WebSocket URL")?;
    *params.server_url.lock() = normalized.clone();
    shared.request_server_switch(normalized.clone());
    Ok(normalized)
}

impl Plugin for SendspinVst3 {
    const NAME: &'static str = "Sendspin";
    const VENDOR: &'static str = "Sendspin";
    const URL: &'static str = "https://github.com/Sendspin";
    const EMAIL: &'static str = "devnull@sendspin.invalid";
    const VERSION: &'static str = EFFECTIVE_PLUGIN_VERSION;

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: None,
        main_output_channels: NonZeroU32::new(2),
        aux_input_ports: &[],
        aux_output_ports: &[],
        names: PortNames::const_default(),
    }];

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        let params = Arc::clone(&self.params);
        let shared = Arc::clone(&self.shared);

        create_egui_editor(
            Arc::clone(&self.params.editor_state),
            EditorUiState::default(),
            |_, _| {},
            move |egui_ctx, setter, state| {
                let discovered_servers = shared.discovered_servers();
                let configured_url = shared.configured_server_url();
                let configured_client_name = shared.configured_client_name();
                {
                    let mut persisted_server_url = params.server_url.lock();
                    if *persisted_server_url != configured_url {
                        *persisted_server_url = configured_url.clone();
                    }
                }
                {
                    let mut persisted_client_name = params.client_name.lock();
                    if *persisted_client_name != configured_client_name {
                        *persisted_client_name = configured_client_name.clone();
                    }
                }
                let connection_state = shared.connection_state();
                let playback_state = shared.group_playback_state();
                let (now_playing_artist, now_playing_title) = shared.now_playing();
                let diagnostics = shared.diagnostics_snapshot();
                let now_playing_label =
                    match (now_playing_artist.is_empty(), now_playing_title.is_empty()) {
                        (true, true) => "Now playing: (metadata unavailable)".to_string(),
                        (false, true) => format!("Now playing: {now_playing_artist}"),
                        (true, false) => format!("Now playing: {now_playing_title}"),
                        (false, false) => {
                            format!("Now playing: {now_playing_artist} - {now_playing_title}")
                        }
                    };
                if !state.client_name_initialized {
                    state.client_name_input = configured_client_name.clone();
                    state.client_name_initialized = true;
                }
                if !state.custom_url_initialized {
                    state.custom_url_input = configured_url.clone();
                    state.custom_url_initialized = true;
                }

                if !discovered_servers
                    .iter()
                    .any(|entry| entry.url == configured_url)
                    && !state.use_custom_url
                {
                    state.use_custom_url = true;
                    state.custom_url_input = configured_url.clone();
                }

                egui::CentralPanel::default().show(egui_ctx, |ui| {
                    ui.heading("Sendspin");
                    ui.label(format!("Connection: {}", connection_state.as_str()));
                    ui.label(format!("Client Name: {}", configured_client_name));
                    ui.label(format!("Server URL: {}", configured_url));
                    ui.label(format!(
                        "Diagnostics: queue_overflow={} decode_error={} late_chunk={}",
                        diagnostics.queue_overflow_count,
                        diagnostics.decode_error_count,
                        diagnostics.late_chunk_count
                    ));
                    ui.horizontal_wrapped(|ui| {
                        let toggle_label = if playback_state == GroupPlaybackState::Playing {
                            "Pause"
                        } else {
                            "Play"
                        };
                        let command = if playback_state == GroupPlaybackState::Playing {
                            "pause"
                        } else {
                            "play"
                        };
                        if ui.button(toggle_label).clicked() {
                            shared.request_controller_command(command.to_string());
                            state.last_message = format!("Sent '{command}' command.");
                        }
                        ui.label(now_playing_label);
                    });
                    ui.label(
                        "Play/Pause sends controller commands to the connected Sendspin server.",
                    );
                    ui.horizontal(|ui| {
                        ui.label("Sync delay trim:");
                        if ui.small_button("<").clicked() {
                            let current = params.pipeline_offset_ms.value();
                            let stepped = (current - PIPELINE_OFFSET_STEP_MS)
                                .clamp(PIPELINE_OFFSET_MIN_MS, PIPELINE_OFFSET_MAX_MS);
                            if stepped != current {
                                setter.begin_set_parameter(&params.pipeline_offset_ms);
                                setter.set_parameter(&params.pipeline_offset_ms, stepped);
                                setter.end_set_parameter(&params.pipeline_offset_ms);
                            }
                        }
                        if ui.small_button(">").clicked() {
                            let current = params.pipeline_offset_ms.value();
                            let stepped = (current + PIPELINE_OFFSET_STEP_MS)
                                .clamp(PIPELINE_OFFSET_MIN_MS, PIPELINE_OFFSET_MAX_MS);
                            if stepped != current {
                                setter.begin_set_parameter(&params.pipeline_offset_ms);
                                setter.set_parameter(&params.pipeline_offset_ms, stepped);
                                setter.end_set_parameter(&params.pipeline_offset_ms);
                            }
                        }
                        ui.label(format!("{} ms", params.pipeline_offset_ms.value()));
                    });
                    ui.label(
                        "Latency compensation: use negative values to make audio earlier, \
                         positive values to add delay.",
                    );
                    ui.add(
                        widgets::ParamSlider::for_param(&params.pipeline_offset_ms, setter)
                            .with_width(220.0),
                    );
                    ui.separator();

                    ui.horizontal(|ui| {
                        ui.label("Client name:");
                        ui.push_id("client-name-input", |ui| {
                            ui.text_edit_singleline(&mut state.client_name_input);
                        });
                        if ui.button("Apply Name").clicked() {
                            match apply_client_name_selection(
                                &params,
                                &shared,
                                &state.client_name_input,
                            ) {
                                Ok(applied_name) => {
                                    state.client_name_input = applied_name;
                                    state.last_message =
                                        "Applying client name and reconnecting...".to_string();
                                }
                                Err(err) => {
                                    state.last_message = err.to_string();
                                }
                            }
                        }
                    });
                    ui.separator();

                    ui.horizontal(|ui| {
                        ui.label("Discovered server:");
                        let selected_discovered_name = discovered_servers
                            .iter()
                            .find(|entry| entry.url == configured_url)
                            .map(|entry| entry.name.clone())
                            .unwrap_or_else(|| "Other...".to_string());

                        egui::ComboBox::from_id_salt("sendspin-server-selector")
                            .selected_text(selected_discovered_name)
                            .show_ui(ui, |ui| {
                                for entry in &discovered_servers {
                                    if ui
                                        .selectable_label(
                                            !state.use_custom_url && configured_url == entry.url,
                                            format!("{} ({})", entry.name, entry.url),
                                        )
                                        .clicked()
                                    {
                                        match apply_server_url_selection(
                                            &params, &shared, &entry.url,
                                        ) {
                                            Ok(applied_url) => {
                                                state.use_custom_url = false;
                                                state.custom_url_input = applied_url;
                                                state.last_message =
                                                    "Switching to selected server...".to_string();
                                            }
                                            Err(err) => {
                                                state.last_message = err.to_string();
                                            }
                                        }
                                    }
                                }

                                ui.separator();
                                if ui
                                    .selectable_label(state.use_custom_url, "Other...")
                                    .clicked()
                                {
                                    state.use_custom_url = true;
                                }
                            });

                        if ui.button("Refresh").clicked() {
                            shared.request_mdns_refresh();
                            state.last_message = "Refreshing mDNS discovery...".to_string();
                        }
                    });

                    if state.use_custom_url {
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label("Custom URL:");
                            ui.push_id("custom-url-input", |ui| {
                                ui.text_edit_singleline(&mut state.custom_url_input);
                            });
                            if ui.button("Connect").clicked() {
                                match apply_server_url_selection(
                                    &params,
                                    &shared,
                                    &state.custom_url_input,
                                ) {
                                    Ok(applied_url) => {
                                        state.custom_url_input = applied_url;
                                        state.last_message =
                                            "Switching to custom server...".to_string();
                                    }
                                    Err(err) => {
                                        state.last_message = err.to_string();
                                    }
                                }
                            }
                        });
                    }

                    if !state.last_message.is_empty() {
                        ui.separator();
                        ui.label(state.last_message.clone());
                    }
                });
            },
        )
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        self.sample_rate_hz = buffer_config.sample_rate.round() as u32;
        self.shared
            .host_sample_rate_hz
            .store(self.sample_rate_hz, Ordering::Relaxed);
        self.shared
            .host_block_size
            .store(buffer_config.max_buffer_size, Ordering::Relaxed);
        self.render_cursor_us = None;
        self.time_anchor = TimeAnchor::capture_now();
        let saved_client_name = self.params.client_name.lock().clone();
        let configured_client_name =
            normalize_client_name(&saved_client_name).unwrap_or_else(default_client_name);
        *self.params.client_name.lock() = configured_client_name.clone();
        self.shared
            .set_configured_client_name(configured_client_name);
        let saved_server_url = self.params.server_url.lock().clone();
        let configured_server_url = normalize_server_url(&saved_server_url).unwrap_or_default();
        *self.params.server_url.lock() = configured_server_url.clone();
        self.shared.set_configured_server_url(configured_server_url);
        self.start_mdns_worker_if_needed();
        self.start_worker_if_needed();
        true
    }

    fn reset(&mut self) {
        self.clear_audio_queue();
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        if self.shared.take_clear_requested() {
            self.clear_audio_queue();
        }

        let host_sample_rate_hz = self.sample_rate_hz.max(1);
        let user_gain = self.params.volume.value();
        let user_mute = self.params.mute.value();
        let user_offset_us = i64::from(self.params.pipeline_offset_ms.value()) * 1_000;

        let remote_gain =
            f32::from(self.shared.remote_volume.load(Ordering::Relaxed).min(100)) / 100.0;
        let remote_mute = self.shared.remote_muted.load(Ordering::Relaxed);

        let block_latency_us =
            ((buffer.samples() as i64) * 1_000_000) / i64::from(host_sample_rate_hz);
        let callback_instant = Instant::now();
        let callback_with_offset = shift_instant_by_micros(
            callback_instant,
            block_latency_us.saturating_add(user_offset_us),
        );
        let block_start_from_wall = self
            .time_anchor
            .instant_to_unix_micros(callback_with_offset);
        let block_duration_us =
            ((buffer.samples() as i64) * 1_000_000) / i64::from(host_sample_rate_hz);
        let had_render_cursor = self.render_cursor_us.is_some();
        let block_start_us = self.render_cursor_us.unwrap_or(block_start_from_wall);
        let next_block_start_us = block_start_us.saturating_add(block_duration_us);

        if had_render_cursor {
            let drift_us = block_start_from_wall.saturating_sub(block_start_us);
            if drift_us.unsigned_abs() > RENDER_CLOCK_REANCHOR_THRESHOLD_US as u64 {
                self.render_cursor_us =
                    Some(block_start_from_wall.saturating_add(block_duration_us));
            } else {
                self.render_cursor_us = Some(next_block_start_us);
            }
        } else {
            self.render_cursor_us = Some(next_block_start_us);
        }

        let mut received_any_frame = false;

        for (frame_index, mut channel_samples) in buffer.iter_samples().enumerate() {
            let frame_offset_us =
                ((frame_index as i64) * 1_000_000) / i64::from(host_sample_rate_hz);
            let sample_time_us = block_start_us.saturating_add(frame_offset_us);

            let (mut left, mut right) = match self.next_frame(sample_time_us, host_sample_rate_hz) {
                Some((l, r)) => {
                    received_any_frame = true;
                    (l, r)
                }
                None => (0.0, 0.0),
            };

            if user_mute || remote_mute {
                left = 0.0;
                right = 0.0;
            } else {
                let gain = user_gain * remote_gain;
                left *= gain;
                right *= gain;
            }

            if let Some(left_out) = channel_samples.get_mut(0) {
                *left_out = left;
            }
            if let Some(right_out) = channel_samples.get_mut(1) {
                *right_out = right;
            }
            for channel_index in 2..channel_samples.len() {
                if let Some(out) = channel_samples.get_mut(channel_index) {
                    *out = 0.0;
                }
            }
        }

        self.update_sync_state_for_block(received_any_frame);

        ProcessStatus::Normal
    }
}

impl Vst3Plugin for SendspinVst3 {
    const VST3_CLASS_ID: [u8; 16] = *b"SndSpnVst3Plug01";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Instrument, Vst3SubCategory::Stereo];
}

nih_export_vst3!(SendspinVst3);

fn shift_instant_by_micros(base: Instant, delta_us: i64) -> Instant {
    if delta_us >= 0 {
        base + Duration::from_micros(delta_us as u64)
    } else {
        base.checked_sub(Duration::from_micros((-delta_us) as u64))
            .unwrap_or(base)
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
    use super::{
        SendspinVst3, SyncState, RECOVERY_SYNC_THRESHOLD_BLOCKS, UNDERRUN_ERROR_THRESHOLD_BLOCKS,
    };

    #[test]
    fn sync_state_hysteresis_resists_flapping() {
        let mut plugin = SendspinVst3::default();
        plugin.shared.set_stream_active(true);
        plugin.shared.set_sync_state(SyncState::Synchronized);

        for _ in 0..(UNDERRUN_ERROR_THRESHOLD_BLOCKS - 1) {
            plugin.update_sync_state_for_block(false);
            assert_eq!(plugin.shared.sync_state(), SyncState::Synchronized);
        }

        plugin.update_sync_state_for_block(false);
        assert_eq!(plugin.shared.sync_state(), SyncState::Error);

        for _ in 0..(RECOVERY_SYNC_THRESHOLD_BLOCKS - 1) {
            plugin.update_sync_state_for_block(true);
            assert_eq!(plugin.shared.sync_state(), SyncState::Error);
        }

        plugin.update_sync_state_for_block(true);
        assert_eq!(plugin.shared.sync_state(), SyncState::Synchronized);
    }
}
