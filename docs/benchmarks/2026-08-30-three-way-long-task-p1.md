# Golutra / Pi / Codex Long-Task Benchmark

Generated: `2026-08-29T20:31:14.405095Z`

## Conditions

- Model/protocol/reasoning: `gpt-5.5` / Responses / `medium`
- Fixture digest: `be218d9c54fc1c843d7d93fef0d18b959447c6a777c652df7110991d76498c56`
- Four turns: multi-file implementation, recovery repair, long-context checkpoint, background process plus resume.
- Each product used its native tool surface. Project instructions, skills, extensions, and prompt templates were disabled for the fixture.
- Measurement mode: `live_provider`; provider calls executed under isolated temporary homes.
- Credentials lived only in owner-only temporary homes and are absent from this report.

## Aggregate

| Metric | Golutra | Pi | Codex |
| --- | ---: | ---: | ---: |
| Workspace verifier | 3/4 | 4/4 | 4/4 |
| Runtime terminal success | 4/4 | 4/4 | 4/4 |
| Strict pass | 3/4 | 4/4 | 4/4 |
| Process return codes | 0:4 | 0:4 | 0:4 |
| Provider total | 1,254,574 | 994,864 | 1,206,556 |
| Prompt input | 1,227,631 | 967,285 | 1,181,563 |
| Uncached input | 117,615 | 88,181 | 69,371 |
| Cache read | 1,110,016 | 879,104 | 1,112,192 |
| Cache write | 0 | 0 | 0 |
| Output | 26,943 | 27,579 | 24,993 |
| Reasoning output | 15,331 | unknown | 15,277 |
| Tool schema (estimated) | 74,306 | unknown | unknown |
| Tool result (estimated) | 451,283 | 12,978 | unknown |
| Cache hit ratio | 90.4% | 90.9% | 94.1% |
| Provider requests | 53 | 36 | unknown |
| Tool calls | 68 | 51 | 47 |
| End-to-end total | 623,339.1 ms | 560,975.4 ms | 534,748.8 ms |
| End-to-end P50 | 157,026.5 ms | 152,540.4 ms | 149,097.2 ms |
| First observable P50 | 5,535.9 ms | 3,859.4 ms | 8,492.4 ms |
| Provider TTFT P50 | 5,109.9 ms | unknown | unknown |

## Per Turn

| Stage | Scenario | Engine | Workspace verifier | Runtime terminal | Return code | Strict | E2E | Prompt | Uncached | Cache read | Hit | Output | Requests | Tools | Provider TTFT |
| ---: | --- | --- | --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | first_turn_cold | golutra | yes | yes | 0 | yes | 219,856.5 ms | 165,477 | 17,509 | 147,968 | 89.4% | 10,352 | 18 | 26 | 5,109.9 ms |
| 1 | first_turn_cold | pi | yes | yes | 0 | yes | 174,470.7 ms | 143,123 | 14,611 | 128,512 | 89.8% | 8,540 | 13 | 21 | unknown |
| 1 | first_turn_cold | codex | yes | yes | 0 | yes | 185,502.9 ms | 254,792 | 15,176 | 239,616 | 94.0% | 8,973 | unknown | 20 | unknown |
| 2 | same_session_tool_round | golutra | yes | yes | 0 | yes | 189,143.2 ms | 241,974 | 30,518 | 211,456 | 87.4% | 8,602 | 14 | 17 | 4,994.5 ms |
| 2 | same_session_tool_round | pi | yes | yes | 0 | yes | 171,712.7 ms | 121,301 | 18,901 | 102,400 | 84.4% | 8,811 | 6 | 9 | unknown |
| 2 | same_session_tool_round | codex | yes | yes | 0 | yes | 175,008.6 ms | 220,903 | 16,615 | 204,288 | 92.5% | 8,670 | unknown | 11 | unknown |
| 3 | same_thread_next_turn | golutra | yes | yes | 0 | yes | 124,909.8 ms | 287,144 | 55,720 | 231,424 | 80.6% | 5,370 | 8 | 10 | 9,317.9 ms |
| 3 | same_thread_next_turn | pi | yes | yes | 0 | yes | 133,368.1 ms | 276,878 | 41,358 | 235,520 | 85.1% | 6,711 | 7 | 11 | unknown |
| 3 | same_thread_next_turn | codex | yes | yes | 0 | yes | 123,185.9 ms | 317,653 | 26,197 | 291,456 | 91.8% | 5,831 | unknown | 10 | unknown |
| 4 | long_task_resume | golutra | no | yes | 0 | no | 89,429.6 ms | 533,036 | 13,868 | 519,168 | 97.4% | 2,619 | 13 | 15 | 7,846.4 ms |
| 4 | long_task_resume | pi | yes | yes | 0 | yes | 81,423.9 ms | 425,983 | 13,311 | 412,672 | 96.9% | 3,517 | 10 | 10 | unknown |
| 4 | long_task_resume | codex | yes | yes | 0 | yes | 51,051.4 ms | 388,215 | 11,383 | 376,832 | 97.1% | 1,519 | unknown | 6 | unknown |

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
| Measured result | First observable P50 5,535.9 ms; cache 90.4%; strict 3/4 | Provider total 994,864; E2E 560,975.4 ms | Provider total 1,206,556; E2E 534,748.8 ms | Measured winners: pi by provider tokens, codex by E2E, codex by tool calls. |

## Findings

### Advantages

- Golutra first observable P50 is 5,535.9 ms, versus Pi 3,859.4 ms and Codex 8,492.4 ms; the measured winner is pi at 3,859.4 ms.
- Golutra cache hit ratio is 90.4%, versus Pi 90.9% and Codex 94.1%; the measured cache-ratio winner is codex at 94.1%.
- Golutra exposes provider-round timing, request counts, and detailed usage coverage that are unavailable from Codex's JSON output.

### Gaps

- Golutra provider total is 1,254,574 (+26.1% vs Pi), with 26,943 output tokens; extra tool/reasoning turns drive the excess.
- End-to-end total is 623,339.1 ms (+11.1% vs Pi; +16.6% vs Codex). Golutra makes 68 tool calls versus Pi 51 and Codex 47.
- Golutra strict status is 3/4; failed-stage details are recorded below. A successful workspace verifier does not erase a failed runtime terminal or process status.

### Golutra failed stages

- stage 4 (long_task_resume): workspace verifier failed

### Improvement priorities

1. P0: keep strict read-only shell inspection on the no-snapshot, read-only path and batch adjacent independent reads; any ambiguous command must retain the fully observed fallback.
2. P1: reduce long-input provider first-response and P95 latency by preserving the stable prefix and measuring uncached input on controlled live runs; do not change reasoning settings.
3. P1: continue auditing repeated validation/tool rounds and make result projections complete enough for the next decision without adding tools or speculative retries.
4. P2: keep real PTY/CJK, background terminal-state, cross-platform build, and installation smoke tests in the release gate.
5. P2: retain provider capability/usage coverage labels (`reported`, `derived`, `estimated`, `unknown`) and do not compare cross-session cache hit rates as a normal-session metric.
