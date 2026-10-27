-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Baseline: the complete schema as of migration 0077 (engine v2.3.1), folding
-- the former 0000_schema.sql through 0077_affinity_event_life_choice.sql into
-- one file. Migrations 0078+ apply on top of it unchanged.
--
-- Existing databases NEVER run this file: the engine's migrate entry
-- (eros_engine_store::migrate, the `eros-engine migrate` subcommand) first
-- collapses their _sqlx_migrations history — rows 0..77 become one version-0
-- row carrying this file's checksum — so sqlx sees it as already applied. A
-- database whose history predates 0077 (older than v2.3.1) is refused there.
--
-- The guard below only fires when raw sqlx is pointed at an existing
-- database, which is unsupported; it aborts before touching anything.
--
-- FROZEN: any byte change to this file breaks checksum validation at
-- version 0 for every existing database. Schema changes go in new
-- incremental migrations, never here.
--
-- Spec: docs/superpowers/specs/2026-10-27-migration-baseline-squash-design.md

DO $baseline_guard$
BEGIN
    IF to_regclass('engine.chat_messages') IS NOT NULL THEN
        RAISE EXCEPTION USING MESSAGE =
            'existing database: run migrations through eros-engine migrate '
            '(eros_engine_store::migrate), not raw sqlx against this directory';
    END IF;
END
$baseline_guard$;

--
-- PostgreSQL database dump
--




--
-- Name: engine; Type: SCHEMA; Schema: -; Owner: -
--

CREATE SCHEMA engine;


--
-- Name: vector; Type: EXTENSION; Schema: -; Owner: -
--

CREATE EXTENSION IF NOT EXISTS vector;


--
-- Name: EXTENSION vector; Type: COMMENT; Schema: -; Owner: -
--



--
-- Name: touch_world_worldviews(); Type: FUNCTION; Schema: engine; Owner: -
--

CREATE FUNCTION engine.touch_world_worldviews() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.content IS DISTINCT FROM OLD.content THEN
        NEW.updated_at := clock_timestamp();
    END IF;
    RETURN NEW;
END
$$;




