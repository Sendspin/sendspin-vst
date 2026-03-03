use std::fs;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver as StdReceiver, Sender as StdSender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mdns_sd::{ServiceDaemon, ServiceEvent};
use nih_plug::prelude::*;
use nih_plug_egui::{create_egui_editor, egui, widgets, EguiState};
use parking_lot::{Mutex, RwLock};
use rtrb::{Consumer, Producer, RingBuffer};
use sendspin::protocol::client::{AudioChunk, WsSender};
use sendspin::protocol::messages::{
    AudioFormatSpec, ClientGoodbye, ClientState, GoodbyeReason, Message, PlayerFormatRequest,
    PlayerState, PlayerSyncState, PlayerV1Support, StreamRequestFormat,
};
use sendspin::ProtocolClientBuilder;
use tokio::runtime::Builder as TokioRuntimeBuilder;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use url::Url;
use uuid::Uuid;

const DEFAULT_SERVER_PATH: &str = "/sendspin";
const SENDSPIN_SERVER_SERVICE_TYPE: &str = "_sendspin-server._tcp.local.";
const DEFAULT_CLIENT_NAME: &str = "Sendspin VST";
const PRODUCT_NAME: &str = "Sendspin VST3";
const BUFFER_CAPACITY_BYTES: u32 = 1_048_576;
const CHUNK_QUEUE_CAPACITY: usize = 512;
const TIMING_JITTER_TOLERANCE_US: i64 = 1_000;
const RENDER_CLOCK_REANCHOR_THRESHOLD_US: i64 = 250_000;
const PREFERRED_PCM_BIT_DEPTH: u8 = 24;
const PIPELINE_OFFSET_MIN_MS: i32 = -100;
const PIPELINE_OFFSET_MAX_MS: i32 = 200;
const PIPELINE_OFFSET_STEP_MS: i32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum SyncState {
    Synchronized = 0,
    Error = 1,
}

impl SyncState {
    fn from_u8(raw: u8) -> Self {
        if raw == Self::Error as u8 {
            Self::Error
        } else {
            Self::Synchronized
        }
    }

    fn as_protocol(self) -> PlayerSyncState {
        match self {
            Self::Synchronized => PlayerSyncState::Synchronized,
            Self::Error => PlayerSyncState::Error,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum ConnectionState {
    Disconnected = 0,
    Connecting = 1,
    Connected = 2,
    Error = 3,
}

impl ConnectionState {
    fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Connecting,
            2 => Self::Connected,
            3 => Self::Error,
            _ => Self::Disconnected,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Disconnected => "Disconnected",
            Self::Connecting => "Connecting",
            Self::Connected => "Connected",
            Self::Error => "Error",
        }
    }
}

#[derive(Debug, Clone)]
struct DiscoveredServer {
    id: String,
    name: String,
    url: String,
}

#[derive(Debug)]
struct SharedState {
    host_sample_rate_hz: AtomicU32,
    host_block_size: AtomicU32,
    stream_active: AtomicBool,
    clear_requested: AtomicBool,
    remote_volume: AtomicU8,
    remote_muted: AtomicBool,
    sync_state: AtomicU8,
    connection_state: AtomicU8,
    state_dirty: AtomicBool,
    configured_server_url: RwLock<String>,
    configured_client_name: RwLock<String>,
    discovered_servers: RwLock<Vec<DiscoveredServer>>,
    worker_command_tx: Mutex<Option<UnboundedSender<WorkerCommand>>>,
    mdns_command_tx: Mutex<Option<StdSender<MdnsCommand>>>,
}

impl SharedState {
    fn new() -> Self {
        Self {
            host_sample_rate_hz: AtomicU32::new(48_000),
            host_block_size: AtomicU32::new(512),
            stream_active: AtomicBool::new(false),
            clear_requested: AtomicBool::new(false),
            remote_volume: AtomicU8::new(100),
            remote_muted: AtomicBool::new(false),
            sync_state: AtomicU8::new(SyncState::Synchronized as u8),
            connection_state: AtomicU8::new(ConnectionState::Disconnected as u8),
            state_dirty: AtomicBool::new(true),
            configured_server_url: RwLock::new(String::new()),
            configured_client_name: RwLock::new(default_client_name()),
            discovered_servers: RwLock::new(Vec::new()),
            worker_command_tx: Mutex::new(None),
            mdns_command_tx: Mutex::new(None),
        }
    }

