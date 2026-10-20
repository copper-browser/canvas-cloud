# Accounts, tokens and sign-in

The `canvas-cloud-accounts` crate (`crates/accounts`) gives a standalone canvas-cloud its
humans: password accounts, OIDC sign-in, personal access tokens, the `/v1/events` stream
(see [events.md](events.md)) and the `/account` page. Everything is under `/v1` except the
page; errors use the common `{"error","message"}` shape.

## Who is a user

There is one `users` table. A row is created by any of:

| How | Password | Notes |
|---|---|---|
| `POST /v1/auth/signup` | yes | subject to the [signup policy](#signup-policy) |
| `canvas-cloud admin users create --email … --password-file …` | yes | works even when signup is closed |
| a linked app (`PUT /v1/apps/self/users/{subject}`) | no | the app authenticates the person; the row is linked in `app_users` |
| OIDC sign-in (`/v1/auth/oidc/{id}/…`) | no | subject to the [creation rules](#who-may-sign-in-and-who-gets-an-account) |

**Linked-app users and standalone accounts are the same rows.** A person an app
provisioned can also use the server directly, and keeps the same id, canvases and
memberships:

- give them a password with `canvas-cloud admin users set-password --email … --password-file …`
  (revokes their sessions), then they sign in at `/account` or `POST /v1/auth/login`; or
- let them sign in through an OIDC provider that verifies their email (linking is by
  verified email, see below).

Rows without a password (app- or OIDC-created) **cannot** log in with a password: login
answers the same `401 invalid_credentials` as for a wrong password. Signup never claims an
existing row, password-less or not (`409 email_taken`): otherwise whoever typed the email
first would get that person's canvases.

Emails are trimmed, validated (`x@y.z`, ≤ 254 bytes, no whitespace) and lower-cased, like
canvas invites; lookups are case-insensitive.

## Credentials

| Prefix | What | Where it comes from | Can manage tokens / sign out |
|---|---|---|---|
| `cvs_` | session (expires 30 days after its last use) | signup, login, OIDC, a `trust_email` linked app ([delegated sign-in](#delegated-sign-in)) | yes |
| `cvt_` | personal access token, `read` or `write` | `POST /v1/tokens` | no |
| `cva_` + `X-Canvas-User: <subject>` | a linked app acting for one of its users | `canvas-cloud apps create` | no |

Send `Authorization: Bearer <credential>`; `?token=` is accepted only on WebSocket
upgrades and on `GET /v1/events` with `Accept: text/event-stream` (EventSource cannot set
headers). Read tokens may only `GET`/`HEAD` and never join a live canvas room (the
WebSocket upgrade is refused with `403`); any other method answers `403 read_only_token`.

## Endpoints

### `POST /v1/auth/signup {email, password, display_name?}` → `201 {token, user}`

`user` is `{id, email, display_name, created_at}`; `display_name` defaults to the email's
local part. Passwords: 10–256 characters, hashed with Argon2id (m = 64 MiB, t = 3, p = 1).

| Error | When |
|---|---|
| `400 bad_request` | invalid email, password too short/long, malformed body |
| `403 signup_closed` | the policy does not allow signup |
| `403 signup_domain_not_allowed` | signup is open but not for this email's domain |
| `409 email_taken` | an account (of any kind) already has this email; only reported once the policy admits the caller |
| `429 rate_limited` | per-IP or per-email limit |

#### Signup policy

1. If **no account with a password exists yet**, anyone may sign up: the first person to
   reach a fresh server becomes its first user. Accounts created by linked apps or OIDC do
   not count (they have no password). Concurrent first signups are serialised (advisory
   lock + re-check), so exactly one of them wins.
2. Otherwise signup follows the `allow_signup` setting (default **false**).
3. When `allow_signup` is true and `signup_domains` is non-empty, only emails whose domain
   is exactly one of them (case-insensitive; subdomains are not included) may sign up.

Change the settings with `canvas-cloud admin settings set allow_signup true|false` and
`canvas-cloud admin settings set signup_domains example.com,example.org` (`""` for none).
`GET /v1/info` → `signup` says whether rule 1 or 2 currently admits anyone.

### `POST /v1/auth/login {email, password}` → `{token, user}`

Wrong password, unknown email, a password-less account and an over-long password all get
the same `401 invalid_credentials` after the same amount of work (a dummy Argon2 verify
for accounts without a hash). A disabled account answers `401 account_disabled`, but only
after the correct password.

### `POST /v1/auth/logout [{all: true}]` → `{ok: true, revoked: n}`

Session only (`403 session_required` for tokens and apps). Ends this session, or with
`all` every session of the user. Access tokens are not affected; revoke them separately.

### `GET /v1/auth/me` → `{user, via}`

Any credential. `via` says how the caller authenticated:

```json
{"kind": "session"}
{"kind": "token", "scope": "read"}
{"kind": "app", "app_id": "…"}
```

### `PATCH /v1/auth/me {display_name}` → `{user, via}`

Any write-capable credential. 1–200 characters after trimming (control characters are
dropped); `400` otherwise.

### `POST /v1/auth/password {current, new}` → `{ok: true, revoked_sessions: n}`

Session only. `old` is accepted as an alias of `current`. Signs out every **other**
session of the user; this session and all access tokens keep working.

| Error | When |
|---|---|
| `400 bad_request` | `new` shorter than 10 or longer than 256 characters |
| `403 invalid_credentials` | `current` is wrong (403, not 401: the session itself is fine) |
| `403 password_not_set` | the account has no password (app- or OIDC-created); an administrator can set one |

### Personal access tokens

All three need a **session** (`403 session_required` otherwise): a token cannot mint,
list or revoke tokens.

- `GET /v1/tokens` → `{tokens: [{id, name, scope, created_at, last_used_at, expires_at}],
  limit: 50}` — live tokens only (not revoked, not expired), newest first. The secret is
  never listed.
- `POST /v1/tokens {name, scope: "read"|"write", expires_in_days?}` → `201 {token: "cvt_…",
  id, name, scope, created_at, last_used_at: null, expires_at}`. `token` is shown **once**.
  `name` 1–200 characters; `expires_in_days` 1–3650, omitted or `null` = never expires.
  At most 50 live tokens per user (`403 token_limit`).
- `DELETE /v1/tokens/{id}` → `{ok: true}`; `404` if it is not one of your live tokens.
  Revocation is immediate; an open `/v1/events` stream on that token ends within a minute.

`last_used_at` is updated (at most once a minute) whenever the token authenticates.

### Rate limits

Every `/v1/auth/*` route is limited per client IP by core (`limits.auth_per_minute`,
default 10 per minute; with `trust_proxy` the IP comes from `X-Forwarded-For`). Signup,
login and password changes are additionally limited per (lower-cased) email with the same
budget, so spreading guesses for one account over many IPs does not help. Both answer
`429 rate_limited`.

### Examples

```sh
S=https://canvas.example.com

# Sign up (or sign in with /v1/auth/login and the same body without display_name).
curl -s -X POST $S/v1/auth/signup -H 'Content-Type: application/json' \
  -d '{"email":"ada@example.org","password":"correct horse battery","display_name":"Ada"}'
# → 201 {"token":"cvs_…","user":{"id":"…","email":"ada@example.org","display_name":"Ada","created_at":"…"}}

SESSION=cvs_…
curl -s $S/v1/auth/me -H "Authorization: Bearer $SESSION"
# → {"user":{…},"via":{"kind":"session"}}

# A read-only token for a script, valid 30 days. Save "token" now; it is not shown again.
curl -s -X POST $S/v1/tokens -H "Authorization: Bearer $SESSION" -H 'Content-Type: application/json' \
  -d '{"name":"ci","scope":"read","expires_in_days":30}'
# → 201 {"token":"cvt_…","id":"…","name":"ci","scope":"read","created_at":"…","last_used_at":null,"expires_at":"…"}

curl -s $S/v1/canvases -H "Authorization: Bearer cvt_…"          # works with the token
curl -s -X DELETE $S/v1/tokens/<id> -H "Authorization: Bearer $SESSION"   # → {"ok":true}

curl -s -X POST $S/v1/auth/logout -H "Authorization: Bearer $SESSION"     # → {"ok":true,"revoked":1}
```

## OIDC sign-in

Any OpenID Connect provider with discovery works (Google, Microsoft Entra, GitLab,
Keycloak, Authentik, Okta, …). Configure one `[[oidc]]` table per provider:

```toml
[[oidc]]
id = "google"                        # path segment: /v1/auth/oidc/google/…  ([a-z0-9_-], ≤ 32)
name = "Google"                      # button label (defaults to id)
issuer = "https://accounts.google.com"
client_id = "1234.apps.googleusercontent.com"
client_secret_file = "/etc/canvas-cloud/google-secret"   # omit for a public (PKCE-only) client
allowed_domains = ["example.com"]    # only these verified email domains (empty = any)
redirect_uris = ["myapp://signed-in"] # extra places results may go (see below)
```

Register **`{public_url}/v1/auth/oidc/{id}/callback`** as the redirect URI at the provider.
`GET /v1/auth/oidc` lists the providers for sign-in buttons as
`{providers: [{id, name, start, kind}]}`, and `GET /v1/info` → `auth.oidc` is the same list
(an array): the `[[oidc]]` providers (`kind: "oidc"`) in config order, then the
[delegated](#delegated-sign-in) ones (`kind: "delegated"`, plus `start_url`). `start` is
always `/v1/auth/oidc/{id}/start` on this server, so a client treats every entry alike.

### Flow

1. The client sends the browser to
   `GET /v1/auth/oidc/{id}/start?redirect_uri=<where the result should go>`.
   `redirect_uri` must be listed verbatim in the provider's `redirect_uris`, or be an
   `http(s)` URL with the same origin as `public_url` (e.g. `{public_url}/account`), and must
   not contain a fragment (`400 redirect_uri_not_allowed` otherwise). An unknown provider is
   `404 unknown_provider`; an unreachable or misconfigured one `502 oidc_unavailable`.
2. The server stores the sign-in (`oidc_states`: SHA-256 of `state`, the `nonce`, the PKCE
   verifier and `redirect_uri`; 10 minutes, single use), sets the `cvc_oidc` cookie
   (`HttpOnly; SameSite=Lax; Path=/v1/auth/oidc/`, `Secure` when `public_url` is https) and
   redirects to the provider with `response_type=code`, `scope=openid email profile`,
   `state`, `nonce` and `code_challenge` (S256).
3. The provider redirects back to `…/callback?code=…&state=…`. The server consumes the
   state, checks the cookie matches it (so a callback link cannot be replayed in another
   browser), exchanges the code (with `code_verifier`; client secret via HTTP Basic, or in
   the form when the provider only advertises `client_secret_post`; none for a public
   client) and verifies the `id_token`:
   - signature with the provider's JWKS (`RS256` or `ES256` only; the key type must match;
     an unknown `kid` refetches the key set, at most every 30 s),
   - `iss` = the discovered issuer, `aud` contains `client_id`, `azp` = `client_id` when
     present (required when there are several audiences), `exp`/`nbf`/`iat` with 60 s of
     leeway, `nonce` = the stored nonce,
   - `email` present and `email_verified` true (`true` or `"true"`).
4. It then finds the user by that email or creates one (rules below), starts a session and
   redirects to **`redirect_uri#token=cvs_…`**. Errors redirect to
   **`redirect_uri#error=<code>&error_description=<text>`**. A callback whose state is
   unknown, already used or malformed has nowhere to go back to and gets a small `400` HTML
   page instead.

The result is in the fragment so it never reaches server logs or `Referer` headers; read it
with `location.hash` and clear it (`history.replaceState`).

| `error` | Meaning |
|---|---|
| `access_denied` | the provider did not sign the person in (they cancelled, or it refused) |
| `state_mismatch` | the callback arrived in a different browser than the one that started |
| `state_expired` | more than 10 minutes passed; start again |
| `invalid_request` | the provider sent no code |
| `token_exchange_failed` | the provider refused the code |
| `invalid_id_token` | signature, issuer, audience, `azp` or time checks failed |
| `nonce_mismatch` | the `id_token` was not issued for this sign-in |
| `email_missing` | the provider did not share an email |
| `email_not_verified` | the provider has not verified the email |
| `domain_not_allowed` | the email's domain is not in the provider's `allowed_domains` |
| `signup_closed` | no account has this email and the rules below do not allow creating one |
| `account_disabled` | the account is disabled |
| `provider_unavailable` | discovery, keys or the secret file could not be read |
| `server_error` | a database error |

Discovery documents and key sets are cached for an hour. Provider responses are capped at
1 MiB, requests time out after 10 s, and redirects are not followed. The issuer and every
discovered endpoint must be `https` (`http` only for `localhost`/`127.0.0.1`).

### Who may sign in, and who gets an account

- `allowed_domains` (when non-empty) restricts **everyone** using that provider, including
  existing accounts.
- A verified email that **matches an existing user** signs in as that user — a password
  account, an app-provisioned user or an earlier OIDC user alike. Only configure providers
  you trust to verify email addresses.
- An email **without an account** gets one (no password; `display_name` from the `name` or
  `given_name` claim) if the provider's `allowed_domains` names its domain, or if
  `allow_signup` is true and `signup_domains` (when set) admits it. The "first password
  account" rule of password signup does not apply to OIDC.

## Delegated sign-in

Another site can be a sign-in provider: a portal that already knows who its people are
(behind its own single sign-on, say) signs them in to this server. Configure one
`[[delegated_sign_in]]` table per such site ([configuration.md](configuration.md#delegated_sign_in--sign-in-through-another-site)):

```toml
[[delegated_sign_in]]
id = "portal"                                   # [a-z0-9_-], ≤ 32, not also an [[oidc]] id
name = "Team portal"                            # button label (defaults to id)
start_url = "https://portal.example.com/canvas-signin"   # https (http only for loopback)
```

The site also holds a [linked-app](apps.md) key whose app has `trust_email` (the default):
it provisions the person (`PUT /v1/apps/self/users/{subject}`) and mints their session with
[`POST /v1/apps/self/users/{subject}/sessions`](apps.md#post-v1appsselfuserssubjectsessions--sign-a-user-in).
The server stores nothing about the provider and never calls it.

For the browser it is **the same protocol as OIDC**; a client needs no special case:

1. The client sends the browser to `GET /v1/auth/oidc/{id}/start?redirect_uri=<where>`
   (the `start` of the listed provider). `redirect_uri` must be an `http(s)` URL with the
   same origin as `public_url` and no fragment (`400 redirect_uri_not_allowed` otherwise).
2. The server redirects (`302`, `Cache-Control: no-store`, no cookie) to the provider's
   `start_url` with `redirect_uri=<where>` added to its query:
   `https://portal.example.com/canvas-signin?redirect_uri=https%3A%2F%2Fcanvas.example.com%2Faccount`.
3. The site signs the person in its own way, provisions them, mints the session and sends
   the browser to **`<where>#token=cvs_…`** — or, when it cannot, to
   **`<where>#error=<code>&error_description=<text>`**. The client reads the fragment exactly
   as after OIDC.

**The site must check `redirect_uri` itself** before it puts a token in it, because a
browser can be sent to `start_url` directly with any value: accept only an `http(s)` URL
whose origin (scheme, host and port) is this server's `public_url` origin — for example
with `new URL(redirect_uri).origin === "https://canvas.example.com"` — with no fragment,
no user name or password, and only then append `#token=…`. Anything else is an error page
on the site, never a redirect. The token must not be logged or stored by the site; mint one
per sign-in and keep the response (`Cache-Control: no-store`) out of caches.

Error codes are the site's to choose; use the ones in the OIDC table above where they fit
(`access_denied`, `account_disabled`, `server_error`, …) so clients can show them in words.

## The account page

`GET /account` (and `/account/`) serves one self-contained page for the people of a
standalone server: sign in or sign up (password, and a button per OIDC or delegated
provider whose redirect is `{public_url}/account`), their canvases, access tokens (create, copy once,
revoke), a ready-to-paste MCP client config for `{public_url}/mcp`, links to `/llms.txt`
and `/v1/openapi.json`, display name and password changes, and sign out. The session token
lives in `sessionStorage` (this tab only). Every action is a call to the API above.

The page is plain HTML, `/account/app.js` and `/account/app.css`, all embedded in the
binary, served with `Content-Security-Policy: default-src 'none'; script-src 'self';
style-src 'self'; img-src 'self' data:; connect-src 'self'; form-action 'self';
frame-ancestors 'none'; base-uri 'none'` and no external requests.

Canvases get an **Open** link only when `web_app_url` contains a `{canvas}` placeholder
(replaced by the canvas id; the `{token}` placeholder core requires for share links is
dropped there), e.g. `web_app_url = "https://canvas.example.com/c/{canvas}{token}"`.
Otherwise the page shows each canvas id with the API and MCP hints.

## Data

| Table | Owner | What |
|---|---|---|
| `users`, `sessions`, `access_tokens`, `server_settings` | core (`0001`–`0099`) | accounts, `cvs_` sessions, `cvt_` tokens (SHA-256 digests only), `allow_signup` / `signup_domains` |
| `oidc_states` | accounts (`0300_oidc_states.sql`) | sign-ins in flight: `state_sha256`, `provider`, `nonce`, `code_verifier`, `redirect_uri`, `expires_at`; swept on every start, a day after they expire |

## Metrics

- `auth_rate_limited_total` (core) and `auth_email_rate_limited_total` — refused auth calls
  per IP / per email.
- `app_sessions_created_total` and `app_sessions_rate_limited_total` — sessions linked apps
  minted (delegated sign-in), and mints refused by the per-app limit.
- `sse_subscribers{stream="user"}` — open `/v1/events` streams.
