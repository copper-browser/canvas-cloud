# Operations

Running canvas-cloud in production: the service, logs, health checks, metrics, backups,
upgrades and common problems. Installation is in [install.md](install.md); every config key
is in [configuration.md](configuration.md).

## systemd

The installer writes `/etc/systemd/system/canvas-cloud.service`, identical to
[`packaging/canvas-cloud.service`](../packaging/canvas-cloud.service) (CI checks that
`install.sh --print-unit` matches it). It:

- runs `/usr/local/bin/canvas-cloud serve` as user and group `canvas-cloud`, with
  `CANVAS_CLOUD_CONFIG=/etc/canvas-cloud/canvas-cloud.toml`;
- restarts always (2 s delay), stops with `SIGTERM` (20 s timeout), and allows 65536 open
  files;
- grants `CAP_NET_BIND_SERVICE` so the service can bind :443 without root;
- sandboxes the process: the filesystem is read-only except `/var/lib/canvas-cloud`
  (`StateDirectory`, mode 0750), `/etc/canvas-cloud` is read-only, `/home` is hidden
  (`ProtectHome`), plus private `/tmp` and devices, no new privileges, a system-call filter
  and `UMask=0077`.

Consequences: a `web_root`, a certificate or an OIDC `client_secret_file` must live outside
`/home` and be readable by `canvas-cloud`. The ACME cache and a generated certificate must
be under `/var/lib/canvas-cloud`, the only writable path. To change the unit, use a drop-in
(`systemctl edit canvas-cloud`) rather than editing the file; a re-run of the installer
rewrites it.

```sh
sudo systemctl status canvas-cloud
sudo systemctl restart canvas-cloud     # after editing the config
sudo journalctl -u canvas-cloud -f
```

On `SIGTERM` or `SIGINT` the server stops accepting connections, ends event streams, closes
every canvas WebSocket with `1001` (clients reconnect), and gives in-flight requests 10 s.
No edit is lost: every update is written to PostgreSQL before it is broadcast.

## Command line

Every command takes `--config PATH` (default `CANVAS_CLOUD_CONFIG`, else
`/etc/canvas-cloud/canvas-cloud.toml`) and reads the same config as the server, so on an
installed server run it with `sudo`. `admin` and `apps` talk to the database directly; the
server does not need to be running. Every change they make is recorded in `admin_audit`
with actor `cli`.

