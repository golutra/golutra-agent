# Golutra / Pi / Codex Long-Task Benchmark

Generated: `2026-08-28T23:18:48.170735Z`

## Conditions

- Model/protocol/reasoning: `gpt-5.5` / Responses / `medium`
- Fixture digest: `f73ec850ad9bc3a1e1829a51b9330ee71692355f549a1647218ca4881f2a091c`
- Four turns: multi-file implementation, recovery repair, long-context checkpoint, background process plus resume.
- Each product used its native tool surface. Project instructions, skills, extensions, and prompt templates were disabled for the fixture.
- Measurement mode: `live_provider`; provider calls executed under isolated temporary homes.
- Transient benchmark work root was removed after report generation; the JSON/Markdown report retains the measured data.
- Credentials lived only in owner-only temporary homes and are absent from this report.

## Aggregate

| Metric | Golutra | Pi | Codex |
| --- | ---: | ---: | ---: |
| Workspace verifier | 3/4 | 4/4 | 4/4 |
| Runtime terminal success | 3/4 | 4/4 | 4/4 |
| Strict pass | 2/4 | 4/4 | 4/4 |
| Process return codes | 0:3, 1:1 | 0:4 | 0:4 |
| Provider total | 1,358,541 | 790,600 | 1,116,060 |
| Prompt input | 1,326,862 | 765,490 | 1,096,376 |
| Uncached input | 154,894 | 63,538 | 111,288 |
| Cache read | 1,171,968 | 701,952 | 985,088 |
| Cache write | 0 | 0 | 0 |
| Output | 31,679 | 25,110 | 19,684 |
| Reasoning output | unknown (partial 12,312) | unknown | 10,420 |
| Tool schema (estimated) | 33,192 | unknown | unknown |
| Tool result (estimated) | 186,069 | 10,511 | unknown |
| Cache hit ratio | 88.3% | 91.7% | 89.8% |
| Provider requests | 57 | 33 | unknown |
| Tool calls | 76 | 52 | 44 |
| End-to-end total | 704,931.1 ms | 508,219.7 ms | 442,227.4 ms |
| End-to-end P50 | 185,213.8 ms | 103,696.0 ms | 113,234.9 ms |
| First observable P50 | 5,023.7 ms | 4,243.2 ms | 8,752.2 ms |
| Provider TTFT P50 | 4,221.9 ms | unknown | unknown |

## Per Turn

| Stage | Scenario | Engine | Workspace verifier | Runtime terminal | Return code | Strict | E2E | Prompt | Uncached | Cache read | Hit | Output | Requests | Tools | Provider TTFT |
| ---: | --- | --- | --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | first_turn_cold | golutra | yes | no | 1 | no | 162,810.6 ms | 60,439 | 13,335 | 47,104 | 77.9% | 7,855 | 11 | 22 | 3,239.7 ms |
| 1 | first_turn_cold | pi | yes | yes | 0 | yes | 227,127.3 ms | 182,841 | 17,465 | 165,376 | 90.4% | 11,598 | 14 | 28 | unknown |
| 1 | first_turn_cold | codex | yes | yes | 0 | yes | 159,654.7 ms | 179,251 | 14,515 | 164,736 | 91.9% | 7,311 | unknown | 17 | unknown |
| 2 | same_session_tool_round | golutra | yes | yes | 0 | yes | 207,617.0 ms | 197,876 | 20,212 | 177,664 | 89.8% | 9,703 | 15 | 18 | 4,983.9 ms |
| 2 | same_session_tool_round | pi | yes | yes | 0 | yes | 104,586.4 ms | 86,842 | 14,138 | 72,704 | 83.7% | 5,232 | 5 | 7 | unknown |
| 2 | same_session_tool_round | codex | yes | yes | 0 | yes | 110,189.8 ms | 177,786 | 14,842 | 162,944 | 91.7% | 5,219 | unknown | 9 | unknown |
| 3 | same_thread_next_turn | golutra | yes | yes | 0 | yes | 216,145.8 ms | 520,843 | 78,475 | 442,368 | 84.9% | 9,881 | 16 | 22 | 4,221.9 ms |
| 3 | same_thread_next_turn | pi | yes | yes | 0 | yes | 102,805.7 ms | 235,937 | 22,433 | 213,504 | 90.5% | 4,921 | 7 | 9 | unknown |
| 3 | same_thread_next_turn | codex | yes | yes | 0 | yes | 116,279.9 ms | 358,238 | 69,982 | 288,256 | 80.5% | 5,336 | unknown | 10 | unknown |
| 4 | long_task_resume | golutra | no | yes | 0 | no | 118,357.7 ms | 547,704 | 42,872 | 504,832 | 92.2% | 4,240 | 15 | 14 | 5,551.5 ms |
| 4 | long_task_resume | pi | yes | yes | 0 | yes | 73,700.3 ms | 259,870 | 9,502 | 250,368 | 96.3% | 3,359 | 7 | 8 | unknown |
| 4 | long_task_resume | codex | yes | yes | 0 | yes | 56,103.0 ms | 381,101 | 11,949 | 369,152 | 96.9% | 1,818 | unknown | 8 | unknown |

