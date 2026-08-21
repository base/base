CREATE TABLE IF NOT EXISTS shadow_metrics_cursor(
  id              SMALLINT    PRIMARY KEY DEFAULT 1 CHECK (id = 1),
  last_updated_at TIMESTAMPTZ NOT NULL,
  last_number     BIGINT      NOT NULL,
  last_hash       BYTEA       NOT NULL,
  updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
