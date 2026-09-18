# Changelog

All notable changes to Golutra Agent are recorded in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions
follow [Semantic Versioning](https://semver.org/) where applicable.

## [Unreleased]

## [0.3.1] - 2026-09-19

### Fixed

- Provider setup saves configuration and credentials locally without waiting for
  a network probe. The review page shows save failures and the Enter action clearly.
- Explicit provider activation replaces stale model and reasoning overrides in
  the current session and persisted runtime settings, including same-name profiles.
- Bare provider hosts receive protocol-specific API paths: `/v1` for OpenAI Chat
  Completions, Responses and Anthropic; `/v1beta` for Gemini. Recognized operation
  URLs are converted to base URLs without duplicating the operation suffix.
- Existing API versions, proxy prefixes and ChatGPT backend paths are retained.
  Configuration, setup preview and runtime requests use the same URL rules;
  ambiguous native-provider paths produce actionable configuration errors.
- Locally saved provider settings no longer emit a successful connectivity-check
  event when no connectivity check was performed.

## [0.3.0] - 2026-09-17

### Added

- `/login` as an alias for `/auth`, and `/logout` to remove the active provider
  configuration and local credential, then reopen setup while retaining history.
- Keyboard tool details through Ctrl+O, with searchable persisted terminal
  output and file diffs available after resume.
- Native desktop release inventories with fixed versions, SHA-256 checksums,
  npm/native payload parity checks and offline launch acceptance.
- Open-source project governance, contribution, security, and support entry
  points.
- Apache-2.0 and NOTICE files in source and binary distributions.
- Repository metadata checks for public licensing and contribution surfaces.

### Changed

- Background subagents use configurable concurrency (default 10), execution-bound
  lifecycle operations, batched waits and explicit usage accounting.
- Command cards show bounded output previews; file changes show aligned line
  numbers, addition/deletion counts and red/green rows without duplicate headers.
- Main chat and tool details retain native terminal selection; keyboard
  navigation replaces mouse capture for tool cards.
- Model settings persist across restarts; custom setup offers Responses
  explicitly, with lowercase reasoning levels through `max` and `ultra`.
- Breaking: existing database formats are not migrated. Use a fresh Agent home
  for obsolete schemas; only matching current formats can share configuration
  and history. Unsupported data is rejected without rewriting it.
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

[unreleased]: https://github.com/golutra/golutra-agent/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/golutra/golutra-agent/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/golutra/golutra-agent/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/golutra/golutra-agent/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/golutra/golutra-agent/releases/tag/v0.1.0
