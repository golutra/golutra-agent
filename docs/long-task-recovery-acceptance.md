# Long-task recovery acceptance

Date: 2026-09-19. Host: macOS 27.0, arm64. Rust: 1.93.0. Changes are local source changes on top of `6b71a74b7720dc138500ed6641822fc80c5e1d09`; no npm or release publication is part of this acceptance.

See [the recovery contract](long-task-recovery.md) for behavior, limits and commands.

## Coverage and evidence

| Scenario | Evidence | Result |
| --- | --- | --- |
| Connection unavailable before first token | Real localhost TCP refusal, followed by a returning HTTP/SSE server | Same logical request completes without another user prompt |
| Several hours without connectivity | Tokio virtual-time fixture with 240 failures and concurrent background ticks | More than three virtual hours, same request/turn, one bounded recovery reminder |
| Deadline and cancellation | Virtual-time four-hour deadline and cancellation during backoff | Deadline reports failure; cancellation immediately stops retries |
| Interrupted output | Actual HTTP/SSE and Chinese UTF-8 chunks; reducer and observation-queue checks | Old preview is separated from the new attempt, including durable replay |
| Incomplete tool preview | Fault injection before a complete provider response | Partial tool arguments are never returned for execution |
| Recovery after a file write | AgentLoop with a real temporary file and five connection failures | Exactly one tool execution; completed result remains in model history |
| Background process | Actual shell job finishes during connection backoff | Recovery reads the original process result without restarting the job |
| Context compaction unavailable | Auxiliary provider always fails to connect | Bounded retries preserve the local summary and recent context |
| HTTP and configuration errors | 429 with 120-second Retry-After; 503; auth, parser and client errors | Server wait is respected; ordinary failures stay bounded; hard failures do not enter network waiting |
| Terminal and resume | Real PTY process, offline startup, truncated response, recovery, `/status`, exit and new process | Waiting row clears; completed response and interruption boundary survive resume |
| Two-hour real-clock outage | Separate opt-in TCP/SSE soak, approximately 05:17:48–07:18:44 UTC | Passed: endpoint unavailable for 7200 real seconds, 130 scheduled retries, same logical request completes at 7251 seconds |

## Regression runs

- Provider golden integration suite after updating the intended error classification: 24 passed.
- AgentLoop long-task integration group: 3 passed, including the actual background shell job.
- TUI after the error-wrapper correction: 440 passed, one opt-in live-provider test ignored. This includes all 27 real PTY cases and the new offline/partial-stream/resume scenario.
- Clippy for the four changed production crates and their targets: passed with `-D warnings`; TUI Clippy rerun after the final display correction also passed. Formatting and whitespace checks passed.
- Final full workspace run: passed with exit code 0 using `REQUEST_METHOD=GET cargo test --workspace --all-targets --locked --quiet -- --test-threads=1`. All 27 PTY cases also passed in this run. Explicitly ignored live/manual checks remain ignored; the real-clock recovery soak runs separately below.
- A subsequent narrow-status formatting review removed the previous backoff seconds from an already-started `Reconnecting` row. All three activity-formatting regression tests passed on that final small change; no retry or transport behavior changed.

The real-clock soak uses a debug test binary built before the final accounting/metadata/budget refinements. The TCP connection-wait path exercised by the soak did not change after it started. Final source is checked separately by the workspace regression; this is not a release-binary soak.

### Real-clock result

```text
real TCP outage recovered: elapsed=7251s retries=130 request=01a0b819-7cfb-7ff1-8c57-de5d9b9ec20a
test provider_session::recovery_tests::real_connection_outage_soak ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 205 filtered out; finished in 7255.76s
```

The endpoint was restored by the fixture after 7200 seconds. The next scheduled connection attempt returned the complete expected Chinese response, approximately 51 seconds later, without another prompt. The test asserts that the completed request retains the original request ID. The harness duration includes setup; the reported request duration starts after provider construction. Sampled process RSS stayed near 16 MiB during the wait, with no sustained CPU activity observed; these samples are not a production memory benchmark.

## Failures retained during development

- The first parallel workspace run reported nine OAuth fixture timeouts in unchanged authentication code. A single-test rerun and the full 24-test authentication group passed without production changes. The subsequent serialized workspace run also passed authentication. The timeout cause is unconfirmed; no timeout or assertion was relaxed to conceal it.
- The next workspace run exposed an old golden assertion expecting `Failed` for a semantic Responses SSE error. The new classification is intentionally `Malformed`, which prevents keyword-based retries of a permanent parser failure. The assertion was updated while retaining the original upstream-error detail check; all 24 golden tests then passed.
- The following workspace run passed the runtime/client/unit groups but exposed a real terminal presentation regression: the new `Malformed` wrapper prevented the existing display cleanup from extracting the upstream cause. The known-wrapper list was extended and the unit fixture now constructs the actual typed error. The existing PTY assertion requiring the full cause without parser-wrapper noise was retained unchanged.
- The first resume fixture used a raw thread UUID where the existing CLI expects an alias. The fixture now uses the same stable alias on both launches; the product resume contract was not changed.
- A PTY retry-count assertion initially assumed exactly one connection refusal. Cold initial rendering can span another refusal. The test now requires at least two scheduled retries (connection failure plus truncated stream), still requires the final response and durable interruption marker, and still serves exactly two actual HTTP responses.

