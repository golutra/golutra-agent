# External Verification

## Purpose

Model tool output is useful execution evidence, but it is not always the final
acceptance authority. CI, benchmark harnesses and SDK callers can declare a
trusted command that runs after the model stops and before Runtime chooses the
terminal task state.

```text
model/tool loop
-> candidate workspace
-> caller-declared or command-boundary-discovered external verifier
-> artifact + evidence + objective:test check
-> VerificationRecord
-> StopSuccess / StopFailed / StopPartial
```

`ExternalVerificationSpec` contains `program`, `args`, workspace-relative
`cwd`, `timeout_ms`, `expected_exit_code` and `max_output_bytes`. The process is
argv-based and never interpreted by a shell. The runtime rejects cwd values
outside the workspace and executes with its existing sandbox, cancellation and
network restrictions. Output is redacted and bounded before durable storage.

A passing final verifier may supersede a recovered intermediate tool failure,
while the failed tool event remains in the trace for diagnosis. It never
supersedes failed delivery-path, content, schema, policy or verifier assertions.
Delivery-path checks apply to turns that changed files; a resume turn may verify
an unchanged existing delivery without manufacturing another write.

When a code/workspace task omits the `external_verifiers` field, the command
adapter conservatively discovers project checks from regular, bounded manifest
files: Cargo workspace tests, one meaningful Node script, pytest for an
identifiable Python project, and Go package tests. Manifest symlinks and
oversized files are ignored. An explicitly supplied list remains authoritative;
an explicit empty list disables discovery. Each queued turn owns its own list,
so a conversational follow-up never inherits a previous verifier.

## Entry Points

- `golutra exec`: `--completion-criterion`, `--verify-program`, repeated
  `--verify-arg`, `--verify-cwd`, timeout/exit/output controls. With no explicit
  verifier it uses project discovery; `--no-project-verifier-discovery` opts out.
- App Server and Rust SDK: `AgentTurnOptions.external_verifiers`.
- Python SDK: `Thread.run(..., external_verifiers=[...])`.
- TypeScript SDK: `Thread.run(..., { externalVerifiers: [...] })`.

MCP tools intentionally disable project discovery and do not expose this field
because their arguments may be model-generated rather than trusted operator
configuration.

For bounded repair loops, `scripts/run_agent_benchmark.py` starts an exec turn,
captures its thread id from JSONL, runs the same verifier independently for
bounded failure feedback, and resumes that thread up to `--max-attempts`. The
adapter does not add benchmark-specific behavior to Runtime.

## Trust Boundary

Explicit verifier argv is caller-owned. Discovered commands are selected only
from the fixed project catalog, but they can still execute untrusted workspace
scripts, so CI should run Golutra in an isolated machine or container. Explicit
exec `--approval-mode auto` only resolves model
actions already classified as `Ask`; it cannot approve `Block`/`Deny` actions or
disable workspace, secret, metacharacter, network and sandbox controls.
`exec --yolo` is the separate explicit escape hatch for trusted disposable
environments: it removes those local modification and sandbox controls while
leaving network environment configuration, verifier results and runtime bounds
intact. Since process-only execution has no OS network isolation, use it only
behind an appropriate outer machine or container boundary.
