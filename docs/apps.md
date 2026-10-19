# Linked apps — `/v1/apps/self/*`

A **linked app** is another service that brings its own users to canvas-cloud — for example,
a Copper Cloud can link to Canvas Cloud. An operator registers it once:

```sh
canvas-cloud apps create --name "Team portal" [--allow-import] [--no-trust-email]   # prints cva_… once
canvas-cloud apps list                        # status, policy, users and canvases per app (never keys)
canvas-cloud apps set <id> --allow-import true|false [--trust-email true|false]
canvas-cloud apps rotate <id>                 # new key, printed once; the old one stops working
canvas-cloud apps revoke <id>                 # key stops working; users and canvases stay
```

The app then calls canvas-cloud in one of two ways:

* **As itself.** `Authorization: Bearer cva_…` with no `X-Canvas-User` header. This is the
  only way into `/v1/apps/self/*`, the routes on this page. Sending `X-Canvas-User` here
  returns `400`. A user's session token returns `401`.
* **On behalf of a user.** `Authorization: Bearer cva_…` plus `X-Canvas-User: <subject>`, for
  every user-facing route in [canvas.md](canvas.md). The subject must already be
  provisioned, which is what this page is for.

A **subject** is the app's own stable id for a user, unique per app: 1–200 printable ASCII
characters (`0x20`–`0x7e`), with no leading or trailing whitespace. Put it in a path
percent-encoded.

Errors use the server's usual JSON shape, `{"error": "<code>", "message": "…"}`, plus extra
fields where noted. `GET /v1/info` lists `app_events` and `app_import` in `features`.

## Example

```sh
BASE=https://canvas.example.com/v1
KEY=cva_…                                  # printed once by `canvas-cloud apps create`
AUTH="Authorization: Bearer $KEY"

# Provision a user and act as them.
curl -fsS -X PUT "$BASE/apps/self/users/ann" -H "$AUTH" -H 'content-type: application/json' \
  -d '{"email":"ann@example.com","display_name":"Ann"}'
curl -fsS "$BASE/canvases" -H "$AUTH" -H 'X-Canvas-User: ann'

# Bulk provisioning, a list, the event stream, stats.
curl -fsS -X POST "$BASE/apps/self/users:batch" -H "$AUTH" -H 'content-type: application/json' \
  -d '{"users":[{"subject":"ben","email":"ben@example.com"},{"subject":"cy","email":"cy@example.com"}]}'
curl -fsS "$BASE/apps/self/users?limit=500" -H "$AUTH"
curl -fsSN "$BASE/apps/self/events" -H "$AUTH" -H 'Last-Event-ID: 41'
curl -fsS "$BASE/apps/self/stats" -H "$AUTH"

# Remove a user (full delete if they only ever came from this app).
curl -fsS -X DELETE "$BASE/apps/self/users/ann" -H "$AUTH"
```

## Users

### `PUT /v1/apps/self/users/{subject}` — upsert

```json
{"email": "ann@example.com", "display_name": "Ann", "disabled": false, "id": "<uuid hint>"}
```

Creates the user if this app has no user with this subject; otherwise updates it.

* **New users.** The id is `id` if it is a valid, non-nil UUID that no account uses;
  otherwise the server generates a UUIDv7. A migrator sends the user's old id so that
  references to it stay valid.
* **Email.** Email is compared case-insensitively and its case is kept. If an account not
  linked to this app already has the email:
  * when the app trusts email (`trust_email`, the default), that account is linked to this
    subject and its id stays the same;
  * otherwise the call returns `409 email_taken`.

  An email that another user of this app already has also returns `409 email_taken`.
* **`display_name`.**
  * Omitted on create: the name is empty.
  * Omitted on update: the name is unchanged.
  * `null` or blank: the name is cleared.
  * Longer than 200 characters: the name is truncated.
  * Responses show an empty name as `null`.
* **`disabled: true`** blocks the user and closes their live sockets. `false` re-enables them.

Response `200` (always, for both create and update):

```json
{"user_id": "…", "subject": "ann", "email": "ann@example.com", "display_name": "Ann",
 "disabled": false, "created": true}
```

### `POST /v1/apps/self/users:batch` — bulk upsert

The path really is `users:batch`; the colon is part of the segment. Send
`{"users": [{"subject", "email", "display_name"?, "disabled"?, "id"?}, …]}` with at most
1000 items. Each item has the same semantics as `PUT`, runs in its own transaction, and
succeeds or fails on its own. The response is `200`, with results in request order:

```json
{"results": [
  {"subject": "ann", "ok": true, "user_id": "…", "email": "…", "display_name": null, "disabled": false, "created": true},
  {"subject": "ben", "ok": false, "error": "email_taken", "message": "…"}
]}
```

An empty list returns `{"results": []}`. More than 1000 items, or a body that is not this
shape, returns `400`.

