\set k random(1, 100000)
INSERT INTO counters_unlogged (k, n) VALUES ('ctr:' || :k, 1)
  ON CONFLICT (k) DO UPDATE SET n = counters_unlogged.n + 1 RETURNING n;