--
-- Name: character_insights; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.character_insights (
    instance_id uuid NOT NULL,
    location text,
    occupation text,
    current_situation text,
    desires text,
    vulnerabilities text,
    habits text,
    personal_values text,
    likes text[] DEFAULT '{}'::text[] NOT NULL,
    dislikes text[] DEFAULT '{}'::text[] NOT NULL,
    relationships text[] DEFAULT '{}'::text[] NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: character_insights_events; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.character_insights_events (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    run_id uuid NOT NULL,
    instance_id uuid NOT NULL,
    session_id uuid,
    message_id uuid,
    stage text NOT NULL,
    status text NOT NULL,
    payload jsonb,
    generation_id text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT character_insights_events_stage_check CHECK ((stage = ANY (ARRAY['extraction'::text, 'structuring'::text]))),
    CONSTRAINT character_insights_events_status_check CHECK ((status = ANY (ARRAY['ok'::text, 'empty'::text, 'parse_error'::text])))
);


--
-- Name: character_insights_snapshot; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.character_insights_snapshot (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    instance_id uuid NOT NULL,
    snapshot jsonb NOT NULL,
    captured_at timestamp with time zone NOT NULL
);


--
-- Name: chat_images_events; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.chat_images_events (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    source text NOT NULL,
    user_id uuid NOT NULL,
    instance_id uuid,
    session_id uuid,
    status text NOT NULL,
    inputs jsonb NOT NULL,
    subject text,
    caption text,
    composed_prompt text,
    variant text,
    generation_id text,
    attempts smallint NOT NULL,
    last_failure text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    llm_attempts jsonb,
    gateway_errors jsonb,
    CONSTRAINT chat_images_events_source_check CHECK ((source = ANY (ARRAY['chat_reply_text_image'::text, 'chat_reply_image'::text, 'compose_endpoint'::text, 'compose_endpoint_stream'::text, 'image_edit'::text]))),
    CONSTRAINT chat_images_events_status_check CHECK ((status = ANY (ARRAY['ok'::text, 'exhausted'::text, 'not_configured'::text])))
);


--
-- Name: chat_messages; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.chat_messages (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    session_id uuid NOT NULL,
    role text NOT NULL,
    content text NOT NULL,
    sent_at timestamp with time zone DEFAULT now() NOT NULL,
    client_msg_id text,
    ghost_decision boolean DEFAULT false NOT NULL,
    user_message_id uuid,
    continues_from_message_id uuid,
    truncated boolean DEFAULT false NOT NULL,
    generation_id text,
    assistant_action_type text,
    metadata jsonb,
    pre_filter_content text,
    filter_model text,
    filter_triggers jsonb,
    f_client_msg_id text,
    f_generation_id text,
    channel text,
    llm_attempts jsonb,
    gateway_errors jsonb,
    read_at timestamp with time zone,
    reaction text,
    reacted_at timestamp with time zone,
    speaker_session_id uuid,
    CONSTRAINT chat_messages_assistant_action_type_check CHECK (((assistant_action_type IS NULL) OR (assistant_action_type = ANY (ARRAY['reply'::text, 'proactive'::text])))),
    CONSTRAINT chat_messages_channel_check CHECK (((channel IS NULL) OR (channel = ANY (ARRAY['voice'::text, 'product_qa'::text])))),
    CONSTRAINT chat_messages_reaction_assistant_only CHECK (((reaction IS NULL) OR (role = 'assistant'::text))),
    CONSTRAINT chat_messages_reaction_pair CHECK (((reaction IS NULL) = (reacted_at IS NULL))),
    CONSTRAINT chat_messages_role_check CHECK ((role = ANY (ARRAY['user'::text, 'assistant'::text, 'system_error'::text]))),
    CONSTRAINT chat_messages_speaker_assistant_only CHECK (((speaker_session_id IS NULL) OR (role = 'assistant'::text)))
);


--
-- Name: chat_session_members; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.chat_session_members (
    group_session_id uuid NOT NULL,
    member_session_id uuid NOT NULL,
    joined_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: chat_sessions; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.chat_sessions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    user_id uuid NOT NULL,
    instance_id uuid,
    is_converted boolean DEFAULT false NOT NULL,
    last_active_at timestamp with time zone DEFAULT now() NOT NULL,
    metadata jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    classified_at timestamp with time zone,
    classification_claimed_at timestamp with time zone,
    channel text DEFAULT 'text'::text NOT NULL,
    archived boolean DEFAULT false NOT NULL,
    title text,
    CONSTRAINT chat_sessions_channel_check CHECK ((channel = ANY (ARRAY['text'::text, 'voice'::text, 'group'::text]))),
    CONSTRAINT chat_sessions_group_no_instance CHECK (((channel <> 'group'::text) OR (instance_id IS NULL))),
    CONSTRAINT chat_sessions_title_group_only CHECK (((title IS NULL) OR ((channel = 'group'::text) AND ((char_length(btrim(title)) >= 1) AND (char_length(btrim(title)) <= 64)))))
);


--
-- Name: chat_turn_queue; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.chat_turn_queue (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    session_id uuid NOT NULL,
    user_message_id uuid NOT NULL,
    user_id uuid NOT NULL,
    status text DEFAULT 'pending'::text NOT NULL,
    attempts integer DEFAULT 0 NOT NULL,
    claimed_at timestamp with time zone,
    last_error text,
    params jsonb,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT chat_turn_queue_status_check CHECK ((status = ANY (ARRAY['pending'::text, 'claimed'::text, 'done'::text, 'failed'::text])))
);


--
-- Name: chat_vision_events; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.chat_vision_events (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    user_id uuid NOT NULL,
    session_id uuid,
    message_id uuid,
    status text NOT NULL,
    image_url text NOT NULL,
    vision jsonb,
    generation_id text,
    attempts smallint NOT NULL,
    last_failure text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    llm_attempts jsonb,
    gateway_errors jsonb,
    CONSTRAINT chat_vision_events_status_check CHECK ((status = ANY (ARRAY['ok'::text, 'exhausted'::text, 'not_configured'::text])))
);


--
-- Name: companion_affinity; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.companion_affinity (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    session_id uuid NOT NULL,
    user_id uuid NOT NULL,
    instance_id uuid NOT NULL,
    warmth double precision DEFAULT ((1.0 / 3.0) * ((1)::numeric - ((0.35 * 10.0) / 13.0))) NOT NULL,
    trust double precision DEFAULT 0.0 NOT NULL,
    intrigue double precision DEFAULT 0.0 NOT NULL,
    intimacy double precision DEFAULT 0.0 NOT NULL,
    patience double precision DEFAULT ((1.0 / 3.0) * ((1)::numeric - ((0.35 * 10.0) / 13.0))) NOT NULL,
    tension double precision DEFAULT 0.0 NOT NULL,
    ghost_streak integer DEFAULT 0 NOT NULL,
    last_ghost_at timestamp with time zone,
    total_ghosts integer DEFAULT 0 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    pending_deltas jsonb,
    warmth_grade smallint DEFAULT 2 NOT NULL,
    patience_grade smallint DEFAULT 2 NOT NULL,
    bond double precision GENERATED ALWAYS AS (LEAST((1)::double precision, GREATEST((0)::double precision, ((trust + intrigue) / (2)::double precision)))) STORED,
    chemistry double precision GENERATED ALWAYS AS (LEAST((1)::double precision, GREATEST((0)::double precision, ((intimacy + tension) / (2)::double precision)))) STORED,
    bond_tier smallint DEFAULT 1 NOT NULL,
    chem_tier smallint DEFAULT 1 NOT NULL,
    feeling_clause text,
    feeling_clause_at timestamp with time zone,
    CONSTRAINT companion_affinity_patience_grade_range CHECK (((patience_grade >= 1) AND (patience_grade <= 3))),
    CONSTRAINT companion_affinity_warmth_grade_range CHECK (((warmth_grade >= 1) AND (warmth_grade <= 3)))
);


--
-- Name: companion_affinity_events; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.companion_affinity_events (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    affinity_id uuid NOT NULL,
    event_type text NOT NULL,
    deltas jsonb DEFAULT '{}'::jsonb NOT NULL,
    context jsonb DEFAULT '{}'::jsonb,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    effective_deltas jsonb,
    generation_id text,
    label_changes jsonb,
    effective_line_deltas jsonb,
    state_before jsonb,
    state_after jsonb,
    llm_attempts jsonb,
    gateway_errors jsonb,
    user_message_id uuid,
    CONSTRAINT companion_affinity_events_event_type_check CHECK ((event_type = ANY (ARRAY['message'::text, 'ghost'::text, 'gift'::text, 'proactive'::text, 'time_decay'::text, 'life_choice'::text])))
);


--
-- Name: companion_decision_events; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.companion_decision_events (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    run_id uuid NOT NULL,
    user_id uuid NOT NULL,
    session_id uuid,
    message_id uuid,
    status text NOT NULL,
    action text,
    proposed_action text,
    payload jsonb,
    generation_id text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    inputs jsonb,
    llm_attempts jsonb,
    gateway_errors jsonb,
    CONSTRAINT companion_decision_events_status_check CHECK ((status = ANY (ARRAY['ok'::text, 'empty'::text, 'parse_error'::text, 'timeout'::text, 'error'::text, 'upstream_error'::text, 'gateway_error'::text])))
);


--
-- Name: companion_insights_events; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.companion_insights_events (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    run_id uuid NOT NULL,
    user_id uuid NOT NULL,
    session_id uuid,
    message_id uuid,
    stage text NOT NULL,
    status text NOT NULL,
    payload jsonb,
    generation_id text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT companion_insights_events_stage_check CHECK ((stage = ANY (ARRAY['facts'::text, 'structured'::text]))),
    CONSTRAINT companion_insights_events_status_check CHECK ((status = ANY (ARRAY['ok'::text, 'empty'::text, 'parse_error'::text])))
);


--
-- Name: companion_insights_snapshot; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.companion_insights_snapshot (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    user_id uuid NOT NULL,
    insights jsonb NOT NULL,
    training_level double precision NOT NULL,
    captured_at timestamp with time zone NOT NULL
);


--
-- Name: companion_memories; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.companion_memories (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    session_id uuid NOT NULL,
    user_id uuid NOT NULL,
    instance_id uuid,
    content text NOT NULL,
    embedding vector(512) NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    category text,
    metadata jsonb
);


--
-- Name: error_handling_config; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.error_handling_config (
    kind text NOT NULL,
    payload jsonb NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: human_insights; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.human_insights (
    user_id uuid NOT NULL,
    city text,
    occupation text,
    mbti_guess text,
    love_values text,
    emotional_needs text,
    life_rhythm text,
    interests text[] DEFAULT '{}'::text[] NOT NULL,
    personality_traits text[] DEFAULT '{}'::text[] NOT NULL,
    preferred_gender text,
    age_min integer,
    age_max integer,
    deal_breakers text[] DEFAULT '{}'::text[] NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    location text,
    hometown text,
    nationality text,
    education text,
    family text,
    relationship_history text,
    social_pattern text,
    future_plans text,
    finance_status text
);


--
-- Name: human_insights_snapshot; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.human_insights_snapshot (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    user_id uuid NOT NULL,
    snapshot jsonb NOT NULL,
    captured_at timestamp with time zone NOT NULL
);


--
-- Name: life_choices; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.life_choices (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    session_id uuid NOT NULL,
    kind text NOT NULL,
    status text DEFAULT 'pending'::text NOT NULL,
    tier_gap smallint NOT NULL,
    asked_at timestamp with time zone NOT NULL,
    resolved_at timestamp with time zone,
    message_id uuid,
    event_id uuid
);


--
-- Name: llm_generations; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.llm_generations (
    generation_id text NOT NULL,
    session_id uuid,
    task text NOT NULL,
    model text,
    usage jsonb,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: persona_events; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.persona_events (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    instance_id uuid NOT NULL,
    session_id uuid,
    event_key text NOT NULL,
    kind text NOT NULL,
    started_at timestamp with time zone NOT NULL,
    ends_at timestamp with time zone NOT NULL,
    message_id uuid,
    CONSTRAINT persona_events_check CHECK ((ends_at > started_at))
);


--
-- Name: persona_genomes; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.persona_genomes (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    name text NOT NULL,
    system_prompt text NOT NULL,
    tip_personality text,
    art_metadata jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: persona_instance_items; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.persona_instance_items (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    instance_id uuid NOT NULL,
    kind text NOT NULL,
    name text NOT NULL,
    source text NOT NULL,
    user_message_id uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT persona_instance_items_kind_check CHECK ((kind = ANY (ARRAY['item'::text, 'clothing'::text]))),
    CONSTRAINT persona_instance_items_source_check CHECK ((source = ANY (ARRAY['gift'::text, 'caller'::text])))
);


--
-- Name: persona_instances; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.persona_instances (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    genome_id uuid,
    owner_uid uuid NOT NULL,
    status text DEFAULT 'active'::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    byoc_key bytea,
    CONSTRAINT persona_instances_byoc_key_len_chk CHECK (((byoc_key IS NULL) OR (octet_length(byoc_key) = 32))),
    CONSTRAINT persona_instances_kind_chk CHECK (((genome_id IS NULL) <> (byoc_key IS NULL)))
);


--
-- Name: persona_story_events; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.persona_story_events (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    owner_uid uuid NOT NULL,
    instance_id uuid NOT NULL,
    category text NOT NULL,
    content text NOT NULL,
    story_date date NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: persona_story_insights; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.persona_story_insights (
    instance_id uuid NOT NULL,
    owner_uid uuid NOT NULL,
    city text,
    location text,
    hometown text,
    nationality text,
    occupation text,
    mbti_guess text,
    love_values text,
    emotional_needs text,
    life_rhythm text,
    education text,
    family text,
    relationship_history text,
    social_pattern text,
    future_plans text,
    finance_status text,
    interests text[] DEFAULT '{}'::text[] NOT NULL,
    personality_traits text[] DEFAULT '{}'::text[] NOT NULL,
    preferred_gender text,
    age_min integer,
    age_max integer,
    deal_breakers text[] DEFAULT '{}'::text[] NOT NULL,
    work_history text,
    romance_history text,
    family_of_origin text,
    user_relationship text,
    digest text DEFAULT ''::text NOT NULL,
    insight_version integer DEFAULT 1 NOT NULL,
    last_run_at timestamp with time zone,
    claimed_at timestamp with time zone,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: persona_story_memories; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.persona_story_memories (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    owner_uid uuid NOT NULL,
    instance_id uuid NOT NULL,
    event_id uuid NOT NULL,
    content text NOT NULL,
    embedding vector(512) NOT NULL,
    story_date date NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: user_insights; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.user_insights (
    instance_id uuid NOT NULL,
    location text,
    occupation text,
    current_situation text,
    desires text,
    vulnerabilities text,
    habits text,
    personal_values text,
    likes text[] DEFAULT '{}'::text[] NOT NULL,
    dislikes text[] DEFAULT '{}'::text[] NOT NULL,
    relationships text[] DEFAULT '{}'::text[] NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: user_insights_events; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.user_insights_events (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    run_id uuid NOT NULL,
    instance_id uuid NOT NULL,
    session_id uuid,
    message_id uuid,
    stage text NOT NULL,
    status text NOT NULL,
    payload jsonb,
    generation_id text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT user_insights_events_stage_check CHECK ((stage = ANY (ARRAY['extraction'::text, 'structuring'::text]))),
    CONSTRAINT user_insights_events_status_check CHECK ((status = ANY (ARRAY['ok'::text, 'empty'::text, 'parse_error'::text])))
);


--
-- Name: user_insights_snapshot; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.user_insights_snapshot (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    instance_id uuid NOT NULL,
    snapshot jsonb NOT NULL,
    captured_at timestamp with time zone NOT NULL
);


--
-- Name: world_enrollments; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.world_enrollments (
    owner_uid uuid NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    town_enabled boolean DEFAULT false NOT NULL,
    stories_enabled boolean DEFAULT false NOT NULL
);


--
-- Name: world_memories; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.world_memories (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    owner_uid uuid NOT NULL,
    instance_id uuid NOT NULL,
    content text NOT NULL,
    embedding vector(512) NOT NULL,
    script_date date NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: world_post_comments; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.world_post_comments (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    post_id uuid NOT NULL,
    author_instance_id uuid,
    source text,
    content text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT world_post_comments_check CHECK (((author_instance_id IS NULL) = (source IS NULL))),
    CONSTRAINT world_post_comments_source_check CHECK ((source = ANY (ARRAY['round'::text, 'reply'::text])))
);


--
-- Name: world_posts; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.world_posts (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    owner_uid uuid NOT NULL,
    instance_id uuid NOT NULL,
    content text NOT NULL,
    scheduled_at timestamp with time zone NOT NULL,
    published_at timestamp with time zone,
    last_reply_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    last_user_comment_at timestamp with time zone
);


--
-- Name: world_states; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.world_states (
    owner_uid uuid NOT NULL,
    seed jsonb NOT NULL,
    digests jsonb NOT NULL,
    seed_version integer DEFAULT 1 NOT NULL,
    last_run_at timestamp with time zone,
    claimed_at timestamp with time zone,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    last_comment_round_at timestamp with time zone,
    worldview_hash text,
    worldview_set_at timestamp with time zone
);


--
-- Name: world_worldviews; Type: TABLE; Schema: engine; Owner: -
--

CREATE TABLE engine.world_worldviews (
    owner_uid uuid NOT NULL,
    content text NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT worldview_content_len CHECK (((char_length(content) >= 1) AND (char_length(content) <= 10000)))
);


--
-- Name: character_insights_events character_insights_events_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.character_insights_events
    ADD CONSTRAINT character_insights_events_pkey PRIMARY KEY (id);


--
-- Name: character_insights character_insights_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.character_insights
    ADD CONSTRAINT character_insights_pkey PRIMARY KEY (instance_id);


--
-- Name: character_insights_snapshot character_insights_snapshot_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.character_insights_snapshot
    ADD CONSTRAINT character_insights_snapshot_pkey PRIMARY KEY (id);


--
-- Name: chat_images_events chat_images_events_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_images_events
    ADD CONSTRAINT chat_images_events_pkey PRIMARY KEY (id);


--
-- Name: chat_messages chat_messages_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_messages
    ADD CONSTRAINT chat_messages_pkey PRIMARY KEY (id);


--
-- Name: chat_session_members chat_session_members_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_session_members
    ADD CONSTRAINT chat_session_members_pkey PRIMARY KEY (group_session_id, member_session_id);


--
-- Name: chat_sessions chat_sessions_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_sessions
    ADD CONSTRAINT chat_sessions_pkey PRIMARY KEY (id);


--
-- Name: chat_turn_queue chat_turn_queue_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_turn_queue
    ADD CONSTRAINT chat_turn_queue_pkey PRIMARY KEY (id);


--
-- Name: chat_turn_queue chat_turn_queue_user_message_id_key; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_turn_queue
    ADD CONSTRAINT chat_turn_queue_user_message_id_key UNIQUE (user_message_id);


--
-- Name: chat_vision_events chat_vision_events_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_vision_events
    ADD CONSTRAINT chat_vision_events_pkey PRIMARY KEY (id);


--
-- Name: companion_affinity_events companion_affinity_events_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_affinity_events
    ADD CONSTRAINT companion_affinity_events_pkey PRIMARY KEY (id);


--
-- Name: companion_affinity companion_affinity_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_affinity
    ADD CONSTRAINT companion_affinity_pkey PRIMARY KEY (id);


--
-- Name: companion_affinity companion_affinity_session_id_key; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_affinity
    ADD CONSTRAINT companion_affinity_session_id_key UNIQUE (session_id);


--
-- Name: companion_decision_events companion_decision_events_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_decision_events
    ADD CONSTRAINT companion_decision_events_pkey PRIMARY KEY (id);


--
-- Name: companion_insights_events companion_insights_events_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_insights_events
    ADD CONSTRAINT companion_insights_events_pkey PRIMARY KEY (id);


--
-- Name: companion_insights_snapshot companion_insights_snapshot_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_insights_snapshot
    ADD CONSTRAINT companion_insights_snapshot_pkey PRIMARY KEY (id);


--
-- Name: companion_memories companion_memories_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_memories
    ADD CONSTRAINT companion_memories_pkey PRIMARY KEY (id);


--
-- Name: error_handling_config error_handling_config_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.error_handling_config
    ADD CONSTRAINT error_handling_config_pkey PRIMARY KEY (kind);


--
-- Name: human_insights human_insights_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.human_insights
    ADD CONSTRAINT human_insights_pkey PRIMARY KEY (user_id);


--
-- Name: human_insights_snapshot human_insights_snapshot_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.human_insights_snapshot
    ADD CONSTRAINT human_insights_snapshot_pkey PRIMARY KEY (id);


--
-- Name: life_choices life_choices_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.life_choices
    ADD CONSTRAINT life_choices_pkey PRIMARY KEY (id);


--
-- Name: llm_generations llm_generations_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.llm_generations
    ADD CONSTRAINT llm_generations_pkey PRIMARY KEY (generation_id);


--
-- Name: persona_events persona_events_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.persona_events
    ADD CONSTRAINT persona_events_pkey PRIMARY KEY (id);


--
-- Name: persona_genomes persona_genomes_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.persona_genomes
    ADD CONSTRAINT persona_genomes_pkey PRIMARY KEY (id);


--
-- Name: persona_instance_items persona_instance_items_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.persona_instance_items
    ADD CONSTRAINT persona_instance_items_pkey PRIMARY KEY (id);


--
-- Name: persona_instances persona_instances_genome_id_owner_uid_key; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.persona_instances
    ADD CONSTRAINT persona_instances_genome_id_owner_uid_key UNIQUE (genome_id, owner_uid);


--
-- Name: persona_instances persona_instances_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.persona_instances
    ADD CONSTRAINT persona_instances_pkey PRIMARY KEY (id);


--
-- Name: persona_story_events persona_story_events_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.persona_story_events
    ADD CONSTRAINT persona_story_events_pkey PRIMARY KEY (id);


--
-- Name: persona_story_insights persona_story_insights_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.persona_story_insights
    ADD CONSTRAINT persona_story_insights_pkey PRIMARY KEY (instance_id);


--
-- Name: persona_story_memories persona_story_memories_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.persona_story_memories
    ADD CONSTRAINT persona_story_memories_pkey PRIMARY KEY (id);


--
-- Name: user_insights_events user_insights_events_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.user_insights_events
    ADD CONSTRAINT user_insights_events_pkey PRIMARY KEY (id);


--
-- Name: user_insights user_insights_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.user_insights
    ADD CONSTRAINT user_insights_pkey PRIMARY KEY (instance_id);


--
-- Name: user_insights_snapshot user_insights_snapshot_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.user_insights_snapshot
    ADD CONSTRAINT user_insights_snapshot_pkey PRIMARY KEY (id);


--
-- Name: world_enrollments world_enrollments_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.world_enrollments
    ADD CONSTRAINT world_enrollments_pkey PRIMARY KEY (owner_uid);


--
-- Name: world_memories world_memories_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.world_memories
    ADD CONSTRAINT world_memories_pkey PRIMARY KEY (id);


--
-- Name: world_post_comments world_post_comments_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.world_post_comments
    ADD CONSTRAINT world_post_comments_pkey PRIMARY KEY (id);


--
-- Name: world_posts world_posts_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.world_posts
    ADD CONSTRAINT world_posts_pkey PRIMARY KEY (id);


--
-- Name: world_states world_states_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.world_states
    ADD CONSTRAINT world_states_pkey PRIMARY KEY (owner_uid);


--
-- Name: world_worldviews world_worldviews_pkey; Type: CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.world_worldviews
    ADD CONSTRAINT world_worldviews_pkey PRIMARY KEY (owner_uid);


--
-- Name: chat_messages_client_msg_id_uidx; Type: INDEX; Schema: engine; Owner: -
--

CREATE UNIQUE INDEX chat_messages_client_msg_id_uidx ON engine.chat_messages USING btree (session_id, client_msg_id) WHERE (client_msg_id IS NOT NULL);


--
-- Name: chat_messages_f_client_msg_id_uidx; Type: INDEX; Schema: engine; Owner: -
--

CREATE UNIQUE INDEX chat_messages_f_client_msg_id_uidx ON engine.chat_messages USING btree (session_id, f_client_msg_id) WHERE (f_client_msg_id IS NOT NULL);


--
-- Name: chat_messages_needs_idx; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX chat_messages_needs_idx ON engine.chat_messages USING btree (session_id, sent_at DESC) WHERE ((role = 'user'::text) AND (metadata ? 'needs'::text));


--
-- Name: chat_messages_proactive_day_uidx; Type: INDEX; Schema: engine; Owner: -
--

CREATE UNIQUE INDEX chat_messages_proactive_day_uidx ON engine.chat_messages USING btree (session_id, ((metadata ->> 'local_date'::text))) WHERE (assistant_action_type = 'proactive'::text);


--
-- Name: chat_messages_proactive_open_key_uidx; Type: INDEX; Schema: engine; Owner: -
--

CREATE UNIQUE INDEX chat_messages_proactive_open_key_uidx ON engine.chat_messages USING btree (session_id, ((metadata ->> 'open_key'::text))) WHERE (assistant_action_type = 'proactive'::text);


--
-- Name: chat_messages_speaker_session_idx; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX chat_messages_speaker_session_idx ON engine.chat_messages USING btree (speaker_session_id) WHERE (speaker_session_id IS NOT NULL);


--
-- Name: chat_messages_user_message_id_idx; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX chat_messages_user_message_id_idx ON engine.chat_messages USING btree (user_message_id) WHERE (user_message_id IS NOT NULL);


--
-- Name: chat_messages_voice_assistant_reply_uidx; Type: INDEX; Schema: engine; Owner: -
--

CREATE UNIQUE INDEX chat_messages_voice_assistant_reply_uidx ON engine.chat_messages USING btree (user_message_id) WHERE ((role = 'assistant'::text) AND (channel = 'voice'::text));


--
-- Name: chat_session_members_member_idx; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX chat_session_members_member_idx ON engine.chat_session_members USING btree (member_session_id);


--
-- Name: chat_turn_queue_active_idx; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX chat_turn_queue_active_idx ON engine.chat_turn_queue USING btree (session_id, created_at DESC) WHERE (status = ANY (ARRAY['pending'::text, 'claimed'::text]));


--
-- Name: companion_memories_profile_created_idx; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX companion_memories_profile_created_idx ON engine.companion_memories USING btree (created_at, id) WHERE ((instance_id IS NULL) AND (category IS NOT NULL));


--
-- Name: idx_affinity_events_affinity; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_affinity_events_affinity ON engine.companion_affinity_events USING btree (affinity_id);


--
-- Name: idx_affinity_events_affinity_created; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_affinity_events_affinity_created ON engine.companion_affinity_events USING btree (affinity_id, created_at DESC, id DESC);


--
-- Name: idx_affinity_events_user_message; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_affinity_events_user_message ON engine.companion_affinity_events USING btree (user_message_id) WHERE (user_message_id IS NOT NULL);


--
-- Name: idx_affinity_user; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_affinity_user ON engine.companion_affinity USING btree (user_id);


--
-- Name: idx_affinity_user_updated; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_affinity_user_updated ON engine.companion_affinity USING btree (user_id, updated_at DESC, session_id DESC);


--
-- Name: idx_character_insights_events_generation; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_character_insights_events_generation ON engine.character_insights_events USING btree (generation_id);


--
-- Name: idx_character_insights_events_instance_time; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_character_insights_events_instance_time ON engine.character_insights_events USING btree (instance_id, created_at DESC);


--
-- Name: idx_character_insights_events_message; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_character_insights_events_message ON engine.character_insights_events USING btree (message_id);


--
-- Name: idx_character_insights_events_run; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_character_insights_events_run ON engine.character_insights_events USING btree (run_id);


--
-- Name: idx_character_insights_events_session; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_character_insights_events_session ON engine.character_insights_events USING btree (session_id);


--
-- Name: idx_character_insights_snapshot_instance_time; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_character_insights_snapshot_instance_time ON engine.character_insights_snapshot USING btree (instance_id, captured_at DESC);


--
-- Name: idx_chat_images_events_generation; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_chat_images_events_generation ON engine.chat_images_events USING btree (generation_id);


--
-- Name: idx_chat_images_events_instance; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_chat_images_events_instance ON engine.chat_images_events USING btree (instance_id);


--
-- Name: idx_chat_images_events_session_time; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_chat_images_events_session_time ON engine.chat_images_events USING btree (session_id, created_at DESC);


--
-- Name: idx_chat_images_events_user_time; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_chat_images_events_user_time ON engine.chat_images_events USING btree (user_id, created_at DESC);


--
-- Name: idx_chat_messages_continues_from; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_chat_messages_continues_from ON engine.chat_messages USING btree (continues_from_message_id) WHERE (continues_from_message_id IS NOT NULL);


--
-- Name: idx_chat_messages_generation; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_chat_messages_generation ON engine.chat_messages USING btree (generation_id);


--
-- Name: idx_chat_messages_session; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_chat_messages_session ON engine.chat_messages USING btree (session_id, sent_at DESC);


--
-- Name: idx_chat_messages_session_reacted_at; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_chat_messages_session_reacted_at ON engine.chat_messages USING btree (session_id, reacted_at) WHERE (reaction IS NOT NULL);


--
-- Name: idx_chat_sessions_instance; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_chat_sessions_instance ON engine.chat_sessions USING btree (instance_id);


--
-- Name: idx_chat_sessions_user; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_chat_sessions_user ON engine.chat_sessions USING btree (user_id);


--
-- Name: idx_chat_sessions_user_instance_channel; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_chat_sessions_user_instance_channel ON engine.chat_sessions USING btree (user_id, instance_id, channel, last_active_at DESC);


--
-- Name: idx_chat_vision_events_generation; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_chat_vision_events_generation ON engine.chat_vision_events USING btree (generation_id);


--
-- Name: idx_chat_vision_events_message; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_chat_vision_events_message ON engine.chat_vision_events USING btree (message_id);


--
-- Name: idx_chat_vision_events_session; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_chat_vision_events_session ON engine.chat_vision_events USING btree (session_id);


--
-- Name: idx_chat_vision_events_user_time; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_chat_vision_events_user_time ON engine.chat_vision_events USING btree (user_id, created_at DESC);


--
-- Name: idx_companion_affinity_events_generation; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_companion_affinity_events_generation ON engine.companion_affinity_events USING btree (generation_id);


--
-- Name: idx_companion_decision_events_generation; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_companion_decision_events_generation ON engine.companion_decision_events USING btree (generation_id);


--
-- Name: idx_companion_decision_events_message; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_companion_decision_events_message ON engine.companion_decision_events USING btree (message_id);


--
-- Name: idx_companion_decision_events_run; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_companion_decision_events_run ON engine.companion_decision_events USING btree (run_id);


--
-- Name: idx_companion_decision_events_session; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_companion_decision_events_session ON engine.companion_decision_events USING btree (session_id);


--
-- Name: idx_companion_decision_events_user_time; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_companion_decision_events_user_time ON engine.companion_decision_events USING btree (user_id, created_at DESC);


--
-- Name: idx_companion_insights_events_generation; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_companion_insights_events_generation ON engine.companion_insights_events USING btree (generation_id);


--
-- Name: idx_companion_insights_events_message; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_companion_insights_events_message ON engine.companion_insights_events USING btree (message_id);


--
-- Name: idx_companion_insights_events_run; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_companion_insights_events_run ON engine.companion_insights_events USING btree (run_id);


--
-- Name: idx_companion_insights_events_session; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_companion_insights_events_session ON engine.companion_insights_events USING btree (session_id);


--
-- Name: idx_companion_insights_events_user_time; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_companion_insights_events_user_time ON engine.companion_insights_events USING btree (user_id, created_at DESC);


--
-- Name: idx_companion_insights_snapshot_user_time; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_companion_insights_snapshot_user_time ON engine.companion_insights_snapshot USING btree (user_id, captured_at DESC);


--
-- Name: idx_human_insights_interests; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_human_insights_interests ON engine.human_insights USING gin (interests);


--
-- Name: idx_human_insights_snapshot_user_time; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_human_insights_snapshot_user_time ON engine.human_insights_snapshot USING btree (user_id, captured_at DESC);


--
-- Name: idx_human_insights_traits; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_human_insights_traits ON engine.human_insights USING gin (personality_traits);


--
-- Name: idx_human_insights_updated_user; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_human_insights_updated_user ON engine.human_insights USING btree (updated_at, user_id);


--
-- Name: idx_llm_generations_session; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_llm_generations_session ON engine.llm_generations USING btree (session_id);


--
-- Name: idx_llm_generations_task_created; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_llm_generations_task_created ON engine.llm_generations USING btree (task, created_at DESC);


--
-- Name: idx_memories_embedding; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_memories_embedding ON engine.companion_memories USING ivfflat (embedding vector_cosine_ops) WITH (lists='100');


--
-- Name: idx_memories_session; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_memories_session ON engine.companion_memories USING btree (session_id) WHERE (instance_id IS NOT NULL);


--
-- Name: idx_memories_user_profile; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_memories_user_profile ON engine.companion_memories USING btree (user_id) WHERE (instance_id IS NULL);


--
-- Name: idx_persona_instances_owner; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_persona_instances_owner ON engine.persona_instances USING btree (owner_uid);


--
-- Name: idx_persona_story_events_instance_time; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_persona_story_events_instance_time ON engine.persona_story_events USING btree (instance_id, created_at DESC);


--
-- Name: idx_persona_story_insights_owner; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_persona_story_insights_owner ON engine.persona_story_insights USING btree (owner_uid);


--
-- Name: idx_persona_story_memories_embedding; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_persona_story_memories_embedding ON engine.persona_story_memories USING ivfflat (embedding vector_cosine_ops) WITH (lists='100');


--
-- Name: idx_persona_story_memories_instance; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_persona_story_memories_instance ON engine.persona_story_memories USING btree (owner_uid, instance_id);


--
-- Name: idx_user_insights_events_generation; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_user_insights_events_generation ON engine.user_insights_events USING btree (generation_id);


--
-- Name: idx_user_insights_events_instance_time; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_user_insights_events_instance_time ON engine.user_insights_events USING btree (instance_id, created_at DESC);


--
-- Name: idx_user_insights_events_message; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_user_insights_events_message ON engine.user_insights_events USING btree (message_id);


--
-- Name: idx_user_insights_events_run; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_user_insights_events_run ON engine.user_insights_events USING btree (run_id);


--
-- Name: idx_user_insights_events_session; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_user_insights_events_session ON engine.user_insights_events USING btree (session_id);


--
-- Name: idx_user_insights_snapshot_instance_time; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_user_insights_snapshot_instance_time ON engine.user_insights_snapshot USING btree (instance_id, captured_at DESC);


--
-- Name: idx_world_memories_embedding; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_world_memories_embedding ON engine.world_memories USING ivfflat (embedding vector_cosine_ops) WITH (lists='100');


--
-- Name: idx_world_memories_owner_instance; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_world_memories_owner_instance ON engine.world_memories USING btree (owner_uid, instance_id);


--
-- Name: idx_world_post_comments_thread; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_world_post_comments_thread ON engine.world_post_comments USING btree (post_id, created_at);


--
-- Name: idx_world_posts_due; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_world_posts_due ON engine.world_posts USING btree (owner_uid, scheduled_at) WHERE (published_at IS NULL);


--
-- Name: idx_world_posts_feed; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_world_posts_feed ON engine.world_posts USING btree (owner_uid, published_at DESC);


--
-- Name: idx_world_posts_reply; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX idx_world_posts_reply ON engine.world_posts USING btree (last_user_comment_at) WHERE (last_user_comment_at IS NOT NULL);


--
-- Name: life_choices_event_idx; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX life_choices_event_idx ON engine.life_choices USING btree (event_id);


--
-- Name: life_choices_message_idx; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX life_choices_message_idx ON engine.life_choices USING btree (message_id);


--
-- Name: life_choices_session_idx; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX life_choices_session_idx ON engine.life_choices USING btree (session_id, asked_at DESC);


--
-- Name: persona_events_instance_idx; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX persona_events_instance_idx ON engine.persona_events USING btree (instance_id, started_at DESC);


--
-- Name: persona_events_message_uidx; Type: INDEX; Schema: engine; Owner: -
--

CREATE UNIQUE INDEX persona_events_message_uidx ON engine.persona_events USING btree (message_id) WHERE (message_id IS NOT NULL);


--
-- Name: persona_events_session_key_uidx; Type: INDEX; Schema: engine; Owner: -
--

CREATE UNIQUE INDEX persona_events_session_key_uidx ON engine.persona_events USING btree (session_id, event_key);


--
-- Name: persona_instance_items_instance_idx; Type: INDEX; Schema: engine; Owner: -
--

CREATE INDEX persona_instance_items_instance_idx ON engine.persona_instance_items USING btree (instance_id, created_at);


--
-- Name: persona_instance_items_user_message_uidx; Type: INDEX; Schema: engine; Owner: -
--

CREATE UNIQUE INDEX persona_instance_items_user_message_uidx ON engine.persona_instance_items USING btree (user_message_id) WHERE (user_message_id IS NOT NULL);


--
-- Name: persona_instances_byoc_key_uniq; Type: INDEX; Schema: engine; Owner: -
--

CREATE UNIQUE INDEX persona_instances_byoc_key_uniq ON engine.persona_instances USING btree (byoc_key) WHERE (byoc_key IS NOT NULL);


--
-- Name: world_worldviews trg_world_worldviews_touch; Type: TRIGGER; Schema: engine; Owner: -
--

CREATE TRIGGER trg_world_worldviews_touch BEFORE UPDATE ON engine.world_worldviews FOR EACH ROW EXECUTE FUNCTION engine.touch_world_worldviews();


--
-- Name: character_insights_events character_insights_events_generation_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.character_insights_events
    ADD CONSTRAINT character_insights_events_generation_id_fkey FOREIGN KEY (generation_id) REFERENCES engine.llm_generations(generation_id) ON DELETE SET NULL;


--
-- Name: character_insights_events character_insights_events_message_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.character_insights_events
    ADD CONSTRAINT character_insights_events_message_id_fkey FOREIGN KEY (message_id) REFERENCES engine.chat_messages(id) ON DELETE SET NULL NOT VALID;


--
-- Name: character_insights_events character_insights_events_session_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.character_insights_events
    ADD CONSTRAINT character_insights_events_session_id_fkey FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id) ON DELETE SET NULL NOT VALID;


--
-- Name: character_insights character_insights_instance_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.character_insights
    ADD CONSTRAINT character_insights_instance_id_fkey FOREIGN KEY (instance_id) REFERENCES engine.persona_instances(id) ON DELETE CASCADE;


--
-- Name: chat_images_events chat_images_events_generation_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_images_events
    ADD CONSTRAINT chat_images_events_generation_id_fkey FOREIGN KEY (generation_id) REFERENCES engine.llm_generations(generation_id) ON DELETE SET NULL;


--
-- Name: chat_images_events chat_images_events_instance_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_images_events
    ADD CONSTRAINT chat_images_events_instance_id_fkey FOREIGN KEY (instance_id) REFERENCES engine.persona_instances(id) ON DELETE SET NULL NOT VALID;


--
-- Name: chat_images_events chat_images_events_session_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_images_events
    ADD CONSTRAINT chat_images_events_session_id_fkey FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id) ON DELETE SET NULL NOT VALID;


--
-- Name: chat_messages chat_messages_continues_from_message_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_messages
    ADD CONSTRAINT chat_messages_continues_from_message_id_fkey FOREIGN KEY (continues_from_message_id) REFERENCES engine.chat_messages(id) ON DELETE SET NULL NOT VALID;


--
-- Name: chat_messages chat_messages_generation_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_messages
    ADD CONSTRAINT chat_messages_generation_id_fkey FOREIGN KEY (generation_id) REFERENCES engine.llm_generations(generation_id) ON DELETE SET NULL;


--
-- Name: chat_messages chat_messages_session_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_messages
    ADD CONSTRAINT chat_messages_session_id_fkey FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id) ON DELETE CASCADE;


--
-- Name: chat_messages chat_messages_speaker_session_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_messages
    ADD CONSTRAINT chat_messages_speaker_session_id_fkey FOREIGN KEY (speaker_session_id) REFERENCES engine.chat_sessions(id) ON DELETE SET NULL;


--
-- Name: chat_messages chat_messages_user_message_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_messages
    ADD CONSTRAINT chat_messages_user_message_id_fkey FOREIGN KEY (user_message_id) REFERENCES engine.chat_messages(id) ON DELETE SET NULL NOT VALID;


--
-- Name: chat_session_members chat_session_members_group_session_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_session_members
    ADD CONSTRAINT chat_session_members_group_session_id_fkey FOREIGN KEY (group_session_id) REFERENCES engine.chat_sessions(id) ON DELETE CASCADE;


--
-- Name: chat_session_members chat_session_members_member_session_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_session_members
    ADD CONSTRAINT chat_session_members_member_session_id_fkey FOREIGN KEY (member_session_id) REFERENCES engine.chat_sessions(id) ON DELETE CASCADE;


--
-- Name: chat_sessions chat_sessions_instance_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_sessions
    ADD CONSTRAINT chat_sessions_instance_id_fkey FOREIGN KEY (instance_id) REFERENCES engine.persona_instances(id) ON DELETE CASCADE;


--
-- Name: chat_turn_queue chat_turn_queue_session_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_turn_queue
    ADD CONSTRAINT chat_turn_queue_session_id_fkey FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id) ON DELETE CASCADE;


--
-- Name: chat_turn_queue chat_turn_queue_user_message_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_turn_queue
    ADD CONSTRAINT chat_turn_queue_user_message_id_fkey FOREIGN KEY (user_message_id) REFERENCES engine.chat_messages(id) ON DELETE CASCADE;


--
-- Name: chat_vision_events chat_vision_events_generation_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_vision_events
    ADD CONSTRAINT chat_vision_events_generation_id_fkey FOREIGN KEY (generation_id) REFERENCES engine.llm_generations(generation_id) ON DELETE SET NULL;


--
-- Name: chat_vision_events chat_vision_events_message_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_vision_events
    ADD CONSTRAINT chat_vision_events_message_id_fkey FOREIGN KEY (message_id) REFERENCES engine.chat_messages(id) ON DELETE SET NULL NOT VALID;


--
-- Name: chat_vision_events chat_vision_events_session_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.chat_vision_events
    ADD CONSTRAINT chat_vision_events_session_id_fkey FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id) ON DELETE SET NULL NOT VALID;


--
-- Name: companion_affinity_events companion_affinity_events_affinity_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_affinity_events
    ADD CONSTRAINT companion_affinity_events_affinity_id_fkey FOREIGN KEY (affinity_id) REFERENCES engine.companion_affinity(id) ON DELETE CASCADE;


--
-- Name: companion_affinity_events companion_affinity_events_generation_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_affinity_events
    ADD CONSTRAINT companion_affinity_events_generation_id_fkey FOREIGN KEY (generation_id) REFERENCES engine.llm_generations(generation_id) ON DELETE SET NULL;


--
-- Name: companion_affinity_events companion_affinity_events_user_message_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_affinity_events
    ADD CONSTRAINT companion_affinity_events_user_message_id_fkey FOREIGN KEY (user_message_id) REFERENCES engine.chat_messages(id) ON DELETE SET NULL;


--
-- Name: companion_affinity companion_affinity_instance_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_affinity
    ADD CONSTRAINT companion_affinity_instance_id_fkey FOREIGN KEY (instance_id) REFERENCES engine.persona_instances(id) ON DELETE CASCADE;


--
-- Name: companion_affinity companion_affinity_session_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_affinity
    ADD CONSTRAINT companion_affinity_session_id_fkey FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id) ON DELETE CASCADE;


--
-- Name: companion_decision_events companion_decision_events_generation_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_decision_events
    ADD CONSTRAINT companion_decision_events_generation_id_fkey FOREIGN KEY (generation_id) REFERENCES engine.llm_generations(generation_id) ON DELETE SET NULL;


--
-- Name: companion_decision_events companion_decision_events_message_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_decision_events
    ADD CONSTRAINT companion_decision_events_message_id_fkey FOREIGN KEY (message_id) REFERENCES engine.chat_messages(id) ON DELETE SET NULL NOT VALID;


--
-- Name: companion_decision_events companion_decision_events_session_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_decision_events
    ADD CONSTRAINT companion_decision_events_session_id_fkey FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id) ON DELETE SET NULL NOT VALID;


--
-- Name: companion_insights_events companion_insights_events_generation_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_insights_events
    ADD CONSTRAINT companion_insights_events_generation_id_fkey FOREIGN KEY (generation_id) REFERENCES engine.llm_generations(generation_id) ON DELETE SET NULL;


--
-- Name: companion_insights_events companion_insights_events_message_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_insights_events
    ADD CONSTRAINT companion_insights_events_message_id_fkey FOREIGN KEY (message_id) REFERENCES engine.chat_messages(id) ON DELETE SET NULL NOT VALID;


--
-- Name: companion_insights_events companion_insights_events_session_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_insights_events
    ADD CONSTRAINT companion_insights_events_session_id_fkey FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id) ON DELETE SET NULL NOT VALID;


--
-- Name: companion_memories companion_memories_instance_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_memories
    ADD CONSTRAINT companion_memories_instance_id_fkey FOREIGN KEY (instance_id) REFERENCES engine.persona_instances(id) ON DELETE CASCADE;


--
-- Name: companion_memories companion_memories_session_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.companion_memories
    ADD CONSTRAINT companion_memories_session_id_fkey FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id) ON DELETE CASCADE;


