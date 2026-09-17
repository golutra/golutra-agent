# Background subagent acceptance — 2026-09-15

The final local build passed the real-provider lifecycle smoke. This is a functional acceptance task, not a matched Codex/Claude performance benchmark or a latency distribution.

## Setup and assertions

- macOS arm64; debug CLI built from the current working tree.
- Base commit `1865e36f33bb40580ceca88ce3277785266f5b33` plus the uncommitted background/subagent refactor. Final CLI SHA-256: `77f6b89cc4f3fb121e2fb377bbf06945f4c96f32595fee1c58e3dc673e67c42b`. The preceding atomic-checkpoint build was `3b68c391515006c42184ffde1ec49e52219e23b844ec2714bee501a71160b23a`.
- `gpt-5.5`, Responses, `medium` reasoning, `https://api.golutra.cn`; identical task prompt and provider settings across the five attempts below.
- Isolated temporary workspace/home and existing provider credentials. Temporary credential copies were removed after each run.
- Two background read-only children: A has independent context; B explicitly forks the frozen parent context. B must return its file sentinel and the inherited parent marker without receiving that marker in its assigned task text.
- Wait for both, then resume A in the same child session and observe its new execution result. No workspace mutations, automatic repair or nested delegation.
- Strict checks cover actual returned findings, overlapping execution intervals, two sessions/three successful executions, exactly two spawn attempts and one resume, and three distinct completion notices tied to the corresponding execution IDs.

The atomic-checkpoint run passed all 13 checks present when it launched. Replaying its saved events through the tightened 14-check evaluator also passed. The final lifecycle build passed all 14 checks directly: the resumed execution itself returned A's earlier finding. An earlier answer cannot substitute for that execution's answer. Six evaluator regression tests reject missing resumed findings, hidden child failures, extra startup attempts, duplicate execution notices and sequential children reported as parallel.

## All attempts, including failures

Times are UTC. Tool calls and provider rounds include parent and children; token totals are provider-reported aggregates across every session.

| Completion time | Strict result | E2E | Tool calls | Provider rounds | Total tokens | Uncached input | Cache read | Cache write |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 14:59:28 | Fail: fork child did not return the right-file sentinel | 218.97 s | 12 | 15 | 59,810 | 22,496 | 33,408 | 0 |
| 15:04:34 | Fail: upstream 403 on resumed child | 37.38 s | 7 | 10 | unknown | unknown | unknown | unknown |
| 15:11:12 | Fail: checkpoint collision caused two extra spawn attempts | 59.23 s | 8 | 10 | 36,345 | 16,636 | 18,176 | 0 |
| 15:22:32 | Pass: all lifecycle and findings assertions | 28.06 s | 7 | 10 | 34,388 | 10,963 | 22,016 | 0 |
| 15:42:56 | Pass after execution/notification lifecycle separation | 30.02 s | 7 | 10 | 34,349 | 14,261 | 18,688 | 0 |

The final parent's own counters were 5 provider requests, 5 tool calls and 24,787 total tokens. Parent-only counters must not be presented as whole-task consumption. Two child file reads bring the all-session tool count to 7. The parent made two spawn calls, two waits and one resume. Final request usage coverage was 10/10; the 403 attempt had only 9/10 usage records and remains unknown instead of zero or a partial sum presented as complete.

## Fixes driven by actual failures

1. The first fork inherited the parent's orchestration request and drifted from its own assignment. A short delegated-role instruction now belongs to the child's stable system prefix, and the fork history ends with an explicitly labelled assigned subtask. Current child tools and permissions remain authoritative; necessary reads and verification stay allowed.
2. The 403 remained a failed child execution with its original provider diagnostic. No prompt/route changes were made to bypass it. Missing request usage now invalidates session totals, including resume deltas; budget settlement retains the conservative reservation fallback.
3. Concurrent startup wrote the same checkpoint checksum object directly to its final path. Another creator could observe partial bytes and report a false collision. Objects now use complete, synced temporary writes followed by atomic non-overwriting publication. Existing checksum mismatches still fail without overwriting evidence. The 16-thread regression and genuine-corruption regression both pass.
4. The full client regression subsequently caught immediate resume being rejected after a result was delivered but before its notification owner finished. Execution completion now releases admission independently of notification finalization. The owner stays supervised; result selection prefers the latest execution. A deterministic test holds the notification lock across result delivery and resume, and another keeps an old notification pending while verifying latest-result selection. The fifth live run verifies the resulting build.

The third attempt's model retried successfully, but that did not turn the attempt into a strict pass. The fourth and fifth required no startup retry. These samples show functional recovery after the fixes; differing provider timing prevents attributing the entire latency change to local code.

## Reproduction and evidence

```sh
cargo build --locked -p golutra-cli
NO_PROXY=127.0.0.1,localhost,::1 no_proxy=127.0.0.1,localhost,::1 \
  python3 scripts/smoke_subagents.py --output /tmp/golutra-subagents-new.json
python3 -m unittest discover -s scripts/tests -p 'test_smoke_subagents.py'
```

Original local report names are `/tmp/golutra-subagents-20260915{,-role,-final,-atomic,-lifecycle}.json`. Corresponding `.stdout.jsonl` and `.stderr.log` files are retained with owner-only access; the last four attempts also retain `.events.jsonl`. These temporary paths are local evidence, not repository fixtures. Raw traces are not committed. The first report used an earlier evaluator; its known failure is retained unchanged.

Concurrency defaults to 10 per parent session and is configurable with `subagent_max_concurrent`. The live smoke exercises two concurrent children; deterministic admission tests cover the configurable limit, exhaustion, slot release and cross-turn ownership. Worktree retention/missing-checkout errors, cancellation, shutdown, notification recovery and PTY behavior use deterministic/local integration tests. Windows execution and a sustained ten-child upstream load test were not performed in this run.

## Final regression gates (2026-09-16)

Rust unit/integration coverage totals 1,792 passes across staged runs: app-server 66, CLI 33, authentication 24, and the remaining workspace groups 1,669. The final client suite includes 412 passes and the runtime suite 193. Tools passed 172 unit plus 18 integration tests. TUI passed 359 unit, 15 real PTY and 7 driver tests. Three existing ignored entries remain: configurable app-server restart soak, the CLI external-verifier helper (invoked by another test), and a separately credentialed TUI live smoke. They are not counted as passes.

The initial workspace run had nine OAuth timeouts; isolated authentication subsequently passed 24/24 with all upper/lowercase proxy variables removed from that test process. The next client run exposed the notification/resume race (410 passed, one failed); after the lifecycle fix and deterministic regressions, the final client run passed 412/412. Original failures remain in local logs.

All unit/integration groups completed before the final Cargo wrapper was stopped with exit 143: rustdoc was stalled at host loading and the crate source scan found no executable documentation examples. The empty documentation stage is not reported as a successful command. Local logs: `/tmp/golutra-subagent-workspace-final.log`, `/tmp/golutra-subagent-workspace-remainder.log`, and `/tmp/golutra-subagent-workspace-verified.log`.

Final all-targets workspace Clippy completed with exit 0 and `-D warnings` (`/tmp/golutra-subagent-clippy-verified.log`). Formatting/diff checks passed. TypeScript SDK 9/9, Python SDK 14/14, scripts 90/90, open-source metadata checks and generated SDK/schema consistency checks passed. No hosted cross-platform CI or Windows machine run is claimed.
