# Local TUI comparison

This POSIX-only diagnostic sends identical text from a local Responses SSE fixture
through the real executable and a 120×32 PTY. It creates isolated homes and uses a
dummy API key. It does not contact a real model or modify your provider settings.

```sh
cargo build -p golutra-agent-tui --locked --bin golutra-agent-tui
uv run --with pyte python scripts/tui_benchmark/streaming.py debug,codex paragraphs,code,longline,bursty 400
```

The arguments select engines, scenarios and chunk count (1–1000). `debug` means
the local Golutra executable; `codex` must be available on PATH. Output is written
to a new temporary directory unless `TUI_BENCH_OUTPUT` names a results directory.
Each run prints its artifact path and saves screen observations, raw ANSI, the
synthetic request, send/observation timestamps and summary JSON. Missing markers
or fixture transport failures cause a nonzero exit.

Optional environment variables:

| Variable | Meaning |
| --- | --- |
| `DIAG_BINARY` | Absolute Golutra executable path, e.g. a frozen before/after build |
| `TUI_BENCH_CODEX` | Codex executable path |
| `DIAG_TYPING=1` | Measure 30 key-to-visible latencies during streaming (use ≥200 chunks) |
| `DIAG_HISTORY=40` | Populate 40 turns through the same fixture; measure typing before the measured response |
| `DIAG_TIMING=1` | Enable Golutra phase tracing and exit gracefully to save `ui-timing.csv` |
| `DIAG_LABEL` | Prefix the summary filename when retaining several runs |

Compare runs serially with the same build profile and alternate engine order.
Avoid simultaneous compilation or other benchmarks. A debug Golutra versus an
installed release Codex is an application-level observation, not a normalized
renderer microbenchmark. Record executable versions/hashes separately.

Latency includes local provider handling, UI scheduling, PTY delivery and the
Python observer; `observer_feed_ms` reports the screen parser's own time. The
400-marker check means every marker was observed at least once, not that the final
scrollback contains each exactly once. Rust PTY acceptance tests separately check
unique ordered history, final replacements, resize, `/status`, resume and drafts.
These checks do not measure terminal GPU rendering or IME behavior.
