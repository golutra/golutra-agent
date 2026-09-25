# Long-task connection recovery

## Contract

A temporary connection failure keeps the current task and logical provider request alive. The user does not have to submit another prompt. This is waiting for the remote model, not offline inference. No daemon, new slash command, database migration, or separate user data directory is required.

Connection retries use a 5, 10, 20, 40, 60 second schedule with 90–100% request-specific jitter and a 60 second ceiling. Only typed connection failures enter this path. The task deadline and cancellation still apply. The default governor has no total wall-clock, tool-count or cost cutoff. Explicit budgets, cancellation and verification rules still apply. Waiting counts toward an explicitly configured deadline, but not toward an explicitly configured correction no-progress time limit. Reaching the deadline is not reported as successful completion.

HTTP 429 and transient stream/server failures share a bounded retry budget across streaming and buffered transports: two retries by default, for at most three attempts. The shared budget is the larger of the configured stream and request retry limits; the stream limit also caps streaming retries. Only confirmed stream transport failures can switch to buffered transport, and that switch consumes a retry. HTTP errors and errors carried inside successful SSE responses do not trigger a transport switch. Valid `Retry-After` advice is no longer shortened to 30 seconds. Cancellation and the task deadline can interrupt a long server-advised wait. Explicit client errors other than 429, invalid credentials, malformed responses and semantic parser failures do not enter endless retries. TLS certificate/configuration failures are excluded from the connection-wait classification. A proxy that converts an outage into an HTTP error uses the HTTP policy, not the connection policy.

Auxiliary semantic compaction has finite connection retries. If the model summary is unavailable, the already-built local summary remains available; a summary request does not indefinitely block the main task.

## Safe attempt boundaries

Provider deltas are previews. AgentLoop dispatches tools only after a complete provider response. A failed response's incomplete tool arguments never become executable calls.

Response completion also respects the normalized finish reason. Text cut off by an output limit or explicitly marked for continuation stays in the same task; unsuccessful terminal states do not enter completion verification. See [continuous task execution](long-task-execution.md) for protocol mappings, continuation behavior and the tool-dispatch timing evaluation.

Before retrying a response that emitted text or tool previews, the runtime emits a required, durable recovery boundary. The terminal seals the first preview with `[Response interrupted; retrying]`, suppresses further retry previews for that logical request, and displays the confirmed answer when it arrives. New logical requests stream normally. This also applies to transport/provider fallback and to history rebuilt by resume. Already printed terminal scrollback is retained; no mouse capture or history erasure is used.

Completed tool results remain in the request history. A provider retry does not restart the tool loop or rerun a completed write. After at least 60 seconds of retry waits, one small runtime reminder asks the model to recheck relevant mutable facts before further changes; it is added only if the estimated input budget permits, and is included in the returned actual request for accounting. Existing read deduplication is cleared after a long wait so a necessary reread is not suppressed. This reminder is not a transactional guarantee against concurrent external changes.

An interrupted generation may have consumed upstream tokens even if no final usage was delivered. Recovery does not prove zero cost for that attempt, nor does a stable local request ID imply provider-side idempotency. Regenerated text may differ; completed local tool execution remains a separate boundary.

Process crashes remain a separate recovery problem. Started turns are not automatically replayed; unresolved side effects remain `Uncertain` and require reconciliation. This change does not claim global exactly-once execution or survival of child processes after host termination.

## State and observability

The task stays `Running`. The TUI updates one activity row in place:

```text
• Waiting for network (2m 13s waited • retry in 20s • esc to interrupt)
```

While a new connection attempt is in flight it displays `Reconnecting`; on stream progress/completion the normal activity display returns. Ordinary waits use `Waiting to retry`. Retry and transport-fallback notifications do not create repeated transcript cards. Fatal failures continue to use the existing error presentation.

`/status` includes the number of scheduled retries, accumulated retry-backoff time and the timestamp of the last observed provider/tool progress. Retry wait time is distinct from time spent inside a connection/request attempt. Developer retry counts count scheduled waits once, not both the wait and retry-start events.

The existing `RetryScheduled` runtime event carries an additive `recovery` payload:

