-- OIDC sign-ins in flight (GET /v1/auth/oidc/{id}/start → …/callback). One row per started
-- sign-in, looked up by SHA-256 of its `state` (the plaintext state lives only in the
-- browser: the IdP round trip and an HttpOnly cookie), deleted on first use and swept when
-- expired (10 minutes). `code_verifier` is the PKCE secret sent to the token endpoint;
-- `redirect_uri` is where the client wants the result (`#token=…` or `#error=…`).
CREATE TABLE oidc_states (
    state_sha256  bytea       PRIMARY KEY CHECK (length(state_sha256) = 32),
    provider      text        NOT NULL CHECK (length(provider) BETWEEN 1 AND 32),
    nonce         text        NOT NULL,
    code_verifier text        NOT NULL,
    redirect_uri  text        NOT NULL CHECK (length(redirect_uri) <= 2048),
    created_at    timestamptz NOT NULL DEFAULT now(),
    expires_at    timestamptz NOT NULL
);

CREATE INDEX oidc_states_expires_idx ON oidc_states (expires_at);
