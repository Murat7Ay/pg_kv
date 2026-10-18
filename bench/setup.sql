-- Fresh state for each benchmark run.
CREATE EXTENSION IF NOT EXISTS pg_kv;
SELECT kv.flush();
DROP TABLE IF EXISTS cache_unlogged, counters_unlogged;
CREATE UNLOGGED TABLE cache_unlogged (k text PRIMARY KEY, v text NOT NULL, expires_at timestamptz);
CREATE UNLOGGED TABLE counters_unlogged (k text PRIMARY KEY, n bigint NOT NULL);
-- Preload the same 100k keys into both stores so GETs always hit.
INSERT INTO cache_unlogged (k, v) SELECT 'key:' || i, 'vvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvv' FROM generate_series(1, 100000) i;
SELECT count(kv.set('key:' || i, 'vvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvv')) FROM generate_series(1, 100000) i;
VACUUM ANALYZE cache_unlogged;
