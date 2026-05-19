//! `PolicyRegistry` — the global singleton policy precompile interface.

/// Outbound port: the global policy registry precompile.
pub trait PolicyRegistry {}

///
/// Use this as a placeholder
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOpPolicyRegistry;

impl PolicyRegistry for NoOpPolicyRegistry {}
