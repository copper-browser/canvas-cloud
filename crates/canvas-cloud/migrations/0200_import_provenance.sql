-- Linked apps (crates/apps): which app imported a canvas (POST /v1/apps/self/import) and
-- when. `mode=replace` may only replace a canvas imported by the same app. An app that is
-- deleted from the database leaves its imported canvases alone (the provenance is cleared).

ALTER TABLE canvases
    ADD COLUMN imported_by_app uuid REFERENCES apps (id) ON DELETE SET NULL,
    ADD COLUMN imported_at     timestamptz;

CREATE INDEX canvases_imported_by_app_idx ON canvases (imported_by_app)
    WHERE imported_by_app IS NOT NULL;
