# Security

How canvas-cloud protects data and what it assumes about its environment. To report a
vulnerability, see [SECURITY.md](../SECURITY.md).

## Threat model

**Trusted**

- The host and whoever can read the config: the config holds the master key and the database
  password, and the `canvas-cloud` CLI administers the instance directly against the
  database (there is no admin role in the HTTP API).
- PostgreSQL and its operators, for availability and integrity (see below for what they can
  read).
- A reverse proxy in front, **only** when `trust_proxy = true`.
- Linked apps (`cva_…` keys): an app can create users and act as any user it provisioned, and
  with `trust_email` it can link an existing account by email. With `allow_sessions` as well
  (off unless an operator turns it on) it can also sign any of those people in with a full
  session — so such a key can become anyone it can link by email. Give app keys only to
  servers you trust as much as the accounts they will act for ([apps.md](apps.md)).

**Defended against**

- Anonymous internet clients: everything except `/healthz`, `/v1/info`, `/llms.txt`,
  `/v1/openapi.json`, the `/account` page assets, `/join/{token}` and signup/login needs a
  credential.
- Signed-in users reaching canvases they are not members of: membership is checked on every
  REST call, every MCP tool call and at the WebSocket upgrade, and non-members get `404`
  (the canvas's existence is not revealed). Removing a member or deleting a canvas closes
  their sockets at once.
- Password guessing: per-IP and per-email rate limits, Argon2id, and the same answer and
  timing for unknown emails as for wrong passwords.
- Browser-based attacks: no cookies carry credentials (bearer tokens only), so cross-site
  request forgery has nothing to ride on; CORS is an exact allow-list; `/mcp` checks
  `Origin` against DNS rebinding; the HTML pages send a strict CSP.
- Someone with a copy of the database but not the master key: canvas contents are
  encrypted.
- Stolen credentials from the database: tokens and keys are stored only as SHA-256 digests.

**Not defended against**

- Someone with the master key **and** the database, or with root on the host while the
  server runs (it holds the keys in memory).
- Metadata in the database: names of canvases, members, emails, share-link and mention
  metadata, and timestamps are plaintext.
- Traffic interception when `tls.mode = "off"` without a TLS proxy, or when a client accepts
  a self-signed certificate without checking its fingerprint.
- Denial of service beyond the per-route limits; put a proxy or firewall in front for that.

## What is stored, and how

| Data | Stored as |
|---|---|
| Passwords | Argon2id PHC string (m = 64 MiB, t = 3, p = 1, random salt) |
| Sessions `cvs_…`, access tokens `cvt_…`, app keys `cva_…` | SHA-256 of the token; the token itself is shown once and never stored |
| Share-link tokens, invite secrets | SHA-256 |
| OIDC sign-ins in flight | SHA-256 of `state`; the nonce, PKCE verifier and `redirect_uri` (10 minutes, single use) |
| Canvas documents: the Yjs update log and snapshots (shapes, text, images, chat) | AES-256-GCM under the canvas's own random 32-byte key, with the canvas id as associated data, so a row copied to another canvas does not decrypt |
| Mention excerpts | AES-256-GCM under the canvas key, associated data `canvas-cloud/v1/mention:` + the mention id |
| Canvas keys | wrapped (AES-256-GCM) by a key-encryption key derived from `master_key` with HKDF-SHA256 |
| Everything else (users, emails, display names, canvas names, members, invites by email, app links, settings, audit log) | plaintext |
| Awareness (cursors, selections, presence), events | memory only, never written |

The master key is in the config file only; nothing derived from it is in the database. If
it is lost, the documents cannot be recovered; there is no rotation in v0.1.0
([operations.md](operations.md#keys-and-credentials)).

Logs never contain tokens, passwords, keys, document contents or update bytes. The request
log records the path without the query string (where `?token=` may be). The config is logged
at start with the database password and master key redacted.

## Credentials

- Tokens are 32 random bytes in base64url with a prefix (`cvs_`, `cvt_`, `cva_`), checked by
  looking up the SHA-256 digest; a malformed token is rejected before any database work.
- Sessions expire 30 days after last use. Logging out, changing the password, or an admin
  disabling the account or setting a password revokes sessions; disabling also revokes access
  tokens. Already-open WebSockets are not cut by these (a session or token revoked through the
  API ends `GET /v1/events` within 60 s); restart the service to cut them at once.
- Access tokens have a scope: `read` allows only `GET`/`HEAD` and cannot join rooms or call
  MCP tools that write. Only a session can create tokens or change the password.
- The query-string form `?token=` is accepted only on WebSocket upgrades and event-stream
  `GET`s, where browsers cannot set headers.
- Passwords are 10–256 characters. The CLI never accepts one as an argument (only from a file
  or stdin).

## CORS and origin checks

- **CORS** is off by default: with `cors.allowed_origins = []` no CORS headers are sent, so
  browsers on other origins cannot read responses. Listed origins match exactly
  (`scheme://host[:port]`), without credentials mode (bearer tokens only), with a preflight
  cache of 600 s. A web app served from `web_root` is same-origin and needs no entry.
- **MCP `Origin` guard**: `POST /mcp` refuses with `403` any request whose `Origin` header is
  present and is neither `public_url`'s origin nor listed in `cors.allowed_origins` (a
  literal `null` origin is refused). This blocks DNS-rebinding attacks against a server on a
  private address. Requests without `Origin` (non-browser clients) pass this check and still
  need a credential.
- **OIDC redirects** go only to `redirect_uri`s listed for the provider or with
  `public_url`'s origin; the result travels in the URL fragment, which is not sent to servers
  or in `Referer`. A [delegated sign-in](auth.md#delegated-sign-in) `start` accepts only
  `public_url`'s origin, and the delegate must apply the same rule before it appends a token.
- **Sessions minted by linked apps** (`POST /v1/apps/self/users/{subject}/sessions`) need
  an app an operator opted in with `allow_sessions` that also has `trust_email` (which
  already vouches for its users' email addresses). They are limited per app, audited,
  answered with `Cache-Control: no-store`, live at most `limits.app_session_ttl_hours`
  (default 720), and tagged with the app, as are access tokens created with them:
  `apps revoke` and `apps rotate` end them all (`--keep-sessions` keeps them on a routine
  rotation).

## Rate limits

| What | Limit | Answer |
|---|---|---|
| `/v1/auth/*` except `GET /v1/auth/me` | `limits.auth_per_minute` (default 10) per client IP; IPv6 per /64 | `429 rate_limited` |
| Signup, login, password change | the same budget per lower-cased email | `429 rate_limited` |
| Chat mention posts | 60 per minute per user | `429` |
| Argon2 hashing | 2–8 at once (CPU count) | queued |

The client IP is the TCP peer, or with `trust_proxy = true` the right-most `X-Forwarded-For`
address (else `X-Real-IP`). Turn `trust_proxy` on only when the proxy is the only way in;
otherwise clients choose their own IP. Body sizes, WebSocket frames, ops per call and other
fixed limits: [canvas.md](canvas.md#limits).

## HTML pages

canvas-cloud serves two HTML pages of its own:

- **`/account`** (sign-in, display name, password, access tokens): its own JS and CSS only,
  with `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self';
  img-src 'self' data:; connect-src 'self'; form-action 'self'; frame-ancestors 'none';
  base-uri 'none'`, plus `X-Frame-Options: DENY`, `Referrer-Policy: no-referrer` and
  `X-Content-Type-Options: nosniff`. The session lives in `sessionStorage` (that tab only).
- **`/join/{token}`** (a share-link landing page when no `web_app_url` is set): no script
  (`default-src 'none'; style-src 'unsafe-inline'`), `Referrer-Policy: no-referrer`,
  `Cache-Control: no-store`.

Files served from `web_root` are your web app's; set its security headers in your proxy if
it needs more than `X-Content-Type-Options: nosniff`, which canvas-cloud adds.

## Transport

- `self-signed` and `acme` serve TLS 1.2 and 1.3 (rustls, ring) with HTTP/2 and HTTP/1.1.
- With `tls.mode = "off"`, bind to loopback or a private interface behind a TLS proxy. The
  OIDC state cookie gets `Secure` when `public_url` is `https`.
- The metrics listener has no authentication; keep it on loopback (the default) or
  firewalled. `doctor` warns when it is not on loopback.

## Input handling

- Yjs frames are decoded by `yrs`; an undecodable frame or rejected update closes that socket
  (`1007`) without touching the room. WebSocket frames are capped at 8 MiB.
- Ops are validated (types, colours, sizes, URL schemes: `http`, `https`, `mailto` plus
  `ops.extra_url_schemes`; `javascript`, `data`, `vbscript` and `file` can never be added).
- JSON bodies are size-limited per route, and unknown config keys are errors.
- Outbound requests are made only to OIDC providers (discovery, keys, token exchange: `https`
  only except for `localhost`, 1 MiB responses, 10 s timeout, no redirects) and, in `acme`
  mode, to Let's Encrypt.

## Admin actions

CLI changes (users, settings, apps) are recorded in the `admin_audit` table with actor
`cli`. Linked-app user deletions and disconnects are recorded there too
([apps.md](apps.md#audit)).
