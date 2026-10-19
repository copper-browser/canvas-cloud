# Installing canvas-cloud

canvas-cloud is one binary (`canvas-cloud`) plus a PostgreSQL database (14 or newer). It
serves HTTPS itself (a self-signed certificate or Let's Encrypt), or plain HTTP behind a
reverse proxy that terminates TLS. Pick one path:

| Path | For |
|---|---|
| [One-line installer](#one-line-installer) | a Linux VM with systemd: binary, service user, native PostgreSQL, config, certificate, systemd unit |
| [Release tarball](#release-tarball) | the same installer without piping from the network, or a manual install |
| [From source](#from-source) | development, other platforms |
| [Behind a reverse proxy](#behind-a-reverse-proxy) | Caddy, nginx or another proxy already holds :443 |

Then [create the first account](#first-account), and read [operations.md](operations.md)
for backups, upgrades and monitoring. Every config key is in
[configuration.md](configuration.md).

## One-line installer

The command is in the [README](../README.md#quickstart): it downloads `install.sh` from the
repository's `main` branch and pipes it to `sudo sh`. To read the script first, download it
(or use the copy in a [release tarball](#release-tarball)) and run `sudo sh install.sh`.

Supported: Ubuntu 22.04/24.04, Debian 12, Amazon Linux 2023 and Fedora (`apt` or `dnf`),
`x86_64` or `aarch64`, with systemd. It must run as root. What it does, in order:

1. Installs `curl`, `tar` and CA certificates if missing.
2. Puts the binary at `/usr/local/bin/canvas-cloud`. The first source that exists wins:
   `CANVAS_CLOUD_BINARY`, a `canvas-cloud` binary next to `install.sh` (an extracted
   tarball), `CANVAS_CLOUD_BINARY_URL`, else the GitHub release (`CANVAS_CLOUD_VERSION`,
   default latest), checked against the release's `SHA256SUMS` when it has one.
3. Creates the system user `canvas-cloud`, `/var/lib/canvas-cloud` (state, mode 0750) and
   `/etc/canvas-cloud` (mode 0750, group `canvas-cloud`).
4. **Only if `/etc/canvas-cloud/canvas-cloud.toml` does not exist:**
   - without `DATABASE_URL`: installs the distribution's PostgreSQL package (never a
     container), adds two `scram-sha-256` lines for the role `canvas_cloud` on
     `127.0.0.1/32` and `::1/128` to `pg_hba.conf`, and creates the role (random 40-character
     password, sent over stdin) and the database `canvas_cloud`;
   - writes the config with `canvas-cloud gen-config` (a fresh master key unless
     `CANVAS_CLOUD_MASTER_KEY` is given), mode 0600, owned by `canvas-cloud`.
5. `canvas-cloud tls-init` (creates the self-signed certificate in `self-signed` mode;
   nothing in the other modes), then `canvas-cloud migrate`.
6. Installs `/etc/systemd/system/canvas-cloud.service` (the same file as
   [`packaging/canvas-cloud.service`](../packaging/canvas-cloud.service)), enables it,
   opens the port in `ufw` or `firewalld` when one is active (not for a loopback listener
   and not when TLS is off), and restarts the service.
7. Waits for `/healthz` (60 s; 180 s with ACME), then runs `canvas-cloud doctor` and prints
   the next steps.

Re-running it is an upgrade: the config, master key, certificate and database are kept; the
binary is replaced, migrations run and the service restarts.

### Installer inputs

All environment variables, all optional. Except for the binary-source variables, they only
shape the config of a **fresh** install; on a re-run they are ignored with a warning (edit
the config instead). With `sudo`, pass them after `sudo` so they survive it:
`curl -fsSL …/install.sh | sudo CANVAS_CLOUD_DOMAIN=canvas.example.com sh`.

| Variable | Default | Meaning |
|---|---|---|
| `DATABASE_URL` | install PostgreSQL locally | use this existing PostgreSQL (`postgres://user:pass@host:5432/db`) instead |
| `CANVAS_CLOUD_PORT` | `443` | port to listen on, on all interfaces (`0.0.0.0:PORT`) |
| `CANVAS_CLOUD_LISTEN` | `0.0.0.0:$CANVAS_CLOUD_PORT` | full listen address `ip:port`, e.g. `127.0.0.1:8480` or `[::1]:8480`; overrides `CANVAS_CLOUD_PORT`. The host must be an IP address |
| `CANVAS_CLOUD_TLS_MODE` | `acme` if `CANVAS_CLOUD_DOMAIN` is set, else `self-signed` | `off`, `self-signed` or `acme` (see [TLS modes](#tls-modes)) |
| `CANVAS_CLOUD_DOMAIN` | — | the public domain. With the default TLS mode it turns on Let's Encrypt for it; with `self-signed` it is added to the certificate; with `off` it is only the host of `public_url` |
| `CANVAS_CLOUD_ACME_EMAIL` | — | contact email for Let's Encrypt |
| `CANVAS_CLOUD_PUBLIC_HOST` | detected public IP | host or IP clients connect to (used when there is no domain) |
| `CANVAS_CLOUD_PUBLIC_URL` | derived, see below | the base URL clients use, e.g. `https://canvas.example.com`; wins over everything else (only `scheme://host[:port]` is kept) |
| `CANVAS_CLOUD_MASTER_KEY` | generated | the master key, base64url of 32 bytes (to reinstall over an existing database) |
| `CANVAS_CLOUD_BINARY` | — | local path to a `canvas-cloud` binary or release `.tar.gz` |
| `CANVAS_CLOUD_BINARY_URL` | — | URL of a binary or release `.tar.gz` |
| `CANVAS_CLOUD_VERSION` | latest | GitHub release to download, e.g. `0.1.0` |
| `GITHUB_TOKEN` | — | token for downloading releases from a private fork |

`public_url` is derived when `CANVAS_CLOUD_PUBLIC_URL` is not set:

| TLS mode | Host given (`CANVAS_CLOUD_PUBLIC_HOST` or `CANVAS_CLOUD_DOMAIN`) | No host given |
|---|---|---|
| `self-signed`, `acme` | `https://HOST`, plus `:PORT` when the port is not 443 | same, with the detected public IP |
| `off` | `https://HOST` (clients come through a TLS proxy on 443) | loopback listener: `http://LISTEN` (e.g. `http://127.0.0.1:8480`); otherwise `http://DETECTED-IP:PORT` |

The public IP is detected from EC2 instance metadata, then `api.ipify.org`,
`ifconfig.me` and `icanhazip.com`, then the first local address.

### Installer flags

| Flag | What |
|---|---|
| `--dry-run` | print the listen address, TLS mode, `public_url`, database plan and firewall action a fresh install would use, and the `gen-config` command; changes nothing and needs no root (it may query the public-IP services above) |
| `--print-unit` | print the systemd unit |
| `--uninstall` | stop and remove the service and the binary; keep `/etc/canvas-cloud`, the state and the database |
| `--purge` | also drop the local `canvas_cloud` database and role, delete `/etc/canvas-cloud` (config **and master key**) and `/var/lib/canvas-cloud`, and remove the `canvas-cloud` user |
| `--help` | print the inputs and flags |

```sh
CANVAS_CLOUD_LISTEN=127.0.0.1:8480 CANVAS_CLOUD_TLS_MODE=off \
  CANVAS_CLOUD_DOMAIN=canvas.example.com sh install.sh --dry-run
```

```text
listen       127.0.0.1:8480
tls mode     off
public url   https://canvas.example.com
database     local native PostgreSQL: role canvas_cloud, database canvas_cloud, password generated
master key   generated
config       /etc/canvas-cloud/canvas-cloud.toml (0600, owner canvas-cloud)
state        /var/lib/canvas-cloud
firewall     not opened (loopback only)

The config would be written with:
  canvas-cloud gen-config --write /etc/canvas-cloud/canvas-cloud.toml --public-url https://canvas.example.com --listen 127.0.0.1:8480 \
    --state-dir /var/lib/canvas-cloud --tls-mode off
```

### Examples

Shown with a downloaded `install.sh`; the same variables work with the piped form.

```sh
# Public IP, self-signed certificate on :443.
sudo sh install.sh

# A domain that points at this machine, Let's Encrypt on :443.
sudo CANVAS_CLOUD_DOMAIN=canvas.example.com CANVAS_CLOUD_ACME_EMAIL=ops@example.com sh install.sh

# Behind a reverse proxy on the same machine that serves https://canvas.example.com.
sudo CANVAS_CLOUD_LISTEN=127.0.0.1:8480 CANVAS_CLOUD_TLS_MODE=off \
     CANVAS_CLOUD_DOMAIN=canvas.example.com sh install.sh

# Next to another service on the same machine that talks to it over loopback only
# (public_url becomes http://127.0.0.1:8480), with an existing database.
sudo CANVAS_CLOUD_LISTEN=127.0.0.1:8480 CANVAS_CLOUD_TLS_MODE=off \
     DATABASE_URL='postgres://canvas_cloud:…@db.internal:5432/canvas_cloud' sh install.sh
```

With an external `DATABASE_URL`, create the role and database first. The role should own
the database: migrations create tables and indexes in `public`. No extension or superuser
right is needed, so managed PostgreSQL services work.

```sql
CREATE ROLE canvas_cloud LOGIN PASSWORD '…';
CREATE DATABASE canvas_cloud OWNER canvas_cloud;
```

## Release tarball

Each release on the repository's GitHub Releases page has
`canvas-cloud-<version>-x86_64-unknown-linux-gnu.tar.gz`,
`canvas-cloud-<version>-aarch64-unknown-linux-gnu.tar.gz` and `SHA256SUMS`. The binaries
need glibc 2.28 or newer. Each tarball holds `canvas-cloud`, `install.sh`,
`packaging/canvas-cloud.service`, `README.md` and `LICENSE` at its root.

Download the tarball for your architecture and `SHA256SUMS` from the release, then:

```sh
v=0.1.0; t=x86_64-unknown-linux-gnu
sha256sum --ignore-missing -c SHA256SUMS
mkdir canvas-cloud && tar -xzf canvas-cloud-$v-$t.tar.gz -C canvas-cloud && cd canvas-cloud
sudo ./install.sh            # uses ./canvas-cloud and ./packaging/canvas-cloud.service
```

To install by hand instead: copy the binary to `/usr/local/bin`, write a config with
`canvas-cloud gen-config` (below), and install the unit from `packaging/`. The unit runs as
user `canvas-cloud`, reads `/etc/canvas-cloud/canvas-cloud.toml` and keeps state in
`/var/lib/canvas-cloud`; see [operations.md](operations.md#systemd).

## From source

Needs Rust 1.85 or newer (`rust-toolchain.toml` pins `stable`) and a native PostgreSQL.

From a clone of the repository:

```sh
cargo build --release --locked -p canvas-cloud
createdb canvas_cloud
target/release/canvas-cloud gen-config --write ./canvas-cloud.toml \
  --tls-mode off --listen 127.0.0.1:8480 \
  --database-url postgres://localhost/canvas_cloud --state-dir ./state
target/release/canvas-cloud --config ./canvas-cloud.toml migrate
target/release/canvas-cloud --config ./canvas-cloud.toml serve
```

```sh
target/release/canvas-cloud --config ./canvas-cloud.toml healthcheck --wait 30   # → healthy
target/release/canvas-cloud --config ./canvas-cloud.toml doctor
curl -s http://127.0.0.1:8480/v1/info
```

`gen-config` flags:

| Flag | Default | |
|---|---|---|
| `--write PATH` | `--config`, `CANVAS_CLOUD_CONFIG` or `/etc/canvas-cloud/canvas-cloud.toml` | `-` prints to stdout. An existing file is refused unless `--force` |
| `--force` | | overwrite; this generates a **new** master key unless `--master-key` is given |
| `--public-url URL` | none written; [derived](configuration.md#public_url) at start | |
| `--listen ADDR` | `0.0.0.0:443` | |
| `--tls-mode MODE` | `self-signed` | `self-signed`, `acme` or `off` |
| `--domain`, `--acme-email` | | ACME settings (`--domain` is also added to a self-signed certificate) |
| `--database-url URL` | `postgres://canvas_cloud@localhost:5432/canvas_cloud` | or env `CANVAS_CLOUD_DATABASE_URL`, which keeps a password out of `ps` |
| `--master-key KEY` | generated | or env `CANVAS_CLOUD_MASTER_KEY` |
| `--state-dir DIR` | `/var/lib/canvas-cloud` | the certificate goes to `DIR/tls/{cert,key}.pem`, the ACME cache to `DIR/acme` |

The file is written with mode 0600, and is parsed with the server's own loader before it is
written, so a config that `gen-config` writes always loads.

`serve` (the default command) applies pending migrations at start unless `--no-migrate`.

## TLS modes

`tls.mode` in the config; `CANVAS_CLOUD_TLS_MODE` for the installer.

| Mode | What | Needs |
|---|---|---|
| `self-signed` (default) | HTTPS with an ECDSA P-256 certificate valid for 10 years, created by `tls-init` or at first start, for the `public_url` host, `tls.domain`, `localhost` and `127.0.0.1` (`tls-init --name N` adds more). Clients must trust or pin it: `doctor` and the `tls-init` output print its SHA-256 fingerprint | nothing |
| `acme` | HTTPS with a Let's Encrypt certificate for `tls.domain`, ordered and renewed in-process (TLS-ALPN-01). `tls.acme_staging = true` uses the staging directory | `tls.domain` resolving to this machine, the server reachable from the internet on port 443 |
| `off` | plain HTTP | a TLS-terminating proxy in front, or a loopback-only listener |

The fingerprint is the lower-case hex SHA-256 of the certificate's DER bytes, the same as
`openssl x509 -in cert.pem -outform DER | sha256sum`. `tls-init --force` replaces the
certificate, which changes the fingerprint: clients that pinned the old one stop
connecting.

To bring your own certificate, keep `mode = "self-signed"` and point `tls.cert_path` /
`tls.key_path` at your PEM files (chain first, then the key; PKCS#8, SEC1 or PKCS#1). The
server only generates a certificate when a file is missing. It reads the files at start, so
restart after renewing them. (`doctor` still labels the mode `self-signed` and checks the
certificate's names and expiry.)

## Behind a reverse proxy

Run canvas-cloud with TLS off on loopback and let the proxy terminate TLS:

```toml
listen = "127.0.0.1:8480"
public_url = "https://canvas.example.com"   # what clients see, not the loopback address
trust_proxy = true                          # client IPs from X-Forwarded-For

[tls]
mode = "off"
```

(The installer writes this with `CANVAS_CLOUD_LISTEN=127.0.0.1:8480 CANVAS_CLOUD_TLS_MODE=off
CANVAS_CLOUD_DOMAIN=canvas.example.com`, except `trust_proxy`, which you turn on yourself.)

The proxy must:

- pass **WebSocket upgrades** on `/v1/canvases/{id}/ws` (live rooms);
- **not buffer** responses: `GET /v1/events` and `GET /v1/apps/self/events` are server-sent
  event streams;
- allow **long-lived connections**: rooms ping every 30 s and event streams send a keepalive
  every 15 s, so any read timeout above a minute works; the snippets use an hour;
- accept **large bodies**: `POST /v1/canvases/{id}/ops` takes up to 4 MiB and
  `POST /v1/apps/self/import` up to `limits.import_max_bytes` (512 MiB by default);
- send `X-Forwarded-For` with the client address as the **last** entry. With
  `trust_proxy = true` canvas-cloud uses the right-most `X-Forwarded-For` address (else
  `X-Real-IP`) for rate limits and logs. Only turn it on when the proxy is the only way in:
  a client that reaches the port directly can then pick its own IP.

Both snippets below were tested with a WebSocket room join and a `/v1/events` stream (the
event arrived in about 25 ms through each proxy).

### Caddy

Caddy obtains the certificate, proxies WebSockets, and flushes event streams by itself.

```caddyfile
canvas.example.com {
	reverse_proxy 127.0.0.1:8480 {
		flush_interval -1
	}
}
```

### nginx

```nginx
map $http_upgrade $connection_upgrade {
    default upgrade;
    ''      close;
}

server {
    listen 443 ssl;
    server_name canvas.example.com;
    ssl_certificate     /etc/letsencrypt/live/canvas.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/canvas.example.com/privkey.pem;

    client_max_body_size 512m;          # imports; ops calls need 4m

    location / {
        proxy_pass http://127.0.0.1:8480;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header Upgrade $http_upgrade;       # WebSocket rooms
        proxy_set_header Connection $connection_upgrade;
        proxy_buffering off;                          # server-sent events
        proxy_request_buffering off;                  # stream imports
        proxy_read_timeout 1h;
        proxy_send_timeout 1h;
    }
}
```

nginx's default `client_max_body_size` is 1 MiB, which rejects larger ops calls and
imports with `413` before they reach canvas-cloud.

### Serving a path prefix

Not supported: canvas-cloud expects to own the root of its origin (`/v1`, `/mcp`,
`/llms.txt`, `/account`, `/join/{token}`, `/healthz`). Give it its own host name.

## First account

There is no admin role in the HTTP API. Whoever can run the `canvas-cloud` CLI against the
config (it needs to read the config file, so root on an installed server) administers the
instance: accounts, signup policy and linked apps.

On a fresh server, **anyone who reaches it first may sign up**: until a password account
exists, `POST /v1/auth/signup` is open (see [auth.md](auth.md#signup-policy)). Create your
own account before you expose the server:

```sh
sudo canvas-cloud admin users create --email you@example.com --password-file -
# type the password, then Enter and Ctrl-D; or --password-file /path/to/file (first line)
```

Passwords are never accepted on the command line. After that, signup is closed until you
open it:

```sh
sudo canvas-cloud admin settings set allow_signup true
sudo canvas-cloud admin settings set signup_domains example.com,example.org   # optional; "" clears
sudo canvas-cloud admin settings get
```

On an installed server the CLI finds the config at `/etc/canvas-cloud/canvas-cloud.toml`; use
`--config PATH` (or `CANVAS_CLOUD_CONFIG`) elsewhere. People then sign in at
`https://<server>/account`, where they can also create personal access tokens for agents
([agents.md](agents.md)). To let another server act for its own users, create a linked app:
`sudo canvas-cloud apps create --name "My app"` ([apps.md](apps.md)). Sign-in with an OIDC
provider is configured in [auth.md](auth.md#oidc-sign-in).

## Upgrading

- **Installer**: re-run it (optionally with `CANVAS_CLOUD_VERSION`). It keeps the config and
  data, applies migrations and restarts the service.
- **By hand**: replace `/usr/local/bin/canvas-cloud`, then `systemctl restart canvas-cloud`.
  `serve` applies pending migrations at start; or run `canvas-cloud migrate` first.

Take a database backup first. Details, and why a downgrade after a new migration does not
start, are in [operations.md](operations.md#upgrades-and-migrations).

## Uninstalling

```sh
sudo sh install.sh --uninstall   # service + binary; keeps config, master key, state, database
sudo sh install.sh --purge       # also drops the local database + role, config, master key, state, user
```

`--purge` cannot be undone; back up the database and the config (it holds the master key)
first. An external `DATABASE_URL` database is never dropped. The installer does not remove
the PostgreSQL package or its `pg_hba.conf` lines.
