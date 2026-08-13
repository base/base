//! Authenticated Engine API support for constructing replacement L1 branches.

use std::time::Duration;

use alloy_eips::eip1898::BlockNumberOrTag;
use alloy_network::Ethereum;
use alloy_primitives::{Address, B256, keccak256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV5, ForkchoiceState, ForkchoiceUpdated, JwtSecret, PayloadAttributes,
    PayloadStatus, PayloadStatusEnum,
};
use eyre::{OptionExt, Result, WrapErr, ensure};
use jsonrpsee::{core::client::ClientT, http_client::HttpClientBuilder, rpc_params};
use reth_rpc_layer::AuthClientLayer;
use tower::ServiceBuilder;
use url::Url;

/// Description of a replacement branch installed through the Engine API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct L1ReplacementBranch {
    /// Hash of the last block shared with the old canonical branch.
    pub common_ancestor: B256,
    /// Hashes of the replacement blocks in ascending block-number order.
    pub replacement_hashes: Vec<B256>,
}

impl L1ReplacementBranch {
    /// Returns the replacement head hash.
    pub fn head(&self) -> Option<B256> {
        self.replacement_hashes.last().copied()
    }
}

/// Driver for constructing an empty canonical L1 branch through an authenticated Engine API.
#[derive(Clone, Debug)]
pub struct L1ReorgDriver {
    rpc_url: Url,
    engine_url: Url,
    jwt_secret: JwtSecret,
}

impl L1ReorgDriver {
    /// Creates a replacement-branch driver.
    pub fn new(rpc_url: Url, engine_url: Url, jwt_secret_hex: &str) -> Result<Self> {
        let jwt_secret =
            JwtSecret::from_hex(jwt_secret_hex).wrap_err("Invalid L1 Engine API JWT secret")?;
        Ok(Self { rpc_url, engine_url, jwt_secret })
    }

    /// Builds empty replacement blocks from the current head through `target_number`.
    ///
    /// Reth must already be unwound to the intended common ancestor, and `first_timestamp` must be
    /// later than every block timestamp from the branch that was removed.
    pub async fn build_replacement(
        &self,
        target_number: u64,
        first_timestamp: u64,
        block_time: u64,
    ) -> Result<L1ReplacementBranch> {
        ensure!(block_time > 0, "replacement L1 block time must be nonzero");

        let provider = RootProvider::<Ethereum>::new_http(self.rpc_url.clone());
        let ancestor = provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .wrap_err("Failed to fetch replacement branch ancestor")?
            .ok_or_eyre("Replacement branch ancestor is missing")?;
        ensure!(
            ancestor.header.number < target_number,
            "replacement target must be above common ancestor"
        );

        let safe = provider
            .get_block_by_number(BlockNumberOrTag::Safe)
            .await
            .wrap_err("Failed to fetch L1 safe block")?;
        let finalized = provider
            .get_block_by_number(BlockNumberOrTag::Finalized)
            .await
            .wrap_err("Failed to fetch L1 finalized block")?;
        ensure!(
            safe.as_ref().is_none_or(|block| block.header.number <= ancestor.header.number),
            "cannot replace an L1 branch below the safe block"
        );
        ensure!(
            finalized.as_ref().is_none_or(|block| block.header.number <= ancestor.header.number),
            "cannot replace an L1 branch below the finalized block"
        );

        let auth_layer = AuthClientLayer::new(self.jwt_secret);
        let middleware = ServiceBuilder::new().layer(auth_layer);
        let client = HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(15))
            .set_http_middleware(middleware)
            .build(self.engine_url.as_str())
            .wrap_err("Failed to build authenticated L1 Engine API client")?;

        let common_ancestor = ancestor.header.hash;
        let mut forkchoice = ForkchoiceState {
            head_block_hash: common_ancestor,
            safe_block_hash: safe.as_ref().map_or(common_ancestor, |block| block.header.hash),
            finalized_block_hash: finalized
                .as_ref()
                .map_or(common_ancestor, |block| block.header.hash),
        };
        let minimum_timestamp = ancestor
            .header
            .timestamp
            .checked_add(block_time)
            .ok_or_eyre("replacement L1 timestamp overflowed")?;
        let mut timestamp = first_timestamp.max(minimum_timestamp);
        let mut replacement_hashes = Vec::with_capacity(
            usize::try_from(target_number - ancestor.header.number)
                .wrap_err("Replacement branch length does not fit in memory")?,
        );

        for number in (ancestor.header.number + 1)..=target_number {
            let parent_beacon_block_root =
                keccak256(format!("system-test-replacement-parent-{number}"));
            let attributes = PayloadAttributes {
                timestamp,
                prev_randao: keccak256(format!("system-test-replacement-randao-{number}")),
                suggested_fee_recipient: Address::ZERO,
                withdrawals: Some(Vec::new()),
                parent_beacon_block_root: Some(parent_beacon_block_root),
                ..Default::default()
            };
            let started: ForkchoiceUpdated = client
                .request("engine_forkchoiceUpdatedV3", rpc_params![forkchoice, attributes])
                .await
                .wrap_err_with(|| format!("Failed to start replacement L1 block {number}"))?;
            Self::ensure_payload_valid(&started.payload_status, number, "start build")?;
            let payload_id = started.payload_id.ok_or_eyre("Engine did not return a payload ID")?;

            tokio::time::sleep(Duration::from_millis(250)).await;
            let envelope: ExecutionPayloadEnvelopeV5 = client
                .request("engine_getPayloadV5", rpc_params![payload_id])
                .await
                .wrap_err_with(|| format!("Failed to retrieve replacement L1 block {number}"))?;
            let payload = envelope.execution_payload;
            let block_hash = payload.payload_inner.payload_inner.block_hash;
            let payload_number = payload.payload_inner.payload_inner.block_number;
            ensure!(payload_number == number, "Engine built unexpected L1 block {payload_number}");

            let status: PayloadStatus = client
                .request(
                    "engine_newPayloadV4",
                    rpc_params![
                        payload,
                        Vec::<B256>::new(),
                        parent_beacon_block_root,
                        envelope.execution_requests
                    ],
                )
                .await
                .wrap_err_with(|| format!("Failed to submit replacement L1 block {number}"))?;
            Self::ensure_payload_valid(&status, number, "submit payload")?;

            forkchoice.head_block_hash = block_hash;
            let selected: ForkchoiceUpdated = client
                .request(
                    "engine_forkchoiceUpdatedV3",
                    rpc_params![forkchoice, Option::<PayloadAttributes>::None],
                )
                .await
                .wrap_err_with(|| format!("Failed to select replacement L1 block {number}"))?;
            Self::ensure_payload_valid(&selected.payload_status, number, "select forkchoice")?;

            replacement_hashes.push(block_hash);
            timestamp = timestamp
                .checked_add(block_time)
                .ok_or_eyre("replacement L1 timestamp overflowed")?;
        }

        Ok(L1ReplacementBranch { common_ancestor, replacement_hashes })
    }

    /// Requires the Engine API to accept a replacement-branch operation.
    pub fn ensure_payload_valid(
        status: &PayloadStatus,
        number: u64,
        operation: &str,
    ) -> Result<()> {
        ensure!(
            status.status == PayloadStatusEnum::Valid,
            "Engine failed to {operation} for L1 block {number}: {:?}",
            status.status
        );
        Ok(())
    }
}
