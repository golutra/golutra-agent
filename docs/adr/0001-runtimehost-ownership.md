# ADR 0001: Split RuntimeHost Storage and Execution Ownership

- Status: Accepted
- Date: 2026-08-08

## Decision

`RuntimeHost` remains the application facade, but its mutable state is divided into
`RuntimeHostStorageState` and `RuntimeHostExecutionState`.

Storage owns the SQLite store, repository handles, artifacts, thread records, and
governance stores. Execution owns the lane manager, sequence allocator, event bus,
task controls, process supervisor, and event write serialization.

## Rationale

These state groups have different lifetimes and failure modes. Storage must remain
reopenable and queryable during recovery; execution is process-local and must be
cancelled and supervised as one owner. Keeping them as explicit internal seams
reduces accidental coupling without making callers learn two public hosts.

## Consequences

- Application services use repositories rather than reaching into SQL details.
- Process-local handles are never treated as durable recovery facts.
- A future daemon or remote adapter can replace either internal adapter without
  changing the `RuntimeHost` interface.
- The host still owns the ordering lock and task supervision; splitting files must
  not split those invariants.