    fn set_connection_state(&self, state: ConnectionState) {
        self.connection_state.store(state as u8, Ordering::Relaxed);
    }

    fn set_stream_active(&self, active: bool) {
        self.stream_active.store(active, Ordering::Relaxed);
    }

    fn connection_state(&self) -> ConnectionState {
        ConnectionState::from_u8(self.connection_state.load(Ordering::Acquire))
    }

    fn request_clear(&self) {
        self.clear_requested.store(true, Ordering::Release);
    }

    fn take_clear_requested(&self) -> bool {
        self.clear_requested.swap(false, Ordering::AcqRel)
    }

    fn set_sync_state(&self, state: SyncState) {
        let previous = self.sync_state.swap(state as u8, Ordering::AcqRel);
        if previous != state as u8 {
            self.mark_state_dirty();
        }
    }

    fn sync_state(&self) -> SyncState {
        SyncState::from_u8(self.sync_state.load(Ordering::Acquire))
    }

    fn mark_state_dirty(&self) {
        self.state_dirty.store(true, Ordering::Release);
    }

    fn take_state_dirty(&self) -> bool {
        self.state_dirty.swap(false, Ordering::AcqRel)
    }

    fn set_configured_server_url(&self, url: String) {
        *self.configured_server_url.write() = url;
    }

    fn configured_server_url(&self) -> String {
        self.configured_server_url.read().clone()
    }

    fn set_configured_client_name(&self, name: String) {
        *self.configured_client_name.write() = name;
    }

    fn configured_client_name(&self) -> String {
        self.configured_client_name.read().clone()
    }

    fn set_discovered_servers(&self, servers: Vec<DiscoveredServer>) {
        *self.discovered_servers.write() = servers;
    }

    fn discovered_servers(&self) -> Vec<DiscoveredServer> {
        self.discovered_servers.read().clone()
    }

    fn set_worker_command_tx(&self, tx: UnboundedSender<WorkerCommand>) {
        *self.worker_command_tx.lock() = Some(tx);
    }

    fn set_mdns_command_tx(&self, tx: StdSender<MdnsCommand>) {
        *self.mdns_command_tx.lock() = Some(tx);
    }

    fn request_server_switch(&self, url: String) {
        self.set_configured_server_url(url.clone());
        if let Some(tx) = self.worker_command_tx.lock().as_ref() {
            let _ = tx.send(WorkerCommand::SetServerUrl(url));
        }
    }

    fn request_client_name_switch(&self, client_name: String) {
        self.set_configured_client_name(client_name.clone());
        if let Some(tx) = self.worker_command_tx.lock().as_ref() {
            let _ = tx.send(WorkerCommand::SetClientName(client_name));
        }
    }

