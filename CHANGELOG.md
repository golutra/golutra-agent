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

- Breaking: Agent commands, crates, environment variables and data directories
  use `golutra-agent`, `golutra-agent-*`, `GOLUTRA_AGENT_*` and `.golutra-agent`.
  No old-name aliases or automatic data migration are installed. Native desktop
  launch contract v2 uses the same payload as npm; desktop-owned IPC names stay
  unchanged. See `docs/agent-namespace.md`.
- Child processes now inherit the host environment by default, with optional
  `GOLUTRA_AGENT_SHELL_ENVIRONMENT_POLICY` restrictions and a mandatory exclusion of
  Golutra Agent internal credentials. Host CLI scope tokens and third-party
  credentials are inherited normally; Linux sandbox launches no longer place
  inherited values in command arguments. MCP declarations cannot reintroduce
  the excluded internal credentials.
- Agent guidance distinguishes process exit status from CLI business results
  and continues bounded waits on the same background process until terminal,
  without treating external request IDs as process IDs or relaunching mutations.
- TUI pending prompts now appear above the composer and enter the transcript
  only when their turn starts, preserving reply order in live sessions and replay.
  Enter steers an active task; Tab queues a follow-up when no completion is open.
  Alt+Up edits the latest pending prompt from an empty composer; Alt+Q manages
  the full queue. Long previews are bounded without truncating submitted text.
- Current-turn supplements now pass queued follow-ups and enter the next model
  request together as separate messages; follow-ups still execute one at a time.
  Esc with pending supplements interrupts and resubmits them as one message.
  Other interrupted inputs return to the draft with attachments. Inputs rejected
  because the active turn ended wait together for a fresh turn; ambiguous transport
  errors are never automatically retried.
- Token usage events now require the canonical normalized schema. Legacy usage
  fields and partial records are rejected instead of being migrated or
  silently reused.

### Removed

- Temporarily removed the built-in `web_search` tool (provider alias
  `golutra_agent_web_search`) from both coding and full tool profiles, including its
  HTTP backend and host configuration wiring. `GOLUTRA_AGENT_WEB_SEARCH_ENDPOINT`
  and `GOLUTRA_AGENT_WEB_SEARCH_API_KEY` are no longer read. Legacy search-result
  projection and wire aliases remain compatible with stored conversations;
  generic plugin/MCP integration remains available.

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
