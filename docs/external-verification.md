# External Verification

## Purpose

Model tool output is useful execution evidence, but it is not always the final
acceptance authority. CI, benchmark harnesses and SDK callers can declare a
trusted command that runs after the model stops and before Runtime chooses the
terminal task state.

```text
model/tool loop
-> candidate workspace
-> caller-declared external verifier
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

## Entry Points

- `golutra exec`: `--completion-criterion`, `--verify-program`, repeated
  `--verify-arg`, `--verify-cwd`, timeout/exit/output controls.
- App Server and Rust SDK: `AgentTurnOptions.external_verifiers`.
- Python SDK: `Thread.run(..., external_verifiers=[...])`.
- TypeScript SDK: `Thread.run(..., { externalVerifiers: [...] })`.

MCP tools intentionally do not expose this field because their arguments may
be model-generated rather than trusted operator configuration.

For bounded repair loops, `scripts/run_agent_benchmark.py` starts an exec turn,
captures its thread id from JSONL, runs the same verifier independently for
bounded failure feedback, and resumes that thread up to `--max-attempts`. The
adapter does not add benchmark-specific behavior to Runtime.

## Trust Boundary

The caller owns verifier argv and is responsible for its code. A verifier may
execute untrusted workspace content, so CI should run Golutra in an isolated
machine or container. Explicit exec `--approval-mode auto` only resolves model
actions already classified as `Ask`; it cannot approve `Block`/`Deny` actions or
disable workspace, secret, metacharacter, network and sandbox controls.
