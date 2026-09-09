# Changelog

All notable changes to Golutra Agent are recorded in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions
follow [Semantic Versioning](https://semver.org/) where applicable.

## [Unreleased]

### Added

- Open-source project governance, contribution, security, and support entry
  points.
- Apache-2.0 and NOTICE files in source and binary distributions.
- Repository metadata checks for public licensing and contribution surfaces.

### Changed

- Token usage events now require the canonical normalized schema. Legacy usage
  fields and partial records are rejected instead of being migrated or
  silently reused.

## [0.2.0] - 2026-09-09

### Added

- Provider cache capability gating with canonical uncached, cache-read, and
  cache-write usage breakdowns.
- Provider transport timing diagnostics for stream setup, first events, and
  terminal responses.

### Changed

- Long-context projection, compaction, and tool results now preserve required
  execution facts while reducing repeated provider input.
- Session, fork, and subagent requests keep stable cache prefixes and explicit
  provider affinity behavior.
- Native npm packaging covers the CLI and TUI across six supported targets.

### Fixed

- Managed process termination and checkpoint handling now close reaping and
  cancellation races without signalling reused process identifiers.

## [0.1.0] - 2026-08-14

This is the initial public development baseline. See the repository history
and [architecture documentation](docs/ARCHITECTURE.md) for the implementation
details and current compatibility boundaries.

[unreleased]: https://github.com/golutra/golutra-agent/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/golutra/golutra-agent/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/golutra/golutra-agent/releases/tag/v0.1.0
