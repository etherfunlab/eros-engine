-- Issue #254: freeze the engine-computed relationship state the PDE judge was
-- shown, as of the moment the decision ran. Deliberately a separate column from
-- payload — payload means "what the model returned", inputs means "what the
-- engine supplied". Nullable: historical rows stay NULL (the values are not
-- recoverable), and the fail-open audit write leaves it NULL when
-- serialisation fails rather than dropping the row.
ALTER TABLE engine.companion_decision_events
    ADD COLUMN inputs JSONB;
