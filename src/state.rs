use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::mpsc::Sender as StdSender;
use std::thread;

use parking_lot::{Mutex, RwLock};
use sendspin::protocol::messages::PlayerSyncState;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::default_client_name;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum SyncState {
    Synchronized = 0,
    Error = 1,
}

impl SyncState {
    pub(crate) fn from_u8(raw: u8) -> Self {
        if raw == Self::Error as u8 {
            Self::Error
        } else {
            Self::Synchronized
        }
    }

    pub(crate) fn as_protocol(self) -> PlayerSyncState {
        match self {
            Self::Synchronized => PlayerSyncState::Synchronized,
            Self::Error => PlayerSyncState::Error,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ConnectionState {
    Disconnected = 0,
    Connecting = 1,
    Connected = 2,
    Error = 3,
}

impl ConnectionState {
    pub(crate) fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Connecting,
            2 => Self::Connected,
            3 => Self::Error,
            _ => Self::Disconnected,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Disconnected => "Disconnected",
            Self::Connecting => "Connecting",
            Self::Connected => "Connected",
            Self::Error => "Error",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum GroupPlaybackState {
    Unknown = 0,
    Stopped = 1,
    Playing = 2,
}

impl GroupPlaybackState {
    pub(crate) fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Stopped,
            2 => Self::Playing,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DiscoveredServer {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) url: String,
}

#[derive(Debug)]
pub(crate) struct SharedState {
    pub(crate) host_sample_rate_hz: AtomicU32,
    pub(crate) host_block_size: AtomicU32,
    pub(crate) stream_active: AtomicBool,
    clear_requested: AtomicBool,
    pub(crate) remote_volume: AtomicU8,
    pub(crate) remote_muted: AtomicBool,
    sync_state: AtomicU8,
    connection_state: AtomicU8,
    group_playback_state: AtomicU8,
    state_dirty: AtomicBool,
    configured_server_url: RwLock<String>,
    configured_client_name: RwLock<String>,
    now_playing_artist: RwLock<String>,
    now_playing_title: RwLock<String>,
    discovered_servers: RwLock<Vec<DiscoveredServer>>,
    worker_command_tx: Mutex<Option<UnboundedSender<WorkerCommand>>>,
    mdns_command_tx: Mutex<Option<StdSender<MdnsCommand>>>,
}

impl SharedState {
    pub(crate) fn new() -> Self {
        Self {
            host_sample_rate_hz: AtomicU32::new(48_000),
            host_block_size: AtomicU32::new(512),
            stream_active: AtomicBool::new(false),
            clear_requested: AtomicBool::new(false),
            remote_volume: AtomicU8::new(100),
            remote_muted: AtomicBool::new(false),
            sync_state: AtomicU8::new(SyncState::Synchronized as u8),
            connection_state: AtomicU8::new(ConnectionState::Disconnected as u8),
            group_playback_state: AtomicU8::new(GroupPlaybackState::Unknown as u8),
            state_dirty: AtomicBool::new(true),
            configured_server_url: RwLock::new(String::new()),
            configured_client_name: RwLock::new(default_client_name()),
            now_playing_artist: RwLock::new(String::new()),
            now_playing_title: RwLock::new(String::new()),
            discovered_servers: RwLock::new(Vec::new()),
            worker_command_tx: Mutex::new(None),
            mdns_command_tx: Mutex::new(None),
        }
    }

    pub(crate) fn set_connection_state(&self, state: ConnectionState) {
        self.connection_state.store(state as u8, Ordering::Relaxed);
    }

    pub(crate) fn set_stream_active(&self, active: bool) {
        self.stream_active.store(active, Ordering::Relaxed);
    }

    pub(crate) fn connection_state(&self) -> ConnectionState {
        ConnectionState::from_u8(self.connection_state.load(Ordering::Acquire))
    }

    pub(crate) fn set_group_playback_state(&self, state: GroupPlaybackState) {
        self.group_playback_state
            .store(state as u8, Ordering::Relaxed);
    }

    pub(crate) fn group_playback_state(&self) -> GroupPlaybackState {
        GroupPlaybackState::from_u8(self.group_playback_state.load(Ordering::Acquire))
    }

    pub(crate) fn request_clear(&self) {
        self.clear_requested.store(true, Ordering::Release);
    }

    pub(crate) fn take_clear_requested(&self) -> bool {
        self.clear_requested.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn set_sync_state(&self, state: SyncState) {
        let previous = self.sync_state.swap(state as u8, Ordering::AcqRel);
        if previous != state as u8 {
            self.mark_state_dirty();
        }
    }

    pub(crate) fn sync_state(&self) -> SyncState {
        SyncState::from_u8(self.sync_state.load(Ordering::Acquire))
    }

    pub(crate) fn mark_state_dirty(&self) {
        self.state_dirty.store(true, Ordering::Release);
    }

    pub(crate) fn take_state_dirty(&self) -> bool {
        self.state_dirty.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn set_configured_server_url(&self, url: String) {
        *self.configured_server_url.write() = url;
    }

    pub(crate) fn configured_server_url(&self) -> String {
        self.configured_server_url.read().clone()
    }

    pub(crate) fn set_configured_client_name(&self, name: String) {
        *self.configured_client_name.write() = name;
    }

    pub(crate) fn configured_client_name(&self) -> String {
        self.configured_client_name.read().clone()
    }

    pub(crate) fn set_now_playing(&self, artist: Option<&str>, title: Option<&str>) {
        let artist_value = artist
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or_default()
            .to_string();
        let title_value = title
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or_default()
            .to_string();

        *self.now_playing_artist.write() = artist_value;
        *self.now_playing_title.write() = title_value;
    }

    pub(crate) fn now_playing(&self) -> (String, String) {
        (
            self.now_playing_artist.read().clone(),
            self.now_playing_title.read().clone(),
        )
    }

    pub(crate) fn set_discovered_servers(&self, servers: Vec<DiscoveredServer>) {
        *self.discovered_servers.write() = servers;
    }

    pub(crate) fn discovered_servers(&self) -> Vec<DiscoveredServer> {
        self.discovered_servers.read().clone()
    }

    pub(crate) fn set_worker_command_tx(&self, tx: UnboundedSender<WorkerCommand>) {
        *self.worker_command_tx.lock() = Some(tx);
    }

    pub(crate) fn set_mdns_command_tx(&self, tx: StdSender<MdnsCommand>) {
        *self.mdns_command_tx.lock() = Some(tx);
    }

    pub(crate) fn request_server_switch(&self, url: String) {
        self.set_configured_server_url(url.clone());
        if let Some(tx) = self.worker_command_tx.lock().as_ref() {
            let _ = tx.send(WorkerCommand::SetServerUrl(url));
        }
    }

    pub(crate) fn request_client_name_switch(&self, client_name: String) {
        self.set_configured_client_name(client_name.clone());
        if let Some(tx) = self.worker_command_tx.lock().as_ref() {
            let _ = tx.send(WorkerCommand::SetClientName(client_name));
        }
    }

    pub(crate) fn request_controller_command(&self, command: String) {
        if let Some(tx) = self.worker_command_tx.lock().as_ref() {
            let _ = tx.send(WorkerCommand::SendControllerCommand(command));
        }
    }

    pub(crate) fn request_mdns_refresh(&self) {
        if let Some(tx) = self.mdns_command_tx.lock().as_ref() {
            let _ = tx.send(MdnsCommand::Refresh);
        }
    }
}

#[derive(Debug)]
pub(crate) struct TimestampedChunk {
    pub(crate) local_play_time_us: i64,
    pub(crate) sample_rate_hz: u32,
    pub(crate) channels: usize,
    pub(crate) frame_count: usize,
    pub(crate) samples: Vec<f32>,
}

#[derive(Debug, Clone)]
pub(crate) struct ActiveStream {
    pub(crate) codec: String,
    pub(crate) sample_rate_hz: u32,
    pub(crate) channels: u8,
    pub(crate) bit_depth: u8,
}

pub(crate) enum WorkerCommand {
    Shutdown,
    SetServerUrl(String),
    SetClientName(String),
    SendControllerCommand(String),
}

pub(crate) enum MdnsCommand {
    Shutdown,
    Refresh,
}

pub(crate) struct NetworkWorker {
    command_tx: UnboundedSender<WorkerCommand>,
    join_handle: Option<thread::JoinHandle<()>>,
}

impl NetworkWorker {
    pub(crate) fn new(
        command_tx: UnboundedSender<WorkerCommand>,
        join_handle: thread::JoinHandle<()>,
    ) -> Self {
        Self {
            command_tx,
            join_handle: Some(join_handle),
        }
    }

    pub(crate) fn shutdown(&mut self) {
        let _ = self.command_tx.send(WorkerCommand::Shutdown);
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

pub(crate) struct MdnsWorker {
    command_tx: StdSender<MdnsCommand>,
    join_handle: Option<thread::JoinHandle<()>>,
}

impl MdnsWorker {
    pub(crate) fn new(
        command_tx: StdSender<MdnsCommand>,
        join_handle: thread::JoinHandle<()>,
    ) -> Self {
        Self {
            command_tx,
            join_handle: Some(join_handle),
        }
    }

    pub(crate) fn shutdown(&mut self) {
        let _ = self.command_tx.send(MdnsCommand::Shutdown);
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}
