//! Opt-in profiling helpers for pending-state hot paths.

use std::sync::OnceLock;

pub(crate) fn pending_state_profiling_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();

    *ENABLED.get_or_init(|| {
        std::env::var_os("FLASHBLOCKS_PROFILE_PENDING_STATE")
            .is_some_and(|value| !value.is_empty() && value != "0")
    })
}
