# Canvases

The `canvas-cloud-engine` crate (`crates/engine`) serves collaborative whiteboards: a REST
API for canvases, members and invites, and one live Yjs room per open canvas that speaks the
standard `y-websocket` protocol. The wire protocol, document schema and ops/read JSON are in
[canvas-protocol.md](canvas-protocol.md).

Everything here is mounted under `/v1` and authenticated like the rest of the API:
`Authorization: Bearer <credential>` — a session (`cvs_…`), a personal access token
(`cvt_…`; read-scoped tokens may only `GET`/`HEAD` and never join a live room) or an app key
(`cva_…`) plus `X-Canvas-User: <subject>` for a linked app acting as one of its users —
or `?token=` on WebSocket upgrades. Errors use the common `{"error","message"}` shape.

Every operation is also callable in-process through `canvas_cloud_engine::api` (plain async
functions over `(&SharedState, &AuthUser, …)` with the same rules, events and errors); the
REST handlers are thin wrappers over it.

## Model

| Table | What |
|---|---|
| `canvases` | `id`, `owner_id`, `name`, `kind` (`personal`/`shared`), `doc_key_wrapped`, timestamps |
| `canvas_members` | `(canvas_id, user_id)`, `role` (`owner`/`editor`), `added_at` — the owner has a row too |
| `canvas_invites` | email invites: `email` (lower-cased), `invited_by`, `status` (`pending`/`accepted`/`declined`/`revoked`), `nudged_at` (last re-send reminder), `token` (hash, reserved) |
| `canvas_share_links` | opaque share-link token digest, `role` (`editor`), `created_by`, `uses`; tokens are never persisted in plaintext |
| `canvas_updates` | append-only Yjs update log: `seq bigserial`, `canvas_id`, `"update"` (sealed) |
| `canvas_snapshots` | one compacted state per canvas: `seq` (last update folded in), `state` (sealed) |
| `canvas_mentions` | chat @mentions: `canvas_id`, `message_id` (the client's chat message id), `from_user`, `to_user`, `excerpt_sealed`, `created_at`, `read_at`; one row per `(canvas_id, message_id, to_user)` |

Migrations: `crates/canvas-cloud/migrations/0100_canvases.sql`,
`0101_canvas_share_links.sql`, `0102_canvas_invite_nudge.sql` and `0103_canvas_mentions.sql`
(the engine owns `0100`–`0199`).

- **Personal canvas**: every user has exactly one (`kind = personal`, name `Personal`),
  created on first use (`GET /canvases`, or any `/canvases/personal/...` route) — a unique
  partial index on `owner_id WHERE kind = 'personal'` makes concurrent first use safe. It can
  never be renamed, deleted, shared or left (`400`).
- **Roles**: `owner` (rename, delete, remove members) and `editor` (read/write the document,
  invite others, leave). Both can edit the board.
- **Access is membership**: every lookup joins `canvas_members` on the caller. A canvas you
  are not a member of — or that does not exist — is `404` on every route (no existence leak).
  A member who lacks the role for an action gets `403`.

`{id}` in every route below is a canvas UUID **or the literal `personal`**.

## REST

### `GET /canvases`

Your canvases, Personal first, then most recently updated.

```json
[
  {
    "id": "0192…", "name": "Personal", "kind": "personal", "role": "owner",
    "owner": { "id": "0192…", "email": "ann@example.com", "display_name": "Ann" },
    "member_count": 1,
    "created_at": "2026-10-01T12:00:00Z", "updated_at": "2026-10-01T12:05:00Z"
  }
]
```

`updated_at` follows document activity (bumped at most every 2 s while a room is busy).

### `POST /canvases {name}` → `201` canvas

Creates a `shared` canvas owned by you (owner member row included). `name` is trimmed,
control characters dropped, 1–200 characters. The document starts with `meta.name` and
`meta.createdBy`.

### `GET /canvases/{id}` → canvas · `PATCH /canvases/{id} {name}` → canvas · `DELETE /canvases/{id}` → `{"ok":true}`

Rename and delete are owner-only (`403` for editors) and `400` on the Personal canvas.
Rename also updates the document's `meta.name` (synced to connected peers). Delete removes
the canvas, members, invites, updates and snapshot (cascade) and closes its room: connected
sockets get close code `4404`.

### `GET /canvases/{id}/members`

```json
[{ "user_id": "…", "email": "…", "display_name": "…", "role": "owner", "added_at": "…" }]
```

Owner first, then by join time. Any member may list.

### `DELETE /canvases/{id}/members/{user_id}` → `{"ok":true}`

The owner removes anyone else; any member removes themselves (leave). The owner cannot leave
(`400` — delete the canvas instead). Removed users' open sockets close with `4403`.

### `POST /canvases/{id}/invites {email}` → `201` new invite · `200` already pending (re-send)

Owner or editor. The email is trimmed and lower-cased; inviting yourself is `400`, an
existing member `409`, the Personal canvas `400`. An email with no account yet is fine: the
invite waits until someone signs up with it. Even when the email already belongs to a user,
they must accept — nobody is added to a canvas without consent.

- **New invite → `201`.** The invitee (if they have an account) gets a `canvas` event with
  kind `invited`.
- **Already pending → `200`** with the same invite, and the call is a **re-send**: if the
  invitee has an account and the last reminder (`nudged_at`, else `created_at`) is at least
  30 s old, `nudged_at` is set to now and the invitee gets another `invited` event — clients
  use it to surface the invite again. Re-sends inside the 30 s window, or to an email with no
  account yet, change nothing.

The response is the invite view plus `nudged`: `true` only when this call reminded the
invitee (a `200` re-send that was not throttled); `false` on `201` and when throttled or
there is no account yet.

```json
{
  "id": "…", "canvas_id": "…", "canvas_name": "Roadmap", "email": "bob@example.com",
  "status": "pending",
  "invited_by": { "id": "…", "email": "ann@example.com", "display_name": "Ann" },
  "invitee": { "id": "…", "email": "bob@example.com", "display_name": "Bob" },
  "created_at": "…",
  "nudged_at": "…",
  "nudged": true
}
```

Invite views (here and in both lists below) carry:

- `invitee` — the account with that email, or `null` while nobody has signed up with it
  (e.g. "waiting for them to make an account");
- `nudged_at` — RFC 3339 time of the last re-send reminder, `null` until the first one.

### `GET /canvases/{id}/invites`

Pending invites of a canvas (any member), newest first.

### `DELETE /canvases/{id}/invites/{invite_id}` → `204`

Revoke a pending invite. Allowed for the canvas owner and for the member who sent
it; any other member gets `403 {"error":"forbidden"}`. An unknown invite, one of another
canvas, or one that is no longer pending (accepted, declined, already revoked) is
`404 {"error":"not_found"}`; non-members get `404` as everywhere. The invite's status becomes
`revoked`: it disappears from both lists, can no longer be accepted or declined (`404`), and
the email can be invited again (a new `201`). The invitee (if they have an account) gets a
`canvas` event with kind `invite_revoked`.

### `GET /invites`

Pending invites addressed to **your** email (case-insensitive), with canvas name and inviter,
newest first.

### `POST /invites/{id}/accept` → canvas (your view, `role: editor`) · `POST /invites/{id}/decline` → `{"ok":true}`

Only the invitee can accept or decline; anything else (wrong user, already used, declined,
revoked, unknown) is `404`. Invites are single-use.

### Canvas share links

`POST /canvases/{id}/links` is available to any member. The body may be `{}` or
`{"role":"editor"}`; `owner` and other roles are rejected with `400`. It returns `201` and the
one-time-visible token:

```json
{"id":"uuid","token":"<43 chars>","canvas_id":"uuid","role":"editor","created_at":"…"}
```

Owners can list links with `GET /canvases/{id}/links`:

```json
{"links":[{"id":"uuid","role":"editor","created_by":"uuid","created_at":"…","uses":3}]}
```

Tokens are omitted from lists. Owners revoke one (`DELETE .../links/{link_id}`) or every link
(`DELETE .../links`); both return `204`. A missing link on these owner-scoped routes is `404`.

A signed-in user can preview `GET /canvas-links/{token}` without being a member:
`{"canvas_id","name","owner":{"id","display_name"},"role","member"}`. Unknown,
revoked, or deleted-canvas tokens return `404` with `error: "link_not_found"`.

`POST /canvas-links/{token}/join` is safe to retry. It adds an editor membership when needed,
never downgrades an existing owner, accepts/removes a pending email invite for the caller,
increments the link's `uses`, publishes the normal `member_added` canvas event, and returns the
same canvas object as one item from `GET /canvases`.

Share links are unavailable for Personal canvases. They never expire on their own; deleting a
canvas cascades its links.

### `GET /canvases/{id}/state[?sv=<base64>]`

`{"state": "<base64 of encode_state_as_update_v1>"}` — the whole document as one Yjs update
(standard base64, padded). With `sv` (a base64 lib0-v1 state vector) only what that state
vector is missing is returned. Apply with `Y.applyUpdate(doc, bytes)`.

### `GET /canvases/{id}/read[?full=true&ids=a,b&types=sticky,text]`

The agent-friendly read format (see [canvas-protocol.md](canvas-protocol.md#read)), identical
to the canvas page's own read. Text is cut to 500 characters unless `full=true`. It also
carries `chat`: the last 50 messages of the canvas chat, oldest first (see
[Chat and mentions](#chat-and-mentions)); `ids` / `types` narrow the shapes, not the chat.

### `GET /people`

The caller's people directory (for picking whom to invite or @mention):
`{"people":[{"id","display_name","email"}]}`, active accounts only, never the caller. `q`
is a case-insensitive substring filter on display name or email; results are sorted by
display name. `limit` defaults to 20 and is capped at 50.

Who is in the directory depends on how the caller signed in:

* a **linked app** acting for one of its users (`X-Canvas-User`) — every other user linked
  to that same app, so each app's users see exactly their own directory and never another
  app's (see [apps.md](apps.md));
* anyone else (a session or a personal access token) — the users who share at least one
  canvas with the caller.

### `POST /canvases/{id}/ops`

```json
{ "ops": [ { "op": "add", "shape": { "type": "sticky", "text": "Hello" } } ],
  "as": { "name": "Planner", "color": "purple" },
  "near": { "x": 0, "y": 0 },
  "confirm": false }
```

Applies [canvas ops](canvas-protocol.md#ops) to the live document in one transaction, then
persists and broadcasts the change exactly like a WebSocket update — connected clients see
it immediately. Returns `{"applied": n, "ids": [...], "errors": [...]}` (`ids[i]` belongs to
op `i`, `null` when it failed). Lets remote agents draw without a client.

- `as` (optional): who is drawing. Shapes created get `by = as.name`; without `as`, `by` is
  your display name (or email). With `as`, the agent's entry in the `agents` map is set to
  `writing` with its cursor on the last thing touched, then back to `idle` 1.5 s later.
- `near`: where auto-placement looks for free space (default `0,0`).
- `confirm: true` is required for `{op:"clear"}` (or put `confirm: true` on the op).
- At most 500 ops per call; the body may be a bare array of ops too. Envelope problems
  (`ops` not an array, bad `as`, too many ops) are reported like the page does: `200` with
  `errors: [{"index": -1, …}]`. Malformed JSON is `400`.

### Chat and mentions

Shared canvases have a chat. The messages live **in the canvas document** — a
top-level `Y.Array` named `chat` of plain objects
`{id, authorId, authorName, text, mentions: [userId], at, editedAt?, deleted?}` (see
[canvas-protocol.md](canvas-protocol.md#document-schema)) — written by clients and synced,
persisted, compacted and reloaded by the room like every other part of the document. The
server never writes it; `GET /canvases/{id}/read` shows the most recent 50. What the server
adds is **mention notifications**: after a client inserts a message that @mentions people, it
reports them, and the server records one row per recipient and tells them.

#### `POST /canvases/{id}/mentions {message_id, user_ids, excerpt}` → `200`

```json
{ "message_id": "msg_01J…", "user_ids": ["0192…ben", "0192…cy"], "excerpt": "@Ben @Cy dinner Tuesday?" }
```

Any member. `user_ids` is filtered to **current members of the canvas other than the
caller**; each such recipient gets a stored mention and a `canvas` event with kind `mention`.
Idempotent per `(canvas, message_id, recipient)`: repeating the call (a retry) answers the
same and notifies nobody again; the first excerpt wins.

```json
{ "notified": ["0192…ben", "0192…cy"], "skipped": ["0192…left-the-canvas"] }
```

- `notified`: the requested users that are mentioned by this message (members other than you),
  including on a retry; `skipped`: everyone else (yourself, non-members, unknown ids), in
  request order, duplicates removed.
- `message_id`: 1–128 characters, no control characters (`400`). `user_ids`: UUIDs, at most
  100 (`400`); may be empty. `excerpt`: optional; control characters and runs of whitespace
  become one space, then it is cut to 200 characters. It is sealed at rest under the canvas
  doc key.
- Non-members and unknown canvases: `404` (as everywhere). The Personal canvas has no chat:
  `400`. More than 60 calls per minute per user (bursts of 60, then one per second): `429
  {"error":"rate_limited"}`.

#### `GET /mentions[?unread=1&limit=n]`

Your mentions on canvases you are still a member of, newest first. `unread=1` (or `true`)
returns only unread ones; `limit` defaults to 50 and is capped at 100 (`< 1` is `400`).
`unread` (in the body) counts every unread mention you can see, regardless of `limit`.

```json
{
  "mentions": [
    {
      "id": "0192…",
      "canvas": { "id": "0192…", "name": "Team dinners" },
      "from": { "id": "0192…", "display_name": "Ann", "email": "ann@example.com" },
      "message_id": "msg_01J…",
      "excerpt": "@Ben @Cy dinner Tuesday?",
      "created_at": "2026-10-06T18:00:00Z",
      "read_at": null
    }
  ],
  "unread": 1
}
```

Leaving a canvas hides its mentions (they come back if you rejoin); deleting a canvas or
either account deletes them.

#### `POST /mentions/read {ids} | {canvas_id}` → `{"ok": true, "updated": n}`

Marks your own unread mentions read — by mention `id` (at most 500) and/or every mention on
`canvas_id`. Others' mentions and canvases you are not a member of are silently untouched
(`updated` counts what changed). For each canvas where something changed you get a `canvas`
event with kind `mention_read`, so your other devices clear their badges. Neither key → `400`.

Clients detect mention support from `GET /v1/info` `features` containing `mentions`.

### `GET /canvases/{id}/ws` — live room

WebSocket upgrade (HTTP/1.1 `GET`, or HTTP/2 extended `CONNECT`). Auth: the `Authorization`
header, or `?token=` for tools and browsers that cannot set headers. A read-only token is
refused (`403 read_only_token`). Non-members get `404` before the upgrade. Protocol details:
[canvas-protocol.md](canvas-protocol.md#wire-protocol).

Example URL: `wss://HOST:PORT/v1/canvases/personal/ws?token=…`. A stock
`y-websocket` `WebsocketProvider(serverUrl = "wss://HOST:PORT/v1/canvases", room = "<id>/ws",
doc, { params: { token } })` works as-is.

### Change notifications

Members get `event: canvas` on `GET /v1/events` (and a linked app gets every event of its
users on `GET /v1/apps/self/events`)
(`{"type":"canvas","canvas_id":"…","kind":"…"}`) — refetch the list or the canvas:

| `kind` | Sent to | When |
|---|---|---|
| `created` | owner | canvas created |
| `renamed` | members | rename |
| `deleted` | former members | delete |
| `invited` | the invitee (if they have an account) | new invite, or a re-send reminder (at most every 30 s per invite) |
| `invite_revoked` | the invitee (if they have an account) | owner or inviter revoked a pending invite |
| `invite_declined` | the inviter | invite declined |
| `member_added` | members | invite accepted |
| `member_removed` | members + the removed user | member removed / left |
| `update` | members | document changed (at most once per 2 s per canvas, plus one trailing) |
| `mention` | each newly mentioned member | someone @mentioned you in the canvas chat |
| `mention_read` | you | your mentions on that canvas were marked read (e.g. on another device) |

## Rooms

One in-memory room per open canvas: a `yrs::Doc` behind a `tokio::sync::RwLock`, an
awareness table, and one `tokio::sync::broadcast` channel (256 frames) fanned out to peers.

- **Load**: decrypt the snapshot, then every update with `seq > snapshot.seq`, in order.
- **Write path** (WebSocket update, REST ops, rename): take the write lock → apply in one
  transaction → seal and `INSERT` the update the transaction produced → broadcast it to the
  other peers. The lock is held across the insert, so persisted order = applied order =
  broadcast order, and **nothing is broadcast before it is durable**. If the insert fails
  the in-memory document is discarded (reloaded from Postgres on next use) and all peers are
  disconnected (`1011`); y-websocket clients reconnect and their sync handshake re-sends
  whatever the server is missing, so no edit is lost.
- **Compaction**: every 200 persisted updates, `encode_state_as_update_v1` of the room doc is
  sealed into `canvas_snapshots` and the folded updates are deleted — one transaction.
- **Eviction**: a room with no peers and no REST use for 60 s is dropped from memory (it is
  only removed when nothing else holds it, so an in-flight write can never be orphaned).
- **Awareness** (cursors/presence) is relayed between peers and kept in memory only, never
  persisted. When a socket closes, the awareness states it published are removed and peers
  are told.
- **Backpressure**: each peer has a read loop and a write task. Replies go through a bounded
  per-peer queue (32); a peer that falls more than 256 broadcast frames behind, or does not
  accept a frame within 15 s, is disconnected (`1013`) and resyncs on reconnect. The server
  pings every 30 s and drops peers silent for 90 s.
- **Shutdown**: on the process shutdown signal every socket is closed with `1001`.

### Limits

| What | Limit |
|---|---|
| WebSocket message / frame | 8 MiB (the page stores images as `data:` URLs up to 2 MiB) |
| `POST /ops` body | 4 MiB; 500 ops per call |
| Other canvas request bodies | 64 KiB |
| Canvas name | 200 characters |
| Awareness clients per socket | 16 |
| Sticky/text body | 20 000 characters; titles/labels cut to 500 |
| Mentions | 100 `user_ids` per call; excerpt cut to 200 characters; `message_id` ≤ 128; 60 calls per minute per user; `GET /mentions` ≤ 100; `POST /mentions/read` ≤ 500 ids |

### Metrics

| Name | Type | Meaning |
|---|---|---|
| `canvas_rooms` | gauge | rooms in memory |
| `canvas_peers` | gauge | connected WebSocket peers |
| `canvas_updates_persisted_total` | counter | Yjs updates written to `canvas_updates` |
| `canvas_update_bytes_total` | counter | plaintext bytes of those updates |
| `canvas_compactions_total` | counter | snapshot compactions |
| `canvas_ws_bytes_in_total` / `canvas_ws_bytes_out_total` | counter | WebSocket payload bytes |
| `canvas_mentions_total` | counter | chat mentions stored (new recipients notified) |
| `canvas_mentions_rate_limited_total` | counter | `POST /canvases/{id}/mentions` calls refused with `429` |

`canvas_cloud_engine::rooms_metrics()` returns the live `{rooms, peers}` counts.

## Web share-link landing page

`GET /join/{token}` is outside `/v1` and unauthenticated. It never looks up the token or
includes canvas data. When share links have a web target — `web_app_url` (with `{token}`
substituted), or `{public_url}/#/join/{token}` when the server itself serves a web app from
`web_root` — a well-formed token is redirected there with `302`. Otherwise (or for a
malformed token) it returns a tiny self-contained page: "Open this link in your canvas app to
continue." The token is never echoed into HTML. The response is `Cache-Control: no-store`,
`Referrer-Policy: no-referrer`, `X-Content-Type-Options: nosniff`, and CSP
`default-src 'none'; style-src 'unsafe-inline'`.

## Security model

- **Authentication** is core identity (`AuthUser`): sessions, personal access tokens and
  app keys acting for provisioned users.
- **Authorization** is membership, checked on every REST call and at WebSocket upgrade.
  Removing a member or deleting a canvas disconnects affected sockets immediately. (A session
  revoked by logout does not cut an already-open socket; it fails on reconnect.)
- **Encryption at rest**: each canvas has a random 32-byte `doc_key`, wrapped by the KEK
  derived from `master_key` (`canvases.doc_key_wrapped`). Every update and snapshot is
  AES-256-GCM `nonce(12) || ciphertext || tag` under that key with AAD = the canvas id's 16
  raw bytes, so rows copied to another canvas fail to decrypt. Key rotation is out of scope.
  Chat lives in the document, so it is sealed the same way;
  mention excerpts are sealed under the same doc key with AAD
  `"canvas-cloud/v1/mention:" ‖ the mention id's 16 raw bytes`. Mention ids, message ids,
  sender/recipient and timestamps are not encrypted.
- **Invite tokens**: a random secret is generated per invite but only its SHA-256 is stored
  (reserved for future shareable links); v1 is email-invite only and never returns it.
- **Logging**: canvas and user ids, op counts and error kinds only — never tokens, document
  contents, awareness state or update bytes.
- **Input**: y-protocol frames are decoded with `yrs`; undecodable frames or rejected updates
  close that socket (`1007`) without touching the room. Ops are validated like the page
  validates them (types, colours, URLs, sizes); bad ops are skipped and reported.

## Testing

`cargo test -p canvas-cloud-engine` runs unit tests plus integration tests against the
native Postgres database `cvc_engine_test` (default
`postgres://farce@localhost:5433/cvc_engine_test`; override with
`CANVAS_CLOUD_TEST_ENGINE_DATABASE_URL`): REST lifecycle, Personal
canvas, invites (re-send reminders, revocation), 404 scoping, two-client sync + awareness,
persistence across eviction, compaction, ops fan-out, revocation, the `chat` array across
eviction / restart / compaction (`tests/chat.rs`), and mentions — member filtering,
idempotency, read marking, events, sealing, rate limit (`tests/mentions.rs`), and identity —
token scopes, app keys, the service API, the importer snapshot path and configured link
schemes (`tests/identity.rs`).

A cross-implementation test drives the real page code (Yjs, `y-websocket`,
`Canvas/src/ops.ts`) with Bun and checks that the server's `/read` equals the page's
`readCanvas` on the same document:

```sh
CANVAS_PAGE_DIR=path/to/canvas-page \
  cargo test -p canvas-cloud-engine --test js_interop -- --ignored
```
