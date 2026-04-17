-- Backfill legacy solver records so pre-existing solvers keep their historical
-- settle/cancel authority after solver permission categories are enforced.
--
-- Before PR #708, solver capability was represented only by `is_solver = 1`.
-- After PR #708, settle/cancel paths require `category = 2` (read-write).
--
-- Operators should not need to patch this manually, so migrate any legacy solver
-- rows that still have the default/legacy category value to read-write.
UPDATE users
SET category = 2
WHERE is_solver = 1
  AND category = 0;
