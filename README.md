# sendspin-vst3

Experimental Sendspin `player@v1` VST3 plugin implemented in Rust.

## Compatibility

- Requires a Music Assistant nightly build newer than March 3, 2026.

## UI Preview

![TouchDesigner plugin screenshot](assets/touchdesigner-screenshot.png)

## Current scope

- VST3 instrument plugin with stereo output
- Custom plugin editor (`egui`) for server selection and status
- Background Sendspin WebSocket client (`client/hello`, `client/state`, clock sync via `sendspin-rs`)
- Handles `stream/start`, `stream/clear`, `stream/end`, `server/command` (`volume`, `mute`)
- mDNS discovery of Sendspin servers (`_sendspin-server._tcp.local.`)
- PCM chunk ingest (16-bit, 24-bit, and 32-bit integer LE)
- Negotiation preference set to PCM 24-bit stereo at the host sample rate
- Timestamped playback scheduling on the audio thread using an SPSC ring buffer
- Underrun-driven sync state reporting (`synchronized`/`error`)

## Configuration

Server URL can be configured in the plugin GUI:

- Pick a discovered server from mDNS
- Choose `Other...` and enter a custom URL
- Click `Refresh` to re-query mDNS
- Set a custom client name and click `Apply Name`

URL normalization rules:

- `http://...` becomes `ws://...`
- `https://...` becomes `wss://...`
- host-only values are treated as `ws://<host>/sendspin`

There is no implicit default server URL. The plugin starts disconnected until you:

- select a discovered server from the GUI, or
- enter a custom URL in the GUI.

If no server URL is configured yet, the plugin will auto-select the first discovered
Sendspin server and connect to it.

Default client name comes from:

- `SENDSPIN_CLIENT_NAME` (if set and valid)
- otherwise `Sendspin VST`

A persistent `client_id` UUID is stored under your user config directory:

- macOS: `~/Library/Application Support/sendspin-vst3/client_id`
- Linux: `~/.config/sendspin-vst3/client_id`
- Windows: `%APPDATA%\\sendspin-vst3\\client_id`

## Build

```bash
cd sendspin-vst3
cargo check
cargo build --release
```

To produce an actual `.vst3` bundle:

```bash
cd sendspin-vst3
cargo xtask bundle sendspin-vst3 --release
```

The bundle will be written to:

- `target/bundled/Sendspin VST3.vst3`

## Release Builds

- GitHub Actions builds both macOS and Windows `.vst3` bundles when a GitHub Release is published
- Both zip assets are attached to the GitHub release
- Plugin version metadata is set from the release tag (for example `0.2.1` -> plugin version `0.2.1`)
