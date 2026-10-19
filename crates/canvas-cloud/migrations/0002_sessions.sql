-- Opaque bearer sessions (`cvs_…`). Only SHA-256 of the full token string is stored.
-- Sliding expiry: a use (touch throttled to once a minute) pushes expires_at to
-- last_used_at + 30 days.
CREATE TABLE sessions (
    id           uuid        PRIMARY KEY,
    user_id      uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    token_sha256 bytea       NOT NULL UNIQUE CHECK (length(token_sha256) = 32),
    user_agent   text        CHECK (length(user_agent) <= 512),
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL
);

CREATE INDEX sessions_user_idx ON sessions (user_id);
CREATE INDEX sessions_expires_idx ON sessions (expires_at);
