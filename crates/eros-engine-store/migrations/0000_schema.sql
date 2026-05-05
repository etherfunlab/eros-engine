-- SPDX-License-Identifier: AGPL-3.0-only
-- Dedicated Postgres schema for eros-engine. Lives alongside eros-gateway's
-- public schema in the same Supabase project, so user identity (auth.users +
-- JWT realm) is shared while table names stay isolated.
CREATE SCHEMA IF NOT EXISTS engine;
