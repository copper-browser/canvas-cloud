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
PostgreSQL role and database, config with a fresh master key, certificate, systemd unit):

```sh
curl -fsSL https://raw.githubusercontent.com/copper-browser/canvas-cloud/main/install.sh | sudo sh
```

## Command line

| Command | What |
|---|---|
| `serve [--no-migrate]` | run the server (the default command) |
| `migrate` | apply pending migrations |
| `doctor` | check config, database, migrations, TLS, the listener, web root and metrics |
| `healthcheck --wait N` | wait until `/healthz` answers |
| `tls-init [--force] [--name N]` | create the self-signed certificate |
| `gen-config --write PATH` | write a new 0600 config with a fresh master key (`-` prints it) |
| `admin users list\|create\|disable\|enable\|delete\|set-password` | accounts (`--password-file`, never argv) |
| `admin settings get` / `set allow_signup true\|false` / `set signup_domains a.com,b.com` | signup policy |
| `apps create --name N [--allow-import] [--no-trust-email]` / `list` / `revoke ID` / `rotate ID` | linked apps (the `cva_…` key is printed once) |

## Configuration

TOML at `/etc/canvas-cloud/canvas-cloud.toml` (or `--config`, or `CANVAS_CLOUD_CONFIG`).
Every key can be overridden from the environment: `CANVAS_CLOUD_<KEY>`, nested keys with
`__` (`CANVAS_CLOUD_TLS__MODE=off`), lists comma-separated.

| Key | Default | Meaning |
|---|---|---|
| `listen` | `0.0.0.0:443` | listen address |
| `public_url` | derived from `listen`/`tls` | base URL clients use, e.g. `https://canvas.example.com` |
| `database_url` | — (required) | `postgres://…` |
| `db_max_connections` | `20` | pool size |
| `master_key` | — (required) | base64url, 32 bytes; wraps every canvas key. **Back it up.** |
| `trust_proxy` | `false` | honour `X-Forwarded-For` (only behind a proxy) |
| `web_root` | unset | directory with a built web app (`index.html`); served at `/` for every GET no API route claims |
| `web_app_url` | unset | where `/join/{token}` share links go (`{token}` substituted); defaults to `{public_url}/#/join/{token}` when `web_root` is set |
| `metrics_listen` | `127.0.0.1:9474` | Prometheus `/metrics`; `"off"` disables |
| `log_format` / `log_level` | `json` / `info` | logging (`RUST_LOG` overrides the level) |
| `tls.mode` | `self-signed` | `self-signed`, `acme` (Let's Encrypt, needs `tls.domain` and :443) or `off` (behind a TLS proxy) |
| `tls.cert_path` / `tls.key_path` | `/var/lib/canvas-cloud/tls/{cert,key}.pem` | certificate files |
| `tls.domain` / `tls.acme_email` / `tls.acme_cache_dir` / `tls.acme_staging` | — | ACME settings |
| `cors.allowed_origins` | `[]` | other browser origins allowed to call the API (exact match); the `web_root` app is same-origin and needs none |
| `ops.extra_url_schemes` | `[]` | URL schemes accepted in link shapes besides `http`, `https`, `mailto` |
| `limits.auth_per_minute` | `10` | requests per minute per IP on `/v1/auth/*` |
| `limits.import_max_bytes` / `limits.import_max_line_bytes` | 512 MiB / 32 MiB | import limits for linked apps |
| `[[oidc]]` | none | sign-in providers: `id`, `name`, `issuer`, `client_id`, `client_secret_file`, `allowed_domains`, `redirect_uris` |

For local development of a canvas web app against this server:

```toml
[cors]
allowed_origins = ["http://localhost:5173", "http://127.0.0.1:5173"]
```

`GET /v1/info` (public) tells a client what the server offers: name, version,
`public_url`, whether signup is open, sign-in methods (`auth.password`, `auth.oidc`), the
served web app (`web.root`, `web.app_url`), `features` and `limits`. A web app served from
`web_root` probes its own origin's `/v1/info` to find out it is running on canvas-cloud.

## Docs

- [docs/canvas.md](docs/canvas.md) — the canvas REST API, rooms, limits, metrics, security.
- [docs/canvas-protocol.md](docs/canvas-protocol.md) — the WebSocket protocol, document
  schema, ops and read formats.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -j4     # needs native PostgreSQL; see below
```

Integration tests use dedicated databases, overridable per crate:
`CANVAS_CLOUD_TEST_CORE_DATABASE_URL` (`cvc_core_test`),
`CANVAS_CLOUD_TEST_ENGINE_DATABASE_URL` (`cvc_engine_test`) and
`CANVAS_CLOUD_TEST_DATABASE_URL` (`cvc_bin_test`). Create them with `createdb`.

## License

MIT — see [LICENSE](LICENSE).
