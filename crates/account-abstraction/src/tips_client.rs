//! Tips Client for communicating with the Tips Ingress Service
//!
//! This module provides a lightweight client for sending user operations
//! to the Tips ingress service via JSON-RPC.

use alloy_primitives::{Address, B256};
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use eyre::Result;
use op_alloy_network::Optimism;
use url::Url;

use crate::UserOperation;

/// Client for communicating with the Tips Ingress Service
#[derive(Debug, Clone)]
pub struct TipsClient {
    provider: RootProvider<Optimism>,
}

impl TipsClient {
    /// Creates a new Tips client connected to the specified URL
    ///
    /// # Arguments
    /// * `tips_url` - The URL of the Tips ingress service
    ///
    /// # Example
    /// ```no_run
    /// use base_reth_account_abstraction::TipsClient;
    /// use url::Url;
    ///
    /// let url = Url::parse("http://localhost:8080").unwrap();
    /// let client = TipsClient::new(url);
    /// ```
    pub fn new(tips_url: Url) -> Self {
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Optimism>()
            .connect_http(tips_url);

        Self { provider }
    }

    /// Sends a user operation to the Tips ingress service
    ///
    /// # Arguments
    /// * `user_operation` - The user operation to send (supports both v0.6 and v0.7+)
    /// * `entry_point` - The entry point contract address
    ///
    /// # Returns
    /// The user operation hash if successful
    ///
    /// # Example
    /// ```no_run
    /// use base_reth_account_abstraction::{TipsClient, UserOperation, UserOperationV06};
    /// use alloy_primitives::{Address, Bytes, U256};
    /// use url::Url;
    ///
    /// # async fn example() -> eyre::Result<()> {
    /// let url = Url::parse("http://localhost:8080")?;
    /// let client = TipsClient::new(url);
    ///
    /// let user_op = UserOperation::V06(UserOperationV06 {
    ///     sender: Address::ZERO,
    ///     nonce: U256::from(0),
    ///     init_code: Bytes::new(),
    ///     call_data: Bytes::new(),
    ///     call_gas_limit: U256::from(100000),
    ///     verification_gas_limit: U256::from(100000),
    ///     pre_verification_gas: U256::from(21000),
    ///     max_fee_per_gas: U256::from(1000000000),
    ///     max_priority_fee_per_gas: U256::from(1000000000),
    ///     paymaster_and_data: Bytes::new(),
    ///     signature: Bytes::new(),
    /// });
    ///
    /// let entry_point = Address::ZERO;
    /// let user_op_hash = client.send_user_operation(user_op, entry_point).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn send_user_operation(
        &self,
        user_operation: UserOperation,
        entry_point: Address,
    ) -> Result<B256> {
        let user_op_hash = self
            .provider
            .client()
            .request("eth_sendUserOperation", (user_operation, entry_point))
            .await?;

        Ok(user_op_hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UserOperationV06;
    use alloy_primitives::{Bytes, U256};

    #[test]
    fn test_client_creation() {
        let url = Url::parse("http://localhost:8080").unwrap();
        let _client = TipsClient::new(url);
    }

    // Integration test - requires running tips service
    #[tokio::test]
    #[ignore]
    async fn test_send_user_operation() -> Result<()> {
        let url = Url::parse("http://localhost:8080")?;
        let client = TipsClient::new(url);

        let user_op = UserOperation::V06(UserOperationV06 {
            sender: Address::ZERO,
            nonce: U256::from(0),
            init_code: Bytes::new(),
            call_data: Bytes::new(),
            call_gas_limit: U256::from(100000),
            verification_gas_limit: U256::from(100000),
            pre_verification_gas: U256::from(21000),
            max_fee_per_gas: U256::from(1000000000),
            max_priority_fee_per_gas: U256::from(1000000000),
            paymaster_and_data: Bytes::new(),
            signature: Bytes::new(),
        });

        let entry_point = Address::ZERO;
        let user_op_hash = client.send_user_operation(user_op, entry_point).await?;

        assert_ne!(user_op_hash, B256::ZERO);

        Ok(())
    }
}

