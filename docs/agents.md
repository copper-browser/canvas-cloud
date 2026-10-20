# Agents: MCP, llms.txt and OpenAPI

The `canvas-cloud-mcp` crate (`crates/mcp`) is how agents find and use a Canvas Cloud server:

| Method | Path | Auth | What |
|---|---|---|---|
| POST | `/mcp` | any credential | [Model Context Protocol](https://modelcontextprotocol.io/specification/2025-06-18) server (Streamable HTTP, JSON responses) |
| GET, other | `/mcp` | — | `405`, `Allow: POST` |
| GET | `/llms.txt` | public | plain-text guide for agents ([llmstxt.org](https://llmstxt.org/) shape) |
| GET | `/v1/openapi.json` | public | OpenAPI 3.1 for every route ([openapi.md](openapi.md)) |

`GET /v1/info` lists `mcp` in `features` when the crate is mounted.

MCP tools call the engine in-process (`canvas_cloud_engine::api`), so they behave exactly
like the REST routes: the same membership checks, validation, events and errors. An edit
made through `apply_ops` lands in the live Yjs room; everyone who has the canvas open sees
it at once, with the agent's name on the new shapes and its cursor shown while it writes.

## Connect an agent

1. Get a personal access token: sign in at `https://<server>/account` → *Personal access
   tokens* → choose **write** (or **read** for an agent that should only look), or with a
   session token:

   ```sh
   curl -s https://canvas.example.com/v1/tokens -H "Authorization: Bearer $SESSION" \
     -d '{"name":"my agent","scope":"write"}'      # → {"token":"cvt_…", …} (shown once)
   ```

2. Point the MCP client at `https://<server>/mcp` with the token as a bearer header:

   ```json
   {
     "mcpServers": {
       "canvas-cloud": {
         "url": "https://canvas.example.com/mcp",
         "headers": { "Authorization": "Bearer cvt_…" }
       }
     }
   }
   ```

   Claude Code:
   `claude mcp add --transport http canvas-cloud https://canvas.example.com/mcp --header "Authorization: Bearer cvt_…"`.

Any credential works: a session (`cvs_…`), a personal access token (`cvt_…`), or a linked
app key (`cva_…`) plus `X-Canvas-User: <subject>` for a server acting for one of its users.

## Transport

* One JSON-RPC 2.0 message per `POST /mcp`. A request gets `200` with one
  `application/json` response; a notification (no `id`) or a client response gets `202`
  with no body. The server never streams (`text/event-stream` is never returned) and never
  sends requests to the client.
* **Protocol**: `2025-06-18`. `initialize` answers with the client's `protocolVersion` when
  it is `2025-06-18` or `2025-03-26`, otherwise with `2025-06-18`. Later requests may send
  `MCP-Protocol-Version`; an unsupported value is `400`. A request without it is treated as
  `2025-03-26` (per the spec).
* **Batches** (a JSON array) are a `2025-03-26` feature: accepted only with
  `MCP-Protocol-Version: 2025-03-26` (answered with an array of the responses, or `202` if
  all were notifications), otherwise `400` (`-32600`).
* **Stateless**: no `Mcp-Session-Id` is issued (one sent by the client is ignored). Every
  POST authenticates on its own; `initialize` is not required before other methods.
* **Origin**: a request with an `Origin` header must come from an origin in
  `cors.allowed_origins` or from `public_url`'s own origin, else `403` (DNS-rebinding guard;
  checked before the credential). No `Origin` (a non-browser client) is fine. CORS
  preflights allow `MCP-Protocol-Version` and `Mcp-Session-Id`.
* **Body**: at most 4 MiB + 64 KiB (an `apply_ops` call as large as `POST /ops` allows).

Order of checks and their HTTP answers:

| Check | Failure |
|---|---|
| `Origin` | `403 {"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"Forbidden: this Origin may not call /mcp"}}` |
| credential | `401` + `WWW-Authenticate: Bearer realm="canvas-cloud"`, the usual `{"error","message"}` body |
| `MCP-Protocol-Version` | `400`, JSON-RPC error `-32600` |
| body size | `413`, `-32600` |
| JSON | `400`, `-32700` (parse error, `id: null`) |
| message shape (`jsonrpc` ≠ `"2.0"`, no `method`, bad `id`) | `400`, `-32600` |

Methods (all `200`):

| Method | Result |
|---|---|
| `initialize` | `{protocolVersion, capabilities: {tools: {listChanged: false}}, serverInfo: {name: "canvas-cloud", title, version}, instructions}` |
| `ping` | `{}` |
| `tools/list` | `{tools: [{name, title, description, inputSchema, outputSchema, annotations}]}` — no pagination |
| `tools/call` `{name, arguments}` | a tool result (below) |
| anything else | error `-32601` (method not found) |

`tools/call` with an unknown tool name, or without `name`, is error `-32602`. Everything
else — bad arguments, API errors, a read-only token on a write tool — is a **tool result**
with `isError: true`, so the model sees why and can correct itself.

```sh
curl -s https://canvas.example.com/mcp -H "Authorization: Bearer $TOKEN" \
  -H 'MCP-Protocol-Version: 2025-06-18' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"read_canvas","arguments":{"canvas":"personal"}}}'
```

### Tool results

Success:

```json
{ "content": [{ "type": "text", "text": "<the JSON below, pretty-printed>" }],
  "structuredContent": { … },
  "isError": false }
```

`structuredContent` matches the tool's `outputSchema`. Failure:

```json
{ "content": [{ "type": "text", "text": "{\n  \"error\": \"not_found\",\n  \"message\": \"not found: no such canvas, or you are not a member of it (…)\"\n}" }],
  "isError": true }
```

The text is the REST error body: the same `error` codes as the HTTP API (`bad_request`,
`not_found`, `forbidden`, `conflict`, `rate_limited`, `read_only_token`, …), with a hint in
`message` for the generic `not_found` and `forbidden`. Internal errors are logged and shown
as `{"error":"internal","message":"internal error"}`.

Arguments are read leniently, because models are not exact: unknown arguments are ignored;
booleans may be `"true"`/`"false"`; integers may be strings; `ids`/`types` may be an array, a
comma-separated string or a JSON-array string; `ops`, `as` and `near` may be stringified JSON.

## Tools

`canvas` is always a canvas id (UUID from `list_canvases`) or `personal` (any case) for the
caller's own Personal canvas, which is created on first use.

| Tool | Read-only token | Arguments | `structuredContent` |
|---|---|---|---|
| `whoami` | ✓ | — | `{user: {id, email, display_name}, via, read_only, server: {name, version, public_url}}` |
| `list_canvases` | ✓ | — | `{canvases: [Canvas]}` — Personal first, then most recently updated |
| `create_canvas` | | `name` | `Canvas` (a new shared canvas you own) |
| `rename_canvas` | | `canvas`, `name` | `Canvas` (owner only; not Personal) |
| `read_canvas` | ✓ | `canvas`, `ids?`, `types?`, `full?` | the `GET /v1/canvases/{id}/read` JSON, or a summary (below) |
| `apply_ops` | | `canvas`, `ops`, `as?`, `near?`, `confirm?` | `{applied, ids, errors}` |
| `list_members` | ✓ | `canvas` | `{members: [{user_id, email, display_name, role, added_at}]}` |
| `invite` | | `canvas`, `email` | the invite + `nudged` + `created` |
| `create_share_link` | | `canvas` | `{id, token, url, canvas_id, role, created_at}` |
| `list_mentions` | ✓ | `unread_only?`, `limit?` | `{mentions: [Mention], unread}` |
| `mark_mentions_read` | | `ids?`, `canvas?` | `{ok, updated}` |

`Canvas` is `{id, name, kind: personal|shared, role: owner|editor, owner: {id, email,
display_name}, member_count, created_at, updated_at}` — exactly `GET /v1/canvases`.

* **`whoami`** — `via` is `{"kind":"session"}`, `{"kind":"token","scope":"read"|"write"}` or
  `{"kind":"app","app_id","subject"}`.
* **`read_canvas`** — `{canvas: {id, name, kind}, shapes, comments, agents, selection, count,
  chat}` as in [canvas-protocol.md](canvas-protocol.md#read): shapes sorted top-to-bottom then
  left-to-right, text cut to 500 characters unless `full`, `count` = every shape, `chat` = the
  last 50 messages. Checklist cards (`view: "checklist"`: title, columns, rows with picks,
  comments and assignees, tally), timelines (`view: "timeline"`: start, end, step, items) and
  live links (`live: true`) read like Copper's `canvas_read`; `comments` lists the comment
  threads on shapes; `types` also matches `checklist`, `timeline` and `web`. When the pretty-printed result would be over **200 KB** it returns a
  summary instead, still `isError: false`:

  ```json
  { "canvas": {…}, "truncated": true, "bytes": 296113, "limit_bytes": 204800,
    "count": 17, "matched": 17, "by_type": {"sticky": 16, "frame": 1},
    "frames": [{"id": "f", "type": "frame", "x": -2000, "y": -2000, "w": 480, "h": 320, "label": "Area"}],
    "shapes": [{"id", "type", "view"?, "live"?, "x", "y", "w", "h", "frame"?, "label"?}, …],
    "shapes_listed": 17, "agents": [], "comments": 0, "chat_messages": 0,
    "hint": "… Call read_canvas again with `ids` … or `types` …" }
  ```

  (at most 400 shapes and 100 frames listed, labels cut to 60 characters).
* **`apply_ops`** — the `POST /v1/canvases/{id}/ops` envelope; the tool description carries the
  full ops cheat-sheet (shape types and props — checklist cards with notes and assignees,
  timelines, live web frames — every op including `comment`/`reply`/`resolve`, ids and
  `shape:<id>` refs, `as`, `near`, `confirm`). The rich shapes and comment ops need `features`
  to include `ops_v2` (0.2.0+). A bad op is skipped and reported in `errors` while the rest apply
  (`isError: false`); when **nothing** applied (e.g. a lone `clear` without `confirm`) the
  call is `isError: true` with the `{applied: 0, ids, errors}` body. A malformed envelope is
  `isError` `bad_request` (after the membership check, so strangers get `not_found`).
* **`invite`** — any member, shared canvases only. `created: false` means an invite was
  already pending (`nudged: true` when this call reminded the invitee).
* **`create_share_link`** — any member, shared canvases only. `url` is
  `{public_url}/join/{token}`, which opens the canvas app (or a landing page); the token is
  shown only here.
* **`list_mentions`** — `limit` 1–100 (default 50), newest first. `Mention` is `{id, canvas:
  {id, name}, from: {id, email, display_name}, message_id, excerpt, created_at, read_at}`.
* **`mark_mentions_read`** — `ids` (≤ 500 mention ids), or every mention on `canvas`, or with
  neither every unread mention of the caller.

### Read-only tokens

A `read`-scoped personal access token may `POST /mcp` (the transport needs it), but
`tools/list` shows only `whoami`, `list_canvases`, `read_canvas`, `list_members` and
`list_mentions`, and calling any other tool is an `isError` result:

```json
{ "error": "read_only_token",
  "message": "read-only token: `apply_ops` needs a write-scoped personal access token (this one may use whoami, list_canvases, read_canvas, list_members, list_mentions)" }
```

### Tool annotations

Each tool has `annotations: {title, readOnlyHint, destructiveHint, idempotentHint,
openWorldHint: false}`; `apply_ops` is the only destructive one (it can delete and clear).

## `GET /llms.txt`

`text/plain; charset=utf-8`, cached 5 minutes. Rendered from `public_url`: what the server
is, auth (personal access tokens from `/account` or `POST /v1/tokens`, sessions, app keys),
the MCP endpoint with a config snippet, REST basics with curl (list, create, read, ops), the
ops cheat-sheet, the WebSocket/`y-websocket` pointer, limits, and links to
`/v1/openapi.json`, `/v1/info` and `/account`.

```sh
curl -s https://canvas.example.com/llms.txt
```

## `GET /v1/openapi.json`

See [openapi.md](openapi.md).

## Observability

Every tool call logs `mcp tool call` (`tool`, `user_id`, `outcome`, `error` code, `ms`) and
counts `mcp_tool_calls_total{tool, outcome="ok"|"error"}`; `initialize` logs the client name
and negotiated version. Arguments and results are never logged.
