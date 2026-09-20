# Continuous task execution

The subsequent [model-capability implementation](model-capability-implementation-2026-09-19.md)
simplifies the shell contract, adds bounded on-demand runtime observation,
expires validation after code changes, and separates ordinary delivery from full audit export.

This follow-up to [connection recovery](long-task-recovery.md) applies the source review of local Codex `fc269b66` to Golutra's existing loop. It does not add a daemon, goal scheduler, database format or slash command. Source changes are local and are not an npm release.

## Response completion contract

The provider response's finish reason participates in execution control:

| Reason | Runtime behavior |
| --- | --- |
| `stop` | Existing candidate verification, pending-input and child-result handling |
| `tool_calls` | Execute complete calls; reject a tool-call termination with no calls |
| `continue` | Continue the same task with retained history; complete calls may execute |
| `length` | Preserve text and request continuation; reject truncated tool calls |
| `content_filter`, `error`, `unknown` | Report a provider failure; do not dispatch calls or certify completion |

Responses `end_turn=false` becomes `continue`. `response.incomplete` uses `incomplete_details.reason`, distinguishing `max_output_tokens` from `content_filter` and unrecognized failures. The raw frame callback retains only bounded terminal/timing state, not a growing frame collection. Anthropic `pause_turn` also becomes `continue` through the shared adapter. Existing normalized max-token reasons use `length`.

### Shared implementation and protocol coverage

The boundary is `wire adapter -> llm::response_contract -> ProviderResponse -> runtime::ResponseControl`. Connection recovery, continuation budgets, cancellation, compaction and candidate verification stay in the shared runtime; no provider has its own task loop. The contract module owns finish normalization, successful tool-call termination and tool-argument integrity. Responses-specific terminal fields remain in its wire decoder.

| Configured protocol | Transport acceptance | Terminal semantics |
| --- | --- | --- |
| `openai-compatible` | Buffered JSON and SSE | `stop`, `length`, `content_filter`, explicit tools; missing reason never inferred from tools |
| `openai-responses` | Both provider entry points use SSE | Completed/continue/incomplete details; no unsupported buffered fallback |
| `anthropic` | Buffered JSON and SSE | `end_turn`, `stop_sequence`, `pause_turn`, `max_tokens`, `refusal` |
| `gemini` | Buffered JSON and SSE | `STOP`, `MAX_TOKENS`, safety/filter reasons, malformed/unexpected calls |
| `vertex-ai` | Buffered JSON and SSE, Gemini and Claude routes | Same contract for its selected wire protocol |
| `genai` | Chat Completions, Anthropic, Gemini and Responses routes | Auto-selected Responses delegates to the dedicated Responses provider, including transport capabilities, replay and terminal parsing |
| `mock` | In-process fixtures | Same runtime response control; no network protocol |

Only normal `stop`/`continue` with complete calls is normalized to `tool_calls`; a failure, truncation or unknown termination retains its meaning even when calls are present. This covers Gemini's `STOP` with function-call parts. A generic `incomplete` without its reason is not treated as a known token limit. Observed-but-uncaptured calls are rejected before dispatch, including a second unfinished Anthropic call after a complete first call. A normally completed response may carry malformed argument JSON or a non-object value: these values are preserved without repair, rejected by execution-time schema admission, and returned to the model as correctable tool feedback. Missing identities, oversized envelopes and unsuccessful response terminals still fail at the provider boundary.

The matrix tests use actual local HTTP/SSE endpoints and verify returned content, tools, finish reasons and usage. They cover all configured protocol families, not every vendor/backend offered by the third-party `genai` library, nor live cloud interoperability. Runtime contract tests separately exercise continuation and prevent unsafe tool dispatch. Adding another protocol requires its wire-to-contract tests; it must not add a parallel retry/completion state machine.

Protocol continuation creates another model request in the same task and turn. It retains completed tool results, original budgets, cancellation and context-compaction behavior. Output-limit continuation adds a small runtime message requesting the remaining work; explicit provider continuation reuses history without injecting another user instruction. Eight consecutive text-only continuations are allowed before reporting failure; complete tool responses reset this counter, steering does not. This bound is separate from the existing task time/tool/cost limits. Empty responses and repeated identical no-progress output remain subject to existing guards.

Auxiliary summaries are installed only after normal completion with no tool calls. Truncated or nonterminal summaries fall back to the existing local summary instead of recursively extending summary requests.

## Verification scenarios

- Actual AgentLoop text truncation, explicit continuation and normal completion: one candidate-completion boundary, retained earlier text and usage records for every response.
- Filtered, unknown, error and truncated-tool replies cannot write a real temporary file or produce a candidate completion.
- Changing nonterminal text remains bounded; an incomplete auxiliary summary is not installed.
- Twelve actual file reads, a result write, at least three model-summary compactions, a user steer and five connection failures: every subsequent primary request is checked for original/added requirements, completed observations and matching tool calls/results. During the outage an external actor changes a previously read file; a fresh read and subsequent summaries retain the new fact, and an independent `cmp` validates the final file. The fixture summary derives facts from supplied history; it does not manufacture a lost requirement.
- A real background shell first outlives a wait, finishes during network backoff, and is observed through the same process ID and authoritative PID. One launch and one output marker are required.
- HTTP/SSE fixtures cover terminal semantics through the actual adapter, including incomplete tool previews.