## Limits

This verifies recovery mechanics using local controlled providers and isolated temporary data. It does not assert cloud-provider availability, real upstream billing, multi-hour coding quality or global exactly-once external effects. The two-hour soak simulates an unavailable destination endpoint, not OS-wide Wi-Fi or proxy changes. HTTP errors produced by a proxy follow the bounded HTTP policy.

Only macOS arm64 has runtime/PTY evidence here. Windows and Linux were not run. Process crashes, host sleep/reboot and unresolved side effects retain the existing recovery/reconciliation contract. No real user credentials or normal user configuration were used by the fixtures.

## Continuous-execution follow-up (2026-09-19)

The follow-up source review and implementation are documented in [continuous task execution](long-task-execution.md). The following checks exercise the existing AgentLoop and actual provider adapters:

- LLM unit group: 94 passed.
- HTTP/provider golden group: 28 passed, including Responses `end_turn=false`, max-output truncation, filtering, unknown incomplete reasons, truncated tool previews, and Anthropic `pause_turn`/`max_tokens`/`end_turn`. The live smoke remains opt-in and does not use ordinary credentials.
- Runtime group: 214 passed, one opt-in real-clock soak ignored. Four response-control cases were rerun after the final change that avoids an extra prompt on explicit continuation; all passed.
- The multi-compaction scenario completed 12 actual file reads with at least three summaries, one steer and a connection outage. Subsequent model requests retained original and added constraints and paired every retained tool call with its result.
- The background shell scenario observed a nonterminal wait, network recovery and terminal completion on the same process ID/PID, with exactly one launch and one output marker.
- A controlled SSE tail delayed by 300 ms produced `tool_ready_to_terminal_ms=302` in the initial run. This validates the diagnostic, not real-provider speedup. Tool execution remains behind the complete-response boundary; no speculative external effects or new WebSocket/stateful transport were enabled.
- Clippy for LLM/runtime/client/TUI and all targets passed with `-D warnings`; formatting and whitespace checks passed on final source.
- Final source-frozen workspace acceptance passed with exit code 0: `cargo test --workspace --all-targets --locked --quiet -- --test-threads=1`, under the isolated localhost proxy environment described below. All 27 real PTY tests passed (136.11 seconds); opt-in/manual tests remained skipped as designed. No production source changes were made after this run started.

The first golden run encountered proxy-generated 502 errors even for localhost. The isolated test command removes uppercase/lowercase HTTP, HTTPS and ALL proxy variables, sets `NO_PROXY=localhost,127.0.0.1,::1`, and keeps `REQUEST_METHOD=GET`. The whole golden group then passed. This is a test-process setting, not a change to the product or system proxy configuration.

Two fixture failures were retained during implementation: a helper MockProvider automatically ended after one tool result, so the multi-step fixture was replaced by explicit per-step calls without weakening its assertions; the underlying SDK dropped captured tools on an incomplete Responses stream, so the adapter now also checks streamed previews and rejects truncated tool arguments. A workspace run compiled before this latter correction later reported the old assertion; a separate updated 28-case golden run passed, and workspace acceptance was restarted after source freeze.

All evidence in this follow-up uses macOS arm64 and controlled local providers. It does not establish real-model multi-hour completion quality, cloud-provider latency/cache improvements, or superiority to Codex.

## All-protocol contract follow-up (2026-09-19)

The shared adapter response contract now covers finish normalization and tool integrity. `genai` auto-routing to Responses uses the dedicated Responses implementation rather than the generic SDK path. Chat Completions no longer infers success from tools when the finish reason is absent. Gemini/Vertex `STOP` with complete calls becomes `tool_calls`; unsuccessful terminations cannot be upgraded. Anthropic unfinished tool blocks and non-object arguments are rejected.

Final LLM acceptance on macOS arm64: `cargo test -p golutra-agent-llm --all-targets --locked --quiet -- --test-threads=1` under the same isolated proxy environment passed: **94 unit tests and 33 provider tests**. Five new matrix tests cover normal/truncated/filtered/unknown outcomes with text or tools, buffered and streamed responses, missing termination, interrupted streams, partial Anthropic calls, argument integrity, and direct/auto-routed Responses entry points. Vertex routes include both Gemini and Claude. Mock execution is exercised by the shared runtime tests.

This is controlled protocol-contract evidence. No real-provider credentials were used; the many additional SDK vendor routes, Windows/Linux runtimes and live upstream behavior have not all been exercised. The previous full-workspace/PTY run above predates this adapter follow-up; it is not presented as a fresh full-workspace run.

