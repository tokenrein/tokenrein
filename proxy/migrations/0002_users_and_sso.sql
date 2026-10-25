-- People who sign in to the dashboard / admin API. Emails are stored lower-case.
CREATE TABLE users (
    id             BIGSERIAL PRIMARY KEY,
    email          TEXT NOT NULL UNIQUE,
    name           TEXT NOT NULL DEFAULT '',
    -- argon2id; NULL until a password is set (SSO-only users never have one).
    password_hash  TEXT,
    -- Manages the whole install: every org, model prices, users and single sign-on.
    instance_admin BOOLEAN NOT NULL DEFAULT false,
    disabled       BOOLEAN NOT NULL DEFAULT false,
    failed_logins  INT NOT NULL DEFAULT 0,
    locked_until   TIMESTAMPTZ,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_login_at  TIMESTAMPTZ
);

-- A person's role in an org: owner > admin > approver > viewer.
CREATE TABLE memberships (
    user_id    BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    org_id     TEXT NOT NULL,
    role       TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'approver', 'viewer')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, org_id)
);
CREATE INDEX memberships_org ON memberships (org_id);

-- Browser sessions. Only a hash of the cookie value is stored.
CREATE TABLE sessions (
    id_hash      TEXT PRIMARY KEY,
    user_id      BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    csrf         TEXT NOT NULL,
    via          TEXT NOT NULL,
    ip           TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at   TIMESTAMPTZ NOT NULL
);
CREATE INDEX sessions_user ON sessions (user_id);

-- Personal API tokens for scripts and automation; they act as their user.
CREATE TABLE api_tokens (
    id           BIGSERIAL PRIMARY KEY,
    user_id      BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    token_hash   TEXT NOT NULL UNIQUE,
    prefix       TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ,
    expires_at   TIMESTAMPTZ,
    revoked_at   TIMESTAMPTZ
);
CREATE INDEX api_tokens_user ON api_tokens (user_id);

-- One-time links to set (or reset) a password.
CREATE TABLE setup_tokens (
    token_hash TEXT PRIMARY KEY,
    user_id    BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    used_at    TIMESTAMPTZ
);

-- Single sign-on (OpenID Connect) for the install; at most one row.
CREATE TABLE sso_config (
    id         INT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    config     JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Sign-ins in progress (state, nonce, PKCE verifier); deleted when used or after 10 minutes.
CREATE TABLE sso_logins (
    state_hash TEXT PRIMARY KEY,
    nonce      TEXT NOT NULL,
    verifier   TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
