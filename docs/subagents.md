# Subagent execution and results

`subagent` keeps a single model-facing tool with explicit lifecycle actions. A new child defaults to independent context: its task text and workspace instructions. Explicit `context: "fork"` inherits a frozen parent request. Model and reasoning settings inherit unless overridden; a context fork requires the same provider and model. Children cannot delegate further.

| Action | Required input | Behavior |
| --- | --- | --- |
| `spawn` (default) | `task` | Create an independent child session; wait for completion by default. Files are shared unless `isolation: "worktree"` is selected. |
| `status` | `child_session_id` | Read the latest state and available findings, including after reconnect. |
| `wait` | `child_session_id` or `child_session_ids` | Wait up to `wait_ms` (default and maximum 60 seconds). Multiple targets default to `wait_mode: "any"`; `all` waits for every target. Expiration does not cancel or restart children. |
| `send_input` | `child_session_id`, `task` | Supplement an active child through the runtime's durable steering queue. Repeated input identities are deduplicated. A completed child rejects steering. |
| `resume` | `child_session_id`, `task` | Start another turn in the same child session with its history. Active or uncertain children cannot be restarted this way. |
| `cancel` | `child_session_id` | Request cancellation; use `wait` to observe termination. |

Use `run_in_background: true` on spawn/resume to obtain the child session and execution task handles after durable startup, without waiting for its answer. Admission failure returns a failure result instead of an unbacked handle. Background work remains supervised by the host and subject to explicit child cancellation and delegation limits. Interrupting a parent response alone does not cancel background children. Read the result before claiming completion. A finished child is archived from the ordinary thread list; resume reopens that same thread.

The `explore` agent type sets an explicit forbidden-workspace-change contract. Only file reads and strictly read-only direct shell commands are exposed. Scripts, pipelines, redirects, background processes and write tools are rejected at execution, including provider-generated parallel write batches. The existing workspace policy and sandbox remain active. Resuming an exploration child cannot silently remove its read-only restriction.

## Result contract

Execution, verification and findings are separate:

- `child_status` describes the runtime task; `completed` is true only for a successful completed task. `child_terminal` also includes failed, partial and interrupted terminal states. Batch `completed` requires every child to succeed, while batch `child_terminal` requires every child to have stopped.
- `child_execution_status` distinguishes a returned response from a stopped or running task.
- `child_verification_status`, `child_verification_issues` and `child_diagnostic` describe verification independently.
- `child_findings_available` indicates whether assistant output exists. Unverified findings are retained without changing a failure into a verified success.
- Results are paged by Unicode character, using `offset` and `limit`. When `child_result_has_more` is true, query `status` with `child_result_next_offset`. If the provider projection reports `model_visible_truncated`, repeat the same page with a smaller limit. Reading does not consume or delete the result.

The full original answer remains in child events. Synthetic runtime failure messages do not replace the answer returned to the parent. Tool-level errors retain findings and diagnostics where available. A partial task remains distinguishable from a provider failure, cancellation, runtime deadline, or an expired wait.

The TUI labels partial subagent results as verification incomplete, shows the unmet condition, and keeps child identifiers, verification details and workspace-scan uncertainty in the expandable details.

## Verification and ownership

Explicit task contracts govern effect verification. Open analysis tasks do not acquire an effect assertion because their text contains `modify`, a negation, or a quoted deployment example. Requested validation, observed validation failures, actual code changes and strict contracts keep their verification requirements.

The parent owns child creation, admission, cancellation and budget settlement. Only that parent can inspect or control its child handles. Concurrent retries share one operation; another turn uses the existing session and consumes a new admission within the parent budget. Usage settlement subtracts prior child usage when continuing a session. On reconnect, the host reads durable events and reconciles orphaned work before allowing continuation; it does not resubmit uncertain side effects.

Provider requests without matching usage records leave token/cost totals unknown, including a failed resumed request. An earlier turn's known usage cannot make that new request appear free. Admission settlement retains the conservative reservation fallback when usage is unavailable.

Output admission and provider accounting use separate counters. `spent_tokens`
retains observed total provider usage; `spent_output_tokens` settles the output
reservation pool. Long cached or uncached input therefore does not consume an
output allowance. Actual output exhaustion still blocks admission, and explicit
cost limits remain effective. Missing output usage uses conservative accounting;
legacy checkpoints without the output counter retain their previous conservative
total-based charge. Unsettled crash-recovery reservations still exhaust the
available admission cap until durable settlement replaces them.

Implementation boundaries: `delegation` owns child startup/cleanup, `delegation_control` owns lifecycle requests and result projection, `delegation_policy` owns limits, `delegation_context` owns explicit context inheritance, `delegation_notifications` owns durable completion delivery, and `RuntimeVerificationService` owns contract-aware verification.

