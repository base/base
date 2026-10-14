use std::{fmt::Debug, sync::Arc};

use alloy_consensus::Block;
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{B256, Bytes, keccak256};
use alloy_provider::{Provider, RootProvider};
use async_trait::async_trait;
use base_common_chains::Upgrades;
use base_common_consensus::BaseTxEnvelope;
use base_common_genesis::RollupConfig;
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

    /// Failed to fetch the block access list of an Amsterdam block from the L2 EL.
    #[error("failed to fetch block access list of block {block_hash}: {source}")]
    FetchBlockAccessList {
        /// The hash of the block whose list was requested.
        block_hash: B256,
        /// The underlying transport error.
        source: alloy_transport::TransportError,
    },

    /// The L2 EL has no block access list for a block whose header commits to one.
    #[error("block access list of block {0} is not available from the source node")]
    BlockAccessListUnavailable(B256),

    /// The fetched block access list does not hash to the header's commitment.
    #[error(
        "block access list of block {block_hash} hashes to {actual}, header commits to {expected}"
    )]
    BlockAccessListMismatch {
        /// The hash of the block whose list was requested.
        block_hash: B256,
        /// The `block_access_list_hash` committed to by the header.
        expected: B256,
        /// The `keccak256` hash of the list the source node returned.
        actual: B256,
    },

    /// The header commits to a block access list but carries no slot number.
    #[error("block {0} commits to a block access list but its header has no slot number")]
    MissingSlotNumber(B256),

    /// The header commits to a block access list but lacks the fields an Amsterdam payload needs.
    #[error("block {0} commits to a block access list but is not a complete Amsterdam block")]
    IncompleteAmsterdamBlock(B256),

    /// A post-Amsterdam source header carries no block access list commitment.
    #[error("block {0} is post-Amsterdam but its header carries no block access list commitment")]
    MissingBlockAccessListCommitment(B256),

    /// A pre-Amsterdam source header carries a block access list commitment.
    #[error("block {0} carries a block access list commitment before Amsterdam")]
    UnexpectedBlockAccessListCommitment(B256),

    /// The source header's slot number is not its block number.
    #[error(
        "block {block_hash} carries slot number {got} but must carry its block number {expected}"
    )]
    SlotNumberMismatch {
        /// The hash of the block whose header was fetched.
        block_hash: B256,
        /// The block number, which is the expected slot number.
        expected: u64,
        /// The slot number the header carries.
        got: u64,
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
#[derive(Debug, Clone)]
pub struct RemoteL2Client {
    provider: RootProvider<Base>,
    rollup_config: Arc<RollupConfig>,
}

impl RemoteL2Client {
    /// Creates a new [`RemoteL2Client`] from a source L2 node URL.
    ///
    /// The rollup config is this node's own schedule, so the payload version for a block is
    /// chosen from the local fork activation rather than from what the source node's header
    /// happens to carry.
    pub fn new(url: Url, rollup_config: Arc<RollupConfig>) -> Self {
        let provider = RootProvider::<Base>::new_http(url);
        Self { provider, rollup_config }
    }

    /// Fetches the RLP-encoded block access list of the block with the given hash through
    /// `eth_getBlockAccessListRaw` and checks it against the header's commitment.
    ///
    /// The block is addressed by hash so a reorg on the source node cannot return the list of a
    /// different block.
    pub async fn get_block_access_list(
        &self,
        block_hash: B256,
        expected_hash: B256,
    ) -> Result<Bytes, RemoteL2ClientError> {
        let block_access_list = self
            .provider
            .raw_request::<_, Option<Bytes>>(
                "eth_getBlockAccessListRaw".into(),
                (BlockId::hash(block_hash),),
            )
            .await
            .map_err(|source| RemoteL2ClientError::FetchBlockAccessList { block_hash, source })?
            .ok_or(RemoteL2ClientError::BlockAccessListUnavailable(block_hash))?;

        let actual = keccak256(&block_access_list);
        if actual != expected_hash {
            return Err(RemoteL2ClientError::BlockAccessListMismatch {
                block_hash,
                expected: expected_hash,
                actual,
            });
        }

        Ok(block_access_list)
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

        // A header only carries the hash of the block access list, so an Amsterdam block needs the
        // list fetched separately to be rebuilt as a V5 payload. Which payload version is produced
        // is decided by this node's own fork schedule, not by what the source header carries: a
        // source that is behind or malformed must not make an Amsterdam block convert to a
        // listless V4 payload. Denim is Base's Amsterdam activation in this branch, so the Denim
        // gate is the local Amsterdam schedule.
        let is_amsterdam =
            self.rollup_config.is_denim_active_at_timestamp(consensus_block.header.timestamp);
        let (execution_payload, _sidecar) = match consensus_block.header.block_access_list_hash {
            Some(expected_hash) => {
                if !is_amsterdam {
                    return Err(RemoteL2ClientError::UnexpectedBlockAccessListCommitment(
                        block_hash,
                    ));
                }
                // The conversion defaults a missing slot number to zero, which would fabricate
                // a value the EL then rejects, so a header without one is malformed. Base puts the
                // L2 block number in the slot, so any other value is rejected as well.
                match consensus_block.header.slot_number {
                    Some(slot) if slot == consensus_block.header.number => {}
                    Some(got) => {
                        return Err(RemoteL2ClientError::SlotNumberMismatch {
                            block_hash,
                            expected: consensus_block.header.number,
                            got,
                        });
                    }
                    None => return Err(RemoteL2ClientError::MissingSlotNumber(block_hash)),
                }
                let block_access_list =
                    self.get_block_access_list(block_hash, expected_hash).await?;
                let (payload, sidecar) = BaseExecutionPayload::from_block_unchecked_with_extras(
                    block_hash,
                    &consensus_block,
                    Some(block_access_list),
                );
                // A header lacking a field an earlier fork requires (e.g. `requests_hash`)
                // converts to an older payload that silently drops the list.
                if payload.as_v5().is_none() {
                    return Err(RemoteL2ClientError::IncompleteAmsterdamBlock(block_hash));
                }
                (payload, sidecar)
            }
            None if is_amsterdam => {
                return Err(RemoteL2ClientError::MissingBlockAccessListCommitment(block_hash));
            }
            None => BaseExecutionPayload::from_block_unchecked(block_hash, &consensus_block),
        };

        Ok(BaseExecutionPayloadEnvelope { parent_beacon_block_root, execution_payload })
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;
    use alloy_eips::{
        eip7685::EMPTY_REQUESTS_HASH,
        eip7928::{AccountChanges, BalanceChange, BlockAccessIndex},
    };
    use alloy_primitives::{Address, Sealable, U256};
    use alloy_rlp::Decodable;
    use alloy_rpc_types_engine::{CancunPayloadFields, PraguePayloadFields};
    use alloy_rpc_types_eth::{BlockTransactions, Header as RpcHeader};
    use base_common_genesis::{BaseUpgradeConfig, UpgradeConfig};
    use base_common_rpc_types::{BaseBlockResponse, BaseHeaderResponse};
    use base_common_rpc_types_engine::BaseExecutionPayloadSidecar;
    use httpmock::{Mock, MockServer, prelude::*};
    use serde_json::{Value, json};

    use super::*;

    const BLOCK_NUMBER: u64 = 42;
    const PARENT_BEACON_BLOCK_ROOT: B256 = B256::new([0x22; 32]);

    /// Encodes a minimal but structurally valid EIP-7928 list: one touched account.
    fn block_access_list() -> Bytes {
        let mut account = AccountChanges::new(Address::repeat_byte(0xaa));
        account.storage_reads.push(U256::from(7));
        account.balance_changes.push(BalanceChange::new(BlockAccessIndex::new(1), U256::from(9)));
        alloy_rlp::encode(vec![account]).into()
    }

    /// A coherent Isthmus-shaped header, committing to `block_access_list` as an Amsterdam
    /// header does (with the L2 block number as slot) when one is given.
    fn header(block_access_list: Option<&Bytes>) -> Header {
        Header {
            parent_hash: B256::repeat_byte(0x11),
            beneficiary: Address::repeat_byte(0x33),
            state_root: B256::repeat_byte(0x44),
            number: BLOCK_NUMBER,
            gas_limit: 30_000_000,
            timestamp: 1_700_000_000,
            base_fee_per_gas: Some(7),
            withdrawals_root: Some(B256::repeat_byte(0x55)),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(PARENT_BEACON_BLOCK_ROOT),
            requests_hash: Some(EMPTY_REQUESTS_HASH),
            block_access_list_hash: block_access_list.map(keccak256),
            slot_number: block_access_list.map(|_| BLOCK_NUMBER),
            ..Default::default()
        }
    }

    /// The JSON-RPC response of the source node for `header`, whose hash is computed from the
    /// header itself rather than chosen.
    fn block_response(header: &Header) -> String {
        let block: BaseBlockResponse = alloy_rpc_types_eth::Block {
            header: BaseHeaderResponse::new(RpcHeader::from_consensus(
                header.clone().seal_slow(),
                None,
                None,
            )),
            uncles: vec![],
            transactions: BlockTransactions::Full(vec![]),
            withdrawals: Some(Default::default()),
        };
        json!({ "jsonrpc": "2.0", "id": 0, "result": block }).to_string()
    }

    /// Parses a JSON-RPC request body so matchers can compare method and params exactly.
    fn rpc_request(body: String) -> Value {
        serde_json::from_str(&body).expect("the client must send valid JSON")
    }

    /// Mocks exactly the `eth_getBlockByNumber(<BLOCK_NUMBER>, full = true)` request.
    async fn mock_block<'a>(server: &'a MockServer, header: &Header) -> Mock<'a> {
        let response = block_response(header);
        server
            .mock_async(|when, then| {
                when.method(POST).path("/").is_true(|req| {
                    let request = rpc_request(req.body_string());
                    request["method"] == "eth_getBlockByNumber"
                        && request["params"] == json!([format!("{BLOCK_NUMBER:#x}"), true])
                });
                then.status(200).header("content-type", "application/json").body(response);
            })
            .await
    }

    /// Mocks exactly the `eth_getBlockAccessListRaw` request addressed by `block_hash`.
    async fn mock_block_access_list<'a>(
        server: &'a MockServer,
        block_hash: B256,
        result: Value,
    ) -> Mock<'a> {
        let response = json!({ "jsonrpc": "2.0", "id": 0, "result": result }).to_string();
        server
            .mock_async(|when, then| {
                when.method(POST).path("/").is_true(move |req| {
                    let request = rpc_request(req.body_string());
                    request["method"] == "eth_getBlockAccessListRaw"
                        && request["params"] == json!([block_hash])
                });
                then.status(200).header("content-type", "application/json").body(response);
            })
            .await
    }

    /// A rollup config with Amsterdam active at the fixture header's timestamp.
    fn amsterdam_config() -> Arc<RollupConfig> {
        Arc::new(RollupConfig {
            upgrades: UpgradeConfig {
                base: BaseUpgradeConfig { denim: Some(1_699_999_999), ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        })
    }

    /// A rollup config with Amsterdam not yet active at the fixture header's timestamp.
    fn pre_amsterdam_config() -> Arc<RollupConfig> {
        Arc::new(RollupConfig {
            upgrades: UpgradeConfig {
                base: BaseUpgradeConfig { denim: Some(1_700_000_001), ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        })
    }

    fn client_with(server: &MockServer, rollup_config: Arc<RollupConfig>) -> RemoteL2Client {
        RemoteL2Client::new(
            server.url("/").parse().expect("mock server URL is valid"),
            rollup_config,
        )
    }

    fn client(server: &MockServer) -> RemoteL2Client {
        client_with(server, amsterdam_config())
    }

    /// Rebuilds the block the way the insert task does, from the envelope alone.
    fn rebuild(envelope: BaseExecutionPayloadEnvelope) -> Block<BaseTxEnvelope> {
        envelope
            .execution_payload
            .try_into_block_with_sidecar(&BaseExecutionPayloadSidecar::v4(
                CancunPayloadFields::new(envelope.parent_beacon_block_root.unwrap(), vec![]),
                PraguePayloadFields::new(EMPTY_REQUESTS_HASH),
            ))
            .expect("the payload must rebuild into a block")
    }

    /// An Amsterdam block is rebuilt as a V5 payload carrying the fetched list and the header's
    /// slot, which reconstructs the source block exactly, hash included.
    #[tokio::test]
    async fn amsterdam_block_yields_v5_payload_with_block_access_list() {
        let server = MockServer::start_async().await;
        let list = block_access_list();
        let header = header(Some(&list));
        let source_hash = header.hash_slow();
        let block_mock = mock_block(&server, &header).await;
        let list_mock = mock_block_access_list(&server, source_hash, json!(list)).await;

        let envelope = client(&server).get_payload_by_number(BLOCK_NUMBER).await.unwrap();

        block_mock.assert_calls_async(1).await;
        list_mock.assert_calls_async(1).await;
        assert_eq!(
            envelope.parent_beacon_block_root,
            Some(PARENT_BEACON_BLOCK_ROOT),
            "the envelope must carry the header's parent beacon block root"
        );
        assert_eq!(envelope.execution_payload.block_hash(), source_hash, "block hash is kept");
        let payload = envelope.execution_payload.as_v5().expect("an Amsterdam block is V5");
        assert_eq!(payload.block_access_list, list, "the payload carries the fetched list");
        assert_eq!(payload.slot_number, BLOCK_NUMBER, "the payload carries the header's slot");
        assert_eq!(
            Vec::<AccountChanges>::decode(&mut payload.block_access_list.as_ref()).unwrap().len(),
            1,
            "the carried list must be a valid EIP-7928 encoding"
        );

        let rebuilt = rebuild(envelope);
        assert_eq!(rebuilt.header, header, "the payload must rebuild the exact source header");
        assert_eq!(rebuilt.header.hash_slow(), source_hash, "the rebuilt block hash must match");
    }

    /// A block without a commitment is converted as before, without requesting a list.
    #[tokio::test]
    async fn pre_amsterdam_block_yields_v4_payload_without_block_access_list() {
        let server = MockServer::start_async().await;
        let header = header(None);
        let source_hash = header.hash_slow();
        let block_mock = mock_block(&server, &header).await;
        let list_mock = mock_block_access_list(&server, source_hash, Value::Null).await;

        let envelope = client_with(&server, pre_amsterdam_config())
            .get_payload_by_number(BLOCK_NUMBER)
            .await
            .unwrap();

        block_mock.assert_calls_async(1).await;
        list_mock.assert_calls_async(0).await;
        assert_eq!(envelope.parent_beacon_block_root, Some(PARENT_BEACON_BLOCK_ROOT));
        assert!(
            matches!(envelope.execution_payload, BaseExecutionPayload::V4(_)),
            "a block without a commitment must stay a V4 payload"
        );
        let rebuilt = rebuild(envelope);
        assert_eq!(rebuilt.header, header, "the payload must rebuild the exact source header");
        assert_eq!(rebuilt.header.hash_slow(), source_hash, "the rebuilt block hash must match");
    }

    /// An Amsterdam block whose source header carries no commitment is rejected instead of
    /// falling back to a listless V4 payload, and no list is requested.
    #[tokio::test]
    async fn post_amsterdam_header_without_commitment_is_rejected() {
        let server = MockServer::start_async().await;
        let header = header(None);
        let source_hash = header.hash_slow();
        let block_mock = mock_block(&server, &header).await;
        let list_mock = mock_block_access_list(&server, source_hash, Value::Null).await;

        let error = client(&server).get_payload_by_number(BLOCK_NUMBER).await.unwrap_err();

        block_mock.assert_calls_async(1).await;
        list_mock.assert_calls_async(0).await;
        assert!(
            matches!(error, RemoteL2ClientError::MissingBlockAccessListCommitment(hash) if hash == source_hash),
            "a missing commitment on an Amsterdam block must be reported, got {error:?}"
        );
    }

    /// A committed Amsterdam header whose slot is not the block number is rejected before any
    /// list is fetched, so the source cannot inject a slot the EL would reject.
    #[tokio::test]
    async fn slot_number_mismatch_is_rejected() {
        let server = MockServer::start_async().await;
        let list = block_access_list();
        let mut header = header(Some(&list));
        header.slot_number = Some(BLOCK_NUMBER + 1);
        let source_hash = header.hash_slow();
        let block_mock = mock_block(&server, &header).await;
        let list_mock = mock_block_access_list(&server, source_hash, json!(list)).await;

        let error = client(&server).get_payload_by_number(BLOCK_NUMBER).await.unwrap_err();

        block_mock.assert_calls_async(1).await;
        list_mock.assert_calls_async(0).await;
        match error {
            RemoteL2ClientError::SlotNumberMismatch { block_hash, expected, got } => {
                assert_eq!(block_hash, source_hash, "the error must name the block");
                assert_eq!(expected, BLOCK_NUMBER, "the expected slot is the block number");
                assert_eq!(got, BLOCK_NUMBER + 1, "the reported slot is the header's");
            }
            other => panic!("a wrong slot must be reported as a mismatch, got {other:?}"),
        }
    }

    /// A pre-Amsterdam header carrying a commitment is rejected, because converting it would
    /// silently drop the list.
    #[tokio::test]
    async fn pre_amsterdam_header_with_commitment_is_rejected() {
        let server = MockServer::start_async().await;
        let list = block_access_list();
        let header = header(Some(&list));
        let source_hash = header.hash_slow();
        let block_mock = mock_block(&server, &header).await;
        let list_mock = mock_block_access_list(&server, source_hash, json!(list)).await;

        let error = client_with(&server, pre_amsterdam_config())
            .get_payload_by_number(BLOCK_NUMBER)
            .await
            .unwrap_err();

        block_mock.assert_calls_async(1).await;
        list_mock.assert_calls_async(0).await;
        assert!(
            matches!(error, RemoteL2ClientError::UnexpectedBlockAccessListCommitment(hash) if hash == source_hash),
            "a pre-Amsterdam commitment must be reported, got {error:?}"
        );
    }

    /// A committed block whose list the source node cannot return is an error, never a V4 payload.
    #[tokio::test]
    async fn missing_block_access_list_is_an_error() {
        let server = MockServer::start_async().await;
        let list = block_access_list();
        let header = header(Some(&list));
        let source_hash = header.hash_slow();
        let block_mock = mock_block(&server, &header).await;
        let list_mock = mock_block_access_list(&server, source_hash, Value::Null).await;

        let error = client(&server).get_payload_by_number(BLOCK_NUMBER).await.unwrap_err();

        block_mock.assert_calls_async(1).await;
        list_mock.assert_calls_async(1).await;
        assert!(
            matches!(error, RemoteL2ClientError::BlockAccessListUnavailable(hash) if hash == source_hash),
            "a null list must be reported as unavailable, got {error:?}"
        );
    }

    /// An RPC failure while fetching the list is an error, never a V4 payload.
    #[tokio::test]
    async fn block_access_list_rpc_failure_is_an_error() {
        let server = MockServer::start_async().await;
        let list = block_access_list();
        let header = header(Some(&list));
        let source_hash = header.hash_slow();
        let block_mock = mock_block(&server, &header).await;
        let failure = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "error": { "code": -32601, "message": "method not found" }
        })
        .to_string();
        let list_mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .json_body_includes(r#"{"method":"eth_getBlockAccessListRaw"}"#);
                then.status(200).header("content-type", "application/json").body(failure);
            })
            .await;

        let error = client(&server).get_payload_by_number(BLOCK_NUMBER).await.unwrap_err();

        block_mock.assert_calls_async(1).await;
        list_mock.assert_calls_async(1).await;
        assert!(
            matches!(error, RemoteL2ClientError::FetchBlockAccessList { block_hash, .. } if block_hash == source_hash),
            "an RPC failure must be reported as a list fetch error, got {error:?}"
        );
    }

    /// A list that does not hash to the header's commitment is rejected.
    #[tokio::test]
    async fn mismatched_block_access_list_is_an_error() {
        let server = MockServer::start_async().await;
        let list = block_access_list();
        let header = header(Some(&list));
        let source_hash = header.hash_slow();
        let wrong = Bytes::from_static(&[0xc0]);
        let block_mock = mock_block(&server, &header).await;
        let list_mock = mock_block_access_list(&server, source_hash, json!(wrong)).await;

        let error = client(&server).get_payload_by_number(BLOCK_NUMBER).await.unwrap_err();

        block_mock.assert_calls_async(1).await;
        list_mock.assert_calls_async(1).await;
        match error {
            RemoteL2ClientError::BlockAccessListMismatch { block_hash, expected, actual } => {
                assert_eq!(block_hash, source_hash, "the error must name the block");
                assert_eq!(expected, keccak256(&list), "expected is the header commitment");
                assert_eq!(actual, keccak256(&wrong), "actual is the hash of the returned list");
            }
            other => panic!("a wrong list must be reported as a mismatch, got {other:?}"),
        }
    }

    /// A header that commits to a list but has no slot number is rejected before any list is
    /// fetched, instead of defaulting the slot to zero.
    #[tokio::test]
    async fn committed_block_without_slot_number_is_an_error() {
        let server = MockServer::start_async().await;
        let list = block_access_list();
        let mut header = header(Some(&list));
        header.slot_number = None;
        let source_hash = header.hash_slow();
        let block_mock = mock_block(&server, &header).await;
        let list_mock = mock_block_access_list(&server, source_hash, json!(list)).await;

        let error = client(&server).get_payload_by_number(BLOCK_NUMBER).await.unwrap_err();

        block_mock.assert_calls_async(1).await;
        list_mock.assert_calls_async(0).await;
        assert!(
            matches!(error, RemoteL2ClientError::MissingSlotNumber(hash) if hash == source_hash),
            "a missing slot must be reported, got {error:?}"
        );
    }

    /// A header that commits to a list but lacks `requests_hash` would convert to a V3 payload
    /// that drops the list, so it is rejected instead.
    #[tokio::test]
    async fn committed_block_without_requests_hash_is_an_error() {
        let server = MockServer::start_async().await;
        let list = block_access_list();
        let mut header = header(Some(&list));
        header.requests_hash = None;
        let source_hash = header.hash_slow();
        let block_mock = mock_block(&server, &header).await;
        let list_mock = mock_block_access_list(&server, source_hash, json!(list)).await;

        let error = client(&server).get_payload_by_number(BLOCK_NUMBER).await.unwrap_err();

        block_mock.assert_calls_async(1).await;
        list_mock.assert_calls_async(1).await;
        assert!(
            matches!(error, RemoteL2ClientError::IncompleteAmsterdamBlock(hash) if hash == source_hash),
            "an incomplete header must be reported, got {error:?}"
        );
    }
}
