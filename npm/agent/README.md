# @golutra/agent

Install the Golutra coding agent without a Rust toolchain:

```bash
npm install -g @golutra/agent
golutra
```

The package contains a small JavaScript launcher and selects the matching
platform package for the host OS and CPU. Native binaries are published as
versioned npm packages; installation does not run a network download script.

The package exposes:

- `golutra`: the interactive terminal UI when called without arguments, or the
  scriptable CLI when given a subcommand;
- `golutra-tui`: an explicit compatibility entry point for the interactive UI.

For a non-interactive turn, use the CLI explicitly:

```bash
golutra exec "inspect this workspace and run the checks"
golutra exec --json "summarize the current changes"
```

Interactive turns use the compact coding tool surface by default; pass
`--tool-profile full` when a task needs low-frequency extensions.

For app-server, observation, supervisor, and evaluation binaries, use the
platform release archive documented at
<https://github.com/golutra/golutra-agent/releases>.

This package is distributed under the Apache License 2.0. See `LICENSE` and
`NOTICE` in the installed package.