    fn request_mdns_refresh(&self) {
        if let Some(tx) = self.mdns_command_tx.lock().as_ref() {
            let _ = tx.send(MdnsCommand::Refresh);
        }
    }
}

#[derive(Debug)]
struct TimestampedChunk {
    local_play_time_us: i64,
    sample_rate_hz: u32,
    channels: usize,
    frame_count: usize,
    samples: Vec<f32>,
}

#[derive(Debug)]
struct ActiveChunk {
    chunk: TimestampedChunk,
    next_frame_index: usize,
    started: bool,
}

#[derive(Debug, Clone)]
struct ActiveStream {
    codec: String,
    sample_rate_hz: u32,
    channels: u8,
    bit_depth: u8,
}

enum WorkerCommand {
    Shutdown,
    SetServerUrl(String),
    SetClientName(String),
}

enum MdnsCommand {
    Shutdown,
    Refresh,
}

struct NetworkWorker {
    command_tx: UnboundedSender<WorkerCommand>,
    join_handle: Option<thread::JoinHandle<()>>,
}

impl NetworkWorker {
    fn shutdown(&mut self) {
        let _ = self.command_tx.send(WorkerCommand::Shutdown);
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

struct MdnsWorker {
    command_tx: StdSender<MdnsCommand>,
    join_handle: Option<thread::JoinHandle<()>>,
}

impl MdnsWorker {
    fn shutdown(&mut self) {
        let _ = self.command_tx.send(MdnsCommand::Shutdown);
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
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

        self.mdns_worker = Some(MdnsWorker {
            command_tx,
            join_handle: Some(join_handle),
        });
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

        self.worker = Some(NetworkWorker {
            command_tx,
            join_handle: Some(join_handle),
        });
    }

    fn clear_audio_queue(&mut self) {
        self.active_chunk = None;
        self.render_cursor_us = None;
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
                    true
                } else {
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
                        active.next_frame_index = skipped_frames;
                        active.started = true;
                    }

                    if active.next_frame_index >= active.chunk.frame_count {
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
    const NAME: &'static str = "Sendspin VST3";
    const VENDOR: &'static str = "Sendspin";
    const URL: &'static str = "https://github.com/Sendspin";
    const EMAIL: &'static str = "devnull@sendspin.invalid";
    const VERSION: &'static str = match option_env!("SENDSPIN_BUILD_VERSION") {
        Some(version) => version,
        None => env!("CARGO_PKG_VERSION"),
    };

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
                    ui.heading("Sendspin VST3");
                    ui.label(format!(
                        "Connection: {}",
                        shared.connection_state().as_str()
                    ));
                    ui.label(format!("Client Name: {}", configured_client_name));
                    ui.label(format!("Server URL: {}", configured_url));
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

                    ui.separator();
                    ui.label(format!("Discovered servers: {}", discovered_servers.len()));
                    for entry in &discovered_servers {
                        ui.label(format!("{}: {}", entry.name, entry.url));
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
            .store(buffer_config.max_buffer_size as u32, Ordering::Relaxed);
        self.render_cursor_us = None;
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
        // Keep persisted state aligned with background auto-selection (e.g. mDNS auto-connect).
        let configured_server_url = self.shared.configured_server_url();
        {
            let mut persisted_server_url = self.params.server_url.lock();
            if *persisted_server_url != configured_server_url {
                *persisted_server_url = configured_server_url;
            }
        }

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
            ((buffer.samples() as i64) * 1_000_000) / i64::from(host_sample_rate_hz.max(1));
        let block_start_from_wall =
            unix_time_micros().saturating_add(block_latency_us + user_offset_us);
        let block_duration_us =
            ((buffer.samples() as i64) * 1_000_000) / i64::from(host_sample_rate_hz.max(1));
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

        if self.shared.stream_active.load(Ordering::Relaxed) {
            if received_any_frame {
                self.shared.set_sync_state(SyncState::Synchronized);
            } else {
                self.shared.set_sync_state(SyncState::Error);
            }
        }

        ProcessStatus::Normal
    }
}

impl Vst3Plugin for SendspinVst3 {
    const VST3_CLASS_ID: [u8; 16] = *b"SndSpnVst3Plug01";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Instrument, Vst3SubCategory::Stereo];
}

nih_export_vst3!(SendspinVst3);

fn network_thread_main(
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
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break 'outer,
                }
            }

