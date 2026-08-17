# Open-Source Readiness

This repository is prepared for public contribution under Apache-2.0. The
checklist separates changes that can be reviewed in Git from settings that
only a GitHub administrator can apply.

## In the Repository

- `LICENSE` contains the unmodified Apache License 2.0 text.
- `NOTICE` identifies the project and explains third-party notice handling.
- `README.md` provides the public product entry point, quick start, links,
  status, architecture boundaries, and verification commands.
- README visual assets are tracked in `assets/readme/` with their display
  permission and trademark boundary recorded in `NOTICE`.
- `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`, `GOVERNANCE.md`, and
  `SUPPORT.md` define the contribution and maintenance paths.
- `.github` contains issue forms, a pull-request checklist, ownership, and
  dependency update configuration.
- Cargo, Python, and TypeScript metadata use Apache-2.0 and the canonical
  repository URL.
- Local Rust dependencies declare versions, so `cargo package --workspace`
  can construct package manifests instead of failing on path-only references.
- Release archives include `LICENSE` and `NOTICE`, and the package verifier
  checks their presence and file modes.
- The npm launcher is a public Apache-2.0 wrapper. Release jobs build native
  platform packages first, generate the root `@golutra/agent` package from the
  artifacts that actually passed packaging, and never download binaries from a
  `postinstall` hook.
- The release matrix builds and smoke-tests Linux x64/arm64, macOS x64/arm64,
  and Windows x64/arm64 packages on matching hosted runners.
- The publish job pins npm 11.17.0 because trusted publishing requires npm
  11.5.1 or newer; Node 22's bundled npm 10 client is not OIDC-capable.
- `scripts/check_open_source.py` runs without third-party Python packages and
  is part of CI.

## Deliberately Not Enabled Yet

- The Rust workspace is a monorepo; no crate is promised as a stable crates.io
  API. Crate publication should be enabled per package only after its API and
  package contents have an owner.
- The TypeScript SDK remains `private` in npm metadata because it currently
  exports source files and has no release build/publish contract. It is still
  open for source contributions.
- The Python and TypeScript SDKs are tested from the repository, but publishing
  them to PyPI or npm is a separate release decision.
- No CLA or DCO bot is enabled. Apache-2.0 Section 5 is the default inbound
  contribution rule, with provenance review for third-party material.

The committed lockfile resolves `flume`'s transitive `spin` dependency to
`0.9.9`. This avoids the yanked `0.9.8` release without changing the SQLx API
surface, and the locked workspace and package checks enforce that resolution.

## GitHub Administrator Checklist

Before announcing a public release, an administrator should verify:

1. Repository visibility, ownership, default branch, description, and topics.
2. Branch protection requiring the CI checks and restricting force-pushes.
3. Actions permissions, dependency update permissions, and billing/spending
   limits.
4. Security Advisories and private vulnerability reporting.
5. Maintainer membership and the `CODEOWNERS` account/team.
6. npm publishing authentication. New package names cannot be bound to a
   trusted publisher before their first publication, so the initial tagged
   release may use a short-lived granular token stored as the repository
   secret `NPM_BOOTSTRAP_TOKEN`. After that release creates `@golutra/agent`
   and all platform packages, bind every package to the GitHub trusted
   publisher for `.github/workflows/release.yml`, delete the repository secret,
   and revoke the bootstrap token. Subsequent releases use OIDC provenance
   without a long-lived npm token.
7. Release signing or artifact attestation policy, if the project adopts one.

These settings are intentionally not changed by a source commit.
