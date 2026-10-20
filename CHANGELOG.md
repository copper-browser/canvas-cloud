# Changelog

All notable changes to canvas-cloud. Versions follow [Semantic Versioning](https://semver.org/);
before 1.0, breaking changes to the client contract (see
[docs/v0.1.0.md](docs/v0.1.0.md#compatibility)) only come with a new minor version and are
listed under **Breaking**.

## 0.2.0

Server-side ops parity with Canvas App (65803aa), and delegated sign-in.

### Breaking

- `{"op":"add","shape":{"type":"checklist",…}}` now creates the checklist **card** — a
  `sticky` with `view: "checklist"` whose text is a markdown task list (`### Title`,
  `- [x] Ann · Yes`, `  > comment`, `  @Assignee`) — instead of a shape of type
  `checklist`. Older Coppers (and 0.1.0 clients) show it as a note with task boxes. Read
  reports it as `type: "sticky"`, `view: "checklist"` with `title`, `columns`, `rows`
  (`{label, pick, by?, at?, note?, noteBy?, noteAt?, assignees?}`, no row `id`) and `tally`;
  `types=checklist` still finds it. Checklists of the older type still read and update in
  place.

### Added

- `GET /v1/info` → `features` gains `ops_v2`. With it:
  - checklist cards: `picks`, `notes` (a row's comment) and `assignees` (≤ 12 per row; canvas
    members by id, email, name or a unique start of a name, anyone else by name) on add and
    update; `update {view:"checklist"}` / `{view:null}` turns a note with a task list into a
    card and back. Older checklists also take `notes` and `assignees`.
  - timelines: `add {type:"timeline", title?, start, end, step?, items?}` (a sticky with
    `view: "timeline"`); `update` takes the same plus `addItems` and `editItems`
    (`{"<date · label | label | date | index>": patch | null}`).
  - live web frames: a `link` with `live: true` (or `type: "web"`), 960×640 by default,
    http(s) only; `update {live:false}` turns it back into a card.
  - comment threads: `comment` (`on`, `text`; `ids[i]` is the thread id), `reply` (reopens a
    resolved thread) and `resolve` (`resolved: true|false`), attributed to `as` or the caller,
    stored in the document's `comments` map as Canvas App writes them.
  - read: `view`, checklist-card rows and tally, timeline `start`/`end`/`step`/`items`, `live`
    on links, and `comments: [{id, anchor, anchorDeleted?, resolved, resolvedBy?, messages}]`;
    `types` matches `checklist`, `timeline` and `web` by kind.
  - every op validates before it writes: a failing op leaves the document untouched.
  - the MCP `apply_ops` / `read_canvas` descriptions and schemas, `/llms.txt`, and
    `/v1/openapi.json` describe the new vocabulary.

- Delegated sign-in: `[[delegated_sign_in]]` config tables (`id`, `name`, `start_url`) let
  another site act as a sign-in provider. They are listed after the `[[oidc]]` providers in
  `GET /v1/info` → `auth.oidc` and `GET /v1/auth/oidc` with `kind: "delegated"` and
  `start_url`; `GET /v1/auth/oidc/{id}/start?redirect_uri=…` checks `redirect_uri` (this
  server's origin, no fragment) and redirects to the site, which answers
  `redirect_uri#token=cvs_…` like OIDC. `/account` shows them as buttons; `doctor` lists
  every sign-in provider. See [docs/auth.md](docs/auth.md#delegated-sign-in).
- `POST /v1/apps/self/users/{subject}/sessions`: a linked app with `trust_email` signs one of
  its users in (`201 {token, user_id, expires_at}`, an ordinary `cvs_…` session,
  `Cache-Control: no-store`), limited per app by `limits.app_sessions_per_minute` (default
  60), audited as `app.user.session`; metrics `app_sessions_created_total` and
  `app_sessions_rate_limited_total`. `GET /v1/info` → `features` gains `app_sessions` and
  `limits` gains `app_sessions_per_minute`.
- Every sign-in provider entry now has `kind` (`"oidc"` for `[[oidc]]` providers); the other
  fields are unchanged.

### Notes

- Assigning someone through ops does not post the chat notice Canvas App posts for a new
  assignee (the server never writes chat). Placement of shapes without `x`/`y` keeps the
  server's own free-spot search.

## 0.1.0

First release. Release notes: [docs/v0.1.0.md](docs/v0.1.0.md).

### Added

- Canvases: Personal and shared canvases, owner/editor roles, email invites, share links,
  people directory, chat @mentions; REST API under `/v1`.
- Live rooms over the standard `y-websocket` protocol, durable before broadcast, with
  verified snapshot compaction (a snapshot is stored only if it re-applies to the same
  document, which works around a yrs 0.28 surrogate-pair encoding bug).
- Agent read format and validated ops on the live document.
- Encryption at rest: per-canvas keys wrapped by the master key (AES-256-GCM).
- Password accounts with a signup policy, OIDC sign-in, personal access tokens (`read` /
  `write`), the `/account` page, and `GET /v1/events`.
- Linked apps (`/v1/apps/self/*`): user provisioning, app event stream, canvases, stats,
  digests, import format v1.
- MCP server (`POST /mcp`), `/llms.txt`, OpenAPI 3.1 at `/v1/openapi.json`.
- TLS: self-signed, Let's Encrypt (TLS-ALPN-01), or off behind a proxy.
- `install.sh` for systemd Linux with native PostgreSQL, including
  `CANVAS_CLOUD_LISTEN`, `CANVAS_CLOUD_TLS_MODE`, `CANVAS_CLOUD_PUBLIC_URL` and `--dry-run`
  for reverse-proxy and loopback installs.
- CLI: `serve`, `migrate`, `doctor`, `healthcheck`, `tls-init`, `gen-config`, `admin users`,
  `admin settings`, `apps`.
- Prometheus metrics, JSON logs.
- Release binaries for Linux `x86_64` and `aarch64` (glibc 2.28+).
