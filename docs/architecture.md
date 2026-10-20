# Architecture

canvas-cloud is one Rust process (tokio, axum 0.8, yrs, sqlx) in front of one PostgreSQL
database. This page describes how the crates fit together and what happens to a request.
The protocol details live in the other docs; links are given where they apply.

## Crates

| Crate | Path | What it owns |
|---|---|---|
| `canvas-cloud-core` | `crates/core` | config, the database pool and the embedded migrations, crypto (master key → key-encryption key), errors, identity (users, sessions, access tokens, linked apps, the `AuthUser` / `AppAuth` extractors), the event bus, TLS serving, logging and metrics, the auth rate limiter, CORS, `GET /v1/info`, `/healthz`, the static web root, and the router composition `app_with` |
| `canvas-cloud-engine` | `crates/engine` | canvases: REST, members, invites, share links, the people directory, mentions, the ops and read formats, Yjs rooms and the y-websocket endpoint, the `/join/{token}` landing page. Every operation is also a plain async function in `canvas_cloud_engine::api` |
| `canvas-cloud-accounts` | `crates/accounts` | people using the server directly: signup, login, sessions, password changes, personal access tokens, OIDC sign-in, `GET /v1/events`, the `/account` page |
| `canvas-cloud-apps` | `crates/apps` | linked apps: `/v1/apps/self/*` (user provisioning, the app event stream, canvas listing, stats, import format v1, digests) |
| `canvas-cloud-mcp` | `crates/mcp` | agent seams: `POST /mcp`, `GET /llms.txt`, `GET /v1/openapi.json` |
| `canvas-cloud` | `crates/canvas-cloud` | the binary: CLI, `serve`, `doctor`, admin commands; the migrations directory |

Dependencies point one way: every crate depends on core; apps and mcp call the engine's
`api` functions in-process (no HTTP loopback); accounts does not depend on the engine at
runtime. The binary depends on all of them.

## Composition

`canvas_cloud::app` builds the HTTP app from `Mounts`: each crate contributes routers under
`/v1` and at the root, and the `features` it advertises in `GET /v1/info`. Core's
`app_with` then assembles:

```text
               request
                  │
   ┌──────────────▼──────────────┐
   │ observe::track              │  span, access log line, http_* metrics, panic → 500
   ├─────────────────────────────┤
   │ CORS (only if               │  exact origin list, preflight cache 600 s
   │   cors.allowed_origins)     │
   ├─────────────────────────────┤
   │ auth rate limit             │  /v1/auth/* per client IP (GCRA)
   ├─────────────────────────────┤
   │ router                      │
   │  /healthz                   │  core
   │  /v1/info                   │  core
   │  /v1/canvases… /v1/invites… │  engine
   │  /v1/canvas-links… /v1/people /v1/mentions…
   │  /v1/auth/… /v1/tokens…     │  accounts
   │  /v1/events                 │  accounts
   │  /v1/apps/self/…            │  apps
   │  /v1/openapi.json           │  mcp
   │  /mcp /llms.txt             │  mcp (root)
   │  /account…                  │  accounts (root)
   │  /join/{token}              │  engine (root)
   │  /v1/* else                 │  JSON 404
   │  anything else              │  web_root files (GET/HEAD) or JSON 404
   └──────────────┬──────────────┘
                  │ handlers: extractors (AuthUser / AppAuth) → engine::api / crate logic
        ┌─────────┼──────────────────────────────┐
        ▼         ▼                              ▼
  PostgreSQL   rooms (in memory, one per        events (in memory: per-user channels,
  (sqlx pool)  open canvas: yrs::Doc,           global tap, ring buffer of 4096)
               awareness, broadcast)
```