## Measurement Notes

- Golutra and Pi expose provider-round events, so provider TTFT and request counts are measured from host-observed JSONL arrival times.
- Codex `exec --json` exposes a turn aggregate but not provider-round timing/counts. Its provider TTFT and request count remain `unknown`; first observable item is reported separately.
- Codex resume usage is cumulative. Per-turn values are derived by subtracting the previous cumulative turn total; its provider total is derived as input plus output.
- Token fields are provider reported unless a row above is explicitly described as derived. Tool schema/result values are local estimates and are not included in provider totals or cross-product rankings.
- `Workspace verifier` proves the fixture behavior; `Runtime terminal` proves a native terminal event; `Return code` is the wrapper process status; `Strict pass` requires all of these plus immutable inputs.
- This is one controlled sample per product, not a population-level latency claim. Network order rotates by stage to reduce, not eliminate, upstream timing bias.


## Capability comparison

| Dimension | Golutra | Pi | Codex | Practical implication |
| --- | --- | --- | --- | --- |
| Tool surface | Compact seven-tool runtime plus patch/background/subagent boundaries | Compact native coding-agent tools | Broader built-in execution and collaboration surface | Golutra keeps the prompt surface small; its next gain is fewer repeated calls, not more tools. |
| Long-task state | Durable runtime events, verification, token budget, parent-thread/cache scope | Session continuation and compaction centered on session history | Thread/resume model with strong continuation semantics | Golutra has the right primitives, but terminal verification must not turn successful work into a failed turn. |
| Cache/usage observability | Provider-round usage, coverage and local estimates are separately labeled | Provider usage and session affinity are visible; round timing is less exposed here | Turn aggregate usage; request/TTFT detail is limited in this interface | Keep Golutra's detailed diagnostics while preserving a stable provider-facing prefix. |
| Background execution | Event-driven `shell_session` lifecycle with PID cleanup checks | Native background/session behavior | Native command execution and resume | Use deterministic latches and outer deadlines; never infer lifecycle from a fixed sleep. |
| Measured result | First observable P50 5,023.7 ms; cache 88.3%; strict 2/4 | Provider total 790,600; E2E 508,219.7 ms | Provider total 1,116,060; E2E 442,227.4 ms | Measured winners: pi by provider tokens, codex by E2E, codex by tool calls. |

## Findings

### Advantages

- Golutra first observable P50 is 5,023.7 ms, versus Pi 4,243.2 ms and Codex 8,752.2 ms; the measured winner is pi at 4,243.2 ms.
- Golutra cache hit ratio is 88.3%, versus Pi 91.7% and Codex 89.8%; the measured cache-ratio winner is pi at 91.7%.
- Golutra exposes provider-round timing, request counts, and detailed usage coverage that are unavailable from Codex's JSON output.

### Gaps

- Golutra provider total is 1,358,541 (+71.8% vs Pi), with 31,679 output tokens; extra tool/reasoning turns drive the excess.
- End-to-end total is 704,931.1 ms (+38.7% vs Pi; +59.4% vs Codex). Golutra makes 76 tool calls versus Pi 52 and Codex 44.
- Golutra strict status is 2/4; failed-stage details are recorded below. A successful workspace verifier does not erase a failed runtime terminal or process status.

### Golutra failed stages

- stage 1 (first_turn_cold): runtime terminal was not successful, process return code 1
- stage 4 (long_task_resume): workspace verifier failed

### Improvement priorities

1. P0: treat a recoverable tool error as recoverable only after an equivalent successful retry, while preserving hard failures and the full evidence trail.
2. P0: make `shell_session` completion wait for the child process to be reaped and expose one authoritative terminal event; require the caller to use the returned process id.
3. P1: compact long-context projections before the next provider round and batch independent reads, reducing uncached input and repeated tool turns without changing reasoning settings.
4. P1: measure long-input P95, first-token latency, stream/redraw metrics, and live-provider error recovery in a separate controlled job.
5. P2: retain provider capability/usage coverage labels (`reported`, `derived`, `estimated`, `unknown`) and do not compare cross-session cache hit rates as a normal-session metric.
