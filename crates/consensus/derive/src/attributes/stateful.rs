//! The [`AttributesBuilder`] and it's default implementation.

use alloc::{
    boxed::Box, collections::VecDeque, fmt::Debug, string::ToString, sync::Arc, vec, vec::Vec,
};

use alloy_consensus::{Eip658Value, Receipt};
use alloy_eips::{BlockNumHash, eip2718::Encodable2718};
use alloy_genesis::ChainConfig;
use alloy_primitives::{Address, B256, Bytes};
use alloy_rlp::Encodable;
use alloy_rpc_types_engine::PayloadAttributes;
use async_trait::async_trait;
use base_common_consensus::{BaseBlock, Predeploys};
use base_common_genesis::{RollupConfig, SystemConfig};
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_consensus_upgrades::{Upgrade, Upgrades};
use base_protocol::{
    BaseTimeUpdateTx, BatchValidationProvider, Deposits, L1BlockInfoTx, L2BlockInfo,
    to_system_config,
};
use tracing::warn;

use crate::{
    AttributesBuilder, BuilderError, ChainProvider, L2ChainProvider, PipelineEncodingError,
    PipelineError, PipelineErrorKind, PipelineResult,
};

/// The maximum number of [`SystemConfig`]s cached by L2 block hash.
const MAX_SYSTEM_CONFIG_CACHE_ENTRIES: usize = 8;

/// A stateful implementation of the [`AttributesBuilder`].
#[derive(Debug, Default)]
pub struct StatefulAttributesBuilder<L1P, L2P>
where
    L1P: ChainProvider + Debug,
    L2P: L2ChainProvider + Debug,
{
    /// The rollup config.
    rollup_cfg: Arc<RollupConfig>,
    /// The L1 config.
    l1_cfg: Arc<ChainConfig>,
    /// The system config fetcher.
    config_fetcher: L2P,
    /// The L1 receipts fetcher.
    receipts_fetcher: L1P,
    /// Cache of [`SystemConfig`]s keyed by the L2 block hash they were decoded from, most
    /// recently used first. An entry is a pure function of its block, so it can never go stale:
    /// forks, resets, and reorgs need no invalidation.
    system_configs: VecDeque<(B256, SystemConfig)>,
}

impl<L1P, L2P> StatefulAttributesBuilder<L1P, L2P>
where
    L1P: ChainProvider + Debug,
    L2P: L2ChainProvider + Debug,
{
    /// Create a new [`StatefulAttributesBuilder`] with the given epoch.
    pub const fn new(
        rcfg: Arc<RollupConfig>,
        l1_cfg: Arc<ChainConfig>,
        sys_cfg_fetcher: L2P,
        receipts: L1P,
    ) -> Self {
        Self {
            rollup_cfg: rcfg,
            l1_cfg,
            config_fetcher: sys_cfg_fetcher,
            receipts_fetcher: receipts,
            system_configs: VecDeque::new(),
        }
    }

    /// Returns the cached [`SystemConfig`] for the given L2 block hash, promoting it to most
    /// recently used.
    fn cached_system_config(&mut self, block_hash: &B256) -> Option<SystemConfig> {
        let index = self.system_configs.iter().position(|(hash, _)| hash == block_hash)?;
        let entry = self.system_configs.remove(index)?;
        let config = entry.1;
        self.system_configs.push_front(entry);
        Some(config)
    }

    /// Caches the [`SystemConfig`] for the given L2 block hash, evicting the least recently used
    /// entries beyond [`MAX_SYSTEM_CONFIG_CACHE_ENTRIES`].
    fn cache_system_config(&mut self, block_hash: B256, config: SystemConfig) {
        if let Some(index) = self.system_configs.iter().position(|(hash, _)| hash == &block_hash) {
            self.system_configs.remove(index);
        }
        self.system_configs.push_front((block_hash, config));
        self.system_configs.truncate(MAX_SYSTEM_CONFIG_CACHE_ENTRIES);
    }
}

