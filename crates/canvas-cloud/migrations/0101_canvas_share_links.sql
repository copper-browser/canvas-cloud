-- Shareable canvas links. The plaintext token is returned only when a link is created;
-- this table stores its SHA-256 digest, never the token itself.
CREATE TABLE canvas_share_links (
    id            uuid        PRIMARY KEY,
    canvas_id     uuid        NOT NULL REFERENCES canvases (id) ON DELETE CASCADE,
    token_sha256  bytea       NOT NULL UNIQUE CHECK (length(token_sha256) = 32),
    role          text        NOT NULL DEFAULT 'editor' CHECK (role = 'editor'),
    created_by    uuid        REFERENCES users (id) ON DELETE SET NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    uses          bigint      NOT NULL DEFAULT 0 CHECK (uses >= 0)
);

CREATE INDEX canvas_share_links_canvas_idx ON canvas_share_links (canvas_id, created_at DESC);
