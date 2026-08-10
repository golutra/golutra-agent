# ADR 0002: Separate Live Signals from Durable Observations

- Status: Accepted
- Date: 2026-08-08

## Decision

Provider deltas and diagnostic progress enter a bounded, coalescing live recorder.
Lifecycle, tool, checkpoint, verification, and terminal observations are persisted
as canonical runtime events. When a provider stream is coalesced, the retained
`ProviderStreamed` event records the omitted event and byte counts explicitly.

## Rationale

Streaming every delta durably creates unbounded write pressure and makes ordinary
coding tasks depend on terminal rendering speed. Dropping a delta silently would
damage auditability, so coalescing is allowed only with a durable summary. The
recorder is an internal adapter; it does not decide user projection or completion.

## Consequences

- A live queue can be bounded without losing the fact that compression occurred.
- Replay and evaluation consume canonical events, not transient queue state.
- Required events are never coalesced with provider deltas.
- Consumers should tolerate older events that lack the newer nested summary and
  continue reading the compatibility flat fields when present.
