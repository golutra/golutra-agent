# Security Policy

Golutra Agent runs tools in a user's workspace and can connect to external
model providers. Treat runtime state, provider configuration, credentials,
logs, and replay artifacts as sensitive by default.

## Supported Versions

The latest `main` branch and the latest tagged release receive security
triage. Older releases may receive a fix when the change is low risk and the
affected behavior is still supported.

| Version | Security support |
| --- | --- |
| `main` | Supported |
| Latest `0.1.x` release | Supported on a best-effort basis |
| Older releases | Not guaranteed |

## Reporting a Vulnerability

Please report vulnerabilities privately through
[GitHub Security Advisories](https://github.com/golutra/golutra-agent/security/advisories/new).
If private reporting is unavailable, contact the project maintainer through
`golutra@hotmail.com` with `[Security]` in the subject.

Do not open a public issue until the maintainer has acknowledged the report.

Include, when safe to share:

- the affected version or commit;
- the component and operating system;
- a concise description of the impact;
- reliable reproduction steps or a minimal proof of concept;
- mitigations or a proposed fix, if known.

Remove API keys, OAuth tokens, cookies, private workspace contents, full
provider payloads, and unredacted runtime artifacts from every report. The
runtime's redaction guarantees do not make arbitrary user-provided logs safe
to publish.

## Response Process

Maintainers will acknowledge a report when it is received, reproduce or
triage it, decide on severity and affected versions, and coordinate a fix or
mitigation. Disclosure timing depends on exploitability, available fixes, and
the reporter's needs. Reporters will be credited when they request credit and
when doing so does not create a safety or privacy concern.

For dependency vulnerabilities, include the dependency name, version, and
the relevant lockfile entry. Do not use this policy for general support;
please use [SUPPORT.md](SUPPORT.md).
