//! `GET /llms.txt` — a concise plain-text guide for agents ([llmstxt.org] shape: a title, a
//! one-paragraph summary, sections, then links), with absolute URLs built from the configured
//! `public_url`.
//!
//! [llmstxt.org]: https://llmstxt.org/

use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use canvas_cloud_core::config::Config;
use canvas_cloud_core::state::SharedState;

use crate::tools::{all, MAX_READ_BYTES, READ_ONLY_TOOLS};

/// `Content-Type` of `/llms.txt`.
pub const CONTENT_TYPE_TEXT: &str = "text/plain; charset=utf-8";

const TEMPLATE: &str = r#"# Canvas Cloud

> Canvas Cloud is a self-hostable server for collaborative whiteboards ("canvases"). People edit canvases live in a canvas app; agents read and draw on the same canvases through MCP or a small REST API, and everyone sees every change instantly. This server is {base} (version {version}).

All API errors are JSON `{"error": "<code>", "message": "…"}`; `401` means a missing or bad token, `404` a canvas that does not exist or that you are not a member of.

## Auth

Send `Authorization: Bearer <token>` with every request.

- Personal access token (`cvt_…`) — the credential for agents and scripts. Create one at {base}/account (Personal access tokens: choose read or write; it is shown once), or with a session token: `POST /v1/tokens {"name":"my agent","scope":"write"}`. Read-scoped tokens can only read: GET routes and the read-only MCP tools; they cannot join live rooms.
- Session token (`cvs_…`) — returned by `POST /v1/auth/login {"email","password"}` (or sign-up / single sign-on); used by the canvas app and the account page.
- Linked app key (`cva_…`) — a server acting for its own users sends its app key plus `X-Canvas-User: <subject>`.

## MCP

Endpoint: {base}/mcp — MCP Streamable HTTP, protocol 2025-06-18 (2025-03-26 also accepted), JSON responses, stateless. Authenticate with the same bearer token.

```json
{"mcpServers": {"canvas-cloud": {"url": "{base}/mcp", "headers": {"Authorization": "Bearer cvt_…"}}}}
```

Claude Code: `claude mcp add --transport http canvas-cloud {base}/mcp --header "Authorization: Bearer cvt_…"`

Tools: {tools}. A `canvas` argument is a canvas id or `"personal"` (your own Personal canvas). Read-only tokens get only {read_tools}. `read_canvas` results over {read_kb} KB come back as a summary — narrow them with `ids` or `types`.

## REST basics

`{id}` in canvas routes is a canvas UUID or `personal`.

```sh
TOKEN=cvt_…
curl -s {base}/v1/canvases -H "Authorization: Bearer $TOKEN"                          # your canvases, Personal first
curl -s {base}/v1/canvases -H "Authorization: Bearer $TOKEN" -d '{"name":"Roadmap"}'    # create a shared canvas (201)
curl -s "{base}/v1/canvases/personal/read?full=true" -H "Authorization: Bearer $TOKEN"  # shapes, agents, recent chat
curl -s {base}/v1/canvases/personal/ops -H "Authorization: Bearer $TOKEN" \
  -d '{"as":{"name":"Planner"},"ops":[{"op":"add","shape":{"type":"sticky","text":"Hello"}}]}'
```

More: `GET /v1/canvases/{id}/members`, `POST /v1/canvases/{id}/invites {"email"}`, `POST /v1/canvases/{id}/links` (share link), `GET /v1/mentions?unread=1`, `POST /v1/mentions/read {"ids":[…]}`, `GET /v1/events` (server-sent `canvas` events when your canvases change). Every route is in {base}/v1/openapi.json.

## Ops cheat-sheet

`POST /v1/canvases/{id}/ops` (MCP: `apply_ops`) with `{"ops":[…], "as":{"name":"Planner","color":"purple"}, "near":{"x":0,"y":0}, "confirm":false}`. At most 500 ops per call, applied in order in one transaction; a bad op is skipped and reported, the rest still apply. `as` names your agent (shapes get `by`, your cursor shows while you write); `near` is where shapes without `x`/`y` are placed; `confirm: true` unlocks `clear`.

- `{"op":"add","shape":{"type":"sticky","text":"Ship v1","id":"goal"}}` — `id` optional (1-64 of `A-Z a-z 0-9 _ - : .`)
- `{"op":"update","id":"goal","patch":{"text":"Ship v1.1","color":"green"}}`
- `{"op":"move","id":"goal","dx":40,"dy":0}` — a frame carries the shapes inside it
- `{"op":"resize","id":"goal","w":300,"h":200}`
- `{"op":"delete","id":"goal"}` — attached arrows go too
- `{"op":"connect","from":"goal","to":"risk","label":"blocked by"}` — an arrow between two shapes
- `{"op":"clear"}` — deletes every shape; only with `confirm: true`, only when asked

