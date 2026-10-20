-- 0.2.1 (security): minting user sessions is its own opt-in, and what an app mints is tagged so
-- revoking or rotating the app ends it.
--
-- `apps.allow_sessions`: may sign its users in with POST /v1/apps/self/users/{subject}/sessions
-- (it also needs `trust_email`). Off for every existing app: an operator opts an app in with
-- `canvas-cloud apps set <app> --allow-sessions true`.
ALTER TABLE apps ADD COLUMN allow_sessions boolean NOT NULL DEFAULT false;

-- `created_by_app`: the app that minted the session, or whose minted session created the access
-- token. `apps revoke` / `apps rotate` delete those sessions and revoke those tokens.
-- `max_expires_at`: a session never slides past this (app-minted sessions under
-- `limits.app_session_ttl_hours`); NULL = slides forever (30 days after its last use).
ALTER TABLE sessions
    ADD COLUMN created_by_app uuid REFERENCES apps (id) ON DELETE CASCADE,
    ADD COLUMN max_expires_at timestamptz;
ALTER TABLE access_tokens
    ADD COLUMN created_by_app uuid REFERENCES apps (id) ON DELETE CASCADE;

CREATE INDEX sessions_created_by_app_idx ON sessions (created_by_app)
    WHERE created_by_app IS NOT NULL;
CREATE INDEX access_tokens_created_by_app_idx ON access_tokens (created_by_app)
    WHERE created_by_app IS NOT NULL;

-- Best effort for sessions minted before this migration (0.2.0 did not tag them): the mint
-- wrote an `app.user.session` audit row (actor `app:<id>`, target the user id) right after
-- inserting the session, so tag the user's sessions created in the seconds before it. A
-- sign-in by other means in that window is tagged too, which at worst signs it out with the
-- app; a minted session left untagged would outlive the app's revocation.
UPDATE sessions s
SET created_by_app = a.id
FROM admin_audit au
JOIN apps a ON au.actor = 'app:' || a.id::text
WHERE au.action = 'app.user.session'
  AND au.target = s.user_id::text
  AND s.created_at BETWEEN au.at - interval '5 seconds' AND au.at
  AND s.created_by_app IS NULL;
