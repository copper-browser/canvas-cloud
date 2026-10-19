# Configuration

canvas-cloud reads one TOML file, overlaid with environment variables. This page lists every
key the server accepts; it follows `crates/core/src/config.rs` (`RawConfig` and its
validation in `Config::from_raw`).

## Where the config comes from

1. The file: `--config PATH`, else `CANVAS_CLOUD_CONFIG`, else
   `/etc/canvas-cloud/canvas-cloud.toml`. A file named by `--config` or `CANVAS_CLOUD_CONFIG`
   must exist. The default path may be missing, in which case the whole config comes from the
   environment.
2. Environment overrides: `CANVAS_CLOUD_<KEY>`, upper-case, with `__` for a nested key:
   `tls.mode` → `CANVAS_CLOUD_TLS__MODE`, `limits.auth_per_minute` →
   `CANVAS_CLOUD_LIMITS__AUTH_PER_MINUTE`. An override replaces the file's value.
   - Lists are comma-separated: `CANVAS_CLOUD_CORS__ALLOWED_ORIGINS=https://a.example.com,http://localhost:5173`.
   - Booleans: `true`/`1`/`yes`/`on` or `false`/`0`/`no`/`off`.
   - Integers may contain `_` (`536_870_912`).
   - Other `CANVAS_CLOUD_*` variables are ignored. `[[oidc]]` cannot be set from the
     environment.
3. `RUST_LOG`, when set, replaces `log_level`.

Unknown keys in the file are an error (`unknown field`), so a typo fails at start instead of
being silently ignored. `canvas-cloud doctor` loads the config the way `serve` does and
reports the first error.