## Explicit parent context

Use `context: "fork"` only when the child needs the parent's conversation. The host binds the last complete provider request artifact at launch. It validates session ownership, parent relationship, request identity, checksum, bounded size and complete tool-call/result pairs. It excludes the spawning response and its unfinished tool calls; it never inserts placeholder results. New parent turns cannot change the frozen binding.

The child uses its current system/project instructions and permitted tools, followed by inherited history and its own objective. Parent system/project prefix messages are excluded to avoid duplicated or stale instructions. A read-only exploration child stays read-only, and the parent tool surface cannot enable nested delegation. Ordinary token budgeting and model compaction apply to inherited history.

A short child-role instruction is part of the stable system prefix on spawn and resume. It makes inherited parent exchanges background context and limits execution to the assigned subtask; the last fork message explicitly labels that subtask. This prevents inherited orchestration requests from being mistaken for instructions to restart the parent's workflow. It does not suppress legitimate reads, verification, diagnostics or model findings.

Fork requires the parent's provider/model. For a different route, use independent context and provide the relevant task facts. Missing or invalid artifacts fail explicitly. Resume always uses the child's own history and rejects a new `context` selection. Recovery prefers the child's saved request; if that request is no longer reusable, it rebuilds the child's durable history instead of replaying the original parent snapshot.

## Background concurrency

The default limit is 10 active child operations per parent session, including startup and children left running by an earlier parent turn. Configure a positive `subagent_max_concurrent` in global `$GOLUTRA_AGENT_HOME/runtime.json` or project `.golutra-agent/runtime.json`; project settings take precedence. For example:

```json
{"subagent_max_concurrent": 10}
```

There is no cumulative child-count or execution-count limit. Finished children release their slots, and `started_children` is accounting history only. Existing elapsed-time, token-admission and explicitly configured cost budgets remain effective. Concurrency is an upper bound, not a guarantee that exhausted budgets admit new work. Recovery retains the configured limit; legacy snapshots without this field default to 10.

Send multiple independent `subagent` calls with `run_in_background: true` in one provider response. Startup handlers can run concurrently after the batch's checkpoints succeed. Child execution then runs independently. Resume calls for the same child cannot share a batch, and overlapping wait target sets form a scheduling barrier. `send_input`, cancellation and other lifecycle changes remain ordered.

Shared checkpoint objects are fully written and synced in temporary files before atomic, non-overwriting publication. Concurrent startup cannot observe a half-written checksum object. Existing objects must still match exactly; genuine corruption fails explicitly. Unpublished temporary objects are excluded from object collection.

```json
{"action":"wait","child_session_ids":["returned-child-id-1","returned-child-id-2"],"wait_mode":"any","wait_ms":60000}
```

Batch results include `child_results` and `child_pending_ids`. Failed, interrupted and Partial children are terminal for waiting purposes; terminal does not mean successful. Each result keeps its own session/task identity, findings and verification facts. Results are repeatable and never consumed by a wait; request `status` for a child to page its complete output.

Once a batch wait has observed an execution's terminal result, it retains that
result instead of querying the session's latest execution again. A concurrent
resume cannot replace the completed result with a newer running task. Pending
items are refreshed so simultaneous completions remain visible.

## Completion delivery and cancellation

Child completion is persisted as a `SubagentUpdated` event in the parent session. TUI cards update independently, and archived cards receive a single completion notice. The model receives a bounded runtime observation at a request boundary; results already returned by a completed tool call are not injected again. Notifications survive reconnect, and missing notices can be reconstructed from persisted launch/result records after orphan reconciliation. Completed or cancelled parent tasks are not automatically restarted; later input can consume the persisted history.

Waiters receive the actual result before notification finalization, while the host continues supervising the notification owner. Shutdown cannot overwrite an already published result with a cleanup error. Reconstructed notices bind the original execution task, so a later resume cannot supply findings for an earlier launch. Notification lookup advances an event cursor and caches only relevant facts, with limits of 32 sessions, 4,096 facts and 2 MiB per session; oversized histories rebuild from durable storage rather than dropping notices.

An execution releases its concurrency slot when its result is published after child cleanup, archiving and usage settlement. Pending notification persistence does not block an immediate resume. The notification owner stays supervised until it finishes; status selects the latest execution instead of preferring an older pending notification. A second genuinely active execution in the same child session remains prohibited.

Child supervisors query durable state on their own control events, completion signals, cancellation or deadlines. Unrelated sessions and ordinary token/tool output do not trigger repeated state reads; a missing completion receiver does not create a polling loop. Optional `context: null` and `isolation: null` are treated as omitted values, including on resume.

