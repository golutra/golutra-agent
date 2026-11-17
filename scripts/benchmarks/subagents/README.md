# Live subagent acceptance

This opt-in benchmark uses the configured provider and incurs real model usage.
It generates the same eight-module game in four isolated workspaces, in order
4/8/8/4 concurrent children. The runtime cap and scheduling prompt agree. It keeps
all attempts, model responses, durable state, failures and original generated games.

Build and run (the output directory must not already exist):

```sh
cargo build --locked -p golutra-agent-client --example live_subagent_benchmark
node scripts/benchmarks/subagents/run.mjs target/debug/examples/live_subagent_benchmark /tmp/my-subagent-benchmark PROVIDER_PROFILE MODEL
```

The runner freezes one binary, contract and verifier and records their SHA-256s.
It does not edit provider credentials or global settings. Each sample runs the
external logic verifier, records peak concurrency, per-agent input/output/cache
usage, provider time, resource waits, result delivery latency and process cleanup.
Missing usage remains explicit. Token totals are not a monetary cost estimate.

After timing, use `serve.mjs OUTPUT_DIRECTORY` for a loopback-only preview. Follow
the installed ego-browser skill, use one TaskSpace for the assessment and inspect
each page before invoking `checkGame(page, OUTPUT_DIRECTORY)` from `browser-qa.mjs`.
Register page error observation before navigation and read the generated screenshots.
The browser checks use real keys and clicks; `__orbitSnapshot` only observes state.
They also wait for natural game over and verify restart, without changing game state.
They are independent acceptance, not part of generation timing. Never fold later
manual repair results into the original samples. Stop the preview after acceptance.

Two samples per concurrency setting are exploratory. Report every sample, median
and range; do not infer stable P95, causal speedups, or optimal concurrency from it.
Game v2 adds explicit mute/Resume/accessibility and narrow-layout requirements, so
its end-to-end timings are not directly comparable with the earlier v1 game.

Run `node scripts/benchmarks/subagents/summarize.mjs OUTPUT_DIRECTORY` after the
attempts and browser checks to print a machine-readable summary. Missing browser
results stay null; they are never counted as passing. Keep screenshots and failed
attempts alongside the summary for review.

After inspecting desktop title, narrow title, narrow pause and game-over screenshots,
record each original sample's manual review in `ARM-visual.json`, for example:

```json
{"arm":"run-1-c4","reviewed":true,"passed":true,"method":"manual_screenshot_review","screenshots":[],"findings":[]}
```

List the actual screenshot paths and any defects. The summary reports browser
behavior and visual review separately; combined acceptance requires both plus the
logic checks. A later repaired copy belongs in a separate directory and never
changes the original sample's score or timing.
