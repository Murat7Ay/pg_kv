-- Concurrent sorted-set workload over a few shared sets, including whole-set
-- deletes and expiry so node freeing runs under load. Members per set come
-- from -D members=N (keep 4 * N below pg_kv.max_zset_members).
--   pgbench -n -c 32 -j 8 -T 15 -D members=2000 -f /test/zset_stress.sql postgres
\set z random(1, 4)
\set m random(1, :members)
\set s random(1, 1000)
\set op random(1, 40)
\if :op <= 16
SELECT kv.zadd('zs:' || :z, 'm' || :m, :s);
\elif :op <= 22
SELECT kv.zincrby('zs:' || :z, 'm' || :m, :s - 500);
\elif :op <= 26
SELECT kv.zrem('zs:' || :z, 'm' || :m);
\elif :op <= 32
SELECT kv.zrank('zs:' || :z, 'm' || :m);
\elif :op <= 36
SELECT count(*) FROM kv.zrange('zs:' || :z, 0, 9, true);
\elif :op <= 38
SELECT kv.zcard('zs:' || :z);
\elif :op = 39
SELECT kv.expire('zs:' || :z, interval '30 milliseconds');
\else
SELECT kv.del('zs:' || :z);
\endif
