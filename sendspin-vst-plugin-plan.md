# Sendspin VST Plugin — Detailed Implementation Plan

## Overview

A VST3 plugin that acts as a Sendspin `player@v1` client, running inside any DAW. It connects to a Sendspin server via WebSocket, receives timestamped audio chunks, and outputs them through the DAW's audio bus in sync with the server's clock. This enables a DAW to participate in a Sendspin multi-room audio setup as a synchronized speaker endpoint.

## Architecture

```
Sendspin Server
    │
    ├── WebSocket text messages (JSON)
    │     client/hello, server/hello, client/time, server/time,
    │     stream/start, stream/clear, stream/end,
    │     server/command, client/state, group/update,
    │     stream/request-format
    │
    ├── WebSocket binary messages
    │     Type 4: timestamped audio chunks
    │
    ▼
VST Plugin
    ├── WebSocket + Control Thread
    │     ├── Manages WebSocket connection lifecycle
    │     ├── Handles Sendspin handshake
    │     ├── Runs clock synchronization loop
    │     ├── Processes all JSON messages
    │     ├── Decodes audio frames (FLAC/Opus → PCM float, or passthrough for PCM)
    │     └── Writes decoded samples + local play timestamp into ring buffer
    │
    ├── Lock-free SPSC Ring Buffer
    │     └── Stores timestamped PCM chunks
    │
    └── Audio Thread (processBlock)
          ├── Reads chunks from ring buffer
          ├── Maps timestamps to sample positions in the output buffer
          ├── Applies pipeline latency compensation
          ├── Outputs silence on underrun
          └── Signals state changes back to WS thread via atomic flags
```

## Tech Stack

| Component | Choice | Rationale |
|---|---|---|
| Plugin framework | JUCE 8 | Cross-platform, handles VST3/AU/AAX, audio, GUI, and provides AbstractFifo |
| WebSocket client | IXWebSocket | Lightweight, C++, no external dependencies, works well from background threads |
| FLAC decoder | dr_flac (single header) | Zero-dependency, decode-only, trivial to integrate |
| Opus decoder | libopus | Standard Opus decoder, widely available |
| Clock filter | Custom Kalman filter | Per spec recommendation; start with moving median, upgrade later |
| Build system | CMake | JUCE's native CMake support |
| mDNS discovery | dns-sd / Avahi | Platform-native: dns-sd on macOS/Windows, Avahi on Linux |

## Phase 1: Plugin Scaffold

**Goal:** A VST3 plugin that loads in any DAW and outputs silence.

### Tasks

1. Set up a JUCE 8 CMake project targeting VST3 (and optionally AU on macOS)
2. Configure the plugin as a **synth/instrument** (no audio input, stereo audio output) so it appears as a source in the DAW's instrument slot. Alternatively, configure as an effect that replaces its input — this is a design decision based on how users expect to route it
3. Implement `prepareToPlay()` — store sample rate and buffer size
4. Implement `processBlock()` — fill output buffer with silence
5. Create a minimal plugin editor (GUI) with a placeholder UI
6. Test loading in at least two DAWs (e.g., Reaper, Ableton)
7. Persist a stable `client_id` (UUID) in the plugin's saved state so the server can recognize this client across sessions

### Key Decisions

- **Instrument vs. Effect:** Instrument is cleaner (the plugin is a sound source), but some DAWs make it easier to route effects. Consider supporting both configurations via JUCE's `isSynth` flag
- **Channel count:** Start with stereo. The Sendspin format supports arbitrary channel counts, but stereo covers the primary use case. Can be extended later


## Phase 2: WebSocket Connection & Sendspin Handshake

**Goal:** Connect to a Sendspin server and complete the hello handshake.

### Tasks