### `GET /v1/apps/self/users?limit=&offset=`

Returns this app's users, in the order they were linked. `limit` defaults to 100, with a maximum of 500.

```json
{"users": [{"subject", "user_id", "email", "display_name", "disabled", "canvas_count",
            "linked_at", "created_at"}], "total": 2, "limit": 100, "offset": 0}
```

`canvas_count` counts the canvases the user is a member of.

### `DELETE /v1/apps/self/users/{subject}`

What happens depends on the kind of user. Either way, the user's live sockets are closed.

* **A pure app user** is one with no password, no link to another app, and no sessions or
  personal tokens. This call deletes the account, with the same cascade as an admin
  user-delete:
  * The canvases they own are deleted.
  * Those canvases' rooms close with `4404`, and members get a `deleted` event.
  * Invites to their email are removed.
* **Any other user** keeps their account. Only this app's link to them is removed.

The response is
`{"ok": true, "subject", "user_id", "deleted": true|false, "canvases_deleted": n}`. A subject
this app does not have returns `404`.

### `POST /v1/apps/self/users/{subject}/disconnect`

Closes the user's live canvas sockets; their clients reconnect. The response is
`{"ok": true, "subject", "user_id"}`.

## Events — `GET /v1/apps/self/events`

Server-Sent Events covering every canvas event for this app's users:

```text
event: canvas
id: 42
data: {"subject":"ann","user_id":"…","canvas_id":"…","kind":"created"}
```

The `kind` values are the engine's: `created`, `deleted`, `invited`, `invite_revoked`,
`invite_declined`, `update`. A change that concerns several of the app's users produces one
event per user.

* **Keepalive.** A `: keepalive` comment is sent every 15 s.
* **Resume.** Reconnect with `Last-Event-ID: <id>` to get the events after `<id>` that are
  still in the server's resume buffer, which holds the instance's last 4096 events. If some
  of those events are gone, or if `<id>` was never issued by this process, the stream starts
  with:

  ```text
  event: resync
  id: <latest>
  data: {}
  ```

  On `resync`, re-read whatever the app caches (for example `GET …/canvases`). A stream that
  falls too far behind also gets a `resync`.
* **Ids** count up from 1 for each process, so a server restart resets them. A
  `Last-Event-ID` from before a restart that is above the new counter yields `resync`. One at
  or below it replays the new process's events from that point, so on reconnect after a
  restart, treat the stream as a fresh start and re-read state.
* **Directory refresh.** The stream sees users provisioned after it opened: the server
  refreshes the app's user directory on provisioning changes, and at least every 30 s.
* **End of stream.** The stream ends when the server shuts down or the app is revoked.

Streams are cheap: one shared tap per instance, filtered through an in-memory map from user
to subject for each app.

## Canvases

### `GET /v1/apps/self/canvases?kind=personal|shared&limit=&offset=`

Returns the canvases owned by this app's users, newest first. `limit` defaults to 100,
with a maximum of 500.

```json
{"canvases": [{"id", "name", "kind",
               "owner": {"subject", "user_id", "email", "display_name"},
               "member_count", "share_link_count", "created_at", "updated_at",
               "imported_at": "…|null"}],
 "total": 1, "limit": 100, "offset": 0}
```

### `DELETE /v1/apps/self/canvases/{id}`

Deletes the canvas. Its room closes with `4404` and its members get `deleted`. The response is
`{"ok": true, "id", "name"}`. The app may delete a canvas if one of its users owns it or the
app imported it. Any other canvas returns `404`.

### `GET /v1/apps/self/canvases/{id}/digest`

Returns `{"canvas_id", "digest"}`. The digest ([import-format.md](import-format.md#digest))
covers the document as stored, reloaded from the database. Use it to check a migration. The
same rules as delete decide which canvases the app may ask about.

## `GET /v1/apps/self/stats`

```json
{"app": {"id", "name", "allow_import", "trust_email"}, "allow_import": true,
 "users": 2, "canvases": 3, "rooms": {"rooms": 1, "peers": 2}}
```

* `users` and `canvases` count this app's users and the canvases they own.
* `rooms` counts the rooms of those canvases that are open on this server, and the peers in
  them.
* `allow_import` is given both inside `app` and at the top level, so a migrator can refuse an
  import at preflight.

## `POST /v1/apps/self/import[?mode=create|replace]`

Imports one canvas in import format v1. The format and every rule are in
[import-format.md](import-format.md). The app needs `allow_import`; without it the call
returns `403 import_not_allowed`.

## Audit

Every mutating call writes `admin_audit` with the actor `app:<app id>`. The actions are:

* `app.user.create`
* `app.user.unlink`
* `app.user.delete`
* `app.canvas.delete`
* `app.canvas.import`
* `app.set` (from the CLI)
