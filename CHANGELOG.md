# Changelog

All notable changes to this project are documented in this file.

## 0.5 - 2026-03-03

### Refactor

- Reworked the plugin architecture into focused modules (`config`, `constants`, `state`, `mdns`, and `network`) to reduce coupling and make behavior easier to reason about.
- Consolidated connection, discovery, and worker lifecycle flow into clearer state transitions with more predictable startup and reconnection behavior.
- Tightened the audio/render pipeline and shared-state boundaries for more maintainable real-time code paths.
- Improved observability and validation coverage with stronger diagnostics and expanded automated test coverage, including smoke-test workflows.
