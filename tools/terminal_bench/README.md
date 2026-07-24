# Terminal-Bench Adapter

`golutra_tbench_adapter.py` runs each Terminal-Bench trial with:

```bash
golutra --cwd <container-workdir> exec \
  --run-dir /logs/golutra-runtime \
  --approval-mode auto -- "<task prompt>"
```

The adapter discovers the container image's working directory for each task,
so repositories nested below `/app` run with the correct workspace boundary.
Pass the optional `workspace_path` agent kwarg only when a harness needs an
explicit override.

Terminal-Bench mounts each trial's host logging directory at `/logs`. After a
trial exits, its `golutra-runtime/` directory contains isolated raw runtime
state (`state/runtime.sqlite`, artifacts, checkpoints, memory, evaluation and
evolution records), `observations/` with full owner-only event/conversation/
trace JSON, and `debug-export/`, the redacted portable analysis bundle.

Build or copy the architecture-specific Golutra binaries before running:

```bash
tb run \
  --agent-import-path tools.terminal_bench.golutra_tbench_adapter:GolutraAgent \
  --dataset terminal-bench-core==0.1.1 \
  --agent-kwarg proxy_url=http://host.docker.internal:7897 \
  --output-path /tmp/terminal-bench/runs
```

`proxy_url` is optional. When set, the adapter passes HTTP, HTTPS and ALL proxy
variables to Golutra and updates
the tmux server environment so the separate Terminal-Bench test session uses
the same proxy. For a proxy listening on the host loopback interface, use
`host.docker.internal` rather than `127.0.0.1`. The
`GOLUTRA_TBENCH_PROXY` host environment variable provides the same setting.

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
waits for both `results.json` and the runtime manifest, derives only evidence
files that actually exist, and invokes `<collector> --run-bundle ... eval ingest`.
It resolves that host collector from the explicit `collector_binary` agent
argument, `GOLUTRA_TBENCH_COLLECTOR`, then this repository's release or debug
`golutra-cli` binary. It does not select an unrelated `golutra` command from
the host `PATH`.
If the result file, runtime identity, trace, or collector is unavailable, it
keeps a `golutra-evaluation.pending.json` file with the reason and the original
record instead of dropping the structured observation. This makes a later
offline ingestion possible without rerunning the trial.
