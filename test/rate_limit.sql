-- Token-bucket rate limiter. Run after smoke.sql (which installs the extension).
\set ON_ERROR_STOP on
\pset footer off

-- burst up to capacity, then deny with a sensible retry hint
DO $$
DECLARE
  ok int := 0;
  info record;
BEGIN
  PERFORM kv.del('rl:a');
  FOR i IN 1..5 LOOP
    IF kv.rate_limit('rl:a', 3, interval '1 second') THEN ok := ok + 1; END IF;
  END LOOP;
  ASSERT ok = 3, format('burst: expected 3 allowed, got %s', ok);
  SELECT * INTO info FROM kv.rate_limit_info('rl:a', 3, interval '1 second');
  ASSERT NOT info.allowed, 'empty bucket denies';
  ASSERT info.remaining = 0, 'no tokens left';
  ASSERT info.retry_after_ms BETWEEN 1 AND 334, format('retry_after_ms %s', info.retry_after_ms);
  ASSERT info.reset_after_ms BETWEEN 900 AND 1000, format('reset_after_ms %s', info.reset_after_ms);
  ASSERT kv.ttl('rl:a') BETWEEN 900 AND 1000, 'bucket expires when it would be full again';
END $$;
SELECT pg_sleep(0.4);
DO $$
BEGIN
  ASSERT kv.rate_limit('rl:a', 3, interval '1 second'), 'one token refilled after 0.4 s';
  ASSERT NOT kv.rate_limit('rl:a', 3, interval '1 second'), 'but only one';
END $$;

-- cost
DO $$
DECLARE
  info record;
BEGIN
  PERFORM kv.del('rl:c');
  SELECT * INTO info FROM kv.rate_limit_info('rl:c', 10, interval '1 minute', 7);
  ASSERT info.allowed AND info.remaining = 3 AND info.retry_after_ms = 0, 'cost 7 of 10';
  ASSERT NOT kv.rate_limit('rl:c', 10, interval '1 minute', 4), 'cost 4 > 3 remaining';
  ASSERT kv.rate_limit('rl:c', 10, interval '1 minute', 3), 'cost 3 fits';
  ASSERT kv.rate_limit('rl:c', 10, interval '1 minute', 0), 'cost 0 always passes';
END $$;

-- keys are independent
DO $$
BEGIN
  PERFORM kv.del('rl:x');
  PERFORM kv.del('rl:y');
  ASSERT kv.rate_limit('rl:x', 1, interval '1 hour');
  ASSERT NOT kv.rate_limit('rl:x', 1, interval '1 hour');
  ASSERT kv.rate_limit('rl:y', 1, interval '1 hour'), 'other key unaffected';
END $$;

-- argument validation and WRONGTYPE
DO $$
BEGIN
  BEGIN
    PERFORM kv.rate_limit('rl:e', 0, interval '1 second');
    RAISE EXCEPTION 'capacity 0 accepted';
  EXCEPTION WHEN invalid_parameter_value THEN
    IF SQLERRM NOT LIKE '%capacity must be at least 1%' THEN RAISE; END IF;
  END;
  BEGIN
    PERFORM kv.rate_limit('rl:e', 5, interval '1 second', 6);
    RAISE EXCEPTION 'cost > capacity accepted';
  EXCEPTION WHEN invalid_parameter_value THEN
    IF SQLERRM NOT LIKE '%cost must be between%' THEN RAISE; END IF;
  END;
  BEGIN
    PERFORM kv.rate_limit('rl:e', 5, interval '0 seconds');
    RAISE EXCEPTION 'zero interval accepted';
  EXCEPTION WHEN invalid_parameter_value THEN
    IF SQLERRM NOT LIKE '%interval must be positive%' THEN RAISE; END IF;
  END;
  PERFORM kv.set('rl:str', 'hello');
  BEGIN
    PERFORM kv.rate_limit('rl:str', 5, interval '1 second');
    RAISE EXCEPTION 'rate_limit on a string accepted';
  EXCEPTION WHEN wrong_object_type THEN
    IF SQLERRM NOT LIKE '%WRONGTYPE%' THEN RAISE; END IF;
  END;
  PERFORM kv.rate_limit('rl:b', 5, interval '1 second');
  BEGIN
    PERFORM kv.get('rl:b');
    RAISE EXCEPTION 'get on a bucket accepted';
  EXCEPTION WHEN wrong_object_type THEN
    IF SQLERRM NOT LIKE '%WRONGTYPE%' THEN RAISE; END IF;
  END;
  BEGIN
    PERFORM kv.incr('rl:b');
    RAISE EXCEPTION 'incr on a bucket accepted';
  EXCEPTION WHEN wrong_object_type THEN
    IF SQLERRM NOT LIKE '%WRONGTYPE%' THEN RAISE; END IF;
  END;
  PERFORM kv.set('rl:b', 'now a string');
  ASSERT kv.get('rl:b') = 'now a string', 'set overwrites a bucket';
END $$;

-- an idle bucket disappears once it would be full again
DO $$
BEGIN
  PERFORM kv.del('rl:idle');
  PERFORM kv.rate_limit('rl:idle', 5, interval '200 milliseconds');
  ASSERT kv.ttl('rl:idle') BETWEEN 1 AND 40, format('ttl %s', kv.ttl('rl:idle'));
END $$;
SELECT pg_sleep(1.2);
DO $$
BEGIN
  ASSERT kv.ttl('rl:idle') = -2, 'idle bucket expired';
  ASSERT kv.rate_limit('rl:idle', 5, interval '200 milliseconds'), 'and starts full again';
END $$;

SELECT kv.verify() AS verified_entries;
SELECT 'rate_limit OK' AS result;
