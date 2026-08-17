# Contributing to Golutra Agent

Thanks for helping improve Golutra Agent. Contributions of code, tests,
documentation, examples, bug reports, and design feedback are welcome.

## Before You Start

Please read the [Apache License 2.0](LICENSE), [Security Policy](SECURITY.md),
and [Code of Conduct](CODE_OF_CONDUCT.md). Do not open a public issue for a
security vulnerability or include credentials, provider responses, private
workspace data, or unredacted runtime artifacts in a report.

The repository is an early `0.1.0` release. The public source repository is
open for contribution, but the runtime protocol and internal crate APIs are
still evolving. Check an existing issue before starting a large change and
open a design issue when the change affects protocol, storage, policy,
transport, or release behavior.

## Development Setup

Install:

- Rust `1.93` (the version in `rust-toolchain.toml`)
- Python `3.11` or newer
- Node.js `22` or newer for the TypeScript SDK
- `just` is optional; every recipe is also a plain command in this repository

Build and run the TUI:

```bash
cargo run -p golutra-tui
```

The TUI may ask for provider configuration on first launch. Tests that need a
live provider are explicitly opt-in; normal tests must remain usable without
personal credentials or network access.

## Change Workflow

1. Fork the repository and create a focused branch from `main`.
2. Describe the intended behavior and the affected boundary before making a
   cross-cutting change.
3. Keep protocol changes typed and update the schema-generated clients with
   `just schema`.
4. Add or update deterministic tests for behavior, recovery, security, and
   serialization changes.
5. Update user-facing documentation when a command, protocol, configuration,
   or lifecycle behavior changes.
6. Open a pull request using the repository template and explain any known
   test limitations.

Avoid mixing formatting-only changes, generated output, and unrelated
refactors into a behavioral pull request. Preserve existing user changes in
your working tree and do not commit credentials or local `.golutra` state.

## Required Checks

Run the checks relevant to your change. A full local pass is:

```bash
just fmt-check
just clippy
just test
just schema
just ts-check
just py-check
just open-source-check
just release-package-smoke
```

For a documentation-only change, `python3 scripts/check_open_source.py` is
still useful because it verifies required public entry points and links.

Generated files that are expected to change are:

- `schemas/sdk-protocol.schema.json`
- `sdk/typescript/src/generated.ts`
- `sdk/python/src/golutra_sdk/generated.py`

Do not hand-edit generated protocol output. Review the diff after generation.

## Pull Requests

A useful pull request contains:

- a short problem statement and the chosen design;
- tests that demonstrate the changed behavior;
- compatibility or migration notes for protocol/storage changes;
- security and privacy considerations for new tools or artifacts;
- disclosure of material third-party code or generated content;
- disclosure of substantial AI-assisted code, where applicable, so reviewers
  can focus provenance and verification review appropriately.

Maintainers may request a smaller change, a design note, or additional
evidence before merging. Reviewers prioritize correctness, security,
recovery behavior, deterministic tests, and clear ownership boundaries.

## Commit Style

Use an imperative, scoped subject when practical, for example:

```text
fix(tui): preserve the composer after history replay
feat(protocol): add a typed verification outcome
docs: explain local app-server lifecycle
```

Keep commits reversible and avoid committing build output, editor state,
credentials, or generated files that were not produced by the documented
generation command.

## License for Contributions

This project is distributed under Apache-2.0. Unless you explicitly state
otherwise, a contribution intentionally submitted for inclusion is provided
under the terms of Apache-2.0, including the copyright and patent license in
Sections 2, 3, and 5. Each contributor keeps ownership of their original
copyright. There is no separate CLA or DCO workflow in this repository at
present; maintainers may still ask for provenance clarification when a change
contains third-party material.

Questions about this process belong in a GitHub issue or discussion. Report
security issues privately as described in [SECURITY.md](SECURITY.md).