Shape types: `sticky` (text, markdown), `text` (text, fontSize, align), `frame` (title; groups what lies inside it), `arrow` (from, to, label), `image` (src: https or `data:image/…` ≤ 2 MiB, naturalW, naturalH), `link` (url, title), `checklist` (title, columns 1-4, rows ≤ 60, picks `{"<row>": "<column>"}`). Every shape: `x`, `y`, `w`, `h`, `color` (yellow, pink, blue, green, purple, gray, white, or `#rrggbb`).

Result: `{"applied": n, "ids": [shape id or null per op], "errors": [{"index", "op", "error"}]}`.
Read (`GET /v1/canvases/{id}/read`, MCP `read_canvas`): `{canvas, shapes: [{id, type, x, y, w, h, color, z, by, text|title|label, …, frame}], agents, count, chat}`; text is cut to 500 characters unless `full=true`; `ids=a,b` / `types=sticky,frame` narrow the shapes.

## Live rooms (WebSocket)

Every canvas is a Yjs document. `{ws_base}/v1/canvases/{id}/ws?token=<token>` speaks the standard y-websocket protocol (lib0 v1 updates; write tokens or sessions only). With the `y-websocket` package: `new WebsocketProvider("{ws_base}/v1/canvases", "<id>/ws", doc, {params: {token}})`. REST ops and MCP edits go through the same room, so you rarely need this as an agent.

## Limits

- Ops: 500 per call; `POST /ops` bodies ≤ 4 MiB, other request bodies ≤ 64 KiB; `POST /mcp` ≤ {mcp_mib} MiB.
- Sticky/text ≤ 20 000 characters; images ≤ 2 MiB; canvas names ≤ 200 characters.
- Chat mention notifications: 60 calls per minute per user. `/v1/auth/*`: {auth_per_minute} requests per minute per IP. Over a limit: `429` with `Retry-After`.

## Links

- [OpenAPI 3.1]({base}/v1/openapi.json): every /v1 route with request/response schemas and errors
- [Server info]({base}/v1/info): version, features, sign-in methods, limits
- [Account]({base}/account): sign in, create personal access tokens, connect an agent
- [MCP endpoint]({base}/mcp): POST only
"#;

/// `ws(s)://…` for an `http(s)://…` base URL.
fn ws_base(base: &str) -> String {
    if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_owned()
    }
}

/// The guide for this server (`public_url` substituted).
pub fn render(cfg: &Config) -> String {
    let base = cfg.public_url.trim_end_matches('/');
    let tools: Vec<&str> = all().iter().map(|t| t.name).collect();
    #[allow(clippy::cast_precision_loss)]
    let mcp_mib = crate::MAX_BODY_BYTES as f64 / f64::from(1u32 << 20);
    TEMPLATE
        .replace("{ws_base}", &ws_base(base))
        .replace("{base}", base)
        .replace("{version}", env!("CARGO_PKG_VERSION"))
        .replace("{tools}", &tools.join(", "))
        .replace("{read_tools}", &READ_ONLY_TOOLS.join(", "))
        .replace("{read_kb}", &(MAX_READ_BYTES / 1024).to_string())
        .replace("{mcp_mib}", &format!("{mcp_mib:.2}"))
        .replace("{auth_per_minute}", &cfg.limits.auth_per_minute.to_string())
}

/// `GET /llms.txt` — public.
pub(crate) async fn serve(State(state): State<SharedState>) -> Response {
    (
        [
            (CONTENT_TYPE, CONTENT_TYPE_TEXT),
            (CACHE_CONTROL, "public, max-age=300"),
        ],
        render(&state.cfg),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_absolute_urls() {
        let mut cfg = Config::for_tests("postgres://localhost/unused");
        cfg.public_url = "https://canvas.example.com".into();
        let txt = render(&cfg);
        assert!(txt.starts_with("# Canvas Cloud\n\n> "));
        assert!(txt.contains("\"url\": \"https://canvas.example.com/mcp\""));
        assert!(txt.contains("wss://canvas.example.com/v1/canvases/{id}/ws?token="));
        assert!(txt.contains("https://canvas.example.com/v1/openapi.json"));
        assert!(txt.contains("apply_ops, list_members"));
        assert!(!txt.contains("{base}") && !txt.contains("{tools}"));
        // No product other than Canvas Cloud is named (spelled split so a repo-wide grep stays clean).
        assert!(!txt.to_lowercase().contains(concat!("cop", "per")));
        assert_eq!(ws_base("http://127.0.0.1:8480"), "ws://127.0.0.1:8480");
    }
}
