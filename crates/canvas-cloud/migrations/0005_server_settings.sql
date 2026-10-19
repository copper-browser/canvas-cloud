-- Runtime server settings changed by the admin CLI (`allow_signup`, `signup_domains`),
-- without editing the config file.
CREATE TABLE server_settings (
    key        text        PRIMARY KEY,
    value      jsonb       NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
