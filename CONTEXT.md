# Golutra Runtime Context

## Scope

Golutra is a coding-agent runtime. The stable execution path is:

`SessionCommand -> RuntimeEvent -> RuntimeHost -> AgentHarness -> provider/tool loop -> verification -> projection`.

The TUI, CLI, SDK, and app-server are entry and projection adapters. They must not
invent a second task state machine or infer completion from rendered text.

## Ownership

- `golutra-client::RuntimeHostStorageState` owns canonical SQLite repositories,
  governance stores, artifacts, and thread/session facts.
- `golutra-client::RuntimeHostExecutionState` owns the lane manager, event bus,
  sequence allocation, task supervision, cancellation, and managed processes.
- `golutra-runtime::AgentHarness` is the provider/tool execution seam.
- `golutra-tools::ToolRuntime` is the policy, preparation, sandbox, and tool-result
  seam.
- `golutra-protocol` owns shared command/query/event and typed transport data.
- `golutra-tui` renders `UserProjection` and `DebugProjection`; transcript state,
  layout, history replay, and scroll anchoring are kept in `TranscriptState`.

## Invariants

1. A workspace/session has one active task owner. A second runtime process receives
   a busy result instead of taking the lease.
2. Durable events are canonical facts. Rollout files and UI history are rebuildable
   projections.
3. Provider streaming deltas may be coalesced, but the persisted event records an
   explicit coalescing summary. Required lifecycle and terminal facts are never
   silently dropped.
4. Delegated children are host-created, inherit capabilities and cancellation, and
  share a bounded in-memory admission budget. The token limit reserves aggregate
  provider output allowance before children start; observed `spent_tokens` remains
  usage accounting (including input/multiple turns) and is not silently clamped or
  presented as a hard total-token cancellation cap. Parent identity and admission
  metadata are persisted on the child task/thread. Limits fail closed.
5. Normal user output and developer observations are separate projections. Debug
   layout is a rendering concern and cannot change runtime facts.
6. A task terminal result is governed by structured verification and recovery facts,
   not by a provider's final sentence.

## Change Guidance

Prefer a deep module at an existing seam over adding fields to every caller. Keep
ephemeral coordination in the execution owner; persist only facts needed for
replay, recovery, audit, or user-visible state. Do not add benchmark task names,
fixture-specific branches, or evaluator-only behavior to runtime policy.

## Verification

The minimum local gates are:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
CARGO_BUILD_JOBS=1 cargo test --workspace --all-targets -- --test-threads=1
git diff --check
```

The full architecture specification remains in `docs/ARCHITECTURE.md`; decisions
that affect ownership or lifetime are recorded in `docs/adr/`.