--
-- Name: life_choices life_choices_event_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.life_choices
    ADD CONSTRAINT life_choices_event_id_fkey FOREIGN KEY (event_id) REFERENCES engine.persona_events(id) ON DELETE SET NULL;


--
-- Name: life_choices life_choices_message_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.life_choices
    ADD CONSTRAINT life_choices_message_id_fkey FOREIGN KEY (message_id) REFERENCES engine.chat_messages(id) ON DELETE SET NULL;


--
-- Name: life_choices life_choices_session_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.life_choices
    ADD CONSTRAINT life_choices_session_id_fkey FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id) ON DELETE CASCADE;


--
-- Name: llm_generations llm_generations_session_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.llm_generations
    ADD CONSTRAINT llm_generations_session_id_fkey FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id) ON DELETE SET NULL;


--
-- Name: persona_events persona_events_instance_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.persona_events
    ADD CONSTRAINT persona_events_instance_id_fkey FOREIGN KEY (instance_id) REFERENCES engine.persona_instances(id) ON DELETE CASCADE;


--
-- Name: persona_events persona_events_message_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.persona_events
    ADD CONSTRAINT persona_events_message_id_fkey FOREIGN KEY (message_id) REFERENCES engine.chat_messages(id) ON DELETE SET NULL;


--
-- Name: persona_events persona_events_session_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.persona_events
    ADD CONSTRAINT persona_events_session_id_fkey FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id) ON DELETE SET NULL;


