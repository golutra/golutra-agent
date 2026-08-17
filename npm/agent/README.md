# @golutra/agent

Install the Golutra coding agent without a Rust toolchain:

```bash
npm install -g @golutra/agent
golutra-tui
```

The package contains a small JavaScript launcher and selects the matching
platform package for the host OS and CPU. Native binaries are published as
versioned npm packages; installation does not run a network download script.

The package exposes:

- `golutra`: the scriptable CLI;
- `golutra-tui`: the interactive terminal UI.

For app-server, observation, supervisor, and evaluation binaries, use the
platform release archive documented at
<https://github.com/golutra/golutra-agent/releases>.

This package is distributed under the Apache License 2.0. See `LICENSE` and
`NOTICE` in the installed package.
