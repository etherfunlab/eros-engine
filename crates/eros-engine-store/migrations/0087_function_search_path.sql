-- Pin search_path on the two engine functions that ran with the caller's.
-- Both bodies call only pg_catalog functions, which resolve under an empty
-- path, so their results do not change. fact_norm stays IMMUTABLE STRICT and
-- the generated column that calls it is not rewritten.
ALTER FUNCTION engine.touch_world_worldviews() SET search_path = '';
ALTER FUNCTION engine.fact_norm(text) SET search_path = '';
