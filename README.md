# canvas-cloud

A self-hostable, real-time collaborative canvas server. One Rust binary and a PostgreSQL
database give you shared whiteboards that people edit together live (Yjs over
`y-websocket`), that agents can read and draw on over REST (and MCP), and that other
services can link to with an app key — e.g. a Copper Cloud can link to it so its users get
canvases without a second account.

## Features

- **Canvases**: a Personal canvas per user plus shared canvases with owner/editor roles,
  email invites, share links and a people directory.
- **Live rooms**: one in-memory Yjs room per open canvas, speaking the standard
  `y-websocket` protocol; every update is durable before it is broadcast.
- **Agent API**: `GET /v1/canvases/{id}/read` (a compact, agent-friendly JSON view) and
  `POST /v1/canvases/{id}/ops` (add/update/move/delete shapes with validation), applied to the
  live document so everyone sees them at once.
- **Chat mentions**: the canvas chat lives in the document; the server records and notifies
  @mentions.
- **Identity**: sessions, personal access tokens (`read`/`write` scopes) and linked apps
  that act for their own users with `X-Canvas-User`.
- **Encryption at rest**: every canvas has its own key, wrapped by the server's master key;
  updates, snapshots and mention excerpts are AES-256-GCM sealed.
- **Serve a web app**: point `web_root` at a built canvas web app and the server hosts it on
  the same origin; share links open it directly.
- **Operations**: built-in TLS (self-signed or Let's Encrypt), Prometheus metrics,
  structured logs, `doctor`, a sandboxed systemd unit and a one-command installer.

## Quickstart

Needs Rust (stable) and a native PostgreSQL 14+.

```sh
createdb canvas_cloud
cargo build --release
target/release/canvas-cloud gen-config --write ./canvas-cloud.toml \
  --tls-mode off --listen 127.0.0.1:8480 \
  --database-url postgres://localhost/canvas_cloud --state-dir ./state
target/release/canvas-cloud --config ./canvas-cloud.toml migrate
target/release/canvas-cloud --config ./canvas-cloud.toml serve
```

```sh
curl -s http://127.0.0.1:8480/v1/info
printf 'a long password\n' > pw && \
  target/release/canvas-cloud --config ./canvas-cloud.toml admin users create \
  --email you@example.com --password-file pw
```

On a Linux server, the installer sets everything up (binary, service user, native
PostgreSQL role and database, config with a fresh master key, certificate, systemd unit;
inputs such as `CANVAS_CLOUD_DOMAIN` or `CANVAS_CLOUD_LISTEN` + `CANVAS_CLOUD_TLS_MODE=off`
for a reverse proxy are in [docs/install.md](docs/install.md)):

```sh
curl -fsSL https://raw.githubusercontent.com/copper-browser/canvas-cloud/main/install.sh | sudo sh
```

## Command line

`canvas-cloud serve` (the default), `migrate`, `doctor`, `healthcheck`, `tls-init`,
`gen-config`, `version`, and the admin commands `admin users …`, `admin settings …` and
`apps …` (linked apps). Every command and flag: [docs/operations.md](docs/operations.md#command-line).

## Configuration

One TOML file (`/etc/canvas-cloud/canvas-cloud.toml`, or `--config`, or
`CANVAS_CLOUD_CONFIG`), every key overridable from the environment as
`CANVAS_CLOUD_<KEY>` (nested keys with `__`, e.g. `CANVAS_CLOUD_TLS__MODE=off`). Every key,
default and example: [docs/configuration.md](docs/configuration.md).

`GET /v1/info` (public) tells a client what the server offers: name, version, `public_url`,
whether signup is open, sign-in methods, the served web app, `features` and `limits`.

## Docs

[docs/README.md](docs/README.md) indexes everything. Start with:

- [Install](docs/install.md) — the installer, tarballs, source, reverse proxies, TLS.
- [Operate](docs/operations.md) — backups, upgrades, metrics, troubleshooting.
- [Canvases](docs/canvas.md) and the [canvas protocol](docs/canvas-protocol.md) — the API and
  the WebSocket protocol clients speak.
- [Linked apps](docs/apps.md), [agents and MCP](docs/agents.md), [accounts and
  sign-in](docs/auth.md).
- [Security](docs/security.md) — and [SECURITY.md](SECURITY.md) to report a vulnerability.
- Release notes: [CHANGELOG.md](CHANGELOG.md).

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md): native PostgreSQL test databases, `cargo fmt`,
`cargo clippy -D warnings` and `cargo test`.

## License

MIT — see [LICENSE](LICENSE).