--
-- Name: persona_instance_items persona_instance_items_instance_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.persona_instance_items
    ADD CONSTRAINT persona_instance_items_instance_id_fkey FOREIGN KEY (instance_id) REFERENCES engine.persona_instances(id) ON DELETE CASCADE;


--
-- Name: persona_instance_items persona_instance_items_user_message_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.persona_instance_items
    ADD CONSTRAINT persona_instance_items_user_message_id_fkey FOREIGN KEY (user_message_id) REFERENCES engine.chat_messages(id) ON DELETE SET NULL;


--
-- Name: persona_instances persona_instances_genome_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.persona_instances
    ADD CONSTRAINT persona_instances_genome_id_fkey FOREIGN KEY (genome_id) REFERENCES engine.persona_genomes(id) ON DELETE RESTRICT;


--
-- Name: persona_story_events persona_story_events_instance_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.persona_story_events
    ADD CONSTRAINT persona_story_events_instance_id_fkey FOREIGN KEY (instance_id) REFERENCES engine.persona_instances(id) ON DELETE CASCADE;


--
-- Name: persona_story_insights persona_story_insights_instance_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.persona_story_insights
    ADD CONSTRAINT persona_story_insights_instance_id_fkey FOREIGN KEY (instance_id) REFERENCES engine.persona_instances(id) ON DELETE CASCADE;


