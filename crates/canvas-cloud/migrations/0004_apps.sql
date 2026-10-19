-- Linked apps: another service that owns its users and acts for them with its key
-- (`cva_…`, only SHA-256 stored) plus `X-Canvas-User: <subject>`.
--
-- `allow_import`: may POST canvases in the import format. `trust_email`: provisioning a
-- subject whose email belongs to an existing user not linked to this app links that user
-- (otherwise 409 email_taken).
CREATE TABLE apps (
    id           uuid        PRIMARY KEY,
    name         text        NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    key_sha256   bytea       NOT NULL UNIQUE CHECK (length(key_sha256) = 32),
    allow_import boolean     NOT NULL DEFAULT false,
    trust_email  boolean     NOT NULL DEFAULT true,
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz,
    revoked_at   timestamptz
);

-- Which canvas-cloud user each app subject (the app's own user id) is. A user is linked to
-- an app at most once.
CREATE TABLE app_users (
    app_id     uuid        NOT NULL REFERENCES apps (id) ON DELETE CASCADE,
    subject    text        NOT NULL CHECK (length(subject) BETWEEN 1 AND 200),
    user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (app_id, subject),
    UNIQUE (app_id, user_id)
);

-- ON DELETE CASCADE from users.
CREATE INDEX app_users_user_idx ON app_users (user_id);
