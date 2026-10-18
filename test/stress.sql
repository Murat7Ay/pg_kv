-- Mixed concurrent workload over a small keyspace so sessions collide on the
-- same shards and keys. Run with pgbench, then call kv.verify():
--   pgbench -n -c 32 -j 8 -T 20 -f /test/stress.sql postgres
\set k random(1, 5000)
\set op random(1, 10)
\if :op <= 3
SELECT kv.set('s:' || :k, repeat('x', :k % 200));
\elif :op <= 5
SELECT kv.set('s:' || :k, 'ttl', interval '50 milliseconds');
\elif :op <= 7
SELECT kv.get('s:' || :k);
\elif :op = 8
SELECT kv.del('s:' || :k);
\elif :op = 9
SELECT kv.incr('c:' || :k);
\else
SELECT kv.expire('s:' || :k, interval '20 milliseconds');
\endif
