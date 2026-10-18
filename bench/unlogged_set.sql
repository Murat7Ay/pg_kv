\set k random(1, 100000)
INSERT INTO cache_unlogged (k, v) VALUES ('key:' || :k, 'vvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvv')
  ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v, expires_at = NULL;
