# Terminal-Bench Adapter

`golutra_tbench_adapter.py` runs each Terminal-Bench trial with:

```bash
golutra --cwd <container-workdir> exec \
  --run-dir /logs/golutra-runtime \
  [--allow-network when proxy_url is configured] \
  --yolo --approval-mode auto -- "<task prompt>"
```

The adapter discovers the container image's working directory for each task,
so repositories nested below `/app` run with the correct workspace boundary.
Pass the optional `workspace_path` agent kwarg only when a harness needs an
explicit override.

Each trial already runs inside a disposable Terminal-Bench container, so the
adapter uses `--yolo` to remove Golutra's nested workspace, sensitive-path,
shell and OS sandbox restrictions. Runtime timeouts, cancellation, tool schema
validation, bounded output, verification and observation recording remain
active. Network access is still requested separately only when `proxy_url` is
configured so proxy variables reach the child; the disposable trial container,
not yolo mode, remains the outer network/security boundary.

Terminal-Bench mounts each trial's host logging directory at `/logs`. After a
trial exits, its `golutra-runtime/` directory contains isolated raw runtime
state (`state/runtime.sqlite`, artifacts, checkpoints, memory, evaluation and
evolution records), `observations/` with full owner-only event/conversation/
trace JSON, and `debug-export/`, the redacted portable analysis bundle.
Immediately after the turn starts, Golutra also writes an atomic in-progress
checkpoint. It is a recoverable identity and event boundary, not a successful
terminal result; a normal completion replaces it with the final result/error
manifest. This preserves a usable bundle when the harness kills the agent
before the final export.

The result collector remains alive for one hour by default so agent and test
timeouts from long Terminal-Bench cases can still be attached to that bundle.
Override `result_collection_timeout_sec` only when the dataset has a longer
combined agent/test horizon or a deliberately shorter local feedback loop.

The blocking tmux command is also bounded. The adapter reads the trial's
`max_agent_timeout_sec`, global timeout override, and timeout multiplier from
the active Terminal-Bench run, then gives tmux a short grace period to interrupt
Golutra after the harness deadline. This keeps Terminal-Bench's synchronous
executor from waiting forever after its outer asyncio timeout. Use the
`agent_command_timeout_sec` agent kwarg only when run metadata is unavailable;
it defaults to 600 seconds as a bounded fallback.

Build or copy the architecture-specific Golutra agent binaries before running:

```bash
tb run \
  --agent-import-path tools.terminal_bench.golutra_tbench_adapter:GolutraAgent \
  --dataset terminal-bench-core==0.1.1 \
  --agent-kwarg proxy_url=http://host.docker.internal:7897 \
  --output-path /tmp/terminal-bench/runs
```

The `arm64_binary` and `amd64_binary` arguments are copied into the Linux
trial container and must be Linux ELF binaries for the matching architecture.
`collector_binary` is different: it is executed by the host after the trial
finishes, so it must be a host-native Golutra CLI (for example,
`target/release/golutra-cli` on macOS), not one of the Linux container
binaries. If no usable host collector is available, the adapter keeps the
evaluation in a pending file for later ingestion.

`proxy_url` is optional. When set, the adapter passes HTTP, HTTPS and ALL proxy
variables to Golutra, adds `--allow-network` to the embedded `exec` invocation,
and updates
the tmux server environment so the separate Terminal-Bench test session uses
the same proxy. For a proxy listening on the host loopback interface, use
`host.docker.internal` rather than `127.0.0.1`. The
`GOLUTRA_TBENCH_PROXY` host environment variable provides the same setting.

After the agent command exits, the adapter reads the retained trace's
`token_usage_recorded` events and returns the provider input/output totals to
Terminal-Bench. Harness failures such as `test_timeout` are also retained as a
failed external-evaluation assertion instead of being collapsed into a generic
unresolved result.

For failed evaluator assertions, the adapter scans only the tail of
`panes/post-test.txt` and attaches a bounded, deduplicated failure excerpt to
the assertion and terminal cause. Root-cause lines such as `fatal:`, `error:`,
`AssertionError`, and pytest diagnostics are preferred; ANSI/control sequences
and common credential assignments are removed. The excerpt is capped at 2 KiB,
while the complete test log remains available only through its artifact
reference.

The provider config and credentials copied into the container are not included
in the retained runtime directory. Raw runtime data can still contain prompts,
tool output and workspace-derived content, so store benchmark logs accordingly.

## Structured evaluation handoff

The adapter never imports or changes the upstream Terminal-Bench source. It
only adapts the trial lifecycle and consumes the retained Golutra bundle:

```text
<trial>/golutra-runtime/manifest.json
<trial>/golutra-runtime/observations/manifest.json
<trial>/golutra-runtime/observations/sessions/.../tasks/.../trace.json
<trial>/golutra-runtime/terminal-bench-evaluation.json
```

The preferred bundle directory is `<trial>/golutra-runtime`; older harness
layouts under `<trial>/sessions/golutra-runtime` remain readable. The collector
starts before the blocking agent command, then waits for both `results.json` and
the runtime manifest, derives only evidence files that actually exist, and
invokes `<collector> --run-bundle ... eval ingest`
with the trial directory as the explicit `--artifact-base`. The evaluation JSON
therefore stays in the run bundle while its relative evidence references remain
portable and resolve against the harness-owned trial output.
It resolves that host collector from the explicit `collector_binary` agent
argument or `GOLUTRA_TBENCH_COLLECTOR` first. Those settings are authoritative.
Without either override it considers only this repository's release/debug
`golutra-cli` binaries, selects the newest executable, and rejects candidates
older than the current Rust sources. It does not select an unrelated `golutra`
command from the host `PATH`.
If the result file, runtime identity, trace, or collector is unavailable, it
keeps a `golutra-evaluation.pending.json` file with the reason and the original
record instead of dropping the structured observation. This makes a later
offline ingestion possible without rerunning the trial.

When an ingested assertion failure is correctable, the adapter writes
`external-correction-1.json` with bounded evaluator feedback and an explicit
`exec resume` command. It does not execute that command after Terminal-Bench
has finalized the trial: doing so would mutate the workspace after the scored
state and would not rerun the upstream evaluator. Execute the recorded command
as a separate continuation, then run the task evaluator again and ingest its
new record.
