# Changelog

All notable Carbon Studio plugin changes are documented here.

## [Unreleased]

### Changed

- Stop, Ctrl+C, project reloads, and **Capture Manifest** capture immediately through a Studio quick save: the plugin starts a `StudioTestService` test whose Start Server action writes the edit DataModel, and a capture test server ends itself if `serve` could not stop it. Stop commits only its own fresh quick save and never consults the Studio change generation for it; auto-recovery remains the background capture and the fallback when Studio cannot quick-save. Quick saves work on Wine hosts too. Plugin protocol 5.
- An explicit capture during a playtest ends the playtest first, then quick-saves: the plugin in the play server of a served place waits on `serve` and calls `StudioTestService:EndTest` when the edit Studio asks. A playtest that does not end within 15 seconds falls back to auto-recovery.
- Serve continuously captures Studio's auto-recovery saves; stop and Ctrl+C accept either the next auto-recovery or a manual save over the temporary served place, while `carbon capture <PROJECT> <PLACE>` imports a file without a serve session.
- Studio-owned reflection descriptors now come automatically from the exact installed Studio build; the bundled database only supplies Carbon-specific adapters and historical aliases.

## [0.1.0] - 2026-07-17

### Added

- Automatic connection to a managed `carbon serve` launch.
- One-way synchronization for the mappings frozen at serve startup.
- Explicit **Capture Manifest** operation with progress and cancellation.
- Hard restart faults when the project topology changes during a session.
- Explicit capture requests bound to the managed Studio session.

[unreleased]: https://github.com/Chrrxs/carbon-roblox/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Chrrxs/carbon-roblox/releases/tag/v0.1.0