These fixtures validate runtime behavior with controlled providers. They do not measure a real model's ability to preserve all semantics or establish a coding-completion-rate advantage over Codex.

## Default completion correction

Implicit Open-mode contracts now permit up to eight verification correction rounds in both client payload normalization and the Rust harness. A failed candidate feeds the existing structured verification envelope into the same task, with original history, permissions and budgets. Successful plain conversation still takes one provider request. An explicit contract, including a zero-round limit, takes precedence. Strict and explicitly conversational contract defaults are unchanged.

The change does not turn on project-verifier discovery or infer mandatory file edits from user wording. It consumes available tool, schema, typed-contract and configured verifier evidence. An unavailable independent verifier is reported rather than retried blindly. Reliable semantic acceptance still requires meaningful tests or explicit deliverables; a generic success message cannot prove arbitrary user intent.

An execution-preflight `InvalidArguments` rejection carries a trusted local marker. A successful invocation of the same tool in a strictly later provider step resolves that admission error for derived execution checks. The original error report and model feedback remain intact. Success of a different tool or a parallel call in the same step cannot resolve it. Execution failures still need an equivalent successful retry; hard policy blocks, cancellation, timeout and unknown external effects cannot gain admission-error recovery. Independent delivery checks must pass even after argument syntax is corrected.

An actual paired run exposed a separate source of unnecessary correction: a failed exploratory Python assertion remained a permanent implicit delivery requirement even after changed diagnostics passed. In Open/BestEffort without explicit objective-validation requirements, a recoverable historical diagnostic is no longer made an additional blocking objective when a later provider step supplies successful validation, or the current independent verifier passes. The failed ToolExecution check and original evidence remain failed; they are not rewritten as a successful command. A later arbitrary read/write is insufficient. Failed test suites, current/explicit delivery checks, Strict mode, hard failures and explicitly unknown workspace effects keep their existing verification rules. Model-patch syntax is also checked before side-effect preparation, so a duplicate path named `checkpoint.py` cannot be misreported as checkpoint I/O failure.

Correction preserves the existing default limits: four hours per turn, 256 tool calls, eight consecutive failed calls, a cost limit when cost is available, and no-progress guards (16 correction steps or five active minutes, plus repeated-step detection). Network waiting is excluded from the correction inactivity clock but counts toward the overall deadline. This is bounded autonomous correction, not an unlimited scheduler; process-crash recovery continues to require reconciliation of uncertain effects.

Regression scenarios include malformed string/array/null arguments followed by a real write, two failed independent verifications followed by success, explicit zero corrections, an incorrect final file despite recovered syntax, repeated invalid requests, and single-request chat.

## Reproducible real-model comparison

`scripts/compare_continuous_tasks.py` reuses the existing four-stage jobledger fixture and independent verifier. Both products receive the same model, effort, endpoint, task text and unrestricted workspace permissions in isolated homes. No benchmark repair prompt is injected. Stage order alternates; configuration/credentials are temporary and normal user profiles are not changed. Task artifacts remain available for audit.

For Codex builds whose model catalog enables Responses Lite, conventional third-party gateways may not support its `additional_tools` input items. The optional `--codex-model-catalog` reads the local Codex catalog and sets only the selected model's `use_responses_lite=false` in a temporary catalog. The report explicitly records this transport compatibility override. Verify actual tool execution before interpreting performance; a session with unavailable tools is not a valid coding comparison.

Four sequential user stages with process resume are not equivalent to one autonomous multi-hour task. See the [dated comparison report](long-task-comparison-2026-09-19.md) for real outcomes and the separate recovery acceptance document for controlled outage evidence.

## Tool dispatch timing decision

Codex can start a complete tool item before the whole response ends. Golutra continues to use its complete-response execution boundary. Responses diagnostics now expose optional `tool_ready_to_terminal_ms`: the interval between the first explicitly completed function-call item and the terminal response frame, measured on the same stream clock. Providers that do not send a completed tool item leave it absent, rather than reporting zero.

A local streaming fixture introduces a 300 ms tail delay and verifies that the diagnostic captures it (302 ms in the initial run). This proves the measurement and a possible overlap window, not a real-provider performance gain. Dispatch was not moved earlier: that requires measured production benefit plus call-level durability, cancellation and interrupted-stream reconciliation. The new metric lets that decision be based on evidence while retaining the tested side-effect boundary.

## Request reuse decision

The existing provider instance already reuses its HTTP client and connection pool. Stable cache identity, capability-gated affinity headers, replayable Responses reasoning items and authentication-refresh request reuse remain in place and are covered by protocol tests. This change does not send `previous_response_id`, enable WebSockets, change server storage policy or assume a proxy supports stateful continuation. Those are distinct upstream contracts, not interchangeable with prompt caching.

No new transport is enabled without an explicitly supported provider capability and measured benefit. The generic stateless SSE path continues to send complete context after compaction and recovery. Cross-turn persistent Goals remain a separate feature, not a prerequisite for the single-turn loop changes implemented here.

## Validation scope

Runtime evidence is from macOS arm64 and controlled local providers. Windows/Linux runtime behavior and real cloud-provider long-task quality were not evaluated in this follow-up. See [acceptance results](long-task-recovery-acceptance.md) for commands, results and recorded failures.
