-- Canvases (owned by the canvas-cloud-engine crate; see docs/canvas.md).
--
-- Every canvas has a random 32-byte doc_key wrapped by the KEK (derived from master_key).
-- Yjs updates and snapshots are AES-256-GCM sealed with that key, AAD = the canvas id's 16 raw
-- bytes, so a row copied to another canvas fails to decrypt.

CREATE TABLE canvases (
    id              uuid        PRIMARY KEY,
    owner_id        uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name            text        NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    kind            text        NOT NULL CHECK (kind IN ('personal', 'shared')),
    doc_key_wrapped bytea       NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

-- Exactly one Personal canvas per user.
CREATE UNIQUE INDEX canvases_one_personal_per_owner ON canvases (owner_id) WHERE kind = 'personal';
CREATE INDEX canvases_owner_idx ON canvases (owner_id);

-- Membership is the only access path: every query is scoped through this table. The owner
-- has a row too (role 'owner').
CREATE TABLE canvas_members (
    canvas_id uuid        NOT NULL REFERENCES canvases (id) ON DELETE CASCADE,
    user_id   uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role      text        NOT NULL CHECK (role IN ('owner', 'editor')),
    added_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (canvas_id, user_id)
);

CREATE INDEX canvas_members_user_idx ON canvas_members (user_id);

-- Email invites. `email` is stored lower-cased; the invitee must sign in with that email and
-- accept. `token` is SHA-256 (hex) of a random secret reserved for future shareable links; it
-- is never returned by the API.
CREATE TABLE canvas_invites (
    id          uuid        PRIMARY KEY,
    canvas_id   uuid        NOT NULL REFERENCES canvases (id) ON DELETE CASCADE,
    email       text        NOT NULL CHECK (length(email) BETWEEN 3 AND 254),
    invited_by  uuid        REFERENCES users (id) ON DELETE SET NULL,
    token       text        NOT NULL UNIQUE,
    status      text        NOT NULL DEFAULT 'pending'
                            CHECK (status IN ('pending', 'accepted', 'declined')),
    created_at  timestamptz NOT NULL DEFAULT now(),
    accepted_at timestamptz
);

CREATE UNIQUE INDEX canvas_invites_one_pending ON canvas_invites (canvas_id, email)
    WHERE status = 'pending';
CREATE INDEX canvas_invites_pending_email_idx ON canvas_invites (email) WHERE status = 'pending';

-- Append-only Yjs update log (lib0 v1 updates, sealed). Compacted into canvas_snapshots.
CREATE TABLE canvas_updates (
    seq        bigserial   PRIMARY KEY,
    canvas_id  uuid        NOT NULL REFERENCES canvases (id) ON DELETE CASCADE,
    "update"   bytea       NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX canvas_updates_canvas_seq_idx ON canvas_updates (canvas_id, seq);

-- One compacted state per canvas: encode_state_as_update_v1 of the doc after applying every
-- update with seq <= snapshot.seq (sealed).
CREATE TABLE canvas_snapshots (
    canvas_id  uuid        PRIMARY KEY REFERENCES canvases (id) ON DELETE CASCADE,
    seq        bigint      NOT NULL,
    state      bytea       NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
