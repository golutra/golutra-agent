# Governance

Golutra Agent is maintained as an open-source project under Apache-2.0. The
goal of governance is to keep the runtime safe, understandable, and useful to
contributors without adding process that does not improve the code.

## Roles

- **Maintainers** review changes, protect the main branch, publish releases,
  and make final decisions when consensus cannot be reached.
- **Contributors** propose and implement changes, provide tests and evidence,
  and keep third-party provenance clear.
- **Reviewers** may be invited for a subsystem or release and do not need
  write access to provide technical direction.

The current project steward is the repository owner represented by
[`@seekskyworld`](https://github.com/seekskyworld). The maintainer set may
change through documented repository activity and GitHub permissions.

## Decision Making

Routine changes are decided in pull-request review. Changes that affect the
versioned protocol, persistent storage, security policy, licensing, or public
release behavior should first have a design discussion or an ADR in
[`docs/adr`](docs/adr). Maintainers seek technical consensus, but may merge or
decline a change when a decision is needed to protect users or project scope.

Decisions should be recorded in the pull request, an issue, or an ADR. A
maintainer may request a follow-up issue instead of blocking an otherwise
safe, focused contribution on a larger redesign.

## Releases

Release versions are taken from `workspace.package.version`. A release tag is
`v<version>` and is built by the release workflow for the supported target
platforms. Release archives must pass checksum, manifest, and legal-notice
verification. Breaking protocol or storage changes require explicit release
notes and migration guidance.

## Security and Conduct

Security reports follow [SECURITY.md](SECURITY.md), and community conduct
follows [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md). Maintainers may temporarily
restrict discussion or delay a release to address an active vulnerability,
credential exposure, or abuse of project infrastructure.

## Changes to This Document

Governance changes are proposed and reviewed like code. A change takes effect
when it is merged to the default branch and should explain its motivation and
impact on contributors.
