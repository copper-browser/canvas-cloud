# OpenAPI

`GET /v1/openapi.json` (public, cached 5 minutes) is an OpenAPI 3.1 description of every
route the server answers: the canvas API, accounts and tokens, the event stream, linked-app
routes (`/v1/apps/self/*`, including the import format's NDJSON line types), `/v1/info`,
and the root routes (`/mcp`, `/llms.txt`, `/join/{token}`, `/healthz`).

```sh
curl -s https://canvas.example.com/v1/openapi.json | jq '.paths | keys'
```

* `servers` is `[{"url": <public_url>}]` and `info.version` the running version; both are
  filled in when served.
* Security schemes: `bearer` (`cvs_…` sessions and `cvt_…` personal access tokens; a `cva_…`
  app key also goes there), `appKey` (a `cva_…` key on `/v1/apps/self/*`) and `canvasUser`
  (the `X-Canvas-User` header an app key sends to act as a user).
* Every error is the shared `Error` schema `{"error": "<code>", "message": "…"}`; `401`
  responses carry `WWW-Authenticate: Bearer realm="canvas-cloud"`.
* Main schemas: `Canvas`, `Member`, `Invite`, `ShareLink`, `Mention`, `Person`, `User`,
  `Token`, `AppUser`, `AppCanvas`, `AppStats`, `CanvasRead`/`Shape`, `Op`/`OpsRequest`/
  `OpsResult`, and `ImportLine` (one of `ImportCanvasLine` … `ImportEndLine`).

The document is hand-maintained in `crates/mcp/openapi.json` and embedded in the binary.
Tests in `crates/mcp` check that it is valid OpenAPI 3.1 JSON, that every `$ref` resolves,
that every operation has a unique `operationId`, and that every route (path and method)
registered by the crates' routers appears in it — so a new route without documentation
fails the build. When you add or change a route, update `crates/mcp/openapi.json` in the
same change.