--
-- Name: persona_story_memories persona_story_memories_event_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.persona_story_memories
    ADD CONSTRAINT persona_story_memories_event_id_fkey FOREIGN KEY (event_id) REFERENCES engine.persona_story_events(id) ON DELETE CASCADE;


--
-- Name: persona_story_memories persona_story_memories_instance_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.persona_story_memories
    ADD CONSTRAINT persona_story_memories_instance_id_fkey FOREIGN KEY (instance_id) REFERENCES engine.persona_instances(id) ON DELETE CASCADE;


--
-- Name: user_insights_events user_insights_events_generation_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.user_insights_events
    ADD CONSTRAINT user_insights_events_generation_id_fkey FOREIGN KEY (generation_id) REFERENCES engine.llm_generations(generation_id) ON DELETE SET NULL;


--
-- Name: user_insights_events user_insights_events_message_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.user_insights_events
    ADD CONSTRAINT user_insights_events_message_id_fkey FOREIGN KEY (message_id) REFERENCES engine.chat_messages(id) ON DELETE SET NULL NOT VALID;


--
-- Name: user_insights_events user_insights_events_session_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.user_insights_events
    ADD CONSTRAINT user_insights_events_session_id_fkey FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id) ON DELETE SET NULL NOT VALID;


--
-- Name: user_insights user_insights_instance_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.user_insights
    ADD CONSTRAINT user_insights_instance_id_fkey FOREIGN KEY (instance_id) REFERENCES engine.persona_instances(id) ON DELETE CASCADE;


