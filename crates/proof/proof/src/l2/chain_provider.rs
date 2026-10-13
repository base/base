//! Contains the concrete implementation of the [`L2ChainProvider`] trait for the client program.

use alloc::{boxed::Box, string::ToString, sync::Arc, vec::Vec};

use alloy_consensus::{BlockBody, Header, Sealed};
use alloy_eips::eip2718::Decodable2718;
use alloy_evm::revm::Database;
use alloy_primitives::{Address, B256, Bytes, FixedBytes};
use alloy_rlp::Decodable;
use async_trait::async_trait;
use base_common_consensus::{BaseBlock, BaseTxEnvelope, BlsConstants};
use base_common_evm::{BlsChunkError, BlsChunkVerifier, BlsDatabaseState};
use base_common_genesis::{RollupConfig, SystemConfig};
use base_consensus_derive::{L2ChainProvider, PipelineError, PipelineErrorKind};
use base_proof_driver::PipelineCursor;
use base_proof_executor::{TrieDB, TrieDBProvider};
use base_proof_mpt::{OrderedListWalker, TrieHinter, TrieNode, TrieProvider};
use base_proof_preimage::{CommsClient, PreimageKey, PreimageKeyType};
use base_protocol::{BatchValidationProvider, BlsChunkBatch, L2BlockInfo, to_system_config};
use spin::RwLock;

use crate::{HintType, eip2935::eip_2935_history_lookup, errors::OracleProviderError};

/// The oracle-backed L2 chain provider for the client program.
#[derive(Debug, Clone)]
pub struct OracleL2ChainProvider<T: CommsClient> {
    /// The L2 safe head block hash.
    l2_head: B256,
    /// The rollup configuration.
    rollup_config: Arc<RollupConfig>,
    /// The preimage oracle client.
    oracle: Arc<T>,
    /// The derivation pipeline cursor
    cursor: Option<Arc<RwLock<PipelineCursor>>>,
    /// The L2 chain ID to use for the provider's hints.
    chain_id: Option<u64>,
}

impl<T: CommsClient> OracleL2ChainProvider<T> {
    /// Creates a new [`OracleL2ChainProvider`] with the given boot information and oracle client.
    pub const fn new(l2_head: B256, rollup_config: Arc<RollupConfig>, oracle: Arc<T>) -> Self {
        Self { l2_head, rollup_config, oracle, cursor: None, chain_id: None }
    }

    /// Sets the L2 chain ID to use for the provider's hints.
    pub const fn set_chain_id(&mut self, chain_id: Option<u64>) {
        self.chain_id = chain_id;
    }

    /// Updates the derivation pipeline cursor
    pub fn set_cursor(&mut self, cursor: Arc<RwLock<PipelineCursor>>) {
        self.cursor = Some(cursor);
    }

    /// Fetches the latest known safe head block hash according to the derivation pipeline cursor
    /// or uses the initial `l2_head` value if no cursor is set.
    pub async fn l2_safe_head(&self) -> Result<B256, OracleProviderError> {
        self.cursor
            .as_ref()
            .map_or(Ok(self.l2_head), |cursor| Ok(cursor.read().l2_safe_head().block_info.hash))
    }
}

impl<T: CommsClient> OracleL2ChainProvider<T> {
    /// Returns a [Header] corresponding to the given L2 block number, by walking back from the
    /// L2 safe head.
    async fn header_by_number(&self, block_number: u64) -> Result<Header, OracleProviderError> {
        // Fetch the starting block header.
        let mut header = self.header_by_hash(self.l2_safe_head().await?)?;

        // Check if the block number is in range. If not, we can fail early.
        if block_number > header.number {
            return Err(OracleProviderError::BlockNumberPastHead(block_number, header.number));
        }

        let mut linear_fallback = false;
        while header.number > block_number {
            if self.rollup_config.is_isthmus_active(header.timestamp) && !linear_fallback {
                // If Isthmus is active, the EIP-2935 contract is used to perform leaping lookbacks
                // through consulting the ring buffer within the contract. If this
                // lookup fails for any reason, we fall back to linear walk back.
                let block_hash =
                    match eip_2935_history_lookup(&header, block_number, self, self).await {
                        Ok(hash) => hash,
                        Err(_) => {
                            // If the EIP-2935 lookup fails for any reason, attempt fallback to
                            // linear walk back.
                            linear_fallback = true;
                            continue;
                        }
                    };

                header = self.header_by_hash(block_hash)?;
            } else {
                // Walk back the block headers one-by-one until the desired block number is reached.
                header = self.header_by_hash(header.parent_hash)?;
            }
        }

        Ok(header)
    }
}

