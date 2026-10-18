\set k random(1, 100000)
SELECT v FROM cache_unlogged WHERE k = 'key:' || :k AND (expires_at IS NULL OR expires_at > now());
