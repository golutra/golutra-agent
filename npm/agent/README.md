# @golutra/agent

Golutra is a simple coding agent for turning a plain-language goal into
working, verified code. Install it once, run `golutra`, and start talking to the
agent; a Rust toolchain is not required.

> Describe the outcome. Golutra handles the work and shows the evidence.

## Install and run

```bash
npm install -g @golutra/agent
golutra
```

With no arguments, `golutra` opens the interactive TUI. Describe the outcome
you want and let the agent inspect the workspace, edit files, run checks, and
report the real result. For scripts and CI, use the headless entry point:

```bash
golutra exec "inspect this workspace and run the checks"
golutra exec --json "summarize the current changes"
```

## Why it feels simple

- The default `coding` profile keeps the model-facing tool surface compact and
  bounds context, reducing repeated input and unnecessary round trips without
  limiting the model's reasoning settings.
- A `shell` task can start a runtime-owned background process and return while
  it runs. `shell_session` reports incremental output and one authoritative
  terminal state when it finishes. Independent sessions may run in parallel,
  subject to host resources and policy; each session keeps an ordered task
  lane.
- The model can inspect actual turn, process, checkpoint, and verification
  state, then choose and adapt the next action. Golutra does not invent success
  or silently skip a necessary check.
- The normal TUI stays focused on progress and outcomes. `--json` and explicit
  debug/run-bundle surfaces expose token usage, tool outcomes, events, and
  verification facts when a user or automation needs detail.

Use `--tool-profile full` only for a task that needs low-frequency extensions;
the compact profile is the default for ordinary coding work.

## What gets installed

The package contains a small JavaScript launcher and selects the matching
platform package for the host OS and CPU. Native binaries are published as
versioned npm packages; installation does not run a network download script.

- `golutra`: interactive TUI with no arguments, or the scriptable CLI with a
  subcommand;
- `golutra-tui`: explicit TUI alias.

App-server, observation, supervisor, and evaluation binaries are distributed in
the platform release archive documented at
<https://github.com/golutra/golutra-agent/releases>.

Non-secret runtime defaults may be kept in `$GOLUTRA_HOME/runtime.json` or the
workspace's `.golutra/runtime.json`. Credentials remain in the owner-only
credential store or environment references.

This package is distributed under the Apache License 2.0. See `LICENSE` and
`NOTICE` in the installed package.
