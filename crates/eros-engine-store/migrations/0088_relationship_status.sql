-- Affinity 5.0: the pair's relationship status and its cold war, on the
-- session's affinity row. Every change also writes a 'relationship' event.
ALTER TABLE engine.companion_affinity
    ADD COLUMN relationship_status text NOT NULL DEFAULT 'none'
        CONSTRAINT companion_affinity_relationship_status_check
        CHECK (relationship_status IN ('none', 'dating', 'partner', 'peer', 'fwb',
                                       'situationship', 'engaged', 'ex')),
    ADD COLUMN cold_war_since timestamptz;

ALTER TABLE engine.companion_affinity_events
    DROP CONSTRAINT companion_affinity_events_event_type_check;
ALTER TABLE engine.companion_affinity_events
    ADD CONSTRAINT companion_affinity_events_event_type_check
    CHECK (event_type IN ('message', 'ghost', 'gift', 'proactive', 'time_decay',
                          'life_choice', 'activity', 'relationship')) NOT VALID;
