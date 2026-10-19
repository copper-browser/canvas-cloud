-- Personal access tokens (`cvt_…`) for agents, scripts and MCP clients. Only SHA-256 of
-- the full token string is stored; the plaintext is shown once when the token is made.
-- `read` tokens may only GET/HEAD and cannot join canvas rooms.
CREATE TABLE access_tokens (
    id           uuid        PRIMARY KEY,
    user_id      uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name         text        NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    token_sha256 bytea       NOT NULL UNIQUE CHECK (length(token_sha256) = 32),
    scope        text        NOT NULL CHECK (scope IN ('read', 'write')),
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz,
    expires_at   timestamptz,
    revoked_at   timestamptz
);

CREATE INDEX access_tokens_user_idx ON access_tokens (user_id, created_at DESC);
