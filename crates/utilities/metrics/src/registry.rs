//! Registry for macro-generated metric initializers.
//!
//! `define_metrics!` registers each generated `init()` function here from a
//! startup constructor. The binary calls `initialize_registered_metrics()`
//! after installing the recorder so describes/zeroing happen against the live
//! backend instead of being dropped during process startup.

use std::sync::{Mutex, Once, OnceLock};

type Initializer = fn();

static INITIALIZERS: OnceLock<Mutex<Vec<Initializer>>> = OnceLock::new();
static INITIALIZE_ONCE: Once = Once::new();

#[inline]
fn initializers() -> &'static Mutex<Vec<Initializer>> {
    INITIALIZERS.get_or_init(|| Mutex::new(Vec::new()))
}

#[doc(hidden)]
pub fn register_initializer(initializer: Initializer) {
    initializers().lock().expect("metrics initializer registry poisoned").push(initializer);
}

/// Runs all statically-registered metric initializers exactly once.
///
/// This should be called immediately after the global metrics recorder is installed.
/// Any initializers registered after the first call are intentionally ignored.
/// Subsequent calls are also no-ops, even if they happen under a different
/// local recorder in tests.
pub fn initialize_registered_metrics() {
    INITIALIZE_ONCE.call_once(|| {
        let initializers = initializers().lock().expect("metrics initializer registry poisoned");
        for initializer in initializers.iter().copied() {
            initializer();
        }
    });
}
