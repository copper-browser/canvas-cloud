-- Accounts. Emails are unique per instance, case-insensitively (lower(email) index; no
-- citext extension so managed Postgres without superuser works).
--
-- `password_hash` is an Argon2id PHC string for users who sign in with a password; NULL for
-- federated users (provisioned by a linked app or created through OIDC). `display_name` is
-- '' when the user has none.
CREATE TABLE users (
    id            uuid        PRIMARY KEY,
    email         text        NOT NULL CHECK (length(email) BETWEEN 3 AND 254),
    display_name  text        NOT NULL DEFAULT '' CHECK (length(display_name) <= 200),
    password_hash text,
    disabled      boolean     NOT NULL DEFAULT false,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX users_email_lower_key ON users (lower(email));
