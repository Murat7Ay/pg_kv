-- Functional smoke test. Run against a server started with
-- shared_preload_libraries=pg_kv, e.g.:
--   docker compose exec -T pg psql -U postgres -v ON_ERROR_STOP=1 -f /test/smoke.sql
\set ON_ERROR_STOP on
\pset footer off
-- Recreate so the SQL definitions always match the installed library.
DROP EXTENSION IF EXISTS pg_kv;
CREATE EXTENSION pg_kv;
SELECT kv.flush() AS flushed;

-- basic set / get / del / overwrite
DO $$
BEGIN
  PERFORM kv.set('a', 'hello');
  ASSERT kv.get('a') = 'hello', 'get after set';
  ASSERT kv.get('missing') IS NULL, 'missing key is NULL';
  PERFORM kv.set('a', 'world');
  ASSERT kv.get('a') = 'world', 'overwrite';
  ASSERT kv.del('a'), 'del returns true for live key';
  ASSERT NOT kv.del('a'), 'del returns false for missing key';
  ASSERT kv.get('a') IS NULL, 'deleted key is gone';
  PERFORM kv.set('empty', '');
  ASSERT kv.get('empty') = '', 'empty string is not NULL';
END $$;

-- ttl / expire (PTTL semantics: -1 = no ttl, -2 = missing)
DO $$
BEGIN
  PERFORM kv.set('p', 'x');
  ASSERT kv.ttl('p') = -1, 'no ttl';
  ASSERT kv.ttl('missing') = -2, 'missing ttl';
  PERFORM kv.set('t', 'x', interval '300 milliseconds');
  ASSERT kv.ttl('t') BETWEEN 1 AND 300, 'ttl in range';
  ASSERT kv.expire('p', interval '1 hour'), 'expire existing';
  ASSERT kv.ttl('p') > 3500000, 'expire sets ttl';
  ASSERT kv.expire('p', NULL) AND kv.ttl('p') = -1, 'expire NULL persists';
  ASSERT NOT kv.expire('missing', interval '1 s'), 'expire missing';
  PERFORM kv.set('p', 'y', interval '1 hour');
  PERFORM kv.set('p', 'z');
  ASSERT kv.ttl('p') = -1, 'set without ttl clears ttl';
END $$;
SELECT pg_sleep(0.4);
DO $$
BEGIN
  ASSERT kv.get('t') IS NULL, 'expired key is not returned';
  ASSERT kv.ttl('t') = -2, 'expired key reports missing';
END $$;

-- incr
DO $$
BEGIN
  ASSERT kv.incr('n') = 1, 'incr from missing';
  ASSERT kv.incr('n', 41) = 42, 'incr by delta';
  ASSERT kv.incr('n', -50) = -8, 'negative delta';
  ASSERT kv.get('n') = '-8', 'incr value readable as text';
  PERFORM kv.set('ttl_ctr', '10', interval '1 hour');
  PERFORM kv.incr('ttl_ctr');
  ASSERT kv.ttl('ttl_ctr') > 0, 'incr keeps ttl';
  PERFORM kv.set('s', 'abc');
  BEGIN
    PERFORM kv.incr('s');
    RAISE EXCEPTION 'incr on text should fail';
  EXCEPTION WHEN invalid_text_representation THEN
    IF SQLERRM NOT LIKE '%not an integer%' THEN RAISE; END IF;
  END;
  PERFORM kv.set('big', '9223372036854775807');
  BEGIN
    PERFORM kv.incr('big');
    RAISE EXCEPTION 'overflow should fail';
  EXCEPTION WHEN numeric_value_out_of_range THEN
    IF SQLERRM NOT LIKE '%overflow%' THEN RAISE; END IF;
  END;
END $$;

-- size limits
DO $$
BEGIN
  BEGIN
    PERFORM kv.set(repeat('k', 129), 'v');
    RAISE EXCEPTION 'oversized key should fail';
  EXCEPTION WHEN program_limit_exceeded THEN
    IF SQLERRM NOT LIKE '%max_key_bytes%' THEN RAISE; END IF;
  END;
  BEGIN
    PERFORM kv.set('k', repeat('v', current_setting('pg_kv.max_value_bytes')::int + 1));
    RAISE EXCEPTION 'oversized value should fail';
  EXCEPTION WHEN program_limit_exceeded THEN
    IF SQLERRM NOT LIKE '%max_value_bytes%' THEN RAISE; END IF;
  END;
  PERFORM kv.set('maxv', repeat('v', current_setting('pg_kv.max_value_bytes')::int));
  ASSERT length(kv.get('maxv')) = current_setting('pg_kv.max_value_bytes')::int, 'value at limit';