--
-- Name: world_memories world_memories_instance_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.world_memories
    ADD CONSTRAINT world_memories_instance_id_fkey FOREIGN KEY (instance_id) REFERENCES engine.persona_instances(id) ON DELETE CASCADE;


--
-- Name: world_post_comments world_post_comments_author_instance_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.world_post_comments
    ADD CONSTRAINT world_post_comments_author_instance_id_fkey FOREIGN KEY (author_instance_id) REFERENCES engine.persona_instances(id) ON DELETE CASCADE;


--
-- Name: world_post_comments world_post_comments_post_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.world_post_comments
    ADD CONSTRAINT world_post_comments_post_id_fkey FOREIGN KEY (post_id) REFERENCES engine.world_posts(id) ON DELETE CASCADE;


--
-- Name: world_posts world_posts_instance_id_fkey; Type: FK CONSTRAINT; Schema: engine; Owner: -
--

ALTER TABLE ONLY engine.world_posts
    ADD CONSTRAINT world_posts_instance_id_fkey FOREIGN KEY (instance_id) REFERENCES engine.persona_instances(id) ON DELETE CASCADE;


--
-- Name: character_insights; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.character_insights ENABLE ROW LEVEL SECURITY;

--
-- Name: character_insights_events; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.character_insights_events ENABLE ROW LEVEL SECURITY;

