-- Sorted sets. Run after smoke.sql (which installs the extension).
\set ON_ERROR_STOP on
\pset footer off

-- basic semantics
DO $$
BEGIN
  PERFORM kv.del('z');
  PERFORM kv.del('t');
  ASSERT kv.zadd('z', 'alice', 10), 'new member returns true';
  ASSERT kv.zadd('z', 'bob', 20);
  ASSERT kv.zadd('z', 'carol', 15);
  ASSERT NOT kv.zadd('z', 'alice', 30), 'update returns false';
  ASSERT kv.zcard('z') = 3, 'zcard';
  ASSERT kv.zscore('z', 'alice') = 30, 'zscore after update';
  ASSERT kv.zscore('z', 'nobody') IS NULL, 'zscore of missing member';
  ASSERT kv.zrank('z', 'carol') = 0 AND kv.zrank('z', 'bob') = 1 AND kv.zrank('z', 'alice') = 2, 'ascending ranks';
  ASSERT kv.zrevrank('z', 'alice') = 0 AND kv.zrevrank('z', 'carol') = 2, 'descending ranks';
  ASSERT kv.zrank('z', 'nobody') IS NULL, 'zrank of missing member';
  ASSERT kv.zincrby('z', 'carol', 100) = 115, 'zincrby result';
  ASSERT kv.zrevrank('z', 'carol') = 0, 'zincrby moves the member';
  ASSERT kv.zincrby('z', 'dave', 5) = 5, 'zincrby creates a member';
  ASSERT (SELECT array_agg(member ORDER BY rank) FROM kv.zrange('z', 0, -1))
         = ARRAY['dave', 'bob', 'alice', 'carol'], 'full range';
  ASSERT (SELECT array_agg(member ORDER BY rank) FROM kv.zrange('z', 0, 1, true))
         = ARRAY['carol', 'alice'], 'top 2 descending';
  ASSERT (SELECT array_agg(member ORDER BY rank) FROM kv.zrange('z', -2, -1))
         = ARRAY['alice', 'carol'], 'negative indices';
  ASSERT (SELECT score FROM kv.zrange('z', 0, 0, true)) = 115, 'zrange returns scores';
  ASSERT (SELECT count(*) FROM kv.zrange('z', 3, 1)) = 0, 'start > stop is empty';
  ASSERT (SELECT count(*) FROM kv.zrange('z', 10, 20)) = 0, 'out of range is empty';
  ASSERT (SELECT count(*) FROM kv.zrange('missing', 0, -1)) = 0, 'missing key is empty';
  ASSERT kv.zcard('missing') = 0, 'zcard of missing key';
  PERFORM kv.zadd('t', 'b', 1);
  PERFORM kv.zadd('t', 'a', 1);
  PERFORM kv.zadd('t', 'c', 1);
  ASSERT (SELECT array_agg(member ORDER BY rank) FROM kv.zrange('t', 0, -1))
         = ARRAY['a', 'b', 'c'], 'equal scores are ordered by member';
  ASSERT kv.zrem('z', 'bob') AND NOT kv.zrem('z', 'bob'), 'zrem';
  ASSERT kv.zcard('z') = 3, 'zcard after zrem';
END $$;

-- members are freed by zrem, set, del and expiry; kv itself carries the baseline
DO $$
BEGIN
  PERFORM kv.del('one');
  PERFORM kv.zadd('one', 'x', 1);
  ASSERT kv.zrem('one', 'x');
  ASSERT kv.ttl('one') = -2, 'an emptied sorted set is deleted';

  PERFORM kv.set('test:used0', (SELECT zset_members FROM kv.stats())::text);
  FOR i IN 1..100 LOOP PERFORM kv.zadd('big', 'm' || i, i); END LOOP;
  ASSERT (SELECT zset_members FROM kv.stats()) = kv.get('test:used0')::bigint + 100, 'members counted';
  PERFORM kv.set('big', 'overwritten');
  ASSERT (SELECT zset_members FROM kv.stats()) = kv.get('test:used0')::bigint, 'set frees members';
  FOR i IN 1..100 LOOP PERFORM kv.zadd('big2', 'm' || i, i); END LOOP;
  ASSERT kv.del('big2');
  ASSERT (SELECT zset_members FROM kv.stats()) = kv.get('test:used0')::bigint, 'del frees members';
  FOR i IN 1..100 LOOP PERFORM kv.zadd('big3', 'm' || i, i); END LOOP;
  ASSERT kv.expire('big3', interval '100 milliseconds');
END $$;
SELECT pg_sleep(1.3);
DO $$
BEGIN
  ASSERT kv.zcard('big3') = 0, 'expired sorted set is gone';
  ASSERT (SELECT zset_members FROM kv.stats()) = kv.get('test:used0')::bigint, 'sweeper frees expired members';
END $$;

