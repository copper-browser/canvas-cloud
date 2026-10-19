-- Append-only record of administrative actions (CLI and app-admin APIs): who (`actor`, e.g.
-- `cli` or `app:<id>`), what (`action`, e.g. `user.disable`), on what (`target`), and
-- action-specific `detail`. Never holds secrets.
CREATE TABLE admin_audit (
    id     bigserial   PRIMARY KEY,
    actor  text        NOT NULL,
    action text        NOT NULL,
    target text        NOT NULL DEFAULT '',
    detail jsonb       NOT NULL DEFAULT '{}'::jsonb,
    at     timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX admin_audit_at_idx ON admin_audit (at DESC);
