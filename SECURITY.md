# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub: on this repository, open the
**Security** tab and choose **Report a vulnerability** (a private security advisory). Do not
open a public issue, pull request or discussion for a suspected vulnerability.

Include what you found, how to reproduce it (version, config with secrets removed, requests),
and what an attacker gains. We aim to acknowledge a report within a week, agree on a fix and
a disclosure date with you, and credit you in the advisory unless you prefer otherwise.

## Supported versions

Security fixes go into the latest release. canvas-cloud is pre-1.0: upgrade to the newest
`0.x` release to receive fixes.

## Scope

In scope: the `canvas-cloud` server and CLI, `install.sh`, and the systemd unit in
`packaging/`. The threat model, what is stored hashed or encrypted, and what the server
deliberately trusts (the host, the config and its master key, linked-app keys) are described
in [docs/security.md](docs/security.md). Findings that need those trusted parts are out of
scope.
