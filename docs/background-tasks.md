# Background tasks

Background execution, durable result storage, model delivery, and automatic continuation
have separate lifetimes. The runtime host owns execution; SQLite owns durable facts;
an Agent turn consumes typed observations at a safe boundary.

## Tool interface

The hosted Agent exposes `background: true` on `shell` and `browser_run`. The immediate
receipt contains `background_task_id` and `result_pending: true`; it is not evidence
that the operation succeeded. Very fast operations may already return `result_pending: false`
in that receipt; verification still replaces it with the actual operation result before
judging success or failure. `background_task` supports `list`, `read`, and `cancel`:

```json
{"action":"read","task_id":"<background_task_id>"}
```

Browser arguments otherwise remain backend-specific: Chrome uses OpenCLI actions,
and Ego uses its documented JavaScript SDK. Background execution does not switch the
browser or replay an operation when a transport fails.

The normal Agent host wires durable execution automatically. A standalone embedded
`ToolRuntime` must attach `BackgroundToolSupervisor` and `BackgroundToolObserver` to
enable managed background browser calls and the `background_task` control tool.
Only durably admitted operations publish handles through list/status and startup receipts;
internal admission work remains cancellable through its parent execution token.

## Delivery policy

- Short tool calls remain synchronous. Long operations explicitly opt into background execution.
- Partial results are stored silently. They do not create user messages or notifications.
- New user input takes precedence and receives pending results as tool observations.
- An active loop attaches results for its current execution to the next request
  it already needs. This attachment does not grant permission to wake an idle loop.
  Delivery uses pages of at most 16 observations, keeping the remainder in the
  existing inbox. Paused, cancelled and compacted-away observations retain their
  passive policy until a real input admits them.
- A finite batch is sealed before completion can request continuation. Once all members
  settle, undelivered results may continue the existing Agent loop once.
- While waiting, the loop performs no inference and retains its task, permission, budget,
  deadline, and cancellation state. A newer user turn supersedes the old continuation.
- Stop, pause, and browser handoff suppress automatic continuation. Waiting for a person
  is distinct from successful completion. Once consumed, a handoff observation replaces
  the old running receipt with `waiting_for_user`, without granting a successful terminal result.
- Cancelling one operation revokes its batch's continuation, including an already queued
  continuation, without suppressing newer batches. If the operation already succeeded,
  its true outcome remains successful.
- Admission to the in-memory inbox is not delivery acknowledgment. Results are acknowledged
  only after their model context snapshot is durable.

Synthetic observation carriers have empty assistant text; provenance and trust
remain typed tool data. A no-tool response is a candidate final while background
results and runtime verification remain unresolved. Background-associated response
text is held until that decision, while tool work remains visible. Independent
queued questions retain their separate answers; steering can supersede a candidate.
Provider findings remain verification evidence even when the public final must
report failure, so child partial findings are not replaced by runtime diagnostics.

## Ownership and recovery

Each operation carries workspace, session, parent task, originating turn, batch, and
tool-call identity. The store rejects identity reuse and duplicate terminal results.
Execution is bounded, and large result bodies remain in artifacts.

Process execution reuses `ProcessSupervisor`; browser execution retains its existing
backend ownership and operation journal. Browser scripts use conservative admission,
because arbitrary Node code may touch shared profile, clipboard, filesystem, or page state.

The host must hold the session execution lease before recovering abandoned work. Each
parent task additionally holds an independent background ownership lock while its workers
remain alive. Recovery must acquire that lock before declaring the task abandoned, so
another host cannot mistake work surviving a foreground turn for an orphan.
Uncertain operations become `outcome_unknown`, with automatic continuation suppressed.
Recovery does not replay external effects. Durable records do not keep execution alive
after the owning host exits.

The current SQLite upgrade accepts checksum-verified versions 7 and 8 and
upgrades them transactionally to version 9. Version 9 adds the child registry and
mailbox; historical SQL checksums remain unchanged. Unknown formats and altered checksums are
still rejected. Background audit events are quiet; they do not masquerade as another
provider tool call or change the active task when an older operation finishes late.
The terminal audit, evidence, artifact metadata, task status, and result inbox commit in
one database transaction. A process crash cannot leave a successful audit with a missing
inbox result. User-input admission refreshes the durable inbox even for an already-active
loop, including results completed by another host.

## Bounds and browser behavior

The supervisor executes up to 16 background tool operations concurrently and retains at most 64 operation
records. A batch has at most 64 members; a session has at most 128 pending durable results.
Capacity exhaustion rejects new admission before dispatch instead of dropping unconsumed
verification reports. Consumed reports and reports belonging to ended tasks release their
large in-memory bodies while durable observations and artifacts remain available.

Oversized inline results use the same fact priorities as foreground tool output. Exit status,
uncertain outcomes, browser backend selection and recovery controls take precedence over
page data and verbose diagnostics. The full original result remains in its artifact;
inline truncation is explicit. This does not make omitted output sufficient verification.

Browser operations are serialized within the host process, including synchronous calls.
The browser permit spans cancellation cleanup; waiting browsers cannot occupy all shell
execution slots. This is deliberately conservative, not a declaration of safe parallel
access to arbitrary tabs or profiles across separate hosts.

Live background screenshots use the same model image pipeline as synchronous screenshots.
Retained reports carry at most two images and 4 MiB of image data each, with a 128 MiB total
retention budget. If a report is evicted or the host restarts, durable image descriptors
remain, and context explicitly states that image bytes were not supplied. An artifact
reference alone is not represented as a visual observation.

## Validation coverage

Regression tests cover user-input/completion races, duplicate callback and call IDs,
late results after stop, per-batch cancellation, handoff, deadlines, context compaction,
model-route fallback, image delivery, migration rollback, cross-host ownership, atomic
audit/inbox rollback, and verification counts.

The hosted integration test runs a real shell process, checks its side effect occurs
once, continues the same task without model polling, and acknowledges delivery only after
the durable provider context. Opt-in `background_browser_live` tests use a local page to
verify early receipts, one navigation and one click effect, and PNG output. They retain
their manifest and artifacts; a separate finish invocation ends only the observed test task.

This mechanism is independent of post-task evaluation: evaluators do not own browser
operations or schedule ordinary Agent continuations.

Hosted subagent executions now register in this inbox and batch coordinator through
an external-execution adapter. They retain their own admission limit (default ten)
and do not occupy the sixteen tool execution permits while waiting. The shared
64-record retention bound also applies to external executions. Child-internal shell
or browser work belongs to the child's session and batch. Only the child's result
is delivered to its parent. The old `SubagentUpdated` event remains an audit/UI fact,
without duplicate model delivery or verification accounting for managed children.
See [subagent controls](subagents.md) and the
[integration validation](subagent-validation-2026-11-17.md).
The later [delivery refactor report](subagent-delivery-refactor-2026-11-17.md)
records passive attachment, findings coverage, final-answer publication boundaries
and live game measurements, including failed original visual acceptance.

See the [2026-11-16 validation record](background-tasks-validation-2026-11-16.md) for
observed browser outcomes, regression results, and current environment limitations.
