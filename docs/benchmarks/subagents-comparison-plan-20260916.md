# Subagent comparison and optimization plan

Compare Golutra with installed Codex 0.154.0, including its default multi-agent
interface and explicitly enabled `multi_agent_v2`. Local Codex reference source:
`fc269b66adc37f3c855df222ad80b02733355c46`. Source and installed binary are separate
version identifiers; a source capability is not proof of binary behavior.

## Frozen controls

- Same upstream credential, Responses endpoint, gpt-5.5 and medium reasoning;
  no child model downgrade. Set both concurrency limits to 10. Report native
  defaults separately. Isolate homes and workspaces without user/project rules.
- Explicit context modes and equivalent objectives; interface-specific parameter
  hints are permitted. Count parent and children, not only CLI parent usage.
- Preserve raw evidence privately, including failures, timeouts and retries.
  Missing provider usage, cache-write breakdown or request timing stays unknown.
- Report raw tool calls alongside useful effects, orchestration and validation.
  Necessary reads and checks remain allowed; fewer calls alone is not success.
- Alternate execution order in repeated samples. A small sample establishes
  functionality, not reliable P95 or general superiority.

## Scenarios and acceptance

| Scenario | Required evidence |
| --- | --- |
| Parallel reads and continuation | Two actual overlapping children; independent and fork contexts; sentinel read by each child; same independent child returns a new answer on follow-up; no replacements or recursive spawning |
| Disjoint-file coding | Two real children edit separate assigned modules; unchanged verifier passes boundary/type cases; parent integrates and verifies real workspace |
| Long-context fork | Child retrieves distant parent-only facts without copying them into its task; facts attributed to actual child output; context/usage counted |
| Cancel and recover | Real running child interrupted; terminal state observed; same identity continued; original failure/cancellation retained; no abandoned work |
| Ten-child fanout | Ten actual independent child executions; overlapping intervals within limit; ten distinct file findings returned by children and parent; no modified sources or replacement children |

Boundary regressions cover capacity and slot reuse, read-only permissions, genuine
tool failures, missing/replaced worktrees, reconnect ownership, execution-bound
notices, Unicode result pagination and incomplete usage. Existing deterministic
tests supplement live cases; do not present them as equivalent live measurements.

## Iteration

1. Build common evidence capture and test its evaluator against false positives.
2. Run initial matched samples, identify measured failures and overhead.
3. Implement only justified Golutra changes with focused regressions.
4. Repeat the same cases, preserving before/after evidence and all failures.
5. Publish quality, tokens, cached/uncached input, latency, concurrency and tool
   breakdowns with limitations. Expand sampling where ranking remains uncertain.

Universal superiority is not an acceptance claim that a finite fixture can prove.
Any remaining weakness must remain visible rather than changing the fixture,
lowering reasoning, suppressing verification or selecting only winning runs.
