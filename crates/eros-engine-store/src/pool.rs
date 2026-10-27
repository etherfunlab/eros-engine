// SPDX-License-Identifier: AGPL-3.0-only
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgPool};
use std::time::Duration;

/// Idle age past which a pooled connection is pinged before being handed out.
///
/// A blanket ping on every acquire (sqlx's `test_before_acquire` default)
/// doubles the effective round trips of every single-query call, which on a
/// cross-region deployment doubles the database tax of a whole chat turn. A
/// connection that was in use moments ago is as alive as a ping would prove
/// it to be — a mid-traffic death surfaces on the query itself either way —
/// so only a connection idle long enough to be suspect pays the probe.
const PING_AFTER_IDLE: Duration = Duration::from_secs(60);

/// Pool options with the conditional ping installed; `build` finishes them.
/// The threshold is a parameter so tests can exercise both sides of it.
fn pool_options(ping_after_idle: Duration) -> PgPoolOptions {
    PgPoolOptions::new()
        .test_before_acquire(false)
        .before_acquire(move |conn, meta| {
            Box::pin(async move {
                if meta.idle_for >= ping_after_idle && conn.ping().await.is_err() {
                    // Discard silently; the pool hands out another connection.
                    return Ok(false);
                }
                Ok(true)
            })
        })
}

pub async fn build(database_url: &str) -> Result<PgPool, sqlx::Error> {
    pool_options(PING_AFTER_IDLE)
        .max_connections(20)
        .acquire_timeout(Duration::from_secs(5))
        .connect(database_url)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;

    /// A one-connection pool to the test server with the given ping threshold.
    async fn one_conn_pool(ping_after_idle: Duration) -> PgPool {
        pool_options(ping_after_idle)
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL set by the test env"))
            .await
            .unwrap()
    }

    /// The pooled connection's server-side pid, acquired and released.
    async fn backend_pid(pool: &PgPool) -> i32 {
        sqlx::query("SELECT pg_backend_pid()")
            .fetch_one(pool)
            .await
            .unwrap()
            .get(0)
    }

    #[sqlx::test]
    async fn hot_connection_is_handed_out_without_a_ping(killer: PgPool) {
        // Threshold far above the test's pace: the connection is always "hot".
        let pool = one_conn_pool(Duration::from_secs(600)).await;
        let pid = backend_pid(&pool).await;
        sqlx::query("SELECT pg_terminate_backend($1)")
            .bind(pid)
            .execute(&killer)
            .await
            .unwrap();

        // No ping saves this query: the death surfaces on the query itself.
        // That is the chosen trade — a hot connection's liveness is not worth
        // a round trip per acquire.
        let res = sqlx::query("SELECT 1").fetch_one(&pool).await;
        assert!(
            res.is_err(),
            "a just-killed hot connection errors the query"
        );
    }

    #[sqlx::test]
    async fn stale_idle_connection_is_pinged_and_replaced(killer: PgPool) {
        let pool = one_conn_pool(Duration::from_millis(100)).await;
        let pid = backend_pid(&pool).await;
        sqlx::query("SELECT pg_terminate_backend($1)")
            .bind(pid)
            .execute(&killer)
            .await
            .unwrap();
        // Let the connection sit past the threshold so the next acquire pings.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let one: i32 = sqlx::query("SELECT 1")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get(0);
        assert_eq!(one, 1, "the dead connection is discarded, not handed out");
        assert_ne!(backend_pid(&pool).await, pid, "a fresh connection serves");
    }
}
