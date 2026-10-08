// SPDX-License-Identifier: AGPL-3.0-only
//! Bring-your-own-key chat turns (spec
//! docs/superpowers/specs/2026-10-08-byok-chat-design.md): the request
//! field, its admission and validation, the per-turn client, and the reply
//! chain's hop list.

use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use uuid::Uuid;

/// Process-wide BYOK state: the guarded HTTP client (spec §7.2) and the
/// per-user round-robin cursors (§6.1). Cursors live per process and reset
/// on restart, like config round-robin.
#[derive(Clone)]
pub(crate) struct ByokRuntime {
    http: reqwest::Client,
    cursors: Arc<Mutex<HashMap<Uuid, Arc<AtomicUsize>>>>,
}

impl ByokRuntime {
    pub(crate) fn new(allow_private_network: bool) -> Self {
        Self {
            http: eros_engine_llm::byok::build_http(allow_private_network),
            cursors: Arc::default(),
        }
    }

    /// This user's round-robin cursor, created at 0 on first use.
    pub(crate) fn cursor(&self, user_id: Uuid) -> Arc<AtomicUsize> {
        self.cursors
            .lock()
            .unwrap()
            .entry(user_id)
            .or_default()
            .clone()
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn cursors_are_per_user() {
        let rt = ByokRuntime::new(false);
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        rt.cursor(a).fetch_add(1, Ordering::Relaxed);
        assert_eq!(rt.cursor(a).load(Ordering::Relaxed), 1);
        assert_eq!(rt.cursor(b).load(Ordering::Relaxed), 0);
    }
}
