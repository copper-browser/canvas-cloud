# Changelog

All notable changes to canvas-cloud. Versions follow [Semantic Versioning](https://semver.org/);
before 1.0, breaking changes to the client contract (see
[docs/v0.1.0.md](docs/v0.1.0.md#compatibility)) only come with a new minor version and are
listed under **Breaking**.

## 0.1.0

First release. Release notes: [docs/v0.1.0.md](docs/v0.1.0.md).

### Added

- Canvases: Personal and shared canvases, owner/editor roles, email invites, share links,
  people directory, chat @mentions; REST API under `/v1`.
- Live rooms over the standard `y-websocket` protocol, durable before broadcast, with
  verified snapshot compaction (a snapshot is stored only if it re-applies to the same
  document, which works around a yrs 0.28 surrogate-pair encoding bug).
- Agent read format and validated ops on the live document.
- Encryption at rest: per-canvas keys wrapped by the master key (AES-256-GCM).
- Password accounts with a signup policy, OIDC sign-in, personal access tokens (`read` /
  `write`), the `/account` page, and `GET /v1/events`.
- Linked apps (`/v1/apps/self/*`): user provisioning, app event stream, canvases, stats,
  digests, import format v1.
- MCP server (`POST /mcp`), `/llms.txt`, OpenAPI 3.1 at `/v1/openapi.json`.
- TLS: self-signed, Let's Encrypt (TLS-ALPN-01), or off behind a proxy.
- `install.sh` for systemd Linux with native PostgreSQL, including
  `CANVAS_CLOUD_LISTEN`, `CANVAS_CLOUD_TLS_MODE`, `CANVAS_CLOUD_PUBLIC_URL` and `--dry-run`
  for reverse-proxy and loopback installs.
- CLI: `serve`, `migrate`, `doctor`, `healthcheck`, `tls-init`, `gen-config`, `admin users`,
  `admin settings`, `apps`.
- Prometheus metrics, JSON logs.
- Release binaries for Linux `x86_64` and `aarch64` (glibc 2.28+).