| Field | Meaning |
| --- | --- |
| `provider_request_id` (outer payload) | Stable logical request identity |
| `phase` | `waiting` or `retrying` |
| `attempt` | Retry sequence within the current provider; zero denotes a fallback boundary |
| `network` | Whether this is the typed connection-wait path |
| `delay_ms`, `waited_ms` | Scheduled delay and completed backoff time |
| `reset_stream` | Seal the previous preview before accepting another attempt's deltas |
| `transport` | `streaming` or `buffered` |
| `reason`, `error_metadata` | Redacted error details and available HTTP/provider evidence |

空响应触发的同一 turn 续写也会在 `RetryScheduled.after_request_id` 中保存上一条完整 provider 请求身份。它与新用户 turn、provider fallback 或新的 task 分开，便于 resume/replay 区分“同一逻辑任务继续”与“重新开始”。

Clients consuming deltas must honor `reset_stream`; concatenating all deltas across attempts is not a valid final response. `ProviderCompleted`/`AssistantMessage` remain authoritative completed output. Raw recovery events remain queryable for diagnostics even though they are quiet in the transcript. These events are required observations so recorder backpressure cannot silently drop an attempt boundary.

Error metadata distinguishes the HTTP response status from the error status carried in an SSE payload. It retains the sanitized error type, message and request ID. Recovery and terminal failure metadata include an attempt chain with transport and elapsed time, retaining the first failure and the seven most recent failures. An execution failure's derived verification record does not replace its unresolved producer error as the primary diagnosis; genuine failed checks remain independently eligible.

Late process and diagnostic events retain their task ownership without changing the active session task. Task and turn indexes reduce their own status. Context resume supplements a request snapshot with confirmed messages and terminal facts recorded after that snapshot; tool, queued-input and compaction boundaries fall back to normal history reconstruction.

Environment context includes the local date and UTC offset without a per-second timestamp. Shell observations separate workspace sampling, launch, process and invocation time. Plain `date` and a single `+format` argument use the read-only execution path; time-setting arguments and opaque commands still require workspace observation.

## Validation

Explicit `invalid_prompt` and policy rejection codes remain terminal even when their messages mention an upstream server. Stream diagnostics retain a bounded upstream response ID separately from the HTTP request ID; an SDK that does not expose HTTP response headers must not manufacture a request ID from a response ID.

Deterministic tests cover more than five hours of virtual offline time without a default deadline, explicit deadlines, cancellation, a 120-second `Retry-After`, hard errors, incomplete Chinese text/tool deltas, background progress and bounded auxiliary compaction. Runtime integration executes a real file write and verifies it occurs only once across recovery. HTTP fixtures cover connection refusal and a truncated SSE stream split across UTF-8 byte boundaries. TUI reducer and PTY tests cover waiting, recovery, status counters and resume in a new process.

```sh
REQUEST_METHOD=GET cargo test -p golutra-agent-llm -p golutra-agent-runtime -p golutra-agent-client -p golutra-agent-tui --all-targets --locked
cargo clippy -p golutra-agent-llm -p golutra-agent-runtime -p golutra-agent-client -p golutra-agent-tui --all-targets --locked -- -D warnings
```

`REQUEST_METHOD=GET` is a test-process-only setting used to suppress automatic proxy discovery for localhost fixtures in this macOS environment. It is not a production requirement and does not change system proxy settings. This setting alone does not establish the cause of every fixture timeout; transient failures and reruns are recorded in the acceptance report.

The separate real-clock TCP outage soak is opt-in:

```sh
REQUEST_METHOD=GET GOLUTRA_AGENT_OUTAGE_SOAK_SECONDS=7200 \
  cargo test -p golutra-agent-runtime --lib real_connection_outage_soak --locked -- --ignored --nocapture
```

This keeps the endpoint unavailable for two actual hours, restores a local HTTP/SSE server, and checks that the same logical request completes. It does not prove cloud-provider availability or multi-hour model task quality. Execution results and platform limits are recorded in the [acceptance report](long-task-recovery-acceptance.md).

## Design reference

Connection recovery separates typed connection failures from bounded stream failures. The runtime keeps tool execution behind the complete-response boundary, records every retry in the event journal, and preserves cancellation, budgets and recovery state across reconnects.
