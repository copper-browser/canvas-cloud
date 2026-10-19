# Import format v1

`POST /v1/apps/self/import[?mode=create|replace]` brings one canvas from another system into
canvas-cloud. The request carries the canvas's rows, its document history, and the tokens
that already identify it outside, so existing share links and invites keep working. The
response gives a [digest](#digest) of the stored document, which a migrator can check.

* **Who can call it:** an app authenticating as itself (`Authorization: Bearer cva_…`, without
  `X-Canvas-User`) whose `allow_import` is on. Otherwise the call returns
  `403 import_not_allowed`. An operator turns it on with `canvas-cloud apps set <id>
  --allow-import true`.
* **What to send:** `Content-Type: application/x-ndjson`; the body describes exactly one
  canvas, as one JSON object per line. Stream it: the server reads it line by line.
* **Size limits:** a line is at most `limits.import_max_line_bytes` (32 MiB by default) and the
  whole body at most `limits.import_max_bytes` (512 MiB by default). `GET /v1/info` reports
  both under `limits`.

## Example

```sh
cat > board.ndjson <<'NDJSON'
{"type":"canvas","version":1,"id":"0190f7a2-6c1d-7c3e-9a51-2b7d8e4f1a20","name":"Roadmap","kind":"shared","owner":"ann","created_at":"2024-01-02T03:04:05Z","updated_at":"2025-03-04T05:06:07Z"}
{"type":"member","subject":"ann","role":"owner","added_at":"2024-01-02T03:04:05Z"}
{"type":"member","subject":"ben","role":"editor","added_at":"2024-02-01T00:00:00Z"}
{"type":"update","b64":"AQEBACgBBG1ldGEEbmFtZQF3B1JvYWRtYXAA"}
{"type":"end","updates":1,"members":2,"invites":0,"links":0,"mentions":0}
NDJSON
curl -fsS -X POST "https://canvas.example.com/v1/apps/self/import?mode=create" \
  -H "Authorization: Bearer $KEY" -H 'content-type: application/x-ndjson' \
  --data-binary @board.ndjson
# {"canvas_id":"0190f7a2-…","status":"created","updates_applied":1,"members":2,"invites":0,
#  "links":0,"mentions":0,"digest":"605980e1cc7d393db5cae280abe52a9e38e3f0b130685d3a7c83c186e5f06656"}

# Later: check what is stored.
curl -fsS "https://canvas.example.com/v1/apps/self/canvases/0190f7a2-6c1d-7c3e-9a51-2b7d8e4f1a20/digest" \
  -H "Authorization: Bearer $KEY"
```

Use `--data-binary`, not `-d`: `-d` strips newlines, and the body is newline-delimited.

## Lines

Each line is one JSON object whose `type` names its kind. The lines must come in this order:

| # | `type` | How many |
|---|---|---|
| 1 | `canvas` | exactly 1, and it is the first line |
| 2 | `member` | 0..n |
| 3 | `invite` | 0..n |
| 4 | `link` | 0..n |
| 5 | `mention` | 0..n |
| 6 | `update` | 0..n, in the original order |
| 7 | `end` | exactly 1, and it is the last line |

Every kind is optional apart from `canvas` and `end`. A line of an earlier kind may not come
after a line of a later kind.

Formatting rules:

* Lines end with `\n` or `\r\n`, and the last line may have no newline.
* A blank line (empty or whitespace only) is skipped, but still counts toward line numbers.
* Fields this version does not know are ignored.
* Timestamps are RFC 3339.

A **subject** is the app's id for a user, provisioned beforehand with
[`PUT /v1/apps/self/users/{subject}`](apps.md#users). Any subject the import has not
provisioned is rejected with `422 unknown_subject`.

```json
{"type":"canvas","version":1,"id":"<uuid>","name":"…","kind":"personal|shared","owner":"<subject>","created_at":"…","updated_at":"…"}
{"type":"member","subject":"<subject>","role":"owner|editor","added_at":"…"}
{"type":"invite","id":"<uuid>","email":"…","invited_by":"<subject>|null","status":"pending|accepted|declined|revoked","created_at":"…","accepted_at":"…|null","nudged_at":"…|null","token_sha256_hex":"<64 hex>"}
{"type":"link","id":"<uuid>","token_sha256_hex":"<64 hex>","role":"editor","created_by":"<subject>|null","created_at":"…","uses":3}
{"type":"mention","id":"<uuid>","message_id":"…","from":"<subject>","to":"<subject>","excerpt":"…","created_at":"…","read_at":"…|null"}
{"type":"update","b64":"<lib0 v1 Yjs update>"}
{"type":"end","updates":<n>,"members":<n>,"invites":<n>,"links":<n>,"mentions":<n>}
```

### `canvas`

* `version` must be `1`.
* `id` is a UUID other than nil. The canvas keeps this id.
* `name` is 1–200 characters and not blank.
* `kind` is `personal` or `shared`.
* `owner` is a subject.
* `created_at` and `updated_at` are stored as given, and `updated_at` is not bumped.

### `member`

* The owner's line, if present, must have the role `owner`. If it is absent, the owner becomes
  a member anyway, with `added_at` set to the canvas's `created_at`.
* Every other member must have the role `editor`.
* A Personal canvas has no members besides its owner.
* Each subject may appear once.

### `invite`

* `email` is stored lower-case, which is how canvas-cloud matches it.
* `token_sha256_hex` is the SHA-256 of the invite token, stored as given (lower-cased). An
  invitee who later signs up with that email sees the invite under `GET /v1/invites` and can
  accept it.
* `status` is one of `pending`, `accepted`, `declined` or `revoked`.
* Ids and tokens must be unique, and an email may have at most one pending invite per canvas.
* A Personal canvas has no invites.

### `link`

* `token_sha256_hex` is the SHA-256 of the share-link token. It is stored as given, so the old
  token still opens `GET /v1/canvas-links/{token}` and still joins the canvas.
* `role` is `editor`.
* `uses` is at least 0.
* A Personal canvas has no share links.

### `mention`

* `message_id` is 1–128 characters.
* `excerpt` is plaintext of at most 200 characters. It is sealed again under the canvas's new
  document key, so it can be read with `GET /v1/mentions`.
* `from` and `to` are subjects.
* Ids are unique, and so is each `(message_id, to)` pair.

### `update`

* `b64` holds a lib0 **v1** Yjs update. Standard or URL-safe base64 is accepted, with or
  without padding.
* Send the history in its original order. A snapshot can go first, as an ordinary update.

### `end`

* `end` gives the number of `update`, `member`, `invite`, `link` and `mention` lines the body
  had. If any count differs from what the server saw, the call returns
  `422 count_mismatch`.
* After `end`, only blank lines may follow.

## What the server does

1. **Before reading the history:**
   * It checks the canvas line and the conflict rules below.
   * It replays every `update` into a fresh document, as the lines arrive.
2. **In one database transaction:**
   * It inserts the canvas under the given id with a **new** random document key, sealed under
     this server's KEK, and records `imported_by_app` and `imported_at`.
   * It inserts the members, invites, links and mentions.
   * It writes **one** compacted, sealed snapshot of the document (`encode_state_as_update_v1`)
     with no update log. The snapshot is verified first: re-applied to a fresh document it
     must give the same state vector and digest (garbage-collected if that verifies, else
     uncollected). If neither verifies the import fails with `422 unverifiable_snapshot`.

   Any error anywhere aborts the whole import, and nothing is stored.
3. **After the commit:**
   * Every member gets a `created` event, so their clients refresh.
   * When the import replaced something, members who lost access get `deleted`.
   * The import is audited as `app.canvas.import`.

Memory use is bounded by the document being built plus one line. Rows are written as they
arrive and are never collected.

**History completeness.** After `end`, the server checks the history. Every update must have
integrated, which means:

* no structs are left pending;
* no update skips part of a client's history.

If the history is not complete, the import is rejected with `422 incomplete_history`.

## Conflicts and `mode`

| Situation | `mode=create` (default) | `mode=replace` |
|---|---|---|
| no canvas has this `id` | created | created |
| a canvas has this `id` and **this app imported it** | `409 canvas_exists` + `digest` | replaced atomically |
| a canvas has this `id` and is not this app's import | `409 canvas_exists` (with `digest` only if one of this app's users owns it) | `409 not_replaceable` |
| `personal`, and the owner's Personal canvas is **unused** | that canvas is replaced | that canvas is replaced |
| `personal`, and the owner's Personal canvas is **used** | `409 personal_exists` + `canvas_id` | that canvas is replaced |

A Personal canvas is **unused** when all of the following hold. This is the state
canvas-cloud auto-creates on first sign-in.

* Its document has no root besides `meta`.
* It has no members besides the owner.
* It has no invites, share links or mentions.

**Retries.** A migrator re-running after a partial migration gets `409 canvas_exists` with the
stored `digest` for each canvas it already imported. If that digest equals the one it
expects, it can skip the canvas.

**Replacing with the same id:**

* The canvas's open room is shut first, and its peers are closed with `1001` so they reconnect
  and load the new document.
* Reads and joins that arrive during the transaction wait, then see the new canvas.
* The old document and its key are deleted in the same transaction.

**Replacing a Personal canvas with a different id:** the old canvas is deleted in the same
transaction, its room closes with `4404`, and its members get `deleted`.

The response status is `replaced` when the import replaced a canvas with the same id or a
used Personal canvas. Otherwise it is `created`.

## Response `200`

```json
{"canvas_id":"…","status":"created|replaced","updates_applied":4,"members":2,"invites":1,
 "links":1,"mentions":1,"digest":"<64 hex>"}
```

`members` counts the `member` lines. The owner is a member even when the import had no line
for them.

## Errors

Every error is `{"error": "<code>", "message": "…"}`.

For a `422`, the response also has a `"line": <n>` field: `n` is the 1-based number of the line
at fault, counting blank lines, and the message starts with `line <n>: `. A `413` caused by a
single line also has `line`.

| Status | `error` | When |
|---|---|---|
| 400 | `bad_request` | unknown `mode`; the request body broke off while being read |
| 403 | `import_not_allowed` | the app's `allow_import` is off |
| 409 | `canvas_exists` | see [conflicts](#conflicts-and-mode); also when two imports of one id race |
| 409 | `not_replaceable` | `mode=replace` of a canvas this app did not import |
| 409 | `personal_exists` | the owner's used Personal canvas, without `mode=replace` (`canvas_id` names it) |
| 413 | `payload_too_large` | `Content-Length` or the bytes read exceed the body limit; a line exceeds the line limit |
| 422 | `bad_line` | not JSON; unknown `type`; out of order; a missing or second `canvas`; a missing `end`; a field out of range; a duplicate row; a guest on a Personal canvas; something after `end` |
| 422 | `unknown_subject` | a subject this app has not provisioned (`subject` names it) |
| 422 | `bad_update` | an update that is not base64, not a v1 update, or that yrs rejects |
| 422 | `count_mismatch` | the `end` counts differ from the lines sent |
| 422 | `incomplete_history` | the updates leave pending structs or a gap |
| 422 | `unverifiable_snapshot` | no snapshot encoding re-applies to the same document (yrs 0.28 cannot encode some texts split inside a UTF-16 surrogate pair); keep the source data and report it |

After most errors the server reads the rest of the body, up to the limit, before it answers,
so a streaming client sees the status rather than a reset. The exceptions are a `413` and a
body that broke off.

## Digest

`digest` is the lower-case hex SHA-256 of a canonical JSON rendering of the whole document.
The import returns it, and `GET /v1/apps/self/canvases/{id}/digest` recomputes it from the
database. It is byte-for-byte the reference implementation import producers test against, and
`canvas_cloud_apps::digest::document_digest` documents it in full. In short:

* **Roots.** The rendering is one compact JSON object with a member for each root type
  (`txn.root_refs()`):
  * Root names are sorted byte-wise.
  * A root of undefined type is inferred: map entries → map, else non-empty text → text,
    else array items → array.
  * Roots that render as `{}`, `[]` or `""` are omitted.
* **Maps** (`Y.Map` and `Any::Map`) have their keys sorted byte-wise.
* **Arrays** keep document order.
* **Text and XML** are rendered with `get_string`.
* **A sub-document** is rendered as `{"guid":…}`.
* **Numbers:**
  * Every number becomes an `f64` written by serde_json, so `1` becomes `1.0`.
  * NaN and ±∞ become `null`.
* **Binary** becomes standard base64 with padding.
* **null, undefined and weak links** become `null`.

An empty document renders as `{}`. Its digest is
`44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a`.

The same updates, applied completely, give the same digest whatever their delivery order,
encoding or snapshot compaction. Any change of content changes the digest.