#[async_trait]
impl<T: CommsClient + Send + Sync> BatchValidationProvider for OracleL2ChainProvider<T> {
    type Error = OracleProviderError;

    async fn l2_block_info_by_number(&mut self, number: u64) -> Result<L2BlockInfo, Self::Error> {
        // Get the block at the given number.
        let block = self.block_by_number(number).await?;

        // Construct the system config from the payload.
        L2BlockInfo::from_block_and_genesis(&block, &self.rollup_config.genesis)
            .map_err(OracleProviderError::BlockInfo)
    }

    async fn l2_block_info_by_hash(&mut self, hash: B256) -> Result<L2BlockInfo, Self::Error> {
        let block = self.block_from_header(self.header_by_hash(hash)?, hash).await?;
        L2BlockInfo::from_block_and_genesis(&block, &self.rollup_config.genesis)
            .map_err(OracleProviderError::BlockInfo)
    }

    async fn block_by_number(&mut self, number: u64) -> Result<BaseBlock, Self::Error> {
        // Fetch the header for the given block number.
        let header = self.header_by_number(number).await?;
        let header_hash = header.hash_slow();
        self.block_from_header(header, header_hash).await
    }
}

impl<T: CommsClient + Send + Sync> OracleL2ChainProvider<T> {
    /// Hydrates a [`Header`] into a full [`BaseBlock`] by walking its transactions trie.
    ///
    /// `header_hash` must be the hash of `header`; it is passed separately because callers that
    /// looked the header up by hash already have it.
    async fn block_from_header(
        &self,
        header: Header,
        header_hash: B256,
    ) -> Result<BaseBlock, OracleProviderError> {
        let Header { transactions_root, timestamp, .. } = header;

        // Fetch the transactions in the block.
        HintType::L2Transactions
            .with_data(&[header_hash.as_ref()])
            .with_data(self.chain_id.map_or_else(Vec::new, |id| id.to_be_bytes().to_vec()))
            .send(self.oracle.as_ref())
            .await?;
        let trie_walker = OrderedListWalker::try_new_hydrated(transactions_root, self)
            .map_err(OracleProviderError::TrieWalker)?;

        // Decode the transactions within the transactions trie.
        let transactions = trie_walker
            .into_iter()
            .map(|(_, rlp)| {
                let res = BaseTxEnvelope::decode_2718_exact(rlp.as_ref())?;
                Ok(res)
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(OracleProviderError::Rlp)?;

        let block = BaseBlock {
            header,
            body: BlockBody {
                transactions,
                ommers: Vec::new(),
                withdrawals: self
                    .rollup_config
                    .is_canyon_active(timestamp)
                    .then(|| alloy_eips::eip4895::Withdrawals::new(Vec::new())),
            },
        };
        Ok(block)
    }
}

#[async_trait]
impl<T: CommsClient + Send + Sync> L2ChainProvider for OracleL2ChainProvider<T> {
    type Error = OracleProviderError;

    async fn system_config_by_l2_hash(
        &mut self,
        hash: B256,
        rollup_config: Arc<RollupConfig>,
    ) -> Result<SystemConfig, <Self as L2ChainProvider>::Error> {
        // A hash addresses the header directly, avoiding the walk back from the safe head that a
        // lookup by number requires.
        let block = self.block_from_header(self.header_by_hash(hash)?, hash).await?;

        // Construct the system config from the payload.
        to_system_config(&block, rollup_config.as_ref())
            .map_err(OracleProviderError::BaseBlockConversion)
    }
    /// Runs the chunk verifier over the parent state reached through the oracle.
    ///
    /// The parent header is fetched by hash and its state is read through a [`TrieDB`] with the
    /// usual L2 state hints, so the verdict depends only on oracle preimages. A chunk beyond the
    /// protocol bounds is invalid before any preimage is requested.
    async fn verify_bls_chunk(
        &mut self,
        parent_hash: B256,
        blocks: &[Vec<Bytes>],
        aggregate: Option<FixedBytes<48>>,
    ) -> Result<bool, PipelineErrorKind> {
        let transfers: usize = blocks.iter().map(|txs| BlsChunkBatch::count_transfers(txs)).sum();
        if blocks.len() > BlsChunkBatch::MAX_BLOCKS || transfers > BlsChunkBatch::MAX_TRANSFERS {
            return Ok(false);
        }

        let parent = self.header_by_hash(parent_hash)?;
        let mut db =
            TrieDB::new(Sealed::new_unchecked(parent, parent_hash), self.clone(), self.clone());
        // `TrieDB` serves storage only for accounts loaded through `basic` first; without this,
        // every registry slot would read as zero and no key would ever be found.
        db.basic(BlsConstants::REGISTRY_ADDRESS)
            .map_err(|error| PipelineError::Provider(error.to_string()).crit())?;
        match BlsChunkVerifier::verify(
            &mut BlsDatabaseState(&mut db),
            self.rollup_config.l2_chain_id.id(),
            blocks,
            aggregate,
        ) {
            Ok(()) => Ok(true),
            Err(BlsChunkError::Database(error)) => {
                Err(PipelineError::Provider(error.to_string()).crit())
            }
            Err(error) => {
                warn!(target: "l2_chain_provider", error = %error, "BLS chunk failed verification");
                Ok(false)
            }
        }
    }
}

impl<T: CommsClient> TrieProvider for OracleL2ChainProvider<T> {
    type Error = OracleProviderError;

    fn trie_node_by_hash(&self, key: B256) -> Result<TrieNode, OracleProviderError> {
        // On L2, trie node preimages are stored as keccak preimage types in the oracle. We assume
        // that a hint for these preimages has already been sent, prior to this call.
        crate::block_on(async move {
            TrieNode::decode(
                &mut self
                    .oracle
                    .get(PreimageKey::new(*key, PreimageKeyType::Keccak256))
                    .await
                    .map_err(OracleProviderError::Preimage)?
                    .as_ref(),
            )
            .map_err(OracleProviderError::Rlp)
        })
    }
}

impl<T: CommsClient> TrieDBProvider for OracleL2ChainProvider<T> {
    fn bytecode_by_hash(&self, hash: B256) -> Result<Bytes, OracleProviderError> {
        // Fetch the bytecode preimage from the caching oracle.
        crate::block_on(async move {
            HintType::L2Code
                .with_data(&[hash.as_slice()])
                .with_data(self.chain_id.map_or_else(Vec::new, |id| id.to_be_bytes().to_vec()))
                .send(self.oracle.as_ref())
                .await?;
            self.oracle
                .get(PreimageKey::new_keccak256(*hash))
                .await
                .map(Into::into)
                .map_err(OracleProviderError::Preimage)
        })
    }

    fn header_by_hash(&self, hash: B256) -> Result<Header, OracleProviderError> {
        // Fetch the header from the caching oracle.
        crate::block_on(async move {
            HintType::L2BlockHeader
                .with_data(&[hash.as_slice()])
                .with_data(self.chain_id.map_or_else(Vec::new, |id| id.to_be_bytes().to_vec()))
                .send(self.oracle.as_ref())
                .await?;
            let header_bytes = self.oracle.get(PreimageKey::new_keccak256(*hash)).await?;

            Header::decode(&mut header_bytes.as_slice()).map_err(OracleProviderError::Rlp)
        })
    }
}

impl<T: CommsClient> TrieHinter for OracleL2ChainProvider<T> {
    type Error = OracleProviderError;

    fn hint_trie_node(&self, hash: B256) -> Result<(), Self::Error> {
        crate::block_on(async move {
            HintType::L2StateNode
                .with_data(&[hash.as_slice()])
                .with_data(self.chain_id.map_or_else(Vec::new, |id| id.to_be_bytes().to_vec()))
                .send(self.oracle.as_ref())
                .await
        })
    }

    fn hint_account_proof(&self, address: Address, block_number: u64) -> Result<(), Self::Error> {
        crate::block_on(async move {
            HintType::L2AccountProof
                .with_data(&[block_number.to_be_bytes().as_ref(), address.as_slice()])
                .with_data(self.chain_id.map_or_else(Vec::new, |id| id.to_be_bytes().to_vec()))
                .send(self.oracle.as_ref())
                .await
        })
    }

    fn hint_storage_proof(
        &self,
        address: alloy_primitives::Address,
        slot: alloy_primitives::U256,
        block_number: u64,
    ) -> Result<(), Self::Error> {
        crate::block_on(async move {
            HintType::L2AccountStorageProof
                .with_data(&[
                    block_number.to_be_bytes().as_ref(),
                    address.as_slice(),
                    slot.to_be_bytes::<32>().as_ref(),
                ])
                .with_data(self.chain_id.map_or_else(Vec::new, |id| id.to_be_bytes().to_vec()))
                .send(self.oracle.as_ref())
                .await
        })
    }

    fn hint_execution_witness(
        &self,
        parent_hash: B256,
        base_payload_attributes: &base_common_rpc_types_engine::BasePayloadAttributes,
    ) -> Result<(), Self::Error> {
        crate::block_on(async move {
            let encoded_attributes =
                serde_json::to_vec(base_payload_attributes).map_err(OracleProviderError::Serde)?;

            HintType::L2PayloadWitness
                .with_data(&[parent_hash.as_slice(), &encoded_attributes])
                .with_data(self.chain_id.map_or_else(Vec::new, |id| id.to_be_bytes().to_vec()))
                .send(self.oracle.as_ref())
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{U256, keccak256, map::HashMap};
    use alloy_rlp::Encodable;
    use alloy_trie::{HashBuilder, Nibbles, TrieAccount, proof::ProofRetainer};
    use base_common_consensus::TxBlsTransfer;
    use base_common_evm::{BlsRegistry, test_utils::BlsTestKey};
    use base_proof_preimage::{
        HintWriterClient, PreimageOracleClient, errors::PreimageOracleResult,
    };

    use super::*;

    /// Serves fixed keccak preimages and ignores hints.
    #[derive(Debug, Clone, Default)]
    struct MapOracle(Arc<HashMap<PreimageKey, Vec<u8>>>);

    #[async_trait]
    impl PreimageOracleClient for MapOracle {
        async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
            Ok(self.0.get(&key).cloned().unwrap_or_default())
        }

        async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> PreimageOracleResult<()> {
            buf.copy_from_slice(&self.get(key).await?);
            Ok(())
        }
    }

    #[async_trait]
    impl HintWriterClient for MapOracle {
        async fn write(&self, _hint: &str) -> PreimageOracleResult<()> {
            Ok(())
        }
    }

    /// Builds a trie over `leaves` and returns its root with every node as a keccak preimage.
    fn trie(leaves: Vec<(B256, Vec<u8>)>, preimages: &mut HashMap<PreimageKey, Vec<u8>>) -> B256 {
        let mut leaves: Vec<_> =
            leaves.into_iter().map(|(key, value)| (Nibbles::unpack(key), value)).collect();
        leaves.sort();
        let targets = leaves.iter().map(|(key, _)| *key).collect();
        let mut builder = HashBuilder::default().with_proof_retainer(ProofRetainer::new(targets));
        for (key, value) in &leaves {
            builder.add_leaf(*key, value);
        }
        let root = builder.root();
        for node in builder.take_proof_nodes().values() {
            preimages.insert(PreimageKey::new_keccak256(*keccak256(node)), node.to_vec());
        }
        root
    }

    /// An oracle whose parent block registers `owner`'s key in the BLS registry.
    fn provider(owner: Address, key: &BlsTestKey) -> (OracleL2ChainProvider<MapOracle>, B256) {
        let mut preimages = HashMap::default();
        let public_key: [u8; BlsConstants::PUBKEY_LEN] = key.public_key().0;
        let slots = BlsRegistry::key_slots(owner)
            .into_iter()
            .zip(BlsRegistry::key_words(&public_key))
            .map(|(slot, word)| (keccak256(slot), alloy_rlp::encode(word)))
            .collect();
        let account = TrieAccount {
            nonce: BlsRegistry::ACCOUNT_NONCE,
            storage_root: trie(slots, &mut preimages),
            code_hash: keccak256([]),
            ..Default::default()
        };
        let mut encoded = Vec::new();
        account.encode(&mut encoded);
        let state_root =
            trie(vec![(keccak256(BlsConstants::REGISTRY_ADDRESS), encoded)], &mut preimages);

        let header = Header { number: 7, state_root, ..Default::default() };
        let parent_hash = header.hash_slow();
        preimages.insert(PreimageKey::new_keccak256(*parent_hash), alloy_rlp::encode(&header));

        let config =
            RollupConfig { l2_chain_id: BlsConstants::CHAIN_ID.into(), ..Default::default() };
        let oracle = Arc::new(MapOracle(Arc::new(preimages)));
        (OracleL2ChainProvider::new(parent_hash, Arc::new(config), oracle), parent_hash)
    }

    fn transfer(sender: Address, nonce: u64) -> TxBlsTransfer {
        TxBlsTransfer {
            chain_id: BlsConstants::CHAIN_ID,
            sender,
            nonce,
            recipient: Address::repeat_byte(0xee),
            value: U256::from(1u64),
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: 2,
        }
    }

    fn raw(tx: &TxBlsTransfer) -> Bytes {
        BaseTxEnvelope::BlsTransfer(Sealed::new(*tx)).encoded_2718().into()
    }

    #[tokio::test]
    async fn test_verify_bls_chunk_reads_parent_registry_through_oracle() {
        let owner = Address::repeat_byte(0x01);
        let key = BlsTestKey::from_seed(1);
        let (mut provider, parent_hash) = provider(owner, &key);
        let first = transfer(owner, 0);
        let second = transfer(owner, 1);
        let blocks = vec![vec![raw(&first)], vec![], vec![raw(&second)]];
        let signatures = [key.sign(first.signing_digest()), key.sign(second.signing_digest())];
        let aggregate = BlsTestKey::aggregate(&signatures);

        assert_eq!(
            provider.verify_bls_chunk(parent_hash, &blocks, Some(aggregate)).await,
            Ok(true)
        );

        // An aggregate missing one block's witness does not authenticate the chunk.
        let partial = Some(signatures[0]);
        assert_eq!(provider.verify_bls_chunk(parent_hash, &blocks, partial).await, Ok(false));
        assert_eq!(provider.verify_bls_chunk(parent_hash, &blocks, None).await, Ok(false));

        // A corrupted aggregate never authenticates.
        let mut corrupted = aggregate;
        corrupted.0[47] ^= 1;
        assert_eq!(
            provider.verify_bls_chunk(parent_hash, &blocks, Some(corrupted)).await,
            Ok(false)
        );

        // A later chunk from the same sender verifies on its own with its own aggregate.
        let third = transfer(owner, 2);
        let next_chunk = vec![vec![raw(&third)]];
        let next_aggregate = Some(key.sign(third.signing_digest()));
        assert_eq!(
            provider.verify_bls_chunk(parent_hash, &next_chunk, next_aggregate).await,
            Ok(true)
        );

        // The aggregate binds the set of intents, not their order: reordered nonces still verify
        // and are left to ordered execution, which rejects the out-of-order nonce.
        let reordered = vec![vec![raw(&second)], vec![], vec![raw(&first)]];
        assert_eq!(
            provider.verify_bls_chunk(parent_hash, &reordered, Some(aggregate)).await,
            Ok(true)
        );

        // Altering a signed intent invalidates the aggregate.
        let mut altered = second;
        altered.value = U256::from(2u64);
        let altered_blocks = vec![vec![raw(&first)], vec![], vec![raw(&altered)]];
        assert_eq!(
            provider.verify_bls_chunk(parent_hash, &altered_blocks, Some(aggregate)).await,
            Ok(false)
        );

        // A sender without a registered key in the parent state is never authorized.
        let stranger = transfer(Address::repeat_byte(0x02), 0);
        let stranger_aggregate = Some(key.sign(stranger.signing_digest()));
        assert_eq!(
            provider
                .verify_bls_chunk(parent_hash, &[vec![raw(&stranger)]], stranger_aggregate)
                .await,
            Ok(false)
        );
    }

    #[tokio::test]
    async fn test_verify_bls_chunk_fails_closed() {
        let owner = Address::repeat_byte(0x01);
        let key = BlsTestKey::from_seed(1);
        let (mut provider, _) = provider(owner, &key);
        let tx = transfer(owner, 0);
        let aggregate = Some(key.sign(tx.signing_digest()));

        // An unknown parent is an unavailable verdict, not an authorization.
        let unknown = B256::repeat_byte(0x99);
        let verdict = provider.verify_bls_chunk(unknown, &[vec![raw(&tx)]], aggregate).await;
        assert!(matches!(verdict, Err(PipelineErrorKind::Critical(_))), "{verdict:?}");

        // Oversized chunks are invalid before any preimage is read.
        let oversized = vec![Vec::new(); BlsChunkBatch::MAX_BLOCKS + 1];
        assert_eq!(provider.verify_bls_chunk(unknown, &oversized, None).await, Ok(false));
    }
}