            if active_server_url.is_none() {
                shared.set_connection_state(ConnectionState::Disconnected);
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
                }
            }

            shared.set_connection_state(ConnectionState::Connecting);

            let host_sample_rate_hz = shared.host_sample_rate_hz.load(Ordering::Relaxed).max(44_100);
            let player_support = build_player_support(host_sample_rate_hz);

            let builder = ProtocolClientBuilder::builder()
                .client_id(client_id.clone())
                .name(active_client_name.clone())
                .product_name(Some(PRODUCT_NAME.to_string()))
                .software_version(Some(env!("CARGO_PKG_VERSION").to_string()))
                .player_v1_support(player_support)
                .build();

            let Some(connect_url) = active_server_url.as_ref() else {
                continue;
            };
            let client = match builder.connect(connect_url).await {
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
                                        active_client_name = normalized.clone();
                                        shared.set_configured_client_name(normalized);
                                        reconnect_immediately = true;
                                        break;
                                    }
                                }
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
                                let _ = chunk_producer.push(decoded_chunk);
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
            }
        }
    });
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
            shared.request_clear();
            shared.set_sync_state(SyncState::Synchronized);
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
    if stream.codec != "pcm" || stream.channels == 0 {
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

fn decode_pcm_samples(data: &[u8], channels: usize, bit_depth: u8) -> Option<Vec<f32>> {
    match bit_depth {
        16 => {
            if data.len() % 2 != 0 {
                return None;
            }
            let mut samples = Vec::with_capacity(data.len() / 2);
            for bytes in data.chunks_exact(2) {
                let value = i16::from_le_bytes([bytes[0], bytes[1]]);
                samples.push(f32::from(value) / 32768.0);
            }
            if samples.len() % channels != 0 {
                return None;
            }
            Some(samples)
        }
        24 => {
            if data.len() % 3 != 0 {
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
            if samples.len() % channels != 0 {
                return None;
            }
            Some(samples)
        }
        32 => {
            if data.len() % 4 != 0 {
                return None;
            }
            let mut samples = Vec::with_capacity(data.len() / 4);
            for bytes in data.chunks_exact(4) {
                // Sendspin PCM bit_depth=32 is signed integer PCM, not float PCM.
                let value = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                samples.push((value as f32) / 2_147_483_648.0);
            }
            if samples.len() % channels != 0 {
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
            }
        }
    }
}

fn default_client_name() -> String {
    if let Ok(value) = std::env::var("SENDSPIN_CLIENT_NAME") {
        if let Some(normalized) = normalize_client_name(&value) {
            return normalized;
        }
    }

    DEFAULT_CLIENT_NAME.to_string()
}

fn normalize_client_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Keep the wire payload bounded and readable.
    let max_chars = 80;
    let normalized: String = trimmed.chars().take(max_chars).collect();
    if normalized.is_empty() {
        return None;
    }

    Some(normalized)
}

fn normalize_server_url(raw: &str) -> Option<String> {
    let mut candidate = raw.trim().to_string();
    if candidate.is_empty() {
        return None;
    }

    if !candidate.contains("://") {
        candidate = format!("ws://{candidate}");
    } else if candidate.starts_with("http://") {
        candidate = format!("ws://{}", candidate.trim_start_matches("http://"));
    } else if candidate.starts_with("https://") {
        candidate = format!("wss://{}", candidate.trim_start_matches("https://"));
    }

    let mut parsed = Url::parse(&candidate).ok()?;
    if !matches!(parsed.scheme(), "ws" | "wss") || parsed.host_str().is_none() {
        return None;
    }

    if parsed.path().is_empty() || parsed.path() == "/" {
        parsed.set_path(DEFAULT_SERVER_PATH);
    }

    Some(parsed.to_string())
}

fn mdns_thread_main(shared: Arc<SharedState>, command_rx: StdReceiver<MdnsCommand>) {
    let Ok(mdns) = ServiceDaemon::new() else {
        return;
    };

    let mut browse_rx = match mdns.browse(SENDSPIN_SERVER_SERVICE_TYPE) {
        Ok(receiver) => receiver,
        Err(_) => {
            let _ = mdns.shutdown();
            return;
        }
    };

    let mut discovered_by_id: std::collections::BTreeMap<String, DiscoveredServer> =
        std::collections::BTreeMap::new();

    loop {
        match command_rx.try_recv() {
            Ok(MdnsCommand::Shutdown) | Err(TryRecvError::Disconnected) => break,
            Ok(MdnsCommand::Refresh) => {
                let _ = mdns.stop_browse(SENDSPIN_SERVER_SERVICE_TYPE);
                discovered_by_id.clear();
                shared.set_discovered_servers(Vec::new());

                if let Ok(new_receiver) = mdns.browse(SENDSPIN_SERVER_SERVICE_TYPE) {
                    browse_rx = new_receiver;
                }
                continue;
            }
            Err(TryRecvError::Empty) => {}
        }

        let Ok(event) = browse_rx.recv_timeout(Duration::from_millis(250)) else {
            continue;
        };

        match event {
            ServiceEvent::ServiceResolved(info) => {
                if let Some(server) = discovered_server_from_mdns(&info) {
                    discovered_by_id.insert(server.id.clone(), server);
                }
            }
            ServiceEvent::ServiceRemoved(_, fullname) => {
                discovered_by_id.remove(&fullname);
            }
            _ => {}
        }

        let mut servers: Vec<DiscoveredServer> = discovered_by_id.values().cloned().collect();
        servers.sort_by(|a, b| a.name.cmp(&b.name).then(a.url.cmp(&b.url)));
        shared.set_discovered_servers(servers);

        let configured_server_url = shared.configured_server_url();
        if normalize_server_url(&configured_server_url).is_none() {
            if let Some(first_server) = shared.discovered_servers().first() {
                shared.request_server_switch(first_server.url.clone());
            }
        }
    }

    let _ = mdns.stop_browse(SENDSPIN_SERVER_SERVICE_TYPE);
    if let Ok(shutdown_rx) = mdns.shutdown() {
        let _ = shutdown_rx.recv_timeout(Duration::from_secs(1));
    }
    shared.set_discovered_servers(Vec::new());
}

fn discovered_server_from_mdns(info: &mdns_sd::ServiceInfo) -> Option<DiscoveredServer> {
    let fullname = info.get_fullname().to_string();
    let instance_suffix = format!(".{SENDSPIN_SERVER_SERVICE_TYPE}");
    let name = fullname
        .strip_suffix(&instance_suffix)
        .unwrap_or(fullname.as_str())
        .to_string();

    let mut addresses: Vec<_> = info.get_addresses().iter().copied().collect();
    addresses.sort_by_key(|addr| if addr.is_ipv4() { 0_u8 } else { 1_u8 });
    let host = addresses.first()?.to_string();
    let host_fmt = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };

    let mut path = info
        .get_property_val_str("path")
        .unwrap_or(DEFAULT_SERVER_PATH)
        .to_string();
    if path.is_empty() {
        path = DEFAULT_SERVER_PATH.to_string();
    } else if !path.starts_with('/') {
        path = format!("/{path}");
    }

    let raw_url = format!("ws://{host_fmt}:{}{}", info.get_port(), path);
    let url = normalize_server_url(&raw_url)?;

    Some(DiscoveredServer {
        id: fullname,
        name,
        url,
    })
}

fn load_or_create_client_id() -> String {
    let path = client_id_path();

    if let Ok(existing) = fs::read_to_string(&path) {
        let candidate = existing.trim();
        if Uuid::parse_str(candidate).is_ok() {
            return candidate.to_string();
        }
    }

    let new_id = Uuid::new_v4().to_string();

    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&path, &new_id);

    new_id
}

fn client_id_path() -> PathBuf {
    if let Some(mut config_dir) = dirs::config_dir() {
        config_dir.push("sendspin-vst3");
        config_dir.push("client_id");
        return config_dir;
    }

    PathBuf::from("sendspin-vst3-client-id")
}

fn unix_time_micros() -> i64 {
    let Ok(duration_since_epoch) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return 0;
    };

    duration_since_epoch.as_micros() as i64
}
