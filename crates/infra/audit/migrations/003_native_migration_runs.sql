-- Purpose-specific durable native migration attempt records. No runtime grants.
-- Managed bootstrap executes this same idempotent SQL before schema migrations
-- so a schema failure can be recorded without advancing migration history.
CREATE TABLE IF NOT EXISTS public.audit_migration_identity (
    singleton BOOLEAN PRIMARY KEY CHECK (singleton),
    target_token TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS public.audit_migration_runs (
    generation VARCHAR(128) PRIMARY KEY,
    target_id TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    record JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
