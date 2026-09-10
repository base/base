use async_trait::async_trait;
use base_common_client_ethereum::{Base, Ethereum, Provider};

/// Mock L1 Provider that implements the Provider trait for testing.
///
/// This is a minimal no-op provider that satisfies the trait bounds required
/// by [`MockEngineClient`]. All provider methods return empty/default values.
#[derive(Debug, Clone)]
pub struct MockL1Provider;

#[async_trait]
impl Provider<Ethereum> for MockL1Provider {
    fn root(&self) -> &base_common_client_ethereum::RootProvider<Ethereum> {
        unimplemented!("MockL1Provider does not support root()")
    }
}

/// Mock L2 Provider that implements the Provider trait for Base network.
///
/// This is a minimal no-op provider that satisfies the trait bounds required
/// by [`MockEngineClient`]. All provider methods return empty/default values.
#[derive(Debug, Clone)]
pub struct MockL2Provider;

#[async_trait]
impl Provider<Base> for MockL2Provider {
    fn root(&self) -> &base_common_client_ethereum::RootProvider<Base> {
        unimplemented!("MockL2Provider does not support root()")
    }
}