1. Integrate IXWebSocket into the project
2. Spawn a background thread (or use IXWebSocket's built-in event loop) that manages the WebSocket connection
3. Implement mDNS discovery for `_sendspin-server._tcp.local.` to find available servers. Also support manual URL entry as a fallback for environments where mDNS is unavailable
4. On connection, send `client/hello`:

```json
{
  "type": "client/hello",
  "payload": {
    "client_id": "<persistent-uuid>",
    "name": "DAW Player",
    "device_info": {
      "product_name": "Sendspin VST",
      "software_version": "0.1.0"
    },
    "version": 1,
    "supported_roles": ["player@v1"],
    "player@v1_support": {
      "supported_formats": [
        { "codec": "pcm", "channels": 2, "sample_rate": 48000, "bit_depth": 32 },
        { "codec": "flac", "channels": 2, "sample_rate": 48000, "bit_depth": 24 },
        { "codec": "pcm", "channels": 2, "sample_rate": 44100, "bit_depth": 32 },
        { "codec": "flac", "channels": 2, "sample_rate": 44100, "bit_depth": 24 },
        { "codec": "opus", "channels": 2, "sample_rate": 48000, "bit_depth": 16 }
      ],
      "buffer_capacity": 1048576,
      "supported_commands": ["volume", "mute"]
    }
  }
}
```

5. The `sample_rate` in the first (preferred) format should match the DAW's current sample rate, queried from `getSampleRate()`. List both 48kHz and 44.1kHz variants so the server has options
6. Parse `server/hello` response, store `server_id`, verify `active_roles` includes `player@v1`
7. Immediately send initial `client/state`:

```json
{
  "type": "client/state",
  "payload": {
    "player": {
      "state": "synchronized",
      "volume": 100,
      "muted": false
    }
  }
}
```

8. Implement `client/goodbye` for graceful disconnects (reason: `user_request` when user clicks disconnect, `restart` when plugin is unloaded)

### Format Strategy

Request **raw PCM 32-bit float** as the first preference. This eliminates all decoding — the WS thread just reinterprets bytes as float samples and writes them directly into the ring buffer. Zero CPU overhead, zero decode latency. The bandwidth cost (~384 KB/s for stereo 48kHz float32) is negligible on any LAN or localhost connection.

FLAC as a second preference gives lossless quality at roughly half the bandwidth, useful if the server is remote. Opus as a last resort for low-bandwidth scenarios.


## Phase 3: Clock Synchronization

**Goal:** Maintain an accurate estimate of the offset between the plugin's local monotonic clock and the server's clock.

### Tasks

1. Implement `client/time` → `server/time` exchange loop on the WS thread
2. Compute round-trip time and clock offset using the three timestamps:

```
client_received = local monotonic clock when server/time message arrives

round_trip = (client_received - client_transmitted) - (server_transmitted - server_received)
offset = ((server_received - client_transmitted) + (server_transmitted - client_received)) / 2
```

3. **Initial implementation:** Maintain a sliding window of the last N offset measurements (e.g., N=8). Use the median as the current offset estimate. This is robust against occasional network jitter spikes
4. **Later upgrade:** Implement a two-dimensional Kalman filter tracking both offset and drift, as recommended by the spec. Reference implementations are available at [time-filter](https://github.com/Sendspin-Protocol/time-filter) (C++) and [aiosendspin](https://github.com/Sendspin-Protocol/aiosendspin/blob/main/aiosendspin/client/time_sync.py) (Python)
5. Send `client/time` messages at a regular interval — start with every 2 seconds, reduce frequency once the filter converges
6. Store the offset in an atomic variable so the audio thread can read it without locking

### Clock Source

Use the system's monotonic clock (`std::chrono::steady_clock` or platform-specific equivalents) for all timing. This must be the same clock used for audio thread timing. The unit is microseconds, matching the Sendspin protocol.


## Phase 4: Audio Receiving & Ring Buffer

**Goal:** Receive audio chunks from the server, decode them, and store them in a lock-free buffer for the audio thread.

### Tasks

1. Register a binary message handler on the WebSocket
2. On receiving a binary message:
   - Check byte 0 is `4` (player audio message type)
   - If no active stream, reject the message
   - Extract bytes 1–8 as big-endian int64: this is the server timestamp in microseconds
   - Convert server timestamp to local play time: `local_play_time = server_timestamp - clock_offset`
   - Remaining bytes (9+) are the encoded audio frame
3. Decode the audio frame based on the active stream's codec:
   - **PCM:** Reinterpret bytes directly as samples (respecting bit depth and endianness). For 32-bit float PCM, this is a simple memcpy
   - **FLAC:** Decode with dr_flac into float samples
   - **Opus:** Decode with libopus into float samples
4. Write the decoded chunk into the ring buffer as a `TimestampedChunk`:

```cpp
struct TimestampedChunk {
    int64_t local_play_time_us;  // when first sample should be output
    int num_samples;
    float samples[];             // interleaved stereo
};
```

5. **Ring buffer design:** Use a lock-free SPSC (single-producer, single-consumer) queue. JUCE's `AbstractFifo` works for raw sample streams, but since we need timestamp metadata per chunk, a custom SPSC queue of `TimestampedChunk` entries is more appropriate. Alternatives: `boost::lockfree::spsc_queue`, or a simple ring buffer with atomic read/write indices

### Buffer Capacity

The `buffer_capacity` reported to the server (1 MB in the hello message) tells the server how far ahead it can send. At PCM 48kHz stereo float32, 1 MB holds about 1.4 seconds of audio. This determines the server's send-ahead window and the plugin's resilience to network jitter. Adjust this based on testing — larger buffer = more resilience, more latency before first playback.


## Phase 5: Audio Thread Playback

**Goal:** Output received audio at the correct time, synchronized to the server's clock.

### Tasks

1. Maintain an "audio clock" — a running estimate of the current real time corresponding to the start of each `processBlock()` call. Anchor this to the system monotonic clock:

```cpp
void processBlock(AudioBuffer<float>& buffer, MidiBuffer&) {
    auto now_us = getCurrentMonotonicTimeMicroseconds();
    auto block_duration_us = (buffer.getNumSamples() * 1'000'000LL) / sampleRate;
    // now_us corresponds to the time when sample 0 of this block will be output
    // (plus pipeline compensation, handled below)
}
```

2. Apply pipeline latency compensation — play audio early by the total known pipeline delay:

```cpp
auto playback_time_us = now_us + pipeline_compensation_us;
```

Where `pipeline_compensation_us` includes the DAW buffer latency plus a user-configurable offset (see Phase 10).

3. For each sample position in the output buffer, calculate its target time:

```cpp
for (int i = 0; i < numSamples; i++) {
    auto sample_time_us = playback_time_us + (i * 1'000'000LL) / sampleRate;
    // Find the corresponding sample from the ring buffer
}
```

4. Pull chunks from the ring buffer. For each chunk:
   - If `local_play_time` is in the past (before the current buffer window): **drop it** — it's too late
   - If `local_play_time` falls within the current buffer window: copy its samples to the correct offset in the output buffer
   - If `local_play_time` is in the future (after the current buffer window): leave it in the buffer for next callback

5. Fill any gaps with silence (zero samples)

6. On buffer underrun (ring buffer empty when we expected data, and stream is active):
   - Output silence
   - Set an atomic flag to tell the WS thread to send `client/state` with `state: 'error'`
   - Continue buffering; once enough data accumulates, resume and send `state: 'synchronized'`

### Clock Drift Compensation

Over long playback sessions, the server's audio clock and the DAW's audio clock will drift apart (they're driven by different crystal oscillators). Without compensation, this manifests as gradual buffer growth or depletion.

Monitor the ring buffer fill level over time. If it's consistently growing, playback is slightly too slow; if shrinking, too fast. Compensate by occasionally adding or removing a single sample using linear interpolation. This is subtle enough to be inaudible but keeps sync indefinitely.

Alternatively, track the discrepancy between expected and actual chunk arrival times and use micro-resampling (e.g., outputting 2049 samples over what should be 2048 sample slots, with linear interpolation).


## Phase 6: Stream Lifecycle

**Goal:** Handle all stream-related server messages correctly.

### `stream/start`

- Parse the `player` object to get codec, sample_rate, channels, bit_depth, and optional codec_header
- If this is a new stream, initialize the decoder and mark the stream as active
- If this updates an existing stream (format change), reconfigure the decoder without clearing the buffer
- If the server's sample rate differs from the DAW's, send a `stream/request-format` requesting the DAW's rate. If the server can't match, implement a resampler (libsamplerate/libsoxr) on the WS thread

### `stream/clear`

- Flush the entire ring buffer immediately
- The stream remains active — new chunks will arrive shortly (this is a seek operation)
- The audio thread should output silence until new chunks arrive

### `stream/end`

- Mark the stream as inactive
- Flush the ring buffer
- Audio thread outputs silence
- Send `client/state` with idle/synchronized state

### `server/command` (player)

- **volume:** Update the plugin's output volume. Apply in `processBlock()` as a gain multiplier. Send `client/state` back confirming the new volume
- **mute:** Toggle muting. When muted, output silence in `processBlock()`. Send `client/state` back confirming mute state

### `group/update`

- Track `playback_state`, `group_id`, `group_name`
- Update the GUI accordingly
- Store the `server_id` of the last server that had `playback_state: 'playing'` persistently (for multi-server priority logic)


## Phase 7: State Reporting

**Goal:** Keep the server informed of the plugin's state.

### Tasks

1. Send `client/state` with the full player state immediately after `server/hello`
2. Send delta updates whenever state changes:
   - `state` changes between `'synchronized'` and `'error'`
   - `volume` changes (from GUI slider, DAW automation, or server command)
   - `muted` changes
3. Communication from audio thread to WS thread uses atomic flags/values — never lock a mutex from the audio thread:

```cpp
std::atomic<bool> stateChanged{false};
std::atomic<bool> isError{false};
std::atomic<int> currentVolume{100};
std::atomic<bool> isMuted{false};
```

The WS thread polls these periodically (or on a timer) and sends `client/state` when changes are detected.


## Phase 8: DAW Sample Rate Changes

**Goal:** Handle the DAW changing sample rate mid-session.

### Tasks

1. JUCE calls `prepareToPlay()` again when the sample rate changes. Detect this and store the new rate
2. Send `stream/request-format` to the server requesting PCM at the new sample rate:

```json
{
  "type": "stream/request-format",
  "payload": {
    "player": {
      "codec": "pcm",
      "sample_rate": 44100,
      "bit_depth": 32
    }
  }
}
```

3. The server will respond with a new `stream/start`. Handle the transition gracefully — there may be a brief gap in audio during the switch


## Phase 9: Plugin GUI

**Goal:** A usable editor for connection management, status, and settings.

### Layout

```
┌─────────────────────────────────────────────┐
│  Sendspin VST                          v0.1 │
├─────────────────────────────────────────────┤
│                                             │
│  Server: [dropdown / manual URL        ] 🔄 │
│  Status: ● Connected — "Living Room"        │
│  Group:  Kitchen + Living Room (playing)    │
│                                             │
├─────────────────────────────────────────────┤
│                                             │
│  Format: PCM 48kHz / 32-bit / stereo        │
│  Buffer: [████████░░░░] 340ms               │
│  Sync:   ● Synchronized  (offset: -2.1ms)  │
│                                             │
├─────────────────────────────────────────────┤
│                                             │
│  Volume: [━━━━━━━━━●━━] 80                  │
│  Mute:   [ ]                                │
│                                             │
│  Pipeline offset: [___5___] ms              │
│  (compensate for DAC, amp, speaker delay)   │
│                                             │
└─────────────────────────────────────────────┘
```

### Components

- **Server selector:** Populated from mDNS discovery. Refresh button rescans. Manual URL entry option for non-mDNS environments
- **Connection status:** Connected/disconnected/connecting indicator with server name
- **Group info:** Current group name, member count, playback state
- **Stream info:** Current codec, sample rate, bit depth, channel count
- **Buffer level:** Visual meter showing ring buffer fill level in milliseconds. Helps users diagnose issues
- **Sync status:** Synchronized/error indicator with current clock offset
- **Volume slider:** Bidirectional — changes from slider send `client/state` to server; changes from `server/command` update the slider
- **Mute toggle:** Same bidirectional behavior
- **Pipeline offset:** User-configurable millisecond value for downstream latency compensation (DAC, amplifier, speaker distance)

### Implementation Notes

- All GUI ↔ audio thread communication via atomics
- All GUI ↔ WS thread communication via a thread-safe message queue or `juce::MessageManager::callAsync`
- The GUI must never block on network operations


## Phase 10: Pipeline Latency Compensation

**Goal:** Enable truly synchronized playback by accounting for all latency between the plugin's audio output and sound reaching the listener.

### Automatically Known Latencies

| Source | How to measure | Typical value |
|---|---|---|
| DAW buffer size | `samplesPerBlock / sampleRate` from `prepareToPlay()` | 1–10ms |
| Plugin processing latency | Zero (we're just copying from ring buffer) | 0ms |

### User-Configurable Offset

Everything downstream of the DAW is opaque to the plugin and must be configured by the user:

| Source | Typical range | Notes |
|---|---|---|
| OS audio buffer | 0–20ms | CoreAudio usually matches DAW buffer; WASAPI shared mode adds ~10-20ms |
| USB DAC latency | 1–5ms | Varies by device |
| AVR/amplifier processing | 0–50ms | Receivers with DSP (room correction, lip-sync delay) can add significant latency |
| Speaker distance | ~3ms per meter | Speed of sound ≈ 343 m/s |

### Implementation

```cpp
// In processBlock():
auto daw_buffer_latency_us = (samplesPerBlock * 1'000'000LL) / sampleRate;
auto user_offset_us = userPipelineOffsetMs.load() * 1000LL;
auto total_compensation_us = daw_buffer_latency_us + user_offset_us;

// When deciding which chunks to play:
auto target_time = server_timestamp - clock_offset - total_compensation_us;
```

The user adjusts the pipeline offset in the GUI until playback is perceived as in sync with other Sendspin clients in the room.

### Future: Auto-Calibration

A potential future feature: play a test tone at a known timestamp, have another Sendspin client with a microphone measure when the sound actually arrives, and compute the real-world offset automatically. This eliminates manual tuning but is complex to implement reliably.


## Phase 11: Resilience & Edge Cases

### Reconnection

- On WebSocket disconnect, attempt reconnection with exponential backoff (1s, 2s, 4s, ... capped at 30s)
- On reconnect, re-send `client/hello` and go through the full handshake
- Preserve `client_id` so the server recognizes this as the same client and restores group membership

### Plugin Scanning

- DAWs scan plugins on startup by instantiating them briefly. The plugin constructor must not block on network operations, and should not attempt to connect until the user explicitly triggers it (or until `prepareToPlay` is called in a real session)

### Multiple Instances

- Each plugin instance is an independent Sendspin client with its own `client_id`, WebSocket connection, and server
- This allows routing different Sendspin groups to different DAW channels

### DAW Transport

- The plugin operates independently of DAW play/stop/record. It's always "live" when connected, similar to a hardware input
- Audio is output regardless of whether the DAW transport is running. This is standard behavior for instrument plugins with live input

### Buffer Underrun Recovery

- On underrun, set state to `error`, output silence, continue accumulating chunks
- Resume playback once the buffer reaches a minimum threshold (e.g., 50ms worth of audio)
- Send `state: 'synchronized'` when resuming
- Do not attempt to "catch up" by playing faster — the server will be sending chunks with future timestamps, and the buffer will naturally refill

### Multi-Server Handling

- Since this is a client-initiated connection, multi-server behavior is implementation-defined per the spec
- Simple approach: user picks a server from the GUI, plugin connects to it. If user wants to switch, they pick a different one
- Persist `server_id` of the last server that was playing for potential future auto-reconnect logic


## Development Phases Summary

| Phase | Description | Depends On | Estimated Effort |
|---|---|---|---|
| 1 | Plugin scaffold, silence output | — | 1–2 days |
| 2 | WebSocket + Sendspin handshake | Phase 1 | 2–3 days |
| 3 | Clock synchronization | Phase 2 | 2–3 days |
| 4 | Audio receiving + ring buffer | Phase 2, 3 | 2–3 days |
| 5 | Audio thread playback | Phase 4 | 3–4 days |
| 6 | Stream lifecycle messages | Phase 4, 5 | 1–2 days |
| 7 | State reporting | Phase 5 | 1 day |
| 8 | Sample rate change handling | Phase 6 | 1 day |
| 9 | Plugin GUI | Phase 1–7 | 3–5 days |
| 10 | Pipeline latency compensation | Phase 5 | 1–2 days |
| 11 | Resilience & edge cases | All | 2–3 days |

**Total estimated effort: 3–5 weeks** for a production-quality plugin, assuming one developer familiar with JUCE and audio programming.


## Open Questions

1. **Instrument vs. Effect:** Should the plugin present as an instrument (cleaner conceptually) or an effect (easier routing in some DAWs)? Could offer both via build configuration
2. **Metadata display:** Should the plugin also request the `metadata@v1` role to show now-playing info in the GUI? This would be a nice feature but adds scope
3. **Controller role:** Should the plugin optionally support `controller@v1` so users can play/pause/skip from within their DAW? This would require additional GUI elements but is relatively straightforward
4. **Multi-channel:** Should we support more than stereo from the start, or defer to a later version?
5. **Plugin state persistence:** Beyond `client_id`, should the plugin save the last-connected server URL, volume, pipeline offset, etc. in the DAW project file?
