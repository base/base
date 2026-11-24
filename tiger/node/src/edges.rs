//! Optional edge coverage from `SanitizerCoverage` inline 8-bit counters (the `edge-coverage`
//! feature, built with `sancov-rustc.sh`).

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

/// Start of the counter array the instrumented crates registered.
static START: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
/// Number of counters.
static LEN: AtomicUsize = AtomicUsize::new(0);

/// Called by the constructors `SanitizerCoverage` emits into every instrumented module, with the
/// bounds of the binary's merged counter section.
///
/// # Safety
///
/// `start..stop` must be the counter section, valid for the life of the process.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __sanitizer_cov_8bit_counters_init(start: *mut u8, stop: *mut u8) {
    let len = (stop as usize).saturating_sub(start as usize);
    if len > LEN.load(Ordering::Relaxed) {
        START.store(start, Ordering::Relaxed);
        LEN.store(len, Ordering::Relaxed);
    }
}

/// Edge counters of the instrumented crates (`SANCOV_CRATES`).
#[derive(Debug, Clone, Copy)]
pub struct EdgeCounters;

impl EdgeCounters {
    fn counters() -> &'static mut [u8] {
        let (start, len) = (START.load(Ordering::Relaxed), LEN.load(Ordering::Relaxed));
        if start.is_null() {
            return &mut [];
        }
        // SAFETY: the instrumentation registered `start..start + len` as its counter section,
        // which lives for the whole process. The adapter is single-threaded; in the
        // node, other threads increment counters without synchronization (as `SanitizerCoverage`
        // always does), so readings are exact only for one request at a time.
        unsafe { std::slice::from_raw_parts_mut(start, len) }
    }

    /// Number of instrumented edges.
    pub fn len() -> usize {
        Self::counters().len()
    }

    /// Clears the counters before a request.
    pub fn reset() {
        Self::counters().fill(0);
    }

    /// `coverage` with an `tiger:edge:<index>` feature for every edge hit since the last reset, with
    /// its (8-bit, wrapping) hit count.
    pub fn with_edges(mut coverage: BTreeMap<String, u64>) -> BTreeMap<String, u64> {
        for (index, count) in Self::hits() {
            coverage.insert(format!("tiger:edge:{index}"), u64::from(count));
        }
        coverage
    }

    /// Every edge hit since the last reset, as `(index, count)` in index order.
    pub fn hits() -> Vec<(usize, u8)> {
        Self::counters().iter().enumerate().filter(|(_, c)| **c > 0).map(|(i, c)| (i, *c)).collect()
    }
}
