/// Validation behavior for Base execution and maintenance.
#[derive(Debug, Clone)]
pub enum ValidationMode {
    /// Validate Base consensus rules.
    Base,
    /// Skip checks when replaying existing data for maintenance.
    Skip,
    /// Controllable failures for networking fixtures.
    #[cfg(any(test, feature = "test-utils"))]
    Test(alloc::sync::Arc<crate::TestConsensus>),
    /// Shared Ethereum execution rules for storage fixtures.
    #[cfg(any(test, feature = "test-utils"))]
    EthereumTest(crate::EthereumTestConsensus),
}