-- WRONGTYPE and argument validation
DO $$
BEGIN
  PERFORM kv.set('str', 'x');
  BEGIN
    PERFORM kv.zadd('str', 'm', 1);
    RAISE EXCEPTION 'zadd on a string accepted';
  EXCEPTION WHEN wrong_object_type THEN
    IF SQLERRM NOT LIKE '%WRONGTYPE%' THEN RAISE; END IF;
  END;
  BEGIN
    PERFORM kv.zrank('str', 'm');
    RAISE EXCEPTION 'zrank on a string accepted';
  EXCEPTION WHEN wrong_object_type THEN
    IF SQLERRM NOT LIKE '%WRONGTYPE%' THEN RAISE; END IF;
  END;
  BEGIN
    PERFORM kv.get('z');
    RAISE EXCEPTION 'get on a sorted set accepted';
  EXCEPTION WHEN wrong_object_type THEN
    IF SQLERRM NOT LIKE '%WRONGTYPE%' THEN RAISE; END IF;
  END;
  BEGIN
    PERFORM kv.incr('z');
    RAISE EXCEPTION 'incr on a sorted set accepted';
  EXCEPTION WHEN wrong_object_type THEN
    IF SQLERRM NOT LIKE '%WRONGTYPE%' THEN RAISE; END IF;
  END;
  BEGIN
    PERFORM kv.zadd('z', 'm', 'NaN'::float8);
    RAISE EXCEPTION 'NaN score accepted';
  EXCEPTION WHEN invalid_parameter_value THEN
    IF SQLERRM NOT LIKE '%NaN%' THEN RAISE; END IF;
  END;
  BEGIN
    PERFORM kv.zadd('z', repeat('m', current_setting('pg_kv.max_member_bytes')::int + 1), 1);
    RAISE EXCEPTION 'oversized member accepted';
  EXCEPTION WHEN program_limit_exceeded THEN
    IF SQLERRM NOT LIKE '%max_member_bytes%' THEN RAISE; END IF;
  END;
  PERFORM kv.zincrby('z', 'inf', 'Infinity');
  BEGIN
    PERFORM kv.zincrby('z', 'inf', '-Infinity');
    RAISE EXCEPTION 'Infinity + -Infinity accepted';
  EXCEPTION WHEN invalid_parameter_value THEN
    IF SQLERRM NOT LIKE '%not a number%' THEN RAISE; END IF;
  END;
  ASSERT kv.zscore('z', 'inf') = 'Infinity', 'failed zincrby leaves the score unchanged';
END $$;

-- randomized comparison against a SQL table as ground truth:
-- 30k random zadd / zincrby / zrem over 5k members with heavy score ties
CREATE TEMP TABLE truth (member text PRIMARY KEY, score float8 NOT NULL);
DO $$
DECLARE
  m text;
  s float8;
  r record;
  n int := 0;
BEGIN
  PERFORM kv.del('lb');
  FOR i IN 1..30000 LOOP
    m := 'p' || (random() * 5000)::int;
    IF random() < 0.1 THEN
      PERFORM kv.zrem('lb', m);
      DELETE FROM truth WHERE member = m;
    ELSIF random() < 0.3 THEN
      s := (random() * 20)::int - 10;
      PERFORM kv.zincrby('lb', m, s);
      INSERT INTO truth VALUES (m, s)
        ON CONFLICT (member) DO UPDATE SET score = truth.score + EXCLUDED.score;
    ELSE
      s := (random() * 100)::int;
      PERFORM kv.zadd('lb', m, s);
      INSERT INTO truth VALUES (m, s)
        ON CONFLICT (member) DO UPDATE SET score = EXCLUDED.score;
    END IF;
  END LOOP;
  ASSERT kv.zcard('lb') = (SELECT count(*) FROM truth), 'cardinality matches';
  FOR r IN
    SELECT member, score,
           row_number() OVER (ORDER BY score, member COLLATE "C") - 1 AS rk,
           count(*) OVER () AS cnt
      FROM truth
  LOOP
    ASSERT kv.zscore('lb', r.member) = r.score, 'score of ' || r.member;
    ASSERT kv.zrank('lb', r.member) = r.rk,
           format('rank of %s: %s, expected %s', r.member, kv.zrank('lb', r.member), r.rk);
    ASSERT kv.zrevrank('lb', r.member) = r.cnt - 1 - r.rk, 'revrank of ' || r.member;
    n := n + 1;
  END LOOP;
  ASSERT (SELECT array_agg(member ORDER BY rank) FROM kv.zrange('lb', 0, -1))
         = (SELECT array_agg(member ORDER BY score, member COLLATE "C") FROM truth), 'full order';
  ASSERT (SELECT array_agg(member ORDER BY rank) FROM kv.zrange('lb', 0, 9, true))
         = (SELECT array_agg(member) FROM (SELECT member FROM truth
                                            ORDER BY score DESC, member COLLATE "C" DESC LIMIT 10) t),
         'top 10';
  RAISE NOTICE 'ranks and scores of % members match the SQL ground truth', n;
END $$;

SELECT kv.verify() AS verified_entries;
SELECT 'zset OK' AS result;
