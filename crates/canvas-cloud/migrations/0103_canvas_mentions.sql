-- Chat @mentions on shared canvases (see docs/canvas.md).
--
-- The chat itself lives in the canvas document (a top-level Y.Array `chat`, persisted and
-- sealed like every other update). This table only records who was mentioned in which chat
-- message, so the server can notify them (a `canvas` event with kind `mention`) and clients can
-- list unread mentions across canvases.
--
-- `message_id` is the client's chat message id (opaque). `excerpt_sealed` is the sender's
-- ≤ 200-character preview, AES-256-GCM sealed under the canvas doc key (AAD
-- "canvas-cloud/v1/mention:" ‖ the row id's 16 raw bytes), so chat text never sits in the
-- database in plaintext. One row per (canvas, message, recipient): posting the same mention
-- again changes nothing. Rows go with the canvas or either user (cascade).
-- Additive only: existing rows and older clients are unaffected.

CREATE TABLE canvas_mentions (
    id             uuid        PRIMARY KEY,
    canvas_id      uuid        NOT NULL REFERENCES canvases (id) ON DELETE CASCADE,
    message_id     text        NOT NULL CHECK (length(message_id) BETWEEN 1 AND 128),
    from_user      uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    to_user        uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    excerpt_sealed bytea       NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    read_at        timestamptz
);

CREATE UNIQUE INDEX canvas_mentions_once ON canvas_mentions (canvas_id, message_id, to_user);
-- The recipient's inbox, newest first, and their unread badge.
CREATE INDEX canvas_mentions_inbox_idx ON canvas_mentions (to_user, created_at DESC, id DESC);
CREATE INDEX canvas_mentions_unread_idx ON canvas_mentions (to_user, canvas_id)
    WHERE read_at IS NULL;
-- ON DELETE CASCADE from users (sender side).
CREATE INDEX canvas_mentions_from_idx ON canvas_mentions (from_user);