The four targeted runtime `response_control_tests` passed after the adapter changes. `cargo clippy -p golutra-agent-llm -p golutra-agent-runtime --all-targets --locked -- -D warnings`, `cargo fmt --all -- --check` and `git diff --check` also passed. No task-loop or TUI production changes were made in this follow-up.

## Correctable tool admission and live comparison (2026-09-19)

This phase supersedes the previous rejection of complete non-object tool arguments: completed successful responses preserve malformed JSON/non-object arguments for typed tool admission rejection and model feedback. Incomplete calls, failed terminal states and unknown effects remain blocked. Only `ToolError::InvalidArguments` receives the local `rejected_before_execution` marker; policy rejection and execution errors do not. Default implicit Open contracts allow eight correction rounds; explicit zero-round contracts and ordinary one-request conversations retain their behavior.

A real-model comparison exposed a separate failure: an old exploratory assertion kept creating a Diagnostic objective obligation after successful current validation, despite all artifacts passing. Open/BestEffort now requires later successful objective evidence before superseding this optional diagnostic obligation. Original failed ToolExecution checks remain unchanged. Strict contracts, explicit checks and failed recognized test suites retain their failure semantics. Patch syntax is validated before checkpoint preparation, without I/O, so malformed patches become correctable admission failures.

Final acceptance on frozen production source:

```sh
env -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY \
    -u http_proxy -u https_proxy -u all_proxy \
    NO_PROXY=localhost,127.0.0.1,::1 REQUEST_METHOD=GET \
    cargo test --workspace --all-targets --locked --quiet -- --test-threads=1
python3 -m unittest discover -s scripts/tests -p 'test_compare*.py'
cargo clippy -p golutra-agent-core -p golutra-agent-tools \
    -p golutra-agent-llm -p golutra-agent-runtime \
    -p golutra-agent-client -p golutra-agent-tui --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

- Workspace: exit 0, **1931 passed, 4 ignored, 54 groups**. Includes **27/27 PTY** cases (137.37 seconds). Log: `/tmp/golutra-long-task-workspace-final-20260919.log`.
- LLM: **94 unit + 34 real localhost HTTP/SSE** tests. Runtime correction module: **9** tests. Tools: **182** tests. These are included in the workspace total.
- Python comparison scripts: **64 passed**. Clippy: exit 0, log `/tmp/golutra-long-task-clippy-final-20260919.log`. Formatting and whitespace checks passed. No production edits after the final workspace run started; benchmark digest metadata and documentation were completed afterward.
- Compaction fixture: 12 actual reads, a write, independent `cmp`, at least three summaries, steer, five connection failures, and external modification during the outage. Later requests preserve prior observed facts and reread the changed file. This tests the context contract, not arbitrary live summary quality.

The final live comparison used `gpt-5.6-sol`, medium, the same gateway, isolated data, alternating execution order and zero benchmark repair prompts. Both products passed all four independent verifiers and runtime checks: Golutra **366.6125 seconds**, Codex **318.4625 seconds**. Golutra was **15.1% slower** in this sample. This is four user stages with process resume, not hours of autonomous coding. Codex source reference `fc269b66` and installed `codex-cli 0.154.0` are separate evidence. The temporary model catalog only disabled `use_responses_lite` for gateway compatibility.

Final tested Golutra CLI SHA-256: `0b32474f6dad731333a4055f0752597bf4909729604cc296be188a314f8a1524`. The [comparison report](long-task-comparison-2026-09-19.md) and [sanitized three-run data](benchmarks/2026-09-19-continuous-tasks.json) preserve version, fixture digest, stage timings, failures and limits. Final raw report: `/tmp/golutra-continuous-comparison-final-20260919.json`.

Retained failures and corrections:

- Initial live comparison was ineligible: Codex exposed no usable tools via Responses Lite at this gateway. Conventional Responses restored real tool use. It is not a Golutra 4:0 coding win.
- Second live comparison: Golutra artifacts 4/4, runtime 3/4; stage 3 timed out at 600 seconds. Live SQLite showed ongoing corrections, disproving the initial inference from stale exported traces that the provider was waiting for its first token. Final stage 3 completed in 71.8218 seconds after the diagnostic/patch fixes; this is not a controlled estimate of speedup.
- First workspace attempt: 26/27 PTY passed. The synthetic multi-file display fixture modified unknown `.txt` files and could not supply validation after the new default correction policy. Changing the display-only fixture to `.md` kept its exact two-request and diff/fullscreen assertions; final 27/27 passed. No production verification rule was weakened for this fixture.
- Development fixtures caught a mutex guard across an await, paused virtual time affecting a real `cmp` process, and a shell awaiting approval. The lock scope, clock boundary and isolated fixture permissions were corrected. `ToolStarted` marks an attempt, so no-side-effect assertions check files/results rather than assuming this event means execution.

Only macOS arm64 was run. Controlled protocol tests and the earlier two-hour TCP soak do not establish all cloud providers, other operating systems or multi-hour live coding superiority. No commit, push or publication was performed in this phase.