END $$;

-- writes are deliberately not transactional (cache semantics)
BEGIN;
SELECT kv.set('tx', '1');
ROLLBACK;
DO $$ BEGIN ASSERT kv.get('tx') = '1', 'rolled-back set is still visible'; END $$;

-- backward-shift deletion keeps every remaining key reachable
DO $$
BEGIN
  PERFORM kv.flush();
  FOR i IN 1..50000 LOOP PERFORM kv.set('d:' || i, i::text); END LOOP;
  FOR i IN 1..50000 BY 2 LOOP PERFORM kv.del('d:' || i); END LOOP;
  FOR i IN 2..50000 BY 2 LOOP
    ASSERT kv.get('d:' || i) = i::text, 'lost key d:' || i;
  END LOOP;
  FOR i IN 1..50000 BY 2 LOOP
    ASSERT kv.get('d:' || i) IS NULL, 'deleted key came back d:' || i;
  END LOOP;
  ASSERT (SELECT entries FROM kv.stats()) = 25000, 'entry count after deletes';
  ASSERT kv.verify() = 25000, 'invariants after deletes';
END $$;

-- eviction: writing past capacity never exceeds it
DO $$
DECLARE
  cap bigint;
  ev0 bigint;
  n bigint;
BEGIN
  PERFORM kv.flush();
  SELECT capacity, evictions INTO cap, ev0 FROM kv.stats();
  FOR i IN 1..cap + 5000 LOOP PERFORM kv.set('e:' || i, 'v'); END LOOP;
  SELECT entries INTO n FROM kv.stats();
  ASSERT n <= cap, format('entries %s exceed capacity %s', n, cap);
  ASSERT (SELECT evictions FROM kv.stats()) - ev0 = cap + 5000 - n, 'eviction count matches';
  ASSERT kv.get('e:' || (cap + 5000)) = 'v', 'newest key survives eviction';
  ASSERT kv.verify() = n, 'invariants after eviction';
END $$;

-- the sweeper reclaims expired entries that are never read
DO $$
BEGIN
  PERFORM kv.flush();
  FOR i IN 1..2000 LOOP PERFORM kv.set('x:' || i, 'v', interval '100 milliseconds'); END LOOP;
END $$;
SELECT pg_sleep(1.6);
DO $$ BEGIN ASSERT (SELECT entries FROM kv.stats()) = 0, 'sweeper did not reclaim expired entries'; END $$;

-- access control: the cache is cluster-wide, so only roles granted USAGE on
-- schema kv can use it, and flush/verify stay with the extension owner
DO $$
BEGIN
  IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'kv_app') THEN
    DROP OWNED BY kv_app;
    DROP ROLE kv_app;
  END IF;
  IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'kv_nobody') THEN
    DROP ROLE kv_nobody;
  END IF;
END $$;
CREATE ROLE kv_app;
CREATE ROLE kv_nobody;
GRANT USAGE ON SCHEMA kv TO kv_app;
SET ROLE kv_app;
DO $$
BEGIN
  PERFORM kv.set('acl:x', 'y');
  ASSERT kv.get('acl:x') = 'y', 'a role with USAGE on schema kv can use the cache';
  BEGIN
    PERFORM kv.flush();
    RAISE EXCEPTION 'flush allowed for a non-owner';
  EXCEPTION WHEN insufficient_privilege THEN NULL;
  END;
  BEGIN
    PERFORM kv.verify();
    RAISE EXCEPTION 'verify allowed for a non-owner';
  EXCEPTION WHEN insufficient_privilege THEN NULL;
  END;
END $$;
SET ROLE kv_nobody;
DO $$
BEGIN
  PERFORM kv.get('acl:x');
  RAISE EXCEPTION 'a role without USAGE on schema kv could read the cache';
EXCEPTION WHEN insufficient_privilege THEN NULL;
END $$;
RESET ROLE;
DROP OWNED BY kv_app;
DROP ROLE kv_app;
DROP ROLE kv_nobody;
DO $$ BEGIN ASSERT kv.preloaded(), 'kv.preloaded() on a preloaded server'; END $$;

SELECT kv.flush() AS flushed;
SELECT * FROM kv.stats();
SELECT 'smoke OK' AS result;