| Command | What |
|---|---|
| `serve [--no-migrate]` | run the server (the default command) |
| `migrate` | apply pending migrations and exit |
| `doctor` | check the installation; non-zero exit on failure ([below](#canvas-cloud-doctor)) |
| `healthcheck [--wait N]` | probe `/healthz` on the listen address, retrying for `N` seconds; prints `healthy` |
| `tls-init [--force] [--name N]…` | create the self-signed certificate if missing (`--force` replaces it; `--name` adds names); prints its fingerprint |
| `gen-config …` | write a new config with a fresh master key ([install.md](install.md#from-source)) |
| `version` | print the version |
| `admin users list` | users with status, sign-in kind and canvas count |
| `admin users create --email E [--display-name N] --password-file F` | create a password user, also while signup is closed |
| `admin users disable --email E` / `enable --email E` | block or allow sign-in; disable revokes sessions and access tokens |
| `admin users set-password --email E --password-file F` | set a password and revoke every session |
| `admin users delete --email E [--yes]` | delete the user, the canvases they own and their memberships (asks unless `--yes`; without a terminal it needs `--yes`) |
| `admin settings get` | show `allow_signup` and `signup_domains` |
| `admin settings set allow_signup true\|false` / `set signup_domains a.com,b.com` | the signup policy ([auth.md](auth.md#signup-policy)) |
| `apps create --name N [--allow-import] [--no-trust-email]` | register a linked app; prints its `cva_…` key once |
| `apps list` | linked apps (never their keys) |
| `apps set ID [--allow-import true\|false] [--trust-email true\|false]` | change an app's policy |
| `apps rotate ID` | new key, printed once; the old key stops working |
| `apps revoke ID` | disable the app's key (its users and canvases stay) |

`--password-file` reads the first line of a file, or of stdin with `-`; passwords are never
command-line arguments.

## Logs

One line per event on stdout (journald under systemd): JSON by default (`log_format`),
filtered by `log_level` or `RUST_LOG`. Each HTTP request logs one `request` line with a
`span` object: `method`, `path` (never the query string, which may hold `?token=`), `route`,
`ip`, `user_id` / `app_id` once authenticated, `status`, `latency_ms`, and for errors
`error` (the machine-readable code) and, on `400`s, `error_message`. `/healthz` is logged at
`debug` only. `5xx` responses are logged at `ERROR` with the underlying cause on a preceding
line:

```json
{"timestamp":"2026-10-19T20:38:07.572047Z","level":"ERROR","message":"internal error","error":"blob failed authentication (wrong key/aad or tampered)","target":"canvas_cloud_core::error","span":{"ip":"127.0.0.1","method":"GET","path":"/v1/canvases/personal/read","route":"/v1/canvases/{id}/read","user_id":"01a155da-…","name":"request"}}
```

Tokens, passwords, keys, document contents and update bytes are never logged. At start the
server logs its whole config with the database password and master key redacted. MCP tool
calls log `mcp tool call` with the tool, user, outcome and duration
([agents.md](agents.md#observability)).

## `canvas-cloud doctor`

Checks the installation and exits non-zero if any check fails:

| Check | Fails when | Warns when |
|---|---|---|
| `config` | the config does not load (the error says which key) | the file is readable by group or others |
| `database` | it cannot connect within 10 s | |
| `migrations` | migrations are pending (run `canvas-cloud migrate`) | |
| `accounts` | | (informational: users, live sessions, linked apps, signup state) |
| `tls` | `self-signed`: the certificate is missing, expired or does not match the key | `off`; `self-signed` expiring within 30 days or not naming the `public_url` host; `acme` cache not created yet, or the port is not 443 |
| `listener` | `/healthz` on the listen address does not answer `200 ok` (with `self-signed`, the presented certificate must be the configured one) | |
| `web` | `web_root` is set but is not a directory with `index.html` | |
| `share links`, `cors` | | (informational) |
| `metrics` | | the listener is not on loopback |
| `proxy` | | `trust_proxy = true` while serving TLS directly |

Run it as a user that can read the config (`sudo canvas-cloud doctor` on an installed server).
`canvas-cloud healthcheck --wait N` only probes `/healthz`, retrying for up to `N` seconds
(the installer uses it after a restart).

## Health and metrics endpoints

| Endpoint | Where | What |
|---|---|---|
| `GET /healthz` | main listener, public | `200 ok` (text) while the process serves HTTP. It does not check the database; use `doctor` or the `db_pool_*` metrics for that |
| `GET /v1/info` | main listener, public | name, version, `public_url`, signup state, sign-in methods, features, limits |
| `GET /metrics` | `metrics_listen` (default `127.0.0.1:9474`), a separate listener | Prometheus text format |

Keep the metrics listener on loopback or firewalled: it has no authentication. A metric
appears after its first event, so a fresh process shows only a few.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `http_requests_total` | counter | `route`, `status` | HTTP requests (`route` is the matched route pattern, or `unmatched`) |
| `http_request_duration_seconds` | histogram | `route` | time to response headers |
| `sse_subscribers` | gauge | `stream="user"` | open `GET /v1/events` streams |
| `canvas_rooms` | gauge | | canvas rooms loaded in memory |
| `canvas_peers` | gauge | | WebSocket peers in rooms |
| `canvas_updates_persisted_total` | counter | | Yjs updates written to the update log |
| `canvas_update_bytes_total` | counter | | bytes of those updates |
| `canvas_compactions_total` | counter | | snapshot compactions |
| `canvas_ws_bytes_in_total` / `canvas_ws_bytes_out_total` | counter | | WebSocket payload bytes |
| `canvas_mentions_total` | counter | | chat @mentions recorded |
| `canvas_mentions_rate_limited_total` | counter | | mention posts refused by the per-user limit |
| `auth_rate_limited_total` | counter | | `/v1/auth/*` requests refused by the per-IP limit |
| `auth_email_rate_limited_total` | counter | | signup/login/password calls refused by the per-email limit |
| `mcp_tool_calls_total` | counter | `tool`, `outcome` (`ok`/`error`) | MCP tool calls |
| `db_pool_size` / `db_pool_idle` | gauge | | PostgreSQL pool connections (refreshed on scrape) |
| `process_uptime_seconds` | gauge | | uptime (refreshed on scrape) |

## Backups and restore

Everything that matters is in three places:

| What | Where | Without it |
|---|---|---|
| **The database** | `database_url` (installer: local database `canvas_cloud`) | everything is gone: users, canvases, apps, settings |
| **The master key** | `master_key` in the config (`/etc/canvas-cloud/canvas-cloud.toml`) | the database restores, people can sign in, but **no canvas can be opened**: every document, snapshot and mention excerpt is encrypted under a key wrapped by it (reads fail with `500`, logged as `blob failed authentication`) |
| The certificate | `tls.cert_path` / `tls.key_path` (`/var/lib/canvas-cloud/tls/`) in `self-signed` mode | a new certificate has a new fingerprint; clients that pinned the old one must re-pin |

Also worth keeping: the rest of the config, OIDC `client_secret_file`s, and the ACME cache
(`/var/lib/canvas-cloud/acme`; it can be re-ordered, within Let's Encrypt's rate limits).
Store the master key separately from the database dumps: together they decrypt every canvas.

`pg_dump` takes a consistent snapshot while the server runs; the update log, snapshots and
everything else are in that one database:

```sh
# Installer layout (local database, peer auth as postgres):
sudo -u postgres pg_dump -Fc canvas_cloud > canvas_cloud-$(date +%F).dump
sudo cp /etc/canvas-cloud/canvas-cloud.toml /secure/place/
sudo tar -C /var/lib/canvas-cloud -czf canvas-cloud-state-$(date +%F).tgz tls
```

Restore into an empty database and start the same (or a newer) version with the **same
master key**:

```sh
sudo systemctl stop canvas-cloud
sudo -u postgres dropdb canvas_cloud
sudo -u postgres createdb -O canvas_cloud canvas_cloud
sudo -u postgres pg_restore --no-owner --role=canvas_cloud -d canvas_cloud canvas_cloud-2026-10-19.dump
sudo systemctl start canvas-cloud
sudo canvas-cloud doctor
```

A newer binary applies its newer migrations to the restored database at start. This round
trip (dump, restore into a fresh database, read a canvas with the same key, and the `500`
with a different key) was checked against v0.1.0. Use a `pg_dump` / `pg_restore` of the
server's major version or newer (a newer `pg_restore` may report one ignorable error about
`transaction_timeout` on an older server).

To move a server, restore the dump on the new host and copy the config (with its master key)
and the state directory, then start it.

## Upgrades and migrations

Upgrade by re-running the installer, or by replacing the binary and restarting
([install.md](install.md#upgrading)). Take a backup first.

Migrations are embedded in the binary (`crates/canvas-cloud/migrations`) and tracked by
sqlx in `_sqlx_migrations`. `serve` applies pending ones at start under a PostgreSQL
advisory lock, so concurrent starts are safe; `canvas-cloud migrate` applies them without
starting; `serve --no-migrate` starts without them (and logs a warning); `doctor` fails while
any are pending. Files are numbered by owner:

| Range | Owner | v0.1.0 |
|---|---|---|
| `0001`–`0099` | core identity | users, sessions, access tokens, apps, server settings, admin audit |
| `0100`–`0199` | canvas engine | canvases, share links, invite nudge, mentions |
| `0200`–`0299` | linked apps | import provenance |
| `0300`–`0399` | human sign-in | OIDC states |

Migrations only go forward. A binary refuses to start (`migration N was previously applied
but is missing in the resolved migrations`) on a database that a newer version has
migrated. To downgrade across a migration, restore the backup taken before the upgrade.

## Keys and credentials

- **Master key: rotation is not supported** in v0.1.0. There is no command that re-wraps the
  canvas keys under a new master key, and changing `master_key` makes every existing canvas
  unreadable. Never run `gen-config --force` over a live config: it writes a new key.
- **App keys**: `canvas-cloud apps rotate <id>` prints a new `cva_…` key once; the old one
  stops working at once. `apps revoke <id>` disables the app (its users and canvases stay).
- **People**: `admin users disable --email …` blocks sign-in and revokes sessions and
  tokens; `admin users set-password --email … --password-file …` revokes every session.
  Because the CLI is a separate process, sockets that are already open stay open until they
  next authenticate; restart the service to cut them at once.
- Expired sessions, and access tokens expired for over a day, are purged every hour.

## Capacity

canvas-cloud is a **single process per database**. Live rooms and the event buffer are in
memory: do not run two processes against one database (their rooms would not see each
other's live edits, and event ids would interleave). Scale up, not out.

- **Rooms**: one in memory per open canvas, holding the decoded Yjs document; evicted 60 s
  after the last peer and REST use. Memory grows with the size of open documents (images are
  stored in documents as `data:` URLs up to 2 MiB each). Watch `canvas_rooms` and
  `canvas_peers`.
- **Writes**: every update is one sealed `INSERT`; every 200 updates a canvas is compacted
  into one snapshot and the folded updates are deleted.
- **Connections**: each WebSocket peer and each event stream holds a socket (the unit allows
  65536 files). Peers are pinged every 30 s and dropped after 90 s of silence; a slow peer is
  disconnected (`1013`) and resyncs.
- **Database**: `db_max_connections` (default 20); requests wait at most 5 s for a
  connection.
- **Passwords**: Argon2id with 64 MiB per hash, at most 2–8 hashes at once (the CPU count,
  clamped), so a burst of logins queues instead of exhausting memory.
- **Fixed limits** (WebSocket frames 8 MiB, ops bodies 4 MiB / 500 ops, other bodies
  64 KiB, …): [canvas.md](canvas.md#limits). Configurable: `limits.*`
  ([configuration.md](configuration.md#limits)).
- **Events**: the last 4096 events are kept for `Last-Event-ID` resume; a client further
  behind is told to resync. They are not persisted; after a restart clients resync.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Service restarts in a loop; journal shows `invalid configuration` | a bad or unknown key, a missing `database_url` / `master_key` | fix the key named in the error; `sudo canvas-cloud doctor` |
| `doctor`: `database … timed out` or `connecting to …` | PostgreSQL down, wrong URL/password, `pg_hba.conf` | `systemctl status postgresql`; test with `psql -w "$URL"` |
| `serve` exits with `migration N was previously applied but is missing` | an older binary on a database a newer one migrated | run the newer binary, or restore the pre-upgrade backup |
| `doctor`: `migrations … pending` | started with `--no-migrate`, or `migrate` not run | `sudo canvas-cloud migrate` |
| `doctor`: `listener … nothing is listening` | the service is stopped or crashed | `systemctl status canvas-cloud`, `journalctl -u canvas-cloud` |
| `doctor`: `listener … port is in use but /healthz did not answer correctly` | another program holds the port, or the TLS mode in the config differs from the running server | `ss -ltnp`; restart after config changes |
| Bind fails: `Address already in use` | another service holds :443 | use `listen = "127.0.0.1:8480"` with `tls.mode = "off"` behind a proxy ([install.md](install.md#behind-a-reverse-proxy)) |
| Clients warn about the certificate | `self-signed` mode | pin the fingerprint from `doctor`, use `acme`, or put a proxy with a CA certificate in front |
| ACME: no certificate, `acme` errors in the journal | DNS does not point here, port 443 not reachable from the internet, or a Let's Encrypt rate limit | check DNS and the firewall; test with `tls.acme_staging = true` |
| Every canvas read is `500`; journal: `blob failed authentication` | the master key does not match the database (new config, `gen-config --force`, wrong restore) | put the original `master_key` back and restart |
| Live editing never connects behind a proxy (the WebSocket upgrade fails) | the proxy does not pass `Upgrade` / `Connection` | use the [proxy snippets](install.md#behind-a-reverse-proxy) |
| `/v1/events` delivers nothing until the stream ends | proxy buffering | `proxy_buffering off` (nginx) / `flush_interval -1` (Caddy) |
| `413` on `/ops` or imports, but not from canvas-cloud's logs | the proxy's body limit (nginx: 1 MiB default) | raise `client_max_body_size` |
| Everyone shares one rate-limit bucket (`429 rate_limited` on login) behind a proxy | `trust_proxy` is off, so the proxy's IP is the client IP | set `trust_proxy = true` |
| Browser app: CORS errors | its origin is not in `cors.allowed_origins` | add the exact origin (`scheme://host[:port]`) and restart |
| MCP from a browser: `403 Forbidden: this Origin may not call /mcp` | the page's origin is neither `public_url`'s origin nor in `cors.allowed_origins` | add it to `cors.allowed_origins` |
| Links in `/llms.txt`, share links or OIDC redirects point at `localhost` or the wrong host | `public_url` unset or wrong | set `public_url` to what clients use |
| OIDC: `400 redirect_uri_not_allowed` | the `redirect_uri` is neither in `redirect_uris` nor same-origin with `public_url` | add it to the provider's `redirect_uris` |
| OIDC: `502 oidc_unavailable` on start, or `#error=provider_unavailable` | the issuer is unreachable, or the `client_secret_file` is unreadable (under systemd: outside `/home`, readable by `canvas-cloud`) | check the journal; fix the file's path or permissions |
| A disabled user still edits in an open canvas | the CLI cannot reach live sockets | `systemctl restart canvas-cloud` |
