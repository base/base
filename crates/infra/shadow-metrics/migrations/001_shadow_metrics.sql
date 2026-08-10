-- Placeholder schema for the shadow-metrics noop service. The service performs
-- no real work; this table exists so schema-readiness probes have a relation to
-- verify and so future metrics storage has a migration to build on.
CREATE TABLE IF NOT EXISTS shadow_metrics (
    id BIGSERIAL PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'shadow_metrics') THEN
        GRANT SELECT ON _sqlx_migrations TO shadow_metrics;
        GRANT SELECT, INSERT, UPDATE, DELETE ON shadow_metrics TO shadow_metrics;
    END IF;
END $$;
