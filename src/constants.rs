pub(crate) const DEFAULT_SERVER_PATH: &str = "/sendspin";
pub(crate) const SENDSPIN_SERVER_SERVICE_TYPE: &str = "_sendspin-server._tcp.local.";
pub(crate) const DEFAULT_CLIENT_NAME: &str = "Sendspin VST";
pub(crate) const PRODUCT_NAME: &str = "Sendspin VST";
pub(crate) const EFFECTIVE_PLUGIN_VERSION: &str = match option_env!("SENDSPIN_BUILD_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};
pub(crate) const BUFFER_CAPACITY_BYTES: u32 = 1_048_576;
pub(crate) const CHUNK_QUEUE_CAPACITY: usize = 512;
pub(crate) const TIMING_JITTER_TOLERANCE_US: i64 = 1_000;
pub(crate) const RENDER_CLOCK_REANCHOR_THRESHOLD_US: i64 = 250_000;
pub(crate) const PREFERRED_PCM_BIT_DEPTH: u8 = 24;
pub(crate) const PIPELINE_OFFSET_MIN_MS: i32 = -100;
pub(crate) const PIPELINE_OFFSET_MAX_MS: i32 = 200;
pub(crate) const PIPELINE_OFFSET_STEP_MS: i32 = 1;
pub(crate) const UNDERRUN_ERROR_THRESHOLD_BLOCKS: u32 = 3;
pub(crate) const RECOVERY_SYNC_THRESHOLD_BLOCKS: u32 = 4;
