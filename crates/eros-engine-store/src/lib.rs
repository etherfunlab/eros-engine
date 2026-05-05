// SPDX-License-Identifier: AGPL-3.0-only
//! Postgres + pgvector persistence layer.

pub mod affinity;
pub mod chat;
pub mod insight;
pub mod memory;
pub mod persona;
pub mod pool;

pub use sqlx::PgPool;
