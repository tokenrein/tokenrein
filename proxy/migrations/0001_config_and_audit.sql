-- Managed config history: one row per saved version; the highest version is current.
CREATE TABLE config_versions (
    version    BIGINT PRIMARY KEY CHECK (version > 0),
    doc        JSONB NOT NULL,
    actor      TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Who changed what, and when. Append-only: TokenLens never updates or deletes rows.
CREATE TABLE audit_log (
    id             BIGSERIAL PRIMARY KEY,
    at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    org_id         TEXT,
    actor          TEXT NOT NULL,
    action         TEXT NOT NULL,
    target         TEXT NOT NULL DEFAULT '',
    detail         JSONB NOT NULL DEFAULT '{}',
    config_version BIGINT,
    ip             TEXT
);
CREATE INDEX audit_log_org ON audit_log (org_id, id DESC);