--
-- Name: character_insights_snapshot; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.character_insights_snapshot ENABLE ROW LEVEL SECURITY;

--
-- Name: chat_images_events; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.chat_images_events ENABLE ROW LEVEL SECURITY;

--
-- Name: chat_messages; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.chat_messages ENABLE ROW LEVEL SECURITY;

--
-- Name: chat_session_members; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.chat_session_members ENABLE ROW LEVEL SECURITY;

--
-- Name: chat_sessions; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.chat_sessions ENABLE ROW LEVEL SECURITY;

--
-- Name: chat_turn_queue; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.chat_turn_queue ENABLE ROW LEVEL SECURITY;

--
-- Name: chat_vision_events; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.chat_vision_events ENABLE ROW LEVEL SECURITY;

--
-- Name: companion_affinity; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.companion_affinity ENABLE ROW LEVEL SECURITY;

--
-- Name: companion_affinity_events; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.companion_affinity_events ENABLE ROW LEVEL SECURITY;

--
-- Name: companion_decision_events; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.companion_decision_events ENABLE ROW LEVEL SECURITY;

--
-- Name: companion_insights_events; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.companion_insights_events ENABLE ROW LEVEL SECURITY;

--
-- Name: companion_insights_snapshot; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.companion_insights_snapshot ENABLE ROW LEVEL SECURITY;

