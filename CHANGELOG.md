# Changelog

All notable changes to Golutra Agent are recorded in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions
follow [Semantic Versioning](https://semver.org/) where applicable.

## [Unreleased]

## [0.3.7] - 2026-09-23

### Fixed

- Initialize the Windows console mouse-mode baseline during terminal setup,
  and restore cooked input after mouse cleanup when exiting or suspending the
  TUI so the parent shell and external editor receive the intended input mode.

### Changed

- Selecting Golutra API opens credential entry directly with the built-in
  `https://api.golutra.cn` address. Review and saving resolve the protocol path;
  Esc from either credential screen returns to provider selection.
- Preserve the complete development commit history on the canonical repository.
- Maintainer pushes to `main` skip CI jobs; tagged releases retain platform
  builds and acceptance before publishing.

### Distribution

- Align workspace crates, npm packages, native desktop archives and SDK metadata
  at 0.3.7. No user-data format changes are included.

## [0.3.6] - 2026-09-23

### Fixed

- Keep verification records, residual risks and internal result assessments in
  debug history instead of adding system cards to ordinary conversations.
- Display one accurate terminal failure per task, retain full diagnostics and
  avoid showing recovered provider errors as task failures.
- Prevent terminal background-color replies from appearing in the input field.
- Connect loopback provider, model catalog, OAuth and app-server endpoints
  directly while preserving proxy support for remote services.
- Distinguish missing validation evidence from a failed check, preserve evidence
  freshness after compaction and stop repeated correction cycles without progress.

### Changed

- Reuse transcript projections for streamed text and observational events, with
  full replay on tool, retry, final-response and terminal state changes.
- Keep tool previews compact and retain complete saved details through Ctrl+O.
- Improve long-task recovery, context compaction and prompt scope; add `/handoff`
  for carrying task context into a new conversation.
- Remove obsolete benchmark artifacts and refresh runtime documentation.

### Distribution and validation

- Align npm, native artifacts and SDK metadata at 0.3.6.
- Local auth, client, provider and TUI validation passed 1,121 tests, including
  35 real macOS PTY tests; one optional live-provider test was skipped.
- Formatting and Clippy checks passed. Release automation gates publication on
  six-platform packaging, offline startup, payload parity and shared-data checks.

## [0.3.5] - 2026-09-22

### Fixed

- Rebuild terminal history from source after resizing so narrow-to-wide changes
  restore the banner and conversation layout while retaining drafts and commands.
- Replace genuinely revised assistant output with the final rendering instead of
  appending duplicate replies. Trailing-newline normalization does not trigger a replay.
- Archive complete fenced-code lines while the provider is still streaming.

### Changed

- Coalesce small streaming updates and accelerate refreshes when updates accumulate;
  input, first content and completion retain responsive scheduling.
- Command previews retain the output head, tail and omitted-line count.
- Large pastes use compact previews while preserving full submitted text and undo.
- Code highlighting preserves language state across lines and incremental updates.
- Diff backgrounds adapt to light/dark terminals and supported color depth.
- Local Markdown links avoid duplicate file labels, shorten workspace paths and
  preserve line numbers. Explicit OSC 8 hyperlinks are not included.

### Distribution and validation

- Align npm, native artifacts and SDK metadata at 0.3.5 with no storage schema change.
- Local TUI validation passed 503 tests, including 34 real PTY tests; one opt-in
  live-provider test was skipped. Clippy and formatting checks passed.

## [0.3.4] - 2026-09-21

### Fixed

- Esc closes the root `/auth` and `/login` setup page without reopening it on
  every authentication-state refresh; nested pages still navigate back.
- Debug streaming no longer archives mutable event ranges or unfinished text,
  preventing false "Updated response" notices and duplicate answers.
- Measure the live viewport using the actual two-column layout, so short
  streaming replies are not cropped by full-width height calculations.

### Changed

- Make the Debug timeline denser: diagnostic-only events have no blank chat
  separators, and plain diagnostic text wraps at word and Unicode boundaries.
  Keep chat spacing, column gutters, literal text, event IDs and full details.
- Reload complete debug history asynchronously, with cancellation and merging
  of live events. Remove total history-load caps while retaining cursor guards
  and reclaiming only already archived events from the active window.
- Alt+D opens redacted event details with keyboard navigation, scrolling and
  copying. Preserve native mouse selection and the original chat draft.
- Deduplicate tool/job projections by identity, retain the last good snapshot
  on refresh errors, cache debug layout, and label archived facts as snapshots.

### Distribution

- Release the npm launcher, six platform packages and matching native desktop
  archives as 0.3.4 through the existing build, smoke-test and publish pipeline.

## [0.3.3] - 2026-09-21

### Changed

- Improve TUI input responsiveness and streaming stability with bounded event
  processing, incremental history updates and cached rendering work.
- Provider setup saves keys to local owner-only storage by default. Ctrl+E
  switches to an existing environment-variable reference; its name starts blank.
- Golutra API and Custom Provider model selection starts with a blank manual
  entry. Upstream models load asynchronously and appear below it, without
  moving the selection or overwriting typed or pasted input.
- Discover model catalogs using OpenAI Chat/Responses, Anthropic and Gemini
  authentication and response formats. Failed, empty or unsupported catalogs
  never block manual entry; leaving the page cancels pending discovery.

### Fixed

- Make advanced provider settings editable with the keyboard. Continue is the
  first option; select other fields to cycle values or edit text, then return
  to Continue for offline review and saving.
- Preserve native terminal text selection, drafts and streaming content while
  reducing repeated redraws and long-history rendering work.

### Validation

- Provider discovery and TUI coverage passed 475 tests, including local HTTP
  fixtures, delayed results, cancellation, keyboard navigation and real PTY
  regressions. One live-provider smoke test was intentionally skipped.
- Local catalog fixtures bypass host system proxies for deterministic tests;
  production retains system proxy support and bounded discovery timeouts.
- Six native release targets must pass packaging, npm/native payload parity,
  offline launch and shared-data checks before automatic publication.

## [0.3.2] - 2026-09-20

### Changed

- Long-running tasks no longer have implicit elapsed-time, tool-call, cost,
  correction-round or no-progress cutoffs. Explicit budgets, cancellation,
  permissions and protocol errors remain authoritative.
- Recoverable connection outages wait and resume the same logical request;
  background work can continue during recovery. HTTP, authentication and
  configuration failures retain their classified retry boundaries.
- Context compaction and resume preserve original requirements, user steering
  and complete tool-call/result groups. Recovery and task state are observable
  without injecting an additional supervising model.
- Tool contracts and correction feedback expose actionable execution facts,
  bounded error output and current validation evidence with less repeated work.

### Fixed

- Normalize provider completion, truncation and transport failure handling across
  adapters, so incomplete responses continue without claiming task completion.
- Avoid stale exploration obligations and redundant revalidation after verified
  source-derived cache changes; retain verification after actual source edits.
- Give direct and explicitly wrapped compound Shell commands consistent
  validation semantics, bind checks to their working directory and Shell context,
  and reject masked failures or background launch success as completed checks.
- Treat Delete File followed by Add File at the same exact path as one atomic
  replacement, retaining collision checks, batch validation and file permissions.
- Preserve environment expansion in Shell commands and useful head/tail evidence
  from long tool output; keep repeated-read results and execution timing observable.

### Validation

- Added real-process, provider-stream, outage-recovery, compaction, correction and
  patch regressions, plus reproducible cross-domain coding comparisons.
- Archived favorable and unfavorable samples. Short coding comparisons and a
  120-second connection-outage test do not establish universal or multi-hour
  superiority over other agents; see the dated research reports in `docs/`.

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

[unreleased]: https://github.com/golutra/golutra-agent/compare/v0.3.7...HEAD
[0.3.7]: https://github.com/golutra/golutra-agent/compare/v0.3.6...v0.3.7
[0.3.6]: https://github.com/golutra/golutra-agent/compare/v0.3.5...v0.3.6
[0.3.5]: https://github.com/golutra/golutra-agent/compare/v0.3.4...v0.3.5
[0.3.4]: https://github.com/golutra/golutra-agent/compare/v0.3.3...v0.3.4
[0.3.3]: https://github.com/golutra/golutra-agent/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/golutra/golutra-agent/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/golutra/golutra-agent/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/golutra/golutra-agent/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/golutra/golutra-agent/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/golutra/golutra-agent/releases/tag/v0.1.0
