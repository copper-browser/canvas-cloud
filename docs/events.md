# `GET /v1/events` — your change notifications

One Server-Sent Events stream per signed-in client, carrying the caller's `canvas` events
(the table in [canvas.md](canvas.md#change-notifications)): a canvas was created, renamed,
shared with you, changed, you were @mentioned, …. It tells clients **when** to refetch; it
never carries canvas content. Served by `canvas-cloud-accounts`.

```http
GET /v1/events
Authorization: Bearer <cvs_… | cvt_… | cva_… + X-Canvas-User>
Accept: text/event-stream
Last-Event-ID: 1760903238000412     (optional; to resume)
```

- Any credential works, read-scoped tokens included. A linked app acting as one user
  (`cva_…` + `X-Canvas-User`) gets that user's events; to get **all** its users' events in
  one stream an app uses `GET /v1/apps/self/events` instead.
- Browsers' `EventSource` cannot set headers: pass `?token=<credential>` (accepted only with
  `Accept: text/event-stream`, which `EventSource` always sends) and, when not using the
  browser's own reconnect, `?last_event_id=<id>`. The `Last-Event-ID` header wins over the
  query parameter.
- Response headers: `Content-Type: text/event-stream`, `Cache-Control: no-cache`,
  `X-Accel-Buffering: no` (so nginx does not buffer it).

## Frames

```text
event: canvas
id: 1760903238000415
data: {"type":"canvas","canvas_id":"01a155b4-896a-76b9-a32f-184a9c7d1e18","kind":"renamed"}

event: ready
id: 1760903238000415
data: {"user_id":"01a155b4-…","last_event_id":1760903238000415}

event: resync
data: {"reason":"gap"}

: keepalive
```

| Event | `id` | `data` | Meaning |
|---|---|---|---|
| `canvas` | yes | `{"type":"canvas","canvas_id","kind"}` | something about that canvas changed; refetch it (or the list) |
| `ready` | yes | `{"user_id","last_event_id"}` | sent **once** per connection, after any replay: everything up to this id has been delivered |
| `resync` | no | `{"reason":"gap"}` or `{"reason":"lagged","skipped":n}` | some events were lost; refetch everything you cache, then carry on (the stream stays open) |
| (comment) | — | `: keepalive` | every 15 s, to keep proxies from closing an idle stream |

Order on connect: `resync` (only if the resume point was lost) → replayed `canvas` frames
(only when resuming) → `ready` → live `canvas` frames, with `resync {"reason":"lagged"}`
whenever this connection fell more than 256 events behind.

## Ids and resuming

Every event has an id that only increases, also across server restarts: the server's start
time in Unix microseconds plus a per-process counter. Ids stay below 2^53, so JavaScript
numbers hold them exactly; treat them as opaque except for ordering.

Reconnect with the last id you saw (`EventSource` does this by itself with the
`Last-Event-ID` header):

| Your `Last-Event-ID` | You get |
|---|---|
| none | `ready` at the current id, then live events |
| still in the server's buffer (the last 4096 events of all users) | your missed events with id > yours, then `ready`, then live |
| equal to the newest id | `ready`, then live |
| too old for the buffer, from before a restart, newer than anything issued, or not a number | `resync {"reason":"gap"}`, then `ready`, then live |

After a `resync`, use the `ready` frame's id as your new position. Events are kept in
memory only: they are a cue to refetch, not a durable log, so a client that was offline
longer than the buffer covers (or across a restart) refetches once and carries on.

## When the stream ends

- The credential is revoked, expires, or the user is disabled: checked every 60 s (failing
  open on database errors), then the stream closes. A reconnect then gets `401`.
- The server shuts down (graceful drain).
- The client disconnects.

Holding a stream open does not extend a session; sessions expire 30 days after their last
API use as usual.

## Example (browser)

```js
const es = new EventSource(`/v1/events?token=${encodeURIComponent(token)}`);
es.addEventListener("canvas", (e) => {
  const { canvas_id, kind } = JSON.parse(e.data);
  refreshCanvas(canvas_id, kind); // e.g. kind === "deleted" → drop it
});
es.addEventListener("resync", () => refreshEverything());
es.addEventListener("ready", (e) => console.debug("up to date at", e.lastEventId));
```

## Example (curl)

```sh
curl -N -H "Authorization: Bearer $TOKEN" -H "Accept: text/event-stream" \
  https://canvas.example.com/v1/events
```

## Metrics

`sse_subscribers{stream="user"}` — open `/v1/events` streams.
