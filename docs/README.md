# canvas-cloud documentation

## Use

- [canvas.md](canvas.md) — canvases over REST: the model, members and invites, share links,
  the people directory, chat mentions, live rooms, limits and metrics.
- [canvas-protocol.md](canvas-protocol.md) — the shared contract with clients: the
  `y-websocket` wire protocol, close codes, the document schema, the ops and read formats.
- [auth.md](auth.md) — accounts, the signup policy, personal access tokens, OIDC sign-in and
  the `/account` page.

## Integrate

- [apps.md](apps.md) — linked apps: `/v1/apps/self/*` user provisioning, the app event
  stream, canvases, stats, digests and the `canvas-cloud apps` CLI.
- [import-format.md](import-format.md) — import format v1 (NDJSON) and the document digest.
- [agents.md](agents.md) — agents: the MCP endpoint (`POST /mcp`), its tools, `/llms.txt`.
- [openapi.md](openapi.md) — the OpenAPI 3.1 document at `/v1/openapi.json`.
- [events.md](events.md) — `GET /v1/events`, the per-user live event stream.
- [auth.md](auth.md) — credentials: sessions, access tokens, app keys, OIDC.

## Operate

- [install.md](install.md) — the installer and its inputs, release tarballs, building from
  source, reverse proxies, TLS modes, the first account, upgrading and uninstalling.
- [configuration.md](configuration.md) — every config key, its type, default and
  environment override, with a minimal and a production example.
- [operations.md](operations.md) — systemd, logs, `doctor`, health and metrics endpoints,
  backups and restore, upgrades and migrations, capacity, troubleshooting.
- [architecture.md](architecture.md) — the crates, how the server is composed, request flow,
  persistence and the auth model.
- [security.md](security.md) — threat model, what is hashed or encrypted, CORS and origin
  checks, rate limits, HTML page headers. Reporting: [SECURITY.md](../SECURITY.md).
- [v0.1.0.md](v0.1.0.md) — release notes for v0.1.0; all changes in
  [CHANGELOG.md](../CHANGELOG.md).

Contributing: [CONTRIBUTING.md](../CONTRIBUTING.md).