#[async_trait]
impl<L1P, L2P> AttributesBuilder for StatefulAttributesBuilder<L1P, L2P>
where
    L1P: ChainProvider + Debug + Send,
    L2P: L2ChainProvider + Debug + Send,
    <L2P as BatchValidationProvider>::Error: Into<PipelineErrorKind>,
{
    async fn prepare_payload_attributes(
        &mut self,
        l2_parent: L2BlockInfo,
        epoch: BlockNumHash,
    ) -> PipelineResult<BasePayloadAttributes> {
        let l1_header;
        let deposit_transactions: Vec<Bytes>;

        let next_l2_block_number = l2_parent.block_info.number + 1;
        let (next_l2_time, next_l2_timestamp_millis_part) =
            self.rollup_cfg.l2_block_timestamp_parts(next_l2_block_number);

        // The parent block's system config: decoded from the parent itself, either seeded in
        // memory when the parent was inserted (see [`AttributesBuilder::seed_system_config`]) or
        // read from the L2 EL on a miss (startup, reset, reorg, or a parent built elsewhere).
        // The fetched block's hash is verified against the parent hash so that a read racing a
        // reorg can never cache another block's config under this parent.
        let mut sys_config = match self.cached_system_config(&l2_parent.block_info.hash) {
            Some(config) => config,
            None => {
                let block = self
                    .config_fetcher
                    .block_by_number(l2_parent.block_info.number)
                    .await
                    .map_err(Into::into)?;
                let block_hash = block.header.hash_slow();
                if block_hash != l2_parent.block_info.hash {
                    return Err(PipelineErrorKind::Reset(
                        BuilderError::BlockMismatch(
                            BlockNumHash {
                                number: l2_parent.block_info.number,
                                hash: l2_parent.block_info.hash,
                            },
                            BlockNumHash { number: block.header.number, hash: block_hash },
                        )
                        .into(),
                    ));
                }
                let config = to_system_config(&block, &self.rollup_cfg).map_err(|err| {
                    warn!(target: "attributes", error = ?err, number = block.header.number, "Failed to decode system config from parent block");
                    PipelineError::Provider("system config conversion failed".to_string()).temp()
                })?;
                self.cache_system_config(l2_parent.block_info.hash, config);
                config
            }
        };

        // If the L1 origin changed in this block, then we are in the first block of the epoch.
        // In this case we need to fetch all transaction receipts from the L1 origin block so
        // we can scan for user deposits.
        let sequence_number = if l2_parent.l1_origin.number != epoch.number {
            let header =
                self.receipts_fetcher.header_by_hash(epoch.hash).await.map_err(Into::into)?;
            if l2_parent.l1_origin.hash != header.parent_hash {
                return Err(PipelineErrorKind::Reset(
                    BuilderError::BlockMismatchEpochReset(
                        epoch,
                        l2_parent.l1_origin,
                        header.parent_hash,
                    )
                    .into(),
                ));
            }
            let receipts =
                self.receipts_fetcher.receipts_by_hash(epoch.hash).await.map_err(Into::into)?;
            let deposits =
                derive_deposits(epoch.hash, &receipts, self.rollup_cfg.deposit_contract_address)
                    .await
                    .map_err(|e| PipelineError::BadEncoding(e).crit())?;
            let (updates, errors) = sys_config.update_with_receipts(
                &receipts,
                self.rollup_cfg.l1_system_config_address,
                self.rollup_cfg.is_ecotone_active(header.timestamp),
            );
            for kind in &updates {
                info!(target: "attributes", epoch = epoch.number, %kind, "Applied system config update");
            }
            for err in &errors {
                warn!(target: "attributes", error = ?err, epoch = epoch.number, "Malformed system config update (skipped)");
            }
            l1_header = header;
            deposit_transactions = deposits;
            0
        } else if l2_parent.l1_origin.hash != epoch.hash {
            return Err(PipelineErrorKind::Reset(
                BuilderError::BlockMismatch(epoch, l2_parent.l1_origin).into(),
            ));
        } else {
            let header =
                self.receipts_fetcher.header_by_hash(epoch.hash).await.map_err(Into::into)?;
            l1_header = header;
            deposit_transactions = vec![];
            l2_parent.seq_num + 1
        };

        // Sanity check the L1 origin was correctly selected to maintain the time invariant
        // between L1 and L2.
        if next_l2_time < l1_header.timestamp {
            return Err(PipelineErrorKind::Reset(
                BuilderError::BrokenTimeInvariant(
                    l2_parent.l1_origin,
                    next_l2_time,
                    BlockNumHash { hash: l1_header.hash_slow(), number: l1_header.number },
                    l1_header.timestamp,
                )
                .into(),
            ));
        }

        let mut upgrade_transactions: Vec<Bytes> = vec![];
        if self.rollup_cfg.is_ecotone_active(next_l2_time)
            && !self.rollup_cfg.is_ecotone_active(l2_parent.block_info.timestamp)
        {
            upgrade_transactions.extend(Upgrades::ECOTONE.txs());
        }
        if self.rollup_cfg.is_fjord_active(next_l2_time)
            && !self.rollup_cfg.is_fjord_active(l2_parent.block_info.timestamp)
        {
            upgrade_transactions.extend(Upgrades::FJORD.txs());
        }
        if self.rollup_cfg.is_isthmus_active(next_l2_time)
            && !self.rollup_cfg.is_isthmus_active(l2_parent.block_info.timestamp)
        {
            upgrade_transactions.extend(Upgrades::ISTHMUS.txs());
        }
        if self.rollup_cfg.is_jovian_active(next_l2_time)
            && !self.rollup_cfg.is_jovian_active(l2_parent.block_info.timestamp)
        {
            upgrade_transactions.extend(Upgrades::JOVIAN.txs());
        }

        // Build and encode the L1 info transaction for the current payload.
        let (_, l1_info_tx_envelope) = L1BlockInfoTx::try_new_with_deposit_tx(
            &self.rollup_cfg,
            &self.l1_cfg,
            &sys_config,
            sequence_number,
            &l1_header,
            l2_parent.block_info.timestamp,
            next_l2_time,
        )
        .map_err(|e| {
            PipelineError::AttributesBuilder(BuilderError::Custom(e.to_string())).crit()
        })?;
        let mut encoded_l1_info_tx = Vec::with_capacity(l1_info_tx_envelope.length());
        l1_info_tx_envelope.encode_2718(&mut encoded_l1_info_tx);

        let base_time_active = self.rollup_cfg.is_denim_active(next_l2_time);
        let mut txs = Vec::with_capacity(
            1 + usize::from(base_time_active)
                + deposit_transactions.len()
                + upgrade_transactions.len(),
        );
        txs.push(encoded_l1_info_tx.into());

        if base_time_active {
            let base_time = BaseTimeUpdateTx::new(next_l2_timestamp_millis_part).map_err(|e| {
                PipelineError::AttributesBuilder(BuilderError::BaseTimeUpdate(e)).crit()
            })?;
            let envelope = base_time.into_deposit_tx(next_l2_block_number);
            let mut encoded = Vec::with_capacity(envelope.length());
            envelope.encode_2718(&mut encoded);
            txs.push(encoded.into());
        }

        txs.extend(deposit_transactions);
        txs.extend(upgrade_transactions);

        let mut withdrawals = None;
        if self.rollup_cfg.is_canyon_active(next_l2_time) {
            withdrawals = Some(Vec::default());
        }

        let mut parent_beacon_root = None;
        if self.rollup_cfg.is_ecotone_active(next_l2_time) {
            // if the parent beacon root is not available, default to zero hash
            parent_beacon_root = Some(l1_header.parent_beacon_block_root.unwrap_or_default());
        }

        Ok(BasePayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: next_l2_time,
                prev_randao: l1_header.mix_hash,
                suggested_fee_recipient: Predeploys::SEQUENCER_FEE_VAULT,
                parent_beacon_block_root: parent_beacon_root,
                withdrawals,
                slot_number: None,
                target_gas_limit: None,
            },
            transactions: Some(txs),
            no_tx_pool: Some(true),
            gas_limit: Some(u64::from_be_bytes(
                alloy_primitives::U64::from(sys_config.gas_limit).to_be_bytes(),
            )),
            eip_1559_params: sys_config.eip_1559_params(
                &self.rollup_cfg,
                l2_parent.block_info.timestamp,
                next_l2_time,
            ),
            min_base_fee: self
                .rollup_cfg
                .is_jovian_active(next_l2_time)
                .then(|| sys_config.min_base_fee.unwrap_or_default()), /* Default to zero if not
                                                                        * set at Jovian */
        })
    }

    fn seed_system_config(&mut self, block: &BaseBlock) {
        match to_system_config(block, &self.rollup_cfg) {
            Ok(config) => self.cache_system_config(block.header.hash_slow(), config),
            // A block that cannot be decoded is simply not seeded: the next build on it falls
            // back to the EL read.
            Err(err) => {
                warn!(target: "attributes", error = ?err, number = block.header.number, "Failed to decode system config from inserted block");
            }
        }
    }
}

