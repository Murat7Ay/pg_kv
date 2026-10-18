-- Many sessions draining one bucket: the number of allowed calls must be exactly
-- the capacity (refill over the run is far below one token).
-- (No ':' in the key names: pgbench would read ':name' as a variable.)
SELECT CASE WHEN kv.rate_limit('rl-conc', 1000, interval '1 day') THEN kv.incr('rl-conc-allowed') END;
