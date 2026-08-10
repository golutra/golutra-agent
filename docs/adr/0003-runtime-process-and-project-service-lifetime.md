# ADR 0003: Distinguish Runtime Process Lifetime from Project-Service Lifetime

- Status: Accepted
- Date: 2026-08-08

## Decision

The runtime process owns a session lease, task supervisor, cancellation token, and
managed process registry. Project services expose workspace-scoped lifecycle
adapters and may be started or stopped independently. A managed process is not
implicitly a durable project service just because a tool launched it.

Long-lived services must use an explicit project/system owner such as Docker
Compose, a user service manager, or another declared supervisor. The runtime may
report, poll, reconnect to, or terminate a process it launched, but releasing the
agent task does not promise that the service remains available.

## Rationale

Conflating these lifetimes makes `/resume`, crash recovery, and multi-process attach
ambiguous. It also turns a coding task into an accidental process manager. Explicit
ownership gives users a truthful contract and lets the project-service adapter
choose the correct platform implementation.

## Consequences

- Runtime cleanup is deterministic and bounded.
- Persistent service status is queried from its actual owner, not inferred from a
  stale runtime handle.
- Coding workflows can still start a service through a project adapter while
  keeping task and service records distinct.