Cancelling a wait cancels only that wait. Interrupting the current parent response stops foreground children but leaves explicitly background children running. `subagent` action `cancel` stops the named child. Protocol `Abort` with `cancel_children: true` also cancels children of an active parent. Host shutdown cancels and reaps all owned work. Deadlines continue to apply to background work.

Observing a parent-requested cancellation is a successful control operation,
with `child_cancel_requested: true`, while the child's `cancelled`/`interrupted`
status, `completed: false`, findings and verification diagnostics remain intact.
The acknowledgement must match that execution's task ID, including after
reconnect. Unrequested interruption, an already cancelling operation, failure
or timeout does not become successful through this rule. A new child execution
explicitly identifies its latest assignment; earlier assignments remain history
and are not instructions to restart cancelled side effects.

## Optional worktree isolation

Use `isolation: "worktree"` on spawn for a separate Git checkout of the parent's committed `HEAD`. Uncommitted parent changes are excluded. Tools, project instructions, project verifier discovery and side-effect checkpoints use the child checkout. Runtime identity, storage and permissions still belong to the parent workspace; a worktree is not a security sandbox.

Worktrees are retained under the runtime workspace state directory for review and same-child resume. Results expose `child_workspace_path` and `child_isolation`. No changes are automatically merged, discarded or committed. Resume keeps the original checkout; a missing or replaced checkout fails explicitly instead of silently switching back to the parent directory. Review and integrate changes explicitly, then remove the known checkout with Git when it is no longer needed.

## Batch findings and comparison

Batch waits preserve the terminal execution they actually observed, even if a
concurrent caller resumes that child before the remaining children finish.
Model-facing batch results retain child/session identity, terminal state, short
findings, failure diagnostics and continuation cursors. Repeated governance and
accounting metadata stays in durable envelopes rather than occupying each child's
answer budget. Large findings still require explicit pagination; necessary status
queries, reads and verification remain available.

The [2026-09-16 comparison](benchmarks/subagents-20260916.md) records matched
Golutra/Codex lifecycle, long-fork, cancellation, coding and ten-child tests,
including failures and evaluator corrections. The reusable harness is
`scripts/compare_subagents.py`; run performance samples sequentially, with a new
private output directory, to avoid mixing resource contention into latency.

## Validation (2026-09-15)

The local reference review used Codex's shared agent control and multi-agent handlers, plus `claude-code-main`'s AgentTool/LocalAgentTask implementation. The latter is an unofficial source snapshot, not a guarantee about the current proprietary Claude Code release. Golutra keeps one lifecycle tool, durable events and strict execution outcomes; it does not copy synthetic tool-result placeholders or silently fall back from a missing worktree.

`scripts/smoke_subagents.py --output /tmp/golutra-agent-subagents.json` runs an opt-in real-provider functional check with isolated credentials/workspace: two background children, one explicit fork, bounded waits and a same-child resume. It verifies durable child execution records and returned sentinel facts, reports parent and whole-task usage separately, and retains owner-only CLI evidence beside the report. A single smoke does not establish performance superiority over Codex or Claude Code.

The real-provider acceptance passed two overlapping background children, explicit fork findings, same-child resume and three unique execution-bound completion notices. The final run took 30.02 seconds, with 7 tools and 34,349 provider tokens across parent and children. The [acceptance report](benchmarks/subagents-20260915.md) retains all five attempts, including the preceding failures and the fixes they motivated. These are functional smoke measurements, not a statistical performance comparison.

Deterministic coverage includes configurable default-ten admission and slot reuse, cross-turn limits, fork ownership/checksum/current child tool contracts, current execution result attribution, notification recovery/deduplication, failed-request usage remaining unknown, worktree retention and missing/replaced checkout errors. It also retains English/Chinese contract tests, strict validation, unverified findings, read-only admission, bounded non-destructive waits, Unicode pagination, cancellation and cross-parent rejection. The final checkpoint subset passed 20 tests, the client suite passed 412, and the scripts passed 90 (including 6 acceptance-evaluator regressions). TypeScript/Python SDK checks and real local PTY tests are part of the regression gates; Windows was not exercised on this host.

Final gates completed on 2026-09-16: 1,792 Rust unit/integration tests passed across staged runs, with 3 existing ignored entries retained; TypeScript SDK 9/9, Python SDK 14/14, scripts 90/90, all-targets workspace Clippy with `-D warnings`, formatting and diff whitespace checks passed. This includes 193 runtime tests, 172 tool unit tests plus 18 integrations, 359 TUI tests, 15 PTY acceptance tests and 7 driver tests. OAuth's 24 tests passed with proxy variables removed only from the test process after a proxy-bearing run timed out. The Cargo wrapper was stopped after all unit/integration groups passed because its empty rustdoc stage stalled in host loading; no executable documentation examples were found, and a successful whole-command exit is not claimed. See the acceptance report for the retained failures and exact gate accounting.
