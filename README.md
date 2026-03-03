# sendspin-vst

Sendspin VST3 plugin implemented in Rust.

## Compatibility

- Requires Music Assistant 2.8.0b15 or newer.

## UI Preview

![TouchDesigner plugin screenshot](assets/touchdesigner-screenshot.png)

## Current scope

- VST3 instrument plugin with stereo output
- Custom plugin editor (`egui`) for server selection and status
- Background Sendspin WebSocket client
- Automatic discovery of Sendspin servers
- Timestamped playback scheduling on the audio thread using an SPSC ring buffer
- Underrun-driven sync state reporting (`synchronized`/`error`)

## Configuration

Server URL can be configured in the plugin GUI:

- Pick a discovered server from mDNS
- Choose `Other...` and enter a custom URL
- Click `Refresh` to re-query mDNS
- Set a custom client name and click `Apply Name`

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