`serve` loads the config, starts logging and the metrics listener, connects to PostgreSQL
(retrying for up to 60 s), applies migrations, builds `AppState` (pool, config, crypto,
events), starts the hourly session purge and serves HTTP or HTTPS per `tls.mode` until a
signal (see [operations.md](operations.md#systemd)).

## Request flow

### REST

1. The layers above run; the auth rate limiter only acts on `/v1/auth/*`.
2. The handler's `AuthUser` extractor reads `Authorization: Bearer …` (or `?token=` on a
   WebSocket upgrade or an event-stream GET), hashes the credential with SHA-256 and resolves
   it in **one SQL statement** that also refreshes `last_used_at` (and a session's sliding
   expiry) at most once a minute. The result is cached in the request. A disabled user is
   `401 account_disabled`; a read-only token on anything but GET/HEAD is
   `403 read_only_token`.
3. The handler calls an `api` function, which checks membership with the same query that
   loads the canvas (non-members get `404`), applies the change in a transaction, and
   publishes events.
4. Errors are `{"error": code, "message": …}` with a fixed set of codes; the code is recorded
   on the request's log line.

`POST /v1/canvases/{id}/ops` takes the room's write lock and applies the ops to the live
document, so connected peers see the result at once ([canvas-protocol.md](canvas-protocol.md#ops)).

### WebSocket rooms

`GET /v1/canvases/{id}/ws` (HTTP/1.1 upgrade or HTTP/2 extended CONNECT):

1. Authenticate (header or `?token=`); read-only tokens are refused; non-members get `404`
   before the upgrade.
2. The room is loaded if it is not in memory: the snapshot and every later update are read,
   decrypted with the canvas key and applied to a fresh `yrs::Doc`.
3. The peer speaks standard `y-websocket`: sync step 1/2, updates and awareness
   ([canvas-protocol.md](canvas-protocol.md#wire-protocol)). Each connection has a read loop
   and a write task with a bounded queue.
4. An incoming update is applied under the room's write lock, sealed, `INSERT`ed into
   `canvas_updates`, and only then broadcast to the other peers. Persisted order equals
   applied order equals broadcast order.
5. Members get a throttled `update` event (at most once per 2 s per canvas).
6. Close codes tell the client why it was disconnected (`4403` removed from the canvas,
   `4404` deleted, `1001` shutdown, `1013` too slow, …;
   [canvas-protocol.md](canvas-protocol.md#close-codes)).

### Server-sent events

`GET /v1/events` (accounts) streams the caller's `canvas` events; `GET /v1/apps/self/events`
(apps) streams every event of an app's users. Both read core's event bus:

- `Events::publish(user_id, event)` gives the event an id, puts it in a ring buffer of the
  last 4096 events, sends it to the user's channel (if anyone listens) and to the global tap.
- Ids increase strictly, also across restarts: a process numbers its events from its start
  time in Unix microseconds.
- A client reconnecting with `Last-Event-ID` gets the missed events from the buffer, or
  `event: resync` when they are gone (too old, from an earlier process, or the client lagged).
- Keepalives every 15 s. `/v1/events` re-checks the credential every 60 s and ends when it
  is revoked.

Details: [events.md](events.md) and [apps.md](apps.md#events--get-v1appsselfevents).

## Persistence

| Table | Content |
|---|---|
| `canvases` | id, owner, name, kind, `doc_key_wrapped` (the canvas key, wrapped by the KEK), import provenance |
| `canvas_updates` | append-only Yjs update log (`seq`, sealed update) |
| `canvas_snapshots` | one compacted state per canvas (`seq` of the last folded update, sealed state) |
| `canvas_members`, `canvas_invites`, `canvas_share_links`, `canvas_mentions` | membership, invites, share-link digests, mentions (sealed excerpts) |
| `users`, `sessions`, `access_tokens`, `apps`, `app_users` | identity; credentials as SHA-256 digests |
| `server_settings`, `admin_audit`, `oidc_states` | signup policy, admin audit log, OIDC sign-ins in flight |

- **Update log**: every document change is one row in `canvas_updates`, written before it
  is broadcast. If the write fails, the room's in-memory document is dropped and every peer is
  disconnected; clients reconnect and their sync handshake re-sends what the server lacks.
- **Snapshots and compaction**: after 200 persisted updates the room encodes its whole state
  (`encode_state_as_update_v1`), seals it into `canvas_snapshots`, and deletes the folded
  updates, in one transaction. A snapshot is written only when it provably re-applies to the
  same document (`crates/engine/src/compact.rs`): rooms never garbage-collect in memory; the
  garbage-collected encoding is tried on a copy and kept if a fresh document built from it has
  the same state vector and digest, else the uncollected encoding under the same check, else
  no snapshot (the update log stays the record). This guards against a yrs 0.28 bug: a text
  split inside a UTF-16 surrogate pair (a Yjs peer editing between the two halves of an
  emoji) can encode wrongly, which on real data dropped 55 of 72 shapes from a snapshot.
- **Loading** a room reads the snapshot plus updates with a higher `seq`.
- **Eviction**: a room with no peers and no REST use for 60 s leaves memory.
- **Encryption**: each canvas has a random 32-byte key; the master key derives (HKDF-SHA256)
  a key-encryption key that wraps it. Updates, snapshots and mention excerpts are
  AES-256-GCM sealed with the canvas id (or mention id) as associated data
  ([security.md](security.md#what-is-stored-and-how)).
- Awareness (cursors, presence) and events are memory-only.

## Auth model

Four ways in. All credentials are 32 random bytes, base64url, with a prefix; only their
SHA-256 is stored, and lookup is by digest. Full reference: [auth.md](auth.md).

| Credential | Header | Who | Notes |
|---|---|---|---|
| session `cvs_…` | `Authorization: Bearer` (`?token=` on WS/SSE) | a person, after signup, login, OIDC or delegated sign-in | expires 30 days after last use (refreshed at most once a minute); only sessions manage tokens and passwords |
| personal access token `cvt_…` | same | an agent or script acting as a person | scope `read` (GET/HEAD only, no rooms) or `write`; optional expiry; at most 50 per user |
| app key `cva_…` + `X-Canvas-User: <subject>` | both headers | a linked app acting as one of its users | the subject must be provisioned for that app (`PUT /v1/apps/self/users/{subject}`), else `401 unknown_user`; the app then provisions and retries |
| app key `cva_…` alone | `Authorization: Bearer` | the app itself | only `/v1/apps/self/*`; anywhere else it is `401` |

- **OIDC** (authorization code + PKCE) is a way to get a session: the callback finds or
  creates the user by verified email and redirects with `#token=cvs_…`.
- **Delegated sign-in** is another: `[[delegated_sign_in]]` sends the browser to a site that
  signs the person in, mints their session as a linked app with `allow_sessions` and
  `trust_email` (`POST /v1/apps/self/users/{subject}/sessions`) and redirects with
  `#token=cvs_…`. Minted sessions (and access tokens created with them) carry
  `created_by_app`, so revoking or rotating the app ends them.
- **Admins** are not a role in the API: administration is the `canvas-cloud` CLI on the host,
  which talks to the database directly and writes `admin_audit` rows.
- Every request resolves to one `AuthUser {user_id, email, display_name, via}`; `via` says
  which credential (session, token with scope, or app with subject). The engine and MCP only
  see `AuthUser`, so a canvas behaves the same for every kind of caller.
- Linked-app users and password users are the same `users` rows; an app's users are linked
  through `app_users (app_id, subject, user_id)`.

## What is not here

- No clustering: one process per database (rooms and events are in memory).
- No background workers besides the session purge, the room eviction timers and, in `acme`
  mode, certificate renewal.
- No outgoing email: invites are recorded and announced through events; delivering them is
  up to the client or linked app.