--
-- Name: companion_memories; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.companion_memories ENABLE ROW LEVEL SECURITY;

--
-- Name: error_handling_config; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.error_handling_config ENABLE ROW LEVEL SECURITY;

--
-- Name: human_insights; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.human_insights ENABLE ROW LEVEL SECURITY;

--
-- Name: human_insights_snapshot; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.human_insights_snapshot ENABLE ROW LEVEL SECURITY;

--
-- Name: life_choices; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.life_choices ENABLE ROW LEVEL SECURITY;

--
-- Name: llm_generations; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.llm_generations ENABLE ROW LEVEL SECURITY;

--
-- Name: persona_events; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.persona_events ENABLE ROW LEVEL SECURITY;

--
-- Name: persona_genomes; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.persona_genomes ENABLE ROW LEVEL SECURITY;

--
-- Name: persona_instance_items; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.persona_instance_items ENABLE ROW LEVEL SECURITY;

--
-- Name: persona_instances; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.persona_instances ENABLE ROW LEVEL SECURITY;

--
-- Name: persona_story_events; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.persona_story_events ENABLE ROW LEVEL SECURITY;

--
-- Name: persona_story_insights; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.persona_story_insights ENABLE ROW LEVEL SECURITY;

--
-- Name: persona_story_memories; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.persona_story_memories ENABLE ROW LEVEL SECURITY;

--
-- Name: user_insights; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.user_insights ENABLE ROW LEVEL SECURITY;

--
-- Name: user_insights_events; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.user_insights_events ENABLE ROW LEVEL SECURITY;

--
-- Name: user_insights_snapshot; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.user_insights_snapshot ENABLE ROW LEVEL SECURITY;

--
-- Name: world_enrollments; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.world_enrollments ENABLE ROW LEVEL SECURITY;

--
-- Name: world_memories; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.world_memories ENABLE ROW LEVEL SECURITY;

--
-- Name: world_post_comments; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.world_post_comments ENABLE ROW LEVEL SECURITY;

--
-- Name: world_posts; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.world_posts ENABLE ROW LEVEL SECURITY;

--
-- Name: world_states; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.world_states ENABLE ROW LEVEL SECURITY;

--
-- Name: world_worldviews; Type: ROW SECURITY; Schema: engine; Owner: -
--

ALTER TABLE engine.world_worldviews ENABLE ROW LEVEL SECURITY;

--
-- PostgreSQL database dump complete
--


--
-- Supabase lockdown, carried from the folded chain (0013/0016 and the
-- per-table blocks): REVOKE the PostgREST roles where they exist, and hide
-- the schema from them. RLS enables are table properties and appear above.
--

DO $baseline_lockdown$
DECLARE t regclass;
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        FOR t IN SELECT c.oid::regclass FROM pg_class c
                   JOIN pg_namespace n ON n.oid = c.relnamespace
                  WHERE n.nspname = 'engine' AND c.relkind = 'r' LOOP
            EXECUTE format('REVOKE ALL ON %s FROM anon', t);
        END LOOP;
        REVOKE USAGE ON SCHEMA engine FROM anon;
        REVOKE ALL ON public._sqlx_migrations FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        FOR t IN SELECT c.oid::regclass FROM pg_class c
                   JOIN pg_namespace n ON n.oid = c.relnamespace
                  WHERE n.nspname = 'engine' AND c.relkind = 'r' LOOP
            EXECUTE format('REVOKE ALL ON %s FROM authenticated', t);
        END LOOP;
        REVOKE USAGE ON SCHEMA engine FROM authenticated;
        REVOKE ALL ON public._sqlx_migrations FROM authenticated;
    END IF;
END
$baseline_lockdown$;

-- public._sqlx_migrations exists before any migration runs (sqlx creates
-- it), and 0016 locked it down; RLS on it is carried here.
ALTER TABLE public._sqlx_migrations ENABLE ROW LEVEL SECURITY;

--
-- The one constant seed in the folded chain (0020): the chat-stream failure
-- fallback phrases. Everything else data-touching in 0000..0077 was a no-op
-- on an empty database.
--
INSERT INTO engine.error_handling_config (kind, payload) VALUES
  ('chat_stream_failure_fallback_phrases',
   '["huh?","hm?","...","oh?","mhm","ok","👀","😅","say again?","wait what?"]'::jsonb);
