use std::fmt::Debug;

use alloy_consensus::Block;
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{B256, FixedBytes};
use alloy_provider::{Provider, RootProvider};
use async_trait::async_trait;
use base_common_consensus::BaseTxEnvelope;
use base_common_network::Base;
use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
use base_protocol::BlockInfo;
use thiserror::Error;
use url::Url;

/// Error type for [`RemoteL2Client`] operations.
#[derive(Debug, Error)]
pub enum RemoteL2ClientError {
    /// Failed to fetch block from L2 EL.
    #[error("failed to fetch block at {tag}: {source}")]
    FetchBlock {
        /// The block tag that was requested.
        tag: String,
        /// The underlying transport error.
        source: alloy_transport::TransportError,
    },

    /// Block not found at the requested tag.
    #[error("block not found at {0}")]
    BlockNotFound(String),

    /// Failed to fetch the detached BLS aggregate of a block with native BLS transfers.
    #[error("failed to fetch BLS aggregate of block {hash}: {source}")]
    FetchBlsAggregate {
        /// Hash of the block whose aggregate was requested.
        hash: B256,
        /// The underlying transport error.
        source: alloy_transport::TransportError,
    },

    /// The source has no BLS aggregate for a block with native BLS transfers, so the block
    /// cannot be authorized for insertion.
    ///
    /// Expected whenever the source imported the block through L1 derivation: derivation
    /// authenticates a whole chunk under one re-aggregated signature, from which no per-block
    /// aggregate can be recovered.
    #[error("source has no BLS aggregate for intent block {hash}")]
    MissingBlsAggregate {
        /// Hash of the intent block.
        hash: B256,
    },
}

/// Trait for fetching L2 block data from the remote node.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait RemoteClient: Debug + Send + Sync {
    /// Fetches the block number at the given tag.
    async fn get_block_number(&self, tag: BlockNumberOrTag) -> Result<u64, RemoteL2ClientError>;

    /// Fetches the block info at the given tag.
    async fn get_block_info(&self, tag: BlockNumberOrTag)
    -> Result<BlockInfo, RemoteL2ClientError>;

    /// Fetches a block by number and converts it to an [`BaseExecutionPayloadEnvelope`].
    async fn get_payload_by_number(
        &self,
        number: u64,
    ) -> Result<BaseExecutionPayloadEnvelope, RemoteL2ClientError>;
}

/// Client that polls a source L2 execution layer node for block data and
/// converts blocks into [`BaseExecutionPayloadEnvelope`] for engine insertion.
///
/// Blocks with native BLS transfers also need their detached block aggregate, fetched with
/// [`Self::BLS_BLOCK_AGGREGATE_METHOD`]. The source must therefore be the producing sequencer or a
/// node that imported the block as an unsafe payload and retained its witness. A source that only
/// derived the block from L1 cannot serve it, and following it fails closed: the follower retries
/// that block and never advances past it without a verifiable witness.
#[derive(Debug, Clone)]
pub struct RemoteL2Client {
    provider: RootProvider<Base>,
}

impl RemoteL2Client {
    /// Source EL method returning a block's detached BLS aggregate by block hash, or `null`.
    pub const BLS_BLOCK_AGGREGATE_METHOD: &'static str = "base_blsBlockAggregate";

    /// Creates a new [`RemoteL2Client`] from a source L2 node URL.
    pub fn new(url: Url) -> Self {
        let provider = RootProvider::<Base>::new_http(url);
        Self { provider }
    }

    /// Fetches the aggregate authorizing the native BLS transfers of block `hash`.
    ///
    /// A `null` response is an error rather than a missing optional field: inserting an intent
    /// block without its witness would fail anyway, so the caller retries instead of advancing.
    /// See [`RemoteL2ClientError::MissingBlsAggregate`] for when a source returns `null`.
    pub async fn bls_block_aggregate(
        &self,
        hash: B256,
    ) -> Result<FixedBytes<48>, RemoteL2ClientError> {
        self.provider
            .raw_request::<_, Option<FixedBytes<48>>>(
                Self::BLS_BLOCK_AGGREGATE_METHOD.into(),
                (hash,),
            )
            .await
            .map_err(|source| RemoteL2ClientError::FetchBlsAggregate { hash, source })?
            .ok_or(RemoteL2ClientError::MissingBlsAggregate { hash })
    }
}

#[async_trait]
impl RemoteClient for RemoteL2Client {
    async fn get_block_number(&self, tag: BlockNumberOrTag) -> Result<u64, RemoteL2ClientError> {
        if matches!(tag, BlockNumberOrTag::Latest) {
            return self.provider.get_block_number().await.map_err(|e| {
                RemoteL2ClientError::FetchBlock { tag: format!("{tag:?}"), source: e }
            });
        }

        self.get_block_info(tag).await.map(|block| block.number)
    }

    async fn get_block_info(
        &self,
        tag: BlockNumberOrTag,
    ) -> Result<BlockInfo, RemoteL2ClientError> {
        let block = self
            .provider
            .get_block_by_number(tag)
            .await
            .map_err(|e| RemoteL2ClientError::FetchBlock { tag: format!("{tag:?}"), source: e })?
            .ok_or_else(|| RemoteL2ClientError::BlockNotFound(format!("{tag:?}")))?;
        let block = block.map_header(|header| header.into_inner());

        Ok(BlockInfo::from(&block))
    }

