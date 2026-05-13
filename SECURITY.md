# Security policy

## Supported versions

| Version | Supported          |
|---------|--------------------|
| 0.1.x   | :white_check_mark: |
| < 0.1   | :x:                |

We currently support the most recent minor version on the `0.x` line.
Once the project reaches `1.0`, this table will be updated to cover the
current and previous minor versions.

## Reporting a vulnerability

**Do not file a public GitHub issue for security vulnerabilities.**

Please report via **GitHub Security Advisories**:

<https://github.com/matt-cochran/mcp-flowgate/security/advisories/new>

Include, where possible:

- A description of the issue and its impact.
- Steps to reproduce, ideally with a minimal config.
- The affected version (`mcp-flowgate --version`).
- Any suggested mitigation.

## What to expect

- **Acknowledgement** within **3 business days**.
- **Initial assessment** within **10 business days**.
- **Coordinated disclosure window**: 90 days from the acknowledgement,
  unless an earlier public disclosure is required to protect users.

A CVE will be requested for any vulnerability with a CVSS v3.1 score of
4.0 (medium) or higher. Patched releases will reference the advisory in
the [CHANGELOG](CHANGELOG.md) under a `### Security` heading.

## Scope

In scope:

- The published crates in this workspace (`mcp-flowgate`,
  `mcp-flowgate-core`, `mcp-flowgate-executors`, `mcp-flowgate-mcp-server`,
  `mcp-flowgate-schema`).
- Default executor implementations.
- Audit, store, and reliability subsystems.
- Example configurations that are bundled in this repository.

Out of scope (please report to the appropriate upstream):

- Vulnerabilities in the MCP servers, REST endpoints, or CLI tools that
  the gateway proxies to.
- Vulnerabilities in third-party crates we depend on (e.g. `rmcp`,
  `rusqlite`, `reqwest`) — please report those to their maintainers,
  then notify us so we can pin patched versions.
- Misconfiguration leading to over-permissive policy — but please open
  a regular issue if you believe a misconfiguration is *easy to fall
  into*; documentation hardening matters.

## Hardening recommendations

For production deployments, see the
[README's "Going to production"](README.md#going-to-production)
and [`STABILITY.md`](STABILITY.md). In particular: do not run with the
default `memory` store or `stdout` audit sink in any deployment where
state loss or log mixing matters.
