-- Edge cases that need tiny limits. Run against a server started with
--   pg_kv.max_entries=1024 pg_kv.shards=4 pg_kv.max_zset_members=200 pg_kv.max_member_bytes=16
\set ON_ERROR_STOP on
\pset footer off
CREATE EXTENSION IF NOT EXISTS pg_kv;

-- node pool exhaustion
DO $$
BEGIN
  PERFORM kv.flush();
  FOR i IN 1..200 LOOP PERFORM kv.zadd('full', 'm' || i, i); END LOOP;
  BEGIN
    PERFORM kv.zadd('full', 'one-more', 1);
    RAISE EXCEPTION 'pool overflow accepted';
  EXCEPTION WHEN program_limit_exceeded THEN
    IF SQLERRM NOT LIKE '%member pool is full%' THEN RAISE; END IF;
  END;
  ASSERT kv.zcard('full') = 200, 'a failed zadd leaves the set intact';
  BEGIN
    PERFORM kv.zadd('other', 'x', 1);
    RAISE EXCEPTION 'pool overflow accepted for a new set';
  EXCEPTION WHEN program_limit_exceeded THEN
    IF SQLERRM NOT LIKE '%member pool is full%' THEN RAISE; END IF;
  END;
  ASSERT kv.ttl('other') = -2, 'a failed zadd does not leave an empty set behind';
  ASSERT kv.zrem('full', 'm1');
  ASSERT kv.zadd('other', 'x', 1), 'a freed node is reused';
  ASSERT kv.verify() = 2, 'invariants after pool exhaustion';
END $$;

-- flush and eviction free sorted-set members
DO $$
DECLARE
  live_sets int;
BEGIN
  PERFORM kv.flush();
  ASSERT (SELECT zset_members FROM kv.stats()) = 0, 'flush frees all members';
  FOR i IN 1..50 LOOP
    PERFORM kv.zadd('ev' || i, 'a', 1);
    PERFORM kv.zadd('ev' || i, 'b', 2);
  END LOOP;
  -- capacity is 1024 keys, so these evict (least recently used first)
  FOR i IN 1..5000 LOOP PERFORM kv.set('filler:' || i, 'v'); END LOOP;
  SELECT count(*) INTO live_sets FROM generate_series(1, 50) i WHERE kv.zcard('ev' || i) > 0;
  ASSERT (SELECT zset_members FROM kv.stats()) = 2 * live_sets,
         format('evicted sets must free their members (%s live sets, %s members)',
                live_sets, (SELECT zset_members FROM kv.stats()));
  ASSERT live_sets < 50, 'some sorted sets were evicted';
  PERFORM kv.verify();
END $$;

SELECT 'limits OK' AS result;
