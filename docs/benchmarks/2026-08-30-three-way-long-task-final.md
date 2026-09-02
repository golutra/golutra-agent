# Golutra / Pi / Codex Long-Task Benchmark

Generated: `2026-08-29T19:20:24.908407Z`

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
| Workspace verifier | 4/4 | 4/4 | 4/4 |
| Runtime terminal success | 4/4 | 4/4 | 4/4 |
| Strict pass | 4/4 | 4/4 | 4/4 |
| Process return codes | 0:4 | 0:4 | 0:4 |
| Provider total | 1,216,591 | 891,975 | 1,413,507 |
| Prompt input | 1,188,751 | 861,995 | 1,389,496 |
| Uncached input | 86,927 | 74,027 | 112,696 |
| Cache read | 1,101,824 | 787,968 | 1,276,800 |
| Cache write | 0 | 0 | 0 |
| Output | 27,840 | 29,980 | 24,011 |
| Reasoning output | unknown (partial 15,906) | unknown | 13,220 |
| Tool schema (estimated) | 71,502 | unknown | unknown |
| Tool result (estimated) | 440,320 | 13,281 | unknown |
| Cache hit ratio | 92.7% | 91.4% | 91.9% |
| Provider requests | 51 | 33 | unknown |
| Tool calls | 65 | 51 | 54 |
| End-to-end total | 665,803.0 ms | 599,543.3 ms | 520,775.6 ms |
| End-to-end P50 | 148,935.6 ms | 138,178.9 ms | 128,162.5 ms |
| First observable P50 | 5,448.1 ms | 4,707.4 ms | 8,741.5 ms |
| Provider TTFT P50 | 4,105.1 ms | unknown | unknown |

## Per Turn

| Stage | Scenario | Engine | Workspace verifier | Runtime terminal | Return code | Strict | E2E | Prompt | Uncached | Cache read | Hit | Output | Requests | Tools | Provider TTFT |
| ---: | --- | --- | --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | first_turn_cold | golutra | yes | yes | 0 | yes | 124,061.9 ms | 88,511 | 18,879 | 69,632 | 78.7% | 5,345 | 15 | 19 | 2,071.4 ms |
| 1 | first_turn_cold | pi | yes | yes | 0 | yes | 178,993.2 ms | 134,226 | 16,978 | 117,248 | 87.4% | 8,748 | 12 | 22 | unknown |
| 1 | first_turn_cold | codex | yes | yes | 0 | yes | 205,193.4 ms | 321,403 | 17,147 | 304,256 | 94.7% | 9,835 | unknown | 23 | unknown |
| 2 | same_session_tool_round | golutra | yes | yes | 0 | yes | 173,809.4 ms | 204,312 | 21,528 | 182,784 | 89.5% | 8,108 | 13 | 16 | 5,028.0 ms |
| 2 | same_session_tool_round | pi | yes | yes | 0 | yes | 245,689.3 ms | 158,850 | 17,538 | 141,312 | 89.0% | 12,961 | 7 | 11 | unknown |
| 2 | same_session_tool_round | codex | yes | yes | 0 | yes | 132,877.4 ms | 189,295 | 17,135 | 172,160 | 90.9% | 6,385 | unknown | 11 | unknown |
| 3 | same_thread_next_turn | golutra | yes | yes | 0 | yes | 284,335.5 ms | 383,103 | 31,359 | 351,744 | 91.8% | 11,893 | 10 | 17 | 5,381.9 ms |
| 3 | same_thread_next_turn | pi | yes | yes | 0 | yes | 97,364.6 ms | 229,206 | 27,478 | 201,728 | 88.0% | 4,823 | 6 | 9 | unknown |
| 3 | same_thread_next_turn | codex | yes | yes | 0 | yes | 123,447.7 ms | 489,364 | 26,388 | 462,976 | 94.6% | 5,773 | unknown | 13 | unknown |
| 4 | long_task_resume | golutra | yes | yes | 0 | yes | 83,596.2 ms | 512,825 | 15,161 | 497,664 | 97.0% | 2,494 | 13 | 13 | 4,105.1 ms |
| 4 | long_task_resume | pi | yes | yes | 0 | yes | 77,496.2 ms | 339,713 | 12,033 | 327,680 | 96.5% | 3,448 | 8 | 9 | unknown |
| 4 | long_task_resume | codex | yes | yes | 0 | yes | 59,257.1 ms | 389,434 | 52,026 | 337,408 | 86.6% | 2,018 | unknown | 7 | unknown |

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
| Measured result | First observable P50 5,448.1 ms; cache 92.7%; strict 4/4 | Provider total 891,975; E2E 599,543.3 ms | Provider total 1,413,507; E2E 520,775.6 ms | Measured winners: pi by provider tokens, codex by E2E, pi by tool calls. |

## Findings

### Advantages

- Golutra first observable P50 is 5,448.1 ms, versus Pi 4,707.4 ms and Codex 8,741.5 ms; the measured winner is pi at 4,707.4 ms.
- Golutra cache hit ratio is 92.7%, versus Pi 91.4% and Codex 91.9%; the measured cache-ratio winner is golutra at 92.7%.
- Golutra exposes provider-round timing, request counts, and detailed usage coverage that are unavailable from Codex's JSON output.

### Gaps

- Golutra provider total is 1,216,591 (+36.4% vs Pi), with 27,840 output tokens; extra tool/reasoning turns drive the excess.
- End-to-end total is 665,803.0 ms (+11.1% vs Pi; +27.8% vs Codex). Golutra makes 65 tool calls versus Pi 51 and Codex 54.
- Golutra strict status is 4/4; all measured stages satisfied the strict gate.

### Improvement priorities

1. P0: keep strict read-only shell inspection on the no-snapshot, read-only path and batch adjacent independent reads; any ambiguous command must retain the fully observed fallback.
2. P1: reduce long-input provider first-response and P95 latency by preserving the stable prefix and measuring uncached input on controlled live runs; do not change reasoning settings.
3. P1: continue auditing repeated validation/tool rounds and make result projections complete enough for the next decision without adding tools or speculative retries.
4. P2: keep real PTY/CJK, background terminal-state, cross-platform build, and installation smoke tests in the release gate.
5. P2: retain provider capability/usage coverage labels (`reported`, `derived`, `estimated`, `unknown`) and do not compare cross-session cache hit rates as a normal-session metric.
