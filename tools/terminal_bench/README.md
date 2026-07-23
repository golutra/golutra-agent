# Terminal-Bench Adapter

`golutra_tbench_adapter.py` runs each Terminal-Bench trial with:

```bash
golutra --cwd /app exec \
  --run-dir /logs/golutra-runtime \
  --approval-mode auto -- "<task prompt>"
```

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
  --output-path /tmp/terminal-bench/runs
```

The provider config and credentials copied into the container are not included
in the retained runtime directory. Raw runtime data can still contain prompts,
tool output and workspace-derived content, so store benchmark logs accordingly.