`canvas-cloud gen-config` writes a complete, commented file with a fresh master key (see
[install.md](install.md#from-source)).

## Top-level keys

| Key | Type | Default | Env | Meaning |
|---|---|---|---|---|
| `listen` | `"ip:port"` | `"0.0.0.0:443"` | `CANVAS_CLOUD_LISTEN` | address the HTTP(S) server binds. An IP address, not a host name (`"[::]:443"` for IPv6) |
| `public_url` | URL | derived (below) | `CANVAS_CLOUD_PUBLIC_URL` | the base URL clients use. See [public_url](#public_url) |
| `database_url` | string | **required** | `CANVAS_CLOUD_DATABASE_URL` | `postgres://` or `postgresql://` URL (`?sslmode=require` etc. accepted) |
| `db_max_connections` | integer ≥ 1 | `20` | `CANVAS_CLOUD_DB_MAX_CONNECTIONS` | PostgreSQL pool size |
| `master_key` | string | **required** | `CANVAS_CLOUD_MASTER_KEY` | 32 random bytes, base64url (base64 with or without padding also accepted). Wraps every canvas's document key: **without it the stored canvases cannot be read.** Back it up |
| `trust_proxy` | bool | `false` | `CANVAS_CLOUD_TRUST_PROXY` | take the client IP from the right-most `X-Forwarded-For` entry (else `X-Real-IP`) for rate limits and logs. Only behind a proxy that is the only way in |
| `web_app_url` | URL | unset | `CANVAS_CLOUD_WEB_APP_URL` | where `/join/{token}` share-link pages redirect, with a `{token}` placeholder (appended when missing). Must be `http(s)`. A `{canvas}` placeholder also adds Open links on `/account` ([auth.md](auth.md#the-account-page)) |
| `web_root` | path | unset | `CANVAS_CLOUD_WEB_ROOT` | a directory with `index.html` (e.g. a built canvas web app), served at `/` for every GET/HEAD path no route claims. Checked by `serve` and `doctor`, not at parse time. When set and `web_app_url` is not, share links go to `{public_url}/#/join/{token}` |
| `metrics_listen` | `"ip:port"` or `"off"` | `"127.0.0.1:9474"` | `CANVAS_CLOUD_METRICS_LISTEN` | Prometheus `GET /metrics` listener. `"off"`, `"none"`, `"false"` or `""` disable it |
| `log_format` | `"json"` \| `"pretty"` | `"json"` | `CANVAS_CLOUD_LOG_FORMAT` | log lines on stdout |
| `log_level` | string | `"info"` | `CANVAS_CLOUD_LOG_LEVEL` | a `tracing` filter directive (`"debug"`, `"info,canvas_cloud_engine=debug"`). `RUST_LOG` wins when set |

### `public_url`

Used for: `/v1/info`, `/llms.txt` and `/v1/openapi.json` URLs, the URLs of share links
created over MCP, the OIDC callback (`{public_url}/v1/auth/oidc/{id}/callback`) and the
redirect URIs it accepts, the MCP `Origin` check, the default share-link target with
`web_root`, the `Secure` flag of the OIDC state cookie, and the names in a generated
self-signed certificate.

It is normalised to `scheme://host[:port]`, lower-case: a path is dropped, a bare host gets
`https://` (`http://` with `tls.mode = "off"`), and credentials or non-`http(s)` schemes are
refused. When unset it is derived from `listen` and `tls`:

| `tls.domain` | Result |
|---|---|
| set, and `mode = "acme"` or port 443 | `https://DOMAIN` |
| set, other port | `https://DOMAIN:PORT` (`http://` when TLS is off) |
| unset | `https://localhost[:PORT]` (`http://localhost[:PORT]` when TLS is off) |

`localhost` is only useful locally, so set `public_url` on any real server. Behind a TLS
proxy it is the proxy's URL, not the listen address.

## `[tls]`

| Key | Type | Default | Env | Meaning |
|---|---|---|---|---|
| `mode` | `"self-signed"` \| `"acme"` \| `"off"` | `"self-signed"` | `CANVAS_CLOUD_TLS__MODE` | see [install.md](install.md#tls-modes) |
| `cert_path` | path | `/var/lib/canvas-cloud/tls/cert.pem` | `CANVAS_CLOUD_TLS__CERT_PATH` | PEM certificate chain (`self-signed` mode; generated when missing) |
| `key_path` | path | `/var/lib/canvas-cloud/tls/key.pem` | `CANVAS_CLOUD_TLS__KEY_PATH` | PEM private key, written with mode 0600 when generated |
| `domain` | string | unset | `CANVAS_CLOUD_TLS__DOMAIN` | required for `acme`; added to a generated certificate; lower-cased, trailing dot removed |
| `acme_email` | string | unset | `CANVAS_CLOUD_TLS__ACME_EMAIL` | Let's Encrypt contact |
| `acme_cache_dir` | path | `/var/lib/canvas-cloud/acme` | `CANVAS_CLOUD_TLS__ACME_CACHE_DIR` | ACME account and certificates (created at start) |
| `acme_staging` | bool | `false` | `CANVAS_CLOUD_TLS__ACME_STAGING` | use the Let's Encrypt staging directory |

The defaults do not follow `gen-config --state-dir`; `gen-config` writes the three paths
explicitly.

## `[cors]`

| Key | Type | Default | Env | Meaning |
|---|---|---|---|---|
| `allowed_origins` | list of origins | `[]` | `CANVAS_CLOUD_CORS__ALLOWED_ORIGINS` | browser origins (other than the server's own) allowed to call the API. Each must be a bare `http(s)://host[:port]` (a trailing `/` is dropped; a path is refused). `[]` sends no CORS headers at all |

Matching is exact. These origins may also call `POST /mcp`
([security.md](security.md#cors-and-origin-checks)). For a canvas web app in development:
`allowed_origins = ["http://localhost:5173", "http://127.0.0.1:5173"]`.

## `[ops]`

| Key | Type | Default | Env | Meaning |
|---|---|---|---|---|
| `extra_url_schemes` | list of schemes | `[]` | `CANVAS_CLOUD_OPS__EXTRA_URL_SCHEMES` | URL schemes accepted in link shapes on top of `http`, `https` and `mailto`. Each `[a-z][a-z0-9+.-]*`, at most 32 characters (lower-cased, a trailing `:` removed); `javascript`, `data`, `vbscript` and `file` are refused |

## `[limits]`

| Key | Type | Default | Env | Meaning |
|---|---|---|---|---|
| `auth_per_minute` | integer ≥ 1 | `10` | `CANVAS_CLOUD_LIMITS__AUTH_PER_MINUTE` | requests per minute per client IP (IPv6: per /64) on `/v1/auth/*` except `GET /v1/auth/me`; signup, login and password changes are also limited per email with the same budget |
| `import_max_bytes` | integer, 1 KiB – 16 GiB | `536870912` (512 MiB) | `CANVAS_CLOUD_LIMITS__IMPORT_MAX_BYTES` | largest `POST /v1/apps/self/import` body |
| `import_max_line_bytes` | integer, 1 KiB – `import_max_bytes` | `33554432` (32 MiB) | `CANVAS_CLOUD_LIMITS__IMPORT_MAX_LINE_BYTES` | longest NDJSON line in an import |

All three are reported in `GET /v1/info` → `limits`. The other limits (body sizes, WebSocket
frames, ops per call, mentions) are fixed in code; see
[canvas.md](canvas.md#limits).

## `[[oidc]]` — sign-in providers

One table per provider; any number. File only (no environment overrides). Unknown keys are
an error. Setup and the sign-in rules: [auth.md](auth.md#oidc-sign-in).

| Key | Type | Default | Meaning |
|---|---|---|---|
| `id` | string | **required** | path segment in `/v1/auth/oidc/{id}/…`: `[a-z0-9_-]`, 1–32 characters, unique |
| `name` | string | the `id` | button label (`/v1/info`, `/account`) |
| `issuer` | URL | **required** | discovery at `{issuer}/.well-known/openid-configuration`; `https` (`http` only for `localhost` / `127.0.0.1`) |
| `client_id` | string | **required** | |
| `client_secret_file` | path | unset | file whose content is the client secret (read at sign-in time; never inline in the config). Omit for a public client (PKCE only). Under systemd it must be readable by the `canvas-cloud` user |
| `allowed_domains` | list | `[]` | only verified emails in these domains may sign in (empty = any) |
| `redirect_uris` | list of URLs | `[]` | extra places the result may be sent, exact match (same-origin URLs of `public_url` are always allowed) |

## Settings stored in the database

Two runtime settings live in the `server_settings` table, not in the file, and take effect
immediately:

| Setting | Default | Change with |
|---|---|---|
| `allow_signup` | `false` | `canvas-cloud admin settings set allow_signup true\|false` |
| `signup_domains` | none | `canvas-cloud admin settings set signup_domains a.com,b.com` (`""` clears) |

Linked apps (`canvas-cloud apps …`) and their `allow_import` / `trust_email` flags are also in
the database ([apps.md](apps.md)).

## Minimal example

Plain HTTP on loopback, for development:

```toml
listen = "127.0.0.1:8480"
database_url = "postgres://localhost/canvas_cloud"
master_key = "REPLACE-WITH-canvas-cloud-gen-config-OUTPUT-43-chars"

[tls]
mode = "off"
```

`public_url` becomes `http://localhost:8480`. Generate the key with
`canvas-cloud gen-config --write - | grep master_key`, or any 32 random bytes in base64url
(`head -c 32 /dev/urandom | base64 | tr '+/' '-_' | tr -d '=\n'`).

## Production example

Behind a TLS proxy on the same machine, with a hosted web app, one OIDC provider and the
metrics listener on loopback:

```toml
listen = "127.0.0.1:8480"
public_url = "https://canvas.example.com"
database_url = "postgres://canvas_cloud:REDACTED@127.0.0.1:5432/canvas_cloud"
db_max_connections = 20
master_key = "REPLACE-WITH-canvas-cloud-gen-config-OUTPUT-43-chars"
trust_proxy = true
web_root = "/opt/canvas-app/dist/web"
metrics_listen = "127.0.0.1:9474"
log_format = "json"
log_level = "info"

[tls]
mode = "off"

[cors]
allowed_origins = []

[ops]
extra_url_schemes = []

[limits]
auth_per_minute = 10

[[oidc]]
id = "google"
name = "Google"
issuer = "https://accounts.google.com"
client_id = "1234.apps.googleusercontent.com"
client_secret_file = "/etc/canvas-cloud/google-secret"
allowed_domains = ["example.com"]
```

Keep the file mode 0600 (`doctor` warns otherwise): it holds the master key and the database
password.
