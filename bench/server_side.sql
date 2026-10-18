-- Server-side cost per operation without any client round trip.
-- Every operation runs as its own SQL statement (SPI) and commits, the same as a
-- client issuing one statement per transaction, so neither side is penalised by
-- running many updates inside a single transaction.
-- Requires bench/setup.sql to have run (100k preloaded keys in both stores).
\set ON_ERROR_STOP on
CREATE OR REPLACE PROCEDURE pg_kv_server_side_bench(n int DEFAULT 100000)
LANGUAGE plpgsql AS $$
DECLARE
  t0 timestamptz;
  v text;
  c bigint;
  val constant text := repeat('v', 64);
  report text := '';
BEGIN
  t0 := clock_timestamp();
  FOR i IN 1..n LOOP
    PERFORM kv.set('key:' || (i % 100000 + 1), val);
    COMMIT;
  END LOOP;
  report := report || format(E'\nSET   pg_kv     %7s ns/op', round(extract(epoch FROM clock_timestamp() - t0) * 1e9 / n));

  t0 := clock_timestamp();
  FOR i IN 1..n LOOP
    INSERT INTO cache_unlogged (k, v) VALUES ('key:' || (i % 100000 + 1), val)
      ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v, expires_at = NULL;
    COMMIT;
  END LOOP;
  report := report || format(E'\nSET   unlogged  %7s ns/op', round(extract(epoch FROM clock_timestamp() - t0) * 1e9 / n));

  t0 := clock_timestamp();
  FOR i IN 1..n LOOP
    SELECT kv.get('key:' || (i % 100000 + 1)) INTO v;
    COMMIT;
  END LOOP;
  report := report || format(E'\nGET   pg_kv     %7s ns/op', round(extract(epoch FROM clock_timestamp() - t0) * 1e9 / n));

  t0 := clock_timestamp();
  FOR i IN 1..n LOOP
    SELECT x.v INTO v FROM cache_unlogged x
     WHERE x.k = 'key:' || (i % 100000 + 1) AND (x.expires_at IS NULL OR x.expires_at > now());
    COMMIT;
  END LOOP;
  report := report || format(E'\nGET   unlogged  %7s ns/op', round(extract(epoch FROM clock_timestamp() - t0) * 1e9 / n));

  t0 := clock_timestamp();
  FOR i IN 1..n LOOP
    SELECT kv.incr('ctr:' || (i % 100000 + 1)) INTO c;
    COMMIT;
  END LOOP;
  report := report || format(E'\nINCR  pg_kv     %7s ns/op', round(extract(epoch FROM clock_timestamp() - t0) * 1e9 / n));

  t0 := clock_timestamp();
  FOR i IN 1..n LOOP
    INSERT INTO counters_unlogged (k, n) VALUES ('ctr:' || (i % 100000 + 1), 1)
      ON CONFLICT (k) DO UPDATE SET n = counters_unlogged.n + 1 RETURNING counters_unlogged.n INTO c;
    COMMIT;
  END LOOP;
  report := report || format(E'\nINCR  unlogged  %7s ns/op', round(extract(epoch FROM clock_timestamp() - t0) * 1e9 / n));

  RAISE NOTICE '%', report;
END $$;
CALL pg_kv_server_side_bench();