/// Derive deposits as `Vec<Bytes>` for transaction receipts.
///
/// Successful deposits must be emitted by the deposit contract and have the correct event
/// signature. So the receipt address must equal the specified deposit contract and the first topic
/// must be the [`Deposits::EVENT_ABI_HASH`].
async fn derive_deposits(
    block_hash: B256,
    receipts: &[Receipt],
    deposit_contract: Address,
) -> Result<Vec<Bytes>, PipelineEncodingError> {
    let mut global_index = 0;
    let mut res = Vec::new();
    for r in receipts {
        if Eip658Value::Eip658(false) == r.status {
            continue;
        }
        for l in &r.logs {
            let curr_index = global_index;
            global_index += 1;
            if l.data.topics().first().is_none_or(|i| *i != Deposits::EVENT_ABI_HASH) {
                continue;
            }
            if l.address != deposit_contract {
                continue;
            }
            let decoded = Deposits::decode(block_hash, curr_index, l)?;
            res.push(decoded);
        }
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_consensus::{BlockBody, Header};
    use alloy_eips::eip2718::Decodable2718;
    use alloy_primitives::{B64, B256, Log, LogData, Sealed, U64, U256, address, bytes};
    use base_common_chains::Sepolia;
    use base_common_consensus::{BaseTxEnvelope, SystemAddresses, TxDeposit};
    use base_common_genesis::{
        BaseUpgradeConfig, ChainGenesis, SystemConfig, SystemConfigUpdate, UpgradeConfig,
    };
    use base_protocol::{
        BlockInfo, DepositDecodeError, test_utils::RAW_BEDROCK_INFO_TX,
    };

    use super::*;
    use crate::{
        errors::ResetError,
        test_utils::{TestChainProvider, TestSystemConfigL2Fetcher},
    };

    fn generate_valid_log() -> Log {
        let deposit_contract = address!("1111111111111111111111111111111111111111");
        let mut data = vec![0u8; 192];
        let offset: [u8; 8] = U64::from(32).to_be_bytes();
        data[24..32].copy_from_slice(&offset);
        let len: [u8; 8] = U64::from(128).to_be_bytes();
        data[56..64].copy_from_slice(&len);
        // Copy the u128 mint value
        let mint: [u8; 16] = 10_u128.to_be_bytes();
        data[80..96].copy_from_slice(&mint);
        // Copy the tx value
        let value: [u8; 32] = U256::from(100).to_be_bytes();
        data[96..128].copy_from_slice(&value);
        // Copy the gas limit
        let gas: [u8; 8] = 1000_u64.to_be_bytes();
        data[128..136].copy_from_slice(&gas);
        // Copy the isCreation flag
        data[136] = 1;
        let from = address!("2222222222222222222222222222222222222222");
        let mut from_bytes = vec![0u8; 32];
        from_bytes[12..32].copy_from_slice(from.as_slice());
        let to = address!("3333333333333333333333333333333333333333");
        let mut to_bytes = vec![0u8; 32];
        to_bytes[12..32].copy_from_slice(to.as_slice());
        Log {
            address: deposit_contract,
            data: LogData::new_unchecked(
                vec![
                    Deposits::EVENT_ABI_HASH,
                    B256::from_slice(&from_bytes),
                    B256::from_slice(&to_bytes),
                    B256::default(),
                ],
                Bytes::from(data),
            ),
        }
    }

    fn generate_valid_receipt() -> Receipt {
        let mut bad_dest_log = generate_valid_log();
        bad_dest_log.data.topics_mut()[1] = B256::default();
        let mut invalid_topic_log = generate_valid_log();
        invalid_topic_log.data.topics_mut()[0] = B256::default();
        Receipt {
            status: Eip658Value::Eip658(true),
            logs: vec![generate_valid_log(), bad_dest_log, invalid_topic_log],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_derive_deposits_empty() {
        let receipts = vec![];
        let deposit_contract = Address::default();
        let result = derive_deposits(B256::default(), &receipts, deposit_contract).await;
        assert!(result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_derive_deposits_non_deposit_events_filtered_out() {
        let deposit_contract = address!("1111111111111111111111111111111111111111");
        let mut invalid = generate_valid_receipt();
        invalid.logs[0].data = LogData::new_unchecked(vec![], Bytes::default());
        let receipts = vec![generate_valid_receipt(), generate_valid_receipt(), invalid];
        let result = derive_deposits(B256::default(), &receipts, deposit_contract).await;
        assert_eq!(result.unwrap().len(), 5);
    }

    #[tokio::test]
    async fn test_derive_deposits_non_deposit_contract_addr() {
        let deposit_contract = address!("1111111111111111111111111111111111111111");
        let mut invalid = generate_valid_receipt();
        invalid.logs[0].address = Address::default();
        let receipts = vec![generate_valid_receipt(), generate_valid_receipt(), invalid];
        let result = derive_deposits(B256::default(), &receipts, deposit_contract).await;
        assert_eq!(result.unwrap().len(), 5);
    }

    #[tokio::test]
    async fn test_derive_deposits_decoding_errors() {
        let deposit_contract = address!("1111111111111111111111111111111111111111");
        let mut invalid = generate_valid_receipt();
        invalid.logs[0].data =
            LogData::new_unchecked(vec![Deposits::EVENT_ABI_HASH], Bytes::default());
        let receipts = vec![generate_valid_receipt(), generate_valid_receipt(), invalid];
        let result = derive_deposits(B256::default(), &receipts, deposit_contract).await;
        let downcasted = result.unwrap_err();
        assert_eq!(downcasted, DepositDecodeError::UnexpectedTopicsLen(1).into());
    }

    #[tokio::test]
    async fn test_derive_deposits_succeeds() {
        let deposit_contract = address!("1111111111111111111111111111111111111111");
        let receipts = vec![generate_valid_receipt(), generate_valid_receipt()];
        let result = derive_deposits(B256::default(), &receipts, deposit_contract).await;
        assert_eq!(result.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn test_prepare_payload_block_mismatch_epoch_reset() {
        let cfg = Arc::new(RollupConfig::default());
        let l1_cfg = Arc::new(Sepolia::l1_config());
        let l2_number = 1;
        let mut fetcher = TestSystemConfigL2Fetcher::default();
        let l2_parent_hash =
            fallback_parent(&mut fetcher, Header { number: l2_number, ..Default::default() });
        let mut provider = TestChainProvider::default();
        let header = Header::default();
        let hash = header.hash_slow();
        provider.insert_header(hash, header);
        let mut builder =
            StatefulAttributesBuilder::new(Arc::clone(&cfg), l1_cfg, fetcher, provider);
        let epoch = BlockNumHash { hash, number: l2_number };
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo { hash: l2_parent_hash, number: l2_number, ..Default::default() },
            l1_origin: BlockNumHash { hash: B256::left_padding_from(&[0xFF]), number: 2 },
            seq_num: 0,
        };
        // This should error because the l2 parent's l1_origin.hash should equal the epoch header
        // hash. Here we use the default header whose hash will not equal the custom `l2_hash`.
        let expected =
            BuilderError::BlockMismatchEpochReset(epoch, l2_parent.l1_origin, B256::default());
        let err = builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap_err();
        assert_eq!(err, PipelineErrorKind::Reset(expected.into()));
    }

    #[tokio::test]
    async fn test_prepare_payload_block_mismatch() {
        let cfg = Arc::new(RollupConfig::default());
        let l1_cfg = Arc::new(Sepolia::l1_config());
        let l2_number = 1;
        let mut fetcher = TestSystemConfigL2Fetcher::default();
        let l2_parent_hash =
            fallback_parent(&mut fetcher, Header { number: l2_number, ..Default::default() });
        let mut provider = TestChainProvider::default();
        let header = Header::default();
        let hash = header.hash_slow();
        provider.insert_header(hash, header);
        let mut builder =
            StatefulAttributesBuilder::new(Arc::clone(&cfg), l1_cfg, fetcher, provider);
        let epoch = BlockNumHash { hash, number: l2_number };
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo { hash: l2_parent_hash, number: l2_number, ..Default::default() },
            l1_origin: BlockNumHash { hash: B256::ZERO, number: l2_number },
            seq_num: 0,
        };
        // This should error because the l2 parent's l1_origin.hash should equal the epoch hash
        // Here the default header is used whose hash will not equal the custom `l2_hash` above.
        let expected = BuilderError::BlockMismatch(epoch, l2_parent.l1_origin);
        let err = builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap_err();
        assert_eq!(err, PipelineErrorKind::Reset(ResetError::AttributesBuilder(expected)));
    }

    #[tokio::test]
    async fn test_prepare_payload_broken_time_invariant() {
        let block_time = 10_u64;
        let timestamp = 100_u64;
        let cfg = Arc::new(RollupConfig { block_time, ..Default::default() });
        let l1_cfg = Arc::new(Sepolia::l1_config());
        let l2_number = 1;
        let mut fetcher = TestSystemConfigL2Fetcher::default();
        let l2_parent_hash =
            fallback_parent(&mut fetcher, Header { number: l2_number, ..Default::default() });
        let mut provider = TestChainProvider::default();
        let header = Header { timestamp, ..Default::default() };
        let hash = header.hash_slow();
        provider.insert_header(hash, header);
        let mut builder =
            StatefulAttributesBuilder::new(Arc::clone(&cfg), l1_cfg, fetcher, provider);
        let epoch = BlockNumHash { hash, number: l2_number };
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo { hash: l2_parent_hash, number: l2_number, ..Default::default() },
            l1_origin: BlockNumHash { hash, number: l2_number },
            seq_num: 0,
        };
        let next_l2_time = cfg.l2_block_timestamp(l2_parent.block_info.number + 1);
        let block_id = BlockNumHash { hash, number: 0 };
        let expected = BuilderError::BrokenTimeInvariant(
            l2_parent.l1_origin,
            next_l2_time,
            block_id,
            timestamp,
        );
        let err = builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap_err();
        assert_eq!(err, PipelineErrorKind::Reset(ResetError::AttributesBuilder(expected)));
    }

    #[tokio::test]
    async fn test_prepare_payload_without_forks() {
        let block_time = 10_u64;
        let timestamp = 100_u64;
        let cfg = Arc::new(RollupConfig {
            block_time,
            genesis: ChainGenesis {
                l2_time: timestamp.saturating_sub(block_time),
                ..Default::default()
            },
            ..Default::default()
        });
        let l1_cfg = Arc::new(Sepolia::l1_config());
        let l2_number = 1;
        let mut fetcher = TestSystemConfigL2Fetcher::default();
        let l2_parent_hash =
            fallback_parent(&mut fetcher, Header { number: l2_number, ..Default::default() });
        let mut provider = TestChainProvider::default();
        let header = Header { timestamp, ..Default::default() };
        let prev_randao = header.mix_hash;
        let hash = header.hash_slow();
        provider.insert_header(hash, header);
        let mut builder =
            StatefulAttributesBuilder::new(Arc::clone(&cfg), l1_cfg, fetcher, provider);
        let epoch = BlockNumHash { hash, number: l2_number };
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo {
                hash: l2_parent_hash,
                number: l2_number,
                timestamp,
                parent_hash: hash,
            },
            l1_origin: BlockNumHash { hash, number: l2_number },
            seq_num: 0,
        };
        let next_l2_time = cfg.l2_block_timestamp(l2_parent.block_info.number + 1);
        let payload = builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        let expected = BasePayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: next_l2_time,
                prev_randao,
                suggested_fee_recipient: Predeploys::SEQUENCER_FEE_VAULT,
                parent_beacon_block_root: None,
                withdrawals: None,
                slot_number: None,
                target_gas_limit: None,
            },
            transactions: payload.transactions.clone(),
            no_tx_pool: Some(true),
            gas_limit: Some(u64::from_be_bytes(
                alloy_primitives::U64::from(SystemConfig::default().gas_limit).to_be_bytes(),
            )),
            eip_1559_params: None,
            min_base_fee: None,
        };
        assert_eq!(payload, expected);
        assert_eq!(payload.transactions.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_prepare_payload_inserts_base_time_update_at_tx_one() {
        let block_time = 2_u64;
        let timestamp = 100_u64;
        let chain_id = 9_100_004;
        let cfg = Arc::new(RollupConfig {
            block_time,
            genesis: ChainGenesis {
                l2_time: timestamp.saturating_sub(block_time),
                ..Default::default()
            },
            l2_chain_id: chain_id.into(),
            upgrades: UpgradeConfig {
                ecotone_time: Some(102),
                base: BaseUpgradeConfig { denim: Some(102), ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        });
        let l1_cfg = Arc::new(Sepolia::l1_config());
        let l2_number = 1;
        let mut fetcher = TestSystemConfigL2Fetcher::default();
        let l2_parent_hash =
            fallback_parent(&mut fetcher, Header { number: l2_number, ..Default::default() });
        let mut provider = TestChainProvider::default();
        let header = Header { timestamp, ..Default::default() };
        let hash = header.hash_slow();
        provider.insert_header(hash, header);
        let mut builder =
            StatefulAttributesBuilder::new(Arc::clone(&cfg), l1_cfg, fetcher, provider);
        let epoch = BlockNumHash { hash, number: l2_number };
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo {
                hash: l2_parent_hash,
                number: l2_number,
                timestamp,
                parent_hash: hash,
            },
            l1_origin: BlockNumHash { hash, number: l2_number },
            seq_num: 0,
        };

        let next_l2_block_number = l2_parent.block_info.number + 1;
        let (_, expected_millis_part) = cfg.l2_block_timestamp_parts(next_l2_block_number);
        let payload = builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        assert_eq!(
            payload.payload_attributes.timestamp,
            cfg.l2_block_timestamp(next_l2_block_number)
        );
        let transactions = payload.transactions.unwrap();
        assert_eq!(transactions.len(), 8);
        let envelope = BaseTxEnvelope::decode_2718_exact(&transactions[1]).unwrap();
        let deposit = envelope.as_deposit().unwrap();
        assert_eq!(deposit.from, SystemAddresses::DEPOSITOR_ACCOUNT);
        assert_eq!(deposit.to, alloy_primitives::TxKind::Call(Predeploys::BASE_TIME));
        assert_eq!(
            BaseTimeUpdateTx::decode_calldata(&deposit.input).unwrap().timestamp_millis_part(),
            expected_millis_part
        );
    }

    #[tokio::test]
    async fn test_prepare_payload_uses_denim_formula_for_subsequent_block() {
        let block_time = 2_u64;
        let timestamp = 100_u64;
        let chain_id = 9_100_005;
        let cfg = Arc::new(RollupConfig {
            block_time,
            genesis: ChainGenesis {
                l2_time: timestamp.saturating_sub(block_time),
                ..Default::default()
            },
            l2_chain_id: chain_id.into(),
            upgrades: UpgradeConfig {
                ecotone_time: Some(102),
                base: BaseUpgradeConfig { denim: Some(102), ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        });
        let l1_cfg = Arc::new(Sepolia::l1_config());
        let l2_number = 2;
        let mut fetcher = TestSystemConfigL2Fetcher::default();
        let l2_parent_hash =
            fallback_parent(&mut fetcher, Header { number: l2_number, ..Default::default() });
        let mut provider = TestChainProvider::default();
        let header = Header { timestamp, ..Default::default() };
        let hash = header.hash_slow();
        provider.insert_header(hash, header);
        let mut builder =
            StatefulAttributesBuilder::new(Arc::clone(&cfg), l1_cfg, fetcher, provider);
        let epoch = BlockNumHash { hash, number: l2_number };
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo {
                hash: l2_parent_hash,
                number: l2_number,
                timestamp: 102,
                parent_hash: hash,
            },
            l1_origin: BlockNumHash { hash, number: l2_number },
            seq_num: 0,
        };

        let next_l2_block_number = l2_parent.block_info.number + 1;
        let (expected_timestamp, expected_millis_part) =
            cfg.l2_block_timestamp_parts(next_l2_block_number);
        assert_eq!(expected_millis_part, 200);

        let payload = builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        assert_eq!(payload.payload_attributes.timestamp, expected_timestamp);
        let transactions = payload.transactions.unwrap();
        assert_eq!(transactions.len(), 2);
        let envelope = BaseTxEnvelope::decode_2718_exact(&transactions[1]).unwrap();
        let deposit = envelope.as_deposit().unwrap();
        assert_eq!(
            BaseTimeUpdateTx::decode_calldata(&deposit.input).unwrap().timestamp_millis_part(),
            expected_millis_part
        );
    }

    #[tokio::test]
    async fn test_prepare_payload_with_canyon() {
        let block_time = 10_u64;
        let timestamp = 100_u64;
        let cfg = Arc::new(RollupConfig {
            block_time,
            genesis: ChainGenesis {
                l2_time: timestamp.saturating_sub(block_time),
                ..Default::default()
            },
            upgrades: UpgradeConfig { canyon_time: Some(0), ..Default::default() },
            ..Default::default()
        });
        let l1_cfg = Arc::new(Sepolia::l1_config());
        let l2_number = 1;
        let mut fetcher = TestSystemConfigL2Fetcher::default();
        let l2_parent_hash =
            fallback_parent(&mut fetcher, Header { number: l2_number, ..Default::default() });
        let mut provider = TestChainProvider::default();
        let header = Header { timestamp, ..Default::default() };
        let prev_randao = header.mix_hash;
        let hash = header.hash_slow();
        provider.insert_header(hash, header);
        let mut builder =
            StatefulAttributesBuilder::new(Arc::clone(&cfg), l1_cfg, fetcher, provider);
        let epoch = BlockNumHash { hash, number: l2_number };
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo {
                hash: l2_parent_hash,
                number: l2_number,
                timestamp,
                parent_hash: hash,
            },
            l1_origin: BlockNumHash { hash, number: l2_number },
            seq_num: 0,
        };
        let next_l2_time = cfg.l2_block_timestamp(l2_parent.block_info.number + 1);
        let payload = builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        let expected = BasePayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: next_l2_time,
                prev_randao,
                suggested_fee_recipient: Predeploys::SEQUENCER_FEE_VAULT,
                parent_beacon_block_root: None,
                withdrawals: Some(Vec::default()),
                slot_number: None,
                target_gas_limit: None,
            },
            transactions: payload.transactions.clone(),
            no_tx_pool: Some(true),
            gas_limit: Some(u64::from_be_bytes(
                alloy_primitives::U64::from(SystemConfig::default().gas_limit).to_be_bytes(),
            )),
            eip_1559_params: None,
            min_base_fee: None,
        };
        assert_eq!(payload, expected);
        assert_eq!(payload.transactions.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_prepare_payload_with_ecotone() {
        let block_time = 2_u64;
        let timestamp = 100_u64;
        let cfg = Arc::new(RollupConfig {
            block_time,
            genesis: ChainGenesis {
                l2_time: timestamp.saturating_sub(block_time),
                ..Default::default()
            },
            upgrades: UpgradeConfig { ecotone_time: Some(102), ..Default::default() },
            ..Default::default()
        });
        let l1_cfg = Arc::new(Sepolia::l1_config());
        let l2_number = 1;
        let mut fetcher = TestSystemConfigL2Fetcher::default();
        let l2_parent_hash =
            fallback_parent(&mut fetcher, Header { number: l2_number, ..Default::default() });
        let mut provider = TestChainProvider::default();
        let header = Header { timestamp, ..Default::default() };
        let parent_beacon_block_root = Some(header.parent_beacon_block_root.unwrap_or_default());
        let prev_randao = header.mix_hash;
        let hash = header.hash_slow();
        provider.insert_header(hash, header);
        let mut builder =
            StatefulAttributesBuilder::new(Arc::clone(&cfg), l1_cfg, fetcher, provider);
        let epoch = BlockNumHash { hash, number: l2_number };
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo {
                hash: l2_parent_hash,
                number: l2_number,
                timestamp,
                parent_hash: hash,
            },
            l1_origin: BlockNumHash { hash, number: l2_number },
            seq_num: 0,
        };
        let next_l2_time = cfg.l2_block_timestamp(l2_parent.block_info.number + 1);
        let payload = builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        let expected = BasePayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: next_l2_time,
                prev_randao,
                suggested_fee_recipient: Predeploys::SEQUENCER_FEE_VAULT,
                parent_beacon_block_root,
                withdrawals: Some(vec![]),
                slot_number: None,
                target_gas_limit: None,
            },
            transactions: payload.transactions.clone(),
            no_tx_pool: Some(true),
            gas_limit: Some(u64::from_be_bytes(
                alloy_primitives::U64::from(SystemConfig::default().gas_limit).to_be_bytes(),
            )),
            eip_1559_params: None,
            min_base_fee: None,
        };
        assert_eq!(payload, expected);
        assert_eq!(payload.transactions.unwrap().len(), 7);
    }

    #[tokio::test]
    async fn test_prepare_payload_with_fjord() {
        let block_time = 2_u64;
        let timestamp = 100_u64;
        let cfg = Arc::new(RollupConfig {
            block_time,
            genesis: ChainGenesis {
                l2_time: timestamp.saturating_sub(block_time),
                ..Default::default()
            },
            upgrades: UpgradeConfig { fjord_time: Some(102), ..Default::default() },
            ..Default::default()
        });
        let l1_cfg = Arc::new(Sepolia::l1_config());
        let l2_number = 1;
        let mut fetcher = TestSystemConfigL2Fetcher::default();
        let l2_parent_hash =
            fallback_parent(&mut fetcher, Header { number: l2_number, ..Default::default() });
        let mut provider = TestChainProvider::default();
        let header = Header { timestamp, ..Default::default() };
        let prev_randao = header.mix_hash;
        let hash = header.hash_slow();
        provider.insert_header(hash, header);
        let mut builder =
            StatefulAttributesBuilder::new(Arc::clone(&cfg), l1_cfg, fetcher, provider);
        let epoch = BlockNumHash { hash, number: l2_number };
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo {
                hash: l2_parent_hash,
                number: l2_number,
                timestamp,
                parent_hash: hash,
            },
            l1_origin: BlockNumHash { hash, number: l2_number },
            seq_num: 0,
        };
        let next_l2_time = cfg.l2_block_timestamp(l2_parent.block_info.number + 1);
        let payload = builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        let expected = BasePayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: next_l2_time,
                prev_randao,
                suggested_fee_recipient: Predeploys::SEQUENCER_FEE_VAULT,
                parent_beacon_block_root: Some(B256::ZERO),
                withdrawals: Some(vec![]),
                slot_number: None,
                target_gas_limit: None,
            },
            transactions: payload.transactions.clone(),
            no_tx_pool: Some(true),
            gas_limit: Some(u64::from_be_bytes(
                alloy_primitives::U64::from(SystemConfig::default().gas_limit).to_be_bytes(),
            )),
            eip_1559_params: None,
            min_base_fee: None,
        };
        assert_eq!(payload.transactions.as_ref().unwrap().len(), 10);
        assert_eq!(payload, expected);
    }

    #[tokio::test]
    async fn test_syscfg_update_error_is_nonfatal() {
        let block_time = 10;
        let sys_config_addr = address!("1111111111111111111111111111111111111111");
        let cfg = Arc::new(RollupConfig {
            block_time,
            l1_system_config_address: sys_config_addr,
            ..Default::default()
        });
        let l1_cfg = Arc::new(Sepolia::l1_config());
        let l2_number = 1;
        let mut fetcher = TestSystemConfigL2Fetcher::default();
        let l2_parent_hash =
            fallback_parent(&mut fetcher, Header { number: l2_number, ..Default::default() });
        let mut provider = TestChainProvider::default();

        // The epoch header's parent_hash must match l2_parent.l1_origin.hash.
        let origin_hash = B256::left_padding_from(&[0xBB]);
        let header = Header { parent_hash: origin_hash, ..Default::default() };
        let epoch_hash = header.hash_slow();

        // Malformed system config log: CONFIG_UPDATE_TOPIC present but only 1 topic (needs >= 3),
        // causing update_with_receipts to return an error.
        let bad_log = Log {
            address: sys_config_addr,
            data: LogData::new_unchecked(vec![SystemConfigUpdate::TOPIC], Bytes::default()),
        };
        let bad_receipt = Receipt {
            status: Eip658Value::Eip658(true),
            logs: vec![bad_log],
            ..Default::default()
        };

        provider.insert_header(epoch_hash, header);
        provider.insert_receipts(epoch_hash, vec![bad_receipt]);

        let mut builder =
            StatefulAttributesBuilder::new(Arc::clone(&cfg), l1_cfg, fetcher, provider);
        let epoch = BlockNumHash { hash: epoch_hash, number: l2_number + 1 };
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo { hash: l2_parent_hash, number: l2_number, ..Default::default() },
            l1_origin: BlockNumHash { hash: origin_hash, number: l2_number },
            seq_num: 0,
        };

        // Should succeed despite the malformed system config receipt.
        assert!(builder.prepare_payload_attributes(l2_parent, epoch).await.is_ok());
    }

    /// Builds a rollup config with the requested block time and no scheduled upgrades.
    fn cache_test_cfg(block_time: u64, timestamp: u64) -> Arc<RollupConfig> {
        Arc::new(RollupConfig {
            block_time,
            genesis: ChainGenesis {
                l2_time: timestamp.saturating_sub(block_time),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    /// Sets up an L2 parent at block 1 with L1 origin block 1, and the next epoch's origin
    /// block 2 whose parent hash links back to it. Returns the builder, the epoch block's
    /// [`BlockNumHash`], and the first L2 parent.
    fn transition_setup() -> (
        StatefulAttributesBuilder<TestChainProvider, TestSystemConfigL2Fetcher>,
        BlockNumHash,
        L2BlockInfo,
    ) {
        let block_time = 2_u64;
        let timestamp = 100_u64;
        let cfg = cache_test_cfg(block_time, timestamp);
        let l1_cfg = Arc::new(Sepolia::l1_config());
        let mut fetcher = TestSystemConfigL2Fetcher::default();
        let l2_parent_hash =
            fallback_parent(&mut fetcher, Header { number: 1, timestamp, ..Default::default() });
        let mut provider = TestChainProvider::default();
        let parent_header = Header { number: 1, timestamp, ..Default::default() };
        let parent_hash = parent_header.hash_slow();
        let epoch_header = Header { number: 2, timestamp, parent_hash, ..Default::default() };
        let epoch_hash = epoch_header.hash_slow();
        provider.insert_header(parent_hash, parent_header);
        provider.insert_header(epoch_hash, epoch_header);
        provider.insert_receipts(epoch_hash, vec![]);
        let builder = StatefulAttributesBuilder::new(cfg, l1_cfg, fetcher, provider);
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo { hash: l2_parent_hash, number: 1, timestamp, parent_hash },
            l1_origin: BlockNumHash { hash: parent_hash, number: 1 },
            seq_num: 0,
        };
        (builder, BlockNumHash { hash: epoch_hash, number: 2 }, l2_parent)
    }

    /// Builds a minimal decodable L2 block: `to_system_config` requires an L1 info deposit as
    /// the first transaction.
    fn seedable_block(header: Header) -> BaseBlock {
        BaseBlock {
            header,
            body: BlockBody {
                transactions: vec![BaseTxEnvelope::Deposit(Sealed::new(TxDeposit {
                    input: Bytes::from(&RAW_BEDROCK_INFO_TX),
                    ..Default::default()
                }))],
                ..Default::default()
            },
        }
    }

    /// Installs a decodable block built from `header` into the fetcher and returns its hash:
    /// the EL fallback verifies the fetched block's hash against the parent hash, so tests
    /// exercising it need the parent hash to be a real block hash.
    fn fallback_parent(fetcher: &mut TestSystemConfigL2Fetcher, header: Header) -> B256 {
        let block = seedable_block(header);
        let hash = block.header.hash_slow();
        fetcher.insert_block(block.header.number, block);
        hash
    }

    #[tokio::test]
    async fn test_seeded_parent_config_skips_el_read() {
        let (mut builder, epoch, l2_parent) = transition_setup();

        // The parent's config was seeded when it was inserted; the EL can no longer serve any
        // config, so a cache miss would fail the build.
        builder.cache_system_config(l2_parent.block_info.hash, SystemConfig::default());
        builder.config_fetcher.clear();
        builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        assert!(builder.config_fetcher.block_calls.is_empty());
    }

    #[tokio::test]
    async fn test_unknown_parent_falls_back_to_el_and_caches() {
        let (mut builder, epoch, l2_parent) = transition_setup();

        // An unseeded parent (startup, reset, or a block built elsewhere) is read from the EL.
        builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        assert_eq!(builder.config_fetcher.block_calls, vec![1]);

        // A rebuild on the same parent is served from the cache even when the EL can no longer
        // serve it.
        builder.config_fetcher.clear();
        builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        assert_eq!(builder.config_fetcher.block_calls, vec![1]);
    }

    #[tokio::test]
    async fn test_same_origin_different_parent_misses_cache() {
        let (mut builder, epoch, l2_parent) = transition_setup();
        builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        assert_eq!(builder.config_fetcher.block_calls, vec![1]);

        // The next parent shares the L1 origin but is a different L2 block: entries are keyed
        // by block hash, so without a seed its config is read from the EL rather than reusing
        // the previous parent's entry.
        let cfg = Arc::clone(&builder.rollup_cfg);
        let timestamp = cfg.l2_block_timestamp(2);
        let hash = fallback_parent(
            &mut builder.config_fetcher,
            Header { number: 2, timestamp, ..Default::default() },
        );
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo {
                hash,
                number: 2,
                timestamp,
                parent_hash: l2_parent.block_info.hash,
            },
            l1_origin: epoch,
            seq_num: 1,
        };
        builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        assert_eq!(builder.config_fetcher.block_calls, vec![1, 2]);
    }

    /// A fallback read that returns a different block than the parent being built on (e.g. the
    /// EL reorged between the parent being chosen and the read) must reset instead of caching
    /// another block's config under the parent hash.
    #[tokio::test]
    async fn test_fallback_block_hash_mismatch_resets() {
        let (mut builder, epoch, mut l2_parent) = transition_setup();
        l2_parent.block_info.hash = B256::left_padding_from(&[0xAA]);
        let err = builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap_err();
        assert!(matches!(
            err,
            PipelineErrorKind::Reset(ResetError::AttributesBuilder(BuilderError::BlockMismatch(
                _,
                _
            )))
        ));
        // Nothing was cached under the mismatched parent hash.
        assert!(builder.cached_system_config(&l2_parent.block_info.hash).is_none());
    }

    #[tokio::test]
    async fn test_seeded_config_wins_over_el_value() {
        let (mut builder, epoch, l2_parent) = transition_setup();

        // The seeded entry differs from what the EL reports for the same block number (e.g.
        // after an L1 reorg changed the canonical block at that height): the exact-hash seed
        // must win.
        let seeded_gas_limit = SystemConfig::default().gas_limit + 1;
        builder.cache_system_config(
            l2_parent.block_info.hash,
            SystemConfig { gas_limit: seeded_gas_limit, ..Default::default() },
        );
        let payload = builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        assert_eq!(
            payload.gas_limit,
            Some(u64::from_be_bytes(alloy_primitives::U64::from(seeded_gas_limit).to_be_bytes()))
        );
        assert!(builder.config_fetcher.block_calls.is_empty());
    }

    #[test]
    fn test_system_config_cache_evicts_least_recently_used() {
        let mut builder = StatefulAttributesBuilder::new(
            Arc::new(RollupConfig::default()),
            Arc::new(Sepolia::l1_config()),
            TestSystemConfigL2Fetcher::default(),
            TestChainProvider::default(),
        );
        let hashes: Vec<B256> = (0..10).map(|i| B256::from([i as u8; 32])).collect();
        for &hash in &hashes {
            builder.cache_system_config(hash, SystemConfig::default());
        }
        assert_eq!(builder.system_configs.len(), MAX_SYSTEM_CONFIG_CACHE_ENTRIES);
        assert!(builder.cached_system_config(&hashes[0]).is_none());
        assert!(builder.cached_system_config(&hashes[9]).is_some());

        // A hit promotes the entry, evicting the next-oldest on the following insert.
        assert!(builder.cached_system_config(&hashes[2]).is_some());
        builder.cache_system_config(B256::from([10_u8; 32]), SystemConfig::default());
        assert!(builder.cached_system_config(&hashes[2]).is_some());
        assert!(builder.cached_system_config(&hashes[3]).is_none());
    }

    #[test]
    fn test_seed_system_config_caches_inserted_block() {
        let mut builder = StatefulAttributesBuilder::new(
            Arc::new(RollupConfig::default()),
            Arc::new(Sepolia::l1_config()),
            TestSystemConfigL2Fetcher::default(),
            TestChainProvider::default(),
        );
        let block =
            seedable_block(Header { number: 5, gas_limit: 40_000_000, ..Default::default() });
        builder.seed_system_config(&block);
        let expected = to_system_config(&block, &builder.rollup_cfg).unwrap();
        assert_eq!(builder.cached_system_config(&block.header.hash_slow()), Some(expected));
    }

    #[test]
    fn test_seed_system_config_undecodable_block_is_skipped() {
        let mut builder = StatefulAttributesBuilder::new(
            Arc::new(RollupConfig::default()),
            Arc::new(Sepolia::l1_config()),
            TestSystemConfigL2Fetcher::default(),
            TestChainProvider::default(),
        );
        // No transactions: `to_system_config` fails, so nothing is cached and the next build on
        // this block takes the EL fallback.
        let block = BaseBlock {
            header: Header { number: 5, ..Default::default() },
            body: BlockBody::default(),
        };
        builder.seed_system_config(&block);
        assert!(builder.system_configs.is_empty());
    }

    /// Crossing a fork that changes the system config encoding (here Holocene, which moves the
    /// EIP-1559 parameters into the header's `extra_data`) needs no EL read: the first
    /// post-fork block is seeded from its own payload after insertion, so the next build
    /// decodes the new fields directly from the seed.
    #[tokio::test]
    async fn test_fork_transition_seeded_parent_skips_el_read() {
        let block_time = 2_u64;
        // Blocks: 1 -> ts 100 (pre-Holocene), 2 -> ts 102 (first Holocene), 3 -> ts 104.
        let cfg = Arc::new(RollupConfig {
            block_time,
            genesis: ChainGenesis { l2_time: 98, ..Default::default() },
            upgrades: UpgradeConfig { holocene_time: Some(102), ..Default::default() },
            ..Default::default()
        });
        let l1_cfg = Arc::new(Sepolia::l1_config());
        let mut provider = TestChainProvider::default();
        let origin_header = Header { number: 1, timestamp: 100, ..Default::default() };
        let origin_hash = origin_header.hash_slow();
        provider.insert_header(origin_hash, origin_header);
        let mut builder = StatefulAttributesBuilder::new(
            Arc::clone(&cfg),
            l1_cfg,
            TestSystemConfigL2Fetcher::default(),
            provider,
        );
        let epoch = BlockNumHash { hash: origin_hash, number: 1 };

        // The sequencer inserted block 2 (the first Holocene block), whose `extra_data`
        // persists denominator 250 and elasticity 6, and seeded it.
        let inserted = seedable_block(Header {
            number: 2,
            timestamp: 102,
            extra_data: bytes!("00000000fa00000006"),
            ..Default::default()
        });
        builder.seed_system_config(&inserted);

        // Build block 3 on the seeded post-fork parent: the persisted parameters come from the
        // seed and the EL is never consulted.
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo {
                hash: inserted.header.hash_slow(),
                number: 2,
                timestamp: 102,
                parent_hash: B256::ZERO,
            },
            l1_origin: epoch,
            seq_num: 1,
        };
        let payload = builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        assert_eq!(
            payload.eip_1559_params,
            Some(B64::from_slice(&[250_u32.to_be_bytes(), 6_u32.to_be_bytes()].concat()))
        );
        assert!(builder.config_fetcher.block_calls.is_empty());
    }
}
