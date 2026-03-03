# Changelog

All notable changes to this project will be documented in this file.

## [0.2.0] - 2026-03-03

### Added
- Configurable client name in the plugin UI with apply-and-reconnect behavior.
- Automatic connection to the first discovered mDNS Sendspin server when no server URL is configured.

### Changed
- Removed implicit default server URL and `SENDSPIN_SERVER_URL` fallback.
- Server connections now come only from discovered servers or a manually entered URL in the UI.
- Persisted server URL is kept in sync with background auto-selection.
- Default client name is now `Sendspin VST`.

### Fixed
- Client name and custom URL text fields remain editable in host UIs.
