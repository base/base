//! Sequencer timing defaults for subsecond block production.

/// The default fixed offset into a subsecond slot at which the sequencer requests the
/// sealed payload (`engine_getPayload`) once Cobalt is active.
///
/// This is the single shared default for both timing knobs derived from it: the CL's block
/// seal target and the builder's wall-clock transaction cutoff. The two run in separate
/// processes, so each defaults from this constant; a mismatch shows up as every
/// `getPayload` waiting on an unfinished build.
pub const DEFAULT_SEAL_OFFSET: core::time::Duration = core::time::Duration::from_millis(150);
