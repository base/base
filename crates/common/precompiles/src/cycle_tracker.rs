//! Shared cycle tracker traits for native precompiles.

/// Cycle tracker implementation for native precompiles with a single tracked operation.
pub trait PrecompileCycleTracker {
    /// The SP1 cycle tracker key for this precompile.
    const KEY: &'static str;
}

/// Resolves multifunction precompile calldata into SP1 cycle tracker keys.
pub trait CalldataCycleTracker {
    /// Returns the SP1 cycle tracker key for calldata.
    fn key_for_calldata(calldata: &[u8]) -> Option<&'static str>;
}

impl<T> CalldataCycleTracker for T
where
    T: PrecompileCycleTracker,
{
    fn key_for_calldata(_calldata: &[u8]) -> Option<&'static str> {
        Some(T::KEY)
    }
}
