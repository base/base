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

/// Client for communicating with the Bundler User Operation Inclusion Service
#[derive(Debug, Clone)]
pub struct TipsClient {
    provider: RootProvider<Optimism>,
}

impl TipsClient {
    pub fn new(tips_url: Url) -> Self {
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Optimism>()
            .connect_http(tips_url);

        Self { provider }
    }

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