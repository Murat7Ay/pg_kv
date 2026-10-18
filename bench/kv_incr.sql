\set k random(1, 100000)
SELECT kv.incr('ctr:' || :k);