    async fn get_payload_by_number(
        &self,
        number: u64,
    ) -> Result<BaseExecutionPayloadEnvelope, RemoteL2ClientError> {
        let rpc_block = self
            .provider
            .get_block_by_number(number.into())
            .full()
            .await
            .map_err(|e| RemoteL2ClientError::FetchBlock { tag: format!("{number}"), source: e })?
            .ok_or_else(|| RemoteL2ClientError::BlockNotFound(format!("{number}")))?;
        let rpc_block = rpc_block.map_header(|header| header.into_inner());

        let block_hash = rpc_block.header.hash;
        let parent_beacon_block_root = rpc_block.header.parent_beacon_block_root;

        let txs: Vec<BaseTxEnvelope> = rpc_block
            .transactions
            .into_transactions()
            .map(|tx| tx.inner.inner.into_inner())
            .collect();

        let consensus_block: Block<BaseTxEnvelope> = Block {
            header: rpc_block.header.inner,
            body: alloy_consensus::BlockBody {
                transactions: txs,
                ommers: vec![],
                withdrawals: rpc_block.withdrawals,
            },
        };

        // Fetched by the hash of this exact block, so a source reorg cannot pair the
        // transactions with another block's aggregate.
        let bls_aggregate =
            if consensus_block.body.transactions.iter().any(BaseTxEnvelope::is_bls_transfer) {
                Some(self.bls_block_aggregate(block_hash).await?)
            } else {
                None
            };

        let (execution_payload, _sidecar) =
            BaseExecutionPayload::from_block_unchecked(block_hash, &consensus_block);

        Ok(BaseExecutionPayloadEnvelope {
            parent_beacon_block_root,
            execution_payload,
            bls_aggregate,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{
        Header, Sealable, SignableTransaction, TxEip1559, transaction::Recovered,
    };
    use alloy_primitives::{Address, Signature, U256};
    use alloy_rpc_client::RpcClient;
    use alloy_rpc_types_eth::BlockTransactions;
    use alloy_transport::mock::Asserter;
    use base_common_consensus::{BaseTransactionInfo, TxBlsTransfer};
    use base_common_rpc_types::{BaseBlockResponse, BaseHeaderResponse, Transaction};

    use super::*;

    const NUMBER: u64 = 7;

    fn client(asserter: &Asserter) -> RemoteL2Client {
        RemoteL2Client { provider: RootProvider::new(RpcClient::mocked(asserter.clone())) }
    }

    /// Serves `eth_getBlockByNumber` with a full block of `txs` and returns the block hash.
    fn push_block(asserter: &Asserter, txs: Vec<BaseTxEnvelope>) -> B256 {
        let header = Header { number: NUMBER, ..Default::default() }.seal_slow();
        let hash = header.hash();
        let transactions = txs
            .into_iter()
            .map(|tx| {
                Transaction::from_transaction(
                    Recovered::new_unchecked(tx, Address::repeat_byte(0x11)),
                    BaseTransactionInfo::default(),
                )
            })
            .collect();
        let block: BaseBlockResponse = alloy_rpc_types_eth::Block::new(
            BaseHeaderResponse::new(alloy_rpc_types_eth::Header::from_consensus(
                header, None, None,
            )),
            BlockTransactions::Full(transactions),
        );
        asserter.push_success(&block);
        hash
    }

    fn intent() -> BaseTxEnvelope {
        TxBlsTransfer {
            chain_id: 8453,
            sender: Address::repeat_byte(0x11),
            nonce: 0,
            recipient: Address::repeat_byte(0x22),
            value: U256::from(1),
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: 2,
        }
        .into()
    }

    fn ordinary() -> BaseTxEnvelope {
        TxEip1559 { chain_id: 8453, gas_limit: 21_000, ..Default::default() }
            .into_signed(Signature::test_signature())
            .into()
    }

    #[tokio::test]
    async fn intent_block_carries_source_aggregate() {
        let asserter = Asserter::new();
        let hash = push_block(&asserter, vec![ordinary(), intent()]);
        let aggregate = FixedBytes::<48>::repeat_byte(0x81);
        asserter.push_success(&Some(aggregate));

        let envelope = client(&asserter).get_payload_by_number(NUMBER).await.unwrap();
        assert_eq!(envelope.bls_aggregate, Some(aggregate));
        assert_eq!(envelope.execution_payload.block_hash(), hash);
        assert_eq!(envelope.execution_payload.transactions().len(), 2);
    }

    #[tokio::test]
    async fn intent_block_without_aggregate_fails_instead_of_omitting_it() {
        let asserter = Asserter::new();
        let hash = push_block(&asserter, vec![intent()]);
        asserter.push_success(&Option::<FixedBytes<48>>::None);

        let err = client(&asserter).get_payload_by_number(NUMBER).await.unwrap_err();
        assert!(
            matches!(err, RemoteL2ClientError::MissingBlsAggregate { hash: missing } if missing == hash),
            "{err:?}"
        );

        let hash = push_block(&asserter, vec![intent()]);
        asserter.push_failure_msg("method not found");
        let err = client(&asserter).get_payload_by_number(NUMBER).await.unwrap_err();
        assert!(
            matches!(err, RemoteL2ClientError::FetchBlsAggregate { hash: failed, .. } if failed == hash),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn ordinary_block_does_not_request_aggregate() {
        let asserter = Asserter::new();
        push_block(&asserter, vec![ordinary()]);

        let envelope = client(&asserter).get_payload_by_number(NUMBER).await.unwrap();
        assert_eq!(envelope.bls_aggregate, None);
        // An aggregate request would have failed on the empty response queue.
        assert!(asserter.read_q().is_empty());
    }
}
