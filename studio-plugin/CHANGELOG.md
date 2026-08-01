# Changelog

All notable Carbon Studio plugin changes are documented here.

## [Unreleased]

### Changed

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
