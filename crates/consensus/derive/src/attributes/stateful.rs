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
use base_common_consensus::Predeploys;
use base_common_genesis::{BaseUpgrade, RollupConfig, SystemConfig};
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_consensus_upgrades::{Upgrade, Upgrades};
use base_protocol::{BaseTimeUpdateTx, Deposits, L1BlockInfoTx, L2BlockInfo};
use tracing::warn;

use crate::{
    AttributesBuilder, BuilderError, ChainProvider, L2ChainProvider, PipelineEncodingError,
    PipelineError, PipelineErrorKind, PipelineResult,
};

/// The maximum number of [`SystemConfig`]s cached by L1 origin hash.
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
    /// Cache of [`SystemConfig`]s keyed by L1 origin hash, most recently used first.
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

    /// Returns the cached [`SystemConfig`] for the given L1 origin hash, promoting it to most
    /// recently used.
    fn cached_system_config(&mut self, origin: &B256) -> Option<SystemConfig> {
        let index = self.system_configs.iter().position(|(hash, _)| hash == origin)?;
        let entry = self.system_configs.remove(index)?;
        let config = entry.1;
        self.system_configs.push_front(entry);
        Some(config)
    }

    /// Caches the [`SystemConfig`] for the given L1 origin hash, evicting the least recently used
    /// entries beyond [`MAX_SYSTEM_CONFIG_CACHE_ENTRIES`].
    fn cache_system_config(&mut self, origin: B256, config: SystemConfig) {
        if let Some(index) = self.system_configs.iter().position(|(hash, _)| hash == &origin) {
            self.system_configs.remove(index);
        }
        self.system_configs.push_front((origin, config));
        self.system_configs.truncate(MAX_SYSTEM_CONFIG_CACHE_ENTRIES);
    }
}

#[async_trait]
impl<L1P, L2P> AttributesBuilder for StatefulAttributesBuilder<L1P, L2P>
where
    L1P: ChainProvider + Debug + Send,
    L2P: L2ChainProvider + Debug + Send,
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

        // The system config for the parent's L1 origin: carried forward in memory, read from
        // the L2 EL only when the origin is unknown (startup, reset, reorg, or immediately after
        // a network upgrade).
        let mut sys_config = match self.cached_system_config(&l2_parent.l1_origin.hash) {
            Some(config) => config,
            None => {
                let config = self
                    .config_fetcher
                    .system_config_by_number(
                        l2_parent.block_info.number,
                        Arc::clone(&self.rollup_cfg),
                    )
                    .await
                    .map_err(Into::into)?;
                self.cache_system_config(l2_parent.l1_origin.hash, config);
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
                // A reset means the carried-forward configs may not match the chain the
                // pipeline resets to; drop them so the EL is re-read after the reset.
                self.system_configs.clear();
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
            // Cache the config of the new origin: subsequent blocks in this epoch use it
            // without consulting the EL.
            self.cache_system_config(epoch.hash, sys_config);
            l1_header = header;
            deposit_transactions = deposits;
            0
        } else if l2_parent.l1_origin.hash != epoch.hash {
            // A reset means the carried-forward configs may not match the chain the pipeline
            // resets to; drop them so the EL is re-read after the reset.
            self.system_configs.clear();
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

        // A fork may change which SystemConfig fields can be decoded from an L2 block. Drop the
        // carried value at every configured fork boundary so the next build decodes the new
        // format from the EL. Iterating the canonical upgrade list automatically covers future
        // forks; the extra read happens only once per fork.
        if BaseUpgrade::VARIANTS.iter().any(|&upgrade| {
            self.rollup_cfg.upgrade_activation_timestamp(upgrade).is_some_and(|activation| {
                l2_parent.block_info.timestamp < activation && activation <= next_l2_time
            })
        }) {
            self.system_configs.clear();
        }

        // Sanity check the L1 origin was correctly selected to maintain the time invariant
        // between L1 and L2.
        if next_l2_time < l1_header.timestamp {
            // A reset means the carried-forward configs may not match the chain the pipeline
            // resets to; drop them so the EL is re-read after the reset.
            self.system_configs.clear();
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

    use alloy_consensus::Header;
    use alloy_eips::eip2718::Decodable2718;
    use alloy_primitives::{B64, B256, Log, LogData, U64, U256, address};
    use base_common_chains::Sepolia;
    use base_common_consensus::{BaseTxEnvelope, SystemAddresses};
    use base_common_genesis::{
        BaseUpgradeConfig, ChainGenesis, SystemConfig, SystemConfigUpdate, UpgradeConfig,
    };
    use base_protocol::{BlockInfo, DepositDecodeError};

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
        fetcher.insert(l2_number, SystemConfig::default());
        let mut provider = TestChainProvider::default();
        let header = Header::default();
        let hash = header.hash_slow();
        provider.insert_header(hash, header);
        let mut builder =
            StatefulAttributesBuilder::new(Arc::clone(&cfg), l1_cfg, fetcher, provider);
        let epoch = BlockNumHash { hash, number: l2_number };
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo { hash: B256::ZERO, number: l2_number, ..Default::default() },
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
        fetcher.insert(l2_number, SystemConfig::default());
        let mut provider = TestChainProvider::default();
        let header = Header::default();
        let hash = header.hash_slow();
        provider.insert_header(hash, header);
        let mut builder =
            StatefulAttributesBuilder::new(Arc::clone(&cfg), l1_cfg, fetcher, provider);
        let epoch = BlockNumHash { hash, number: l2_number };
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo { hash: B256::ZERO, number: l2_number, ..Default::default() },
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
        fetcher.insert(l2_number, SystemConfig::default());
        let mut provider = TestChainProvider::default();
        let header = Header { timestamp, ..Default::default() };
        let hash = header.hash_slow();
        provider.insert_header(hash, header);
        let mut builder =
            StatefulAttributesBuilder::new(Arc::clone(&cfg), l1_cfg, fetcher, provider);
        let epoch = BlockNumHash { hash, number: l2_number };
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo { hash: B256::ZERO, number: l2_number, ..Default::default() },
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
        fetcher.insert(l2_number, SystemConfig::default());
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
                hash: B256::ZERO,
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
        fetcher.insert(l2_number, SystemConfig::default());
        let mut provider = TestChainProvider::default();
        let header = Header { timestamp, ..Default::default() };
        let hash = header.hash_slow();
        provider.insert_header(hash, header);
        let mut builder =
            StatefulAttributesBuilder::new(Arc::clone(&cfg), l1_cfg, fetcher, provider);
        let epoch = BlockNumHash { hash, number: l2_number };
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::ZERO,
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
        fetcher.insert(l2_number, SystemConfig::default());
        let mut provider = TestChainProvider::default();
        let header = Header { timestamp, ..Default::default() };
        let hash = header.hash_slow();
        provider.insert_header(hash, header);
        let mut builder =
            StatefulAttributesBuilder::new(Arc::clone(&cfg), l1_cfg, fetcher, provider);
        let epoch = BlockNumHash { hash, number: l2_number };
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::ZERO,
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
        fetcher.insert(l2_number, SystemConfig::default());
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
                hash: B256::ZERO,
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
        fetcher.insert(l2_number, SystemConfig::default());
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
                hash: B256::ZERO,
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
        fetcher.insert(l2_number, SystemConfig::default());
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
                hash: B256::ZERO,
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
        fetcher.insert(l2_number, SystemConfig::default());
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
            block_info: BlockInfo { hash: B256::ZERO, number: l2_number, ..Default::default() },
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
        fetcher.insert(1, SystemConfig::default());
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
            block_info: BlockInfo { hash: B256::ZERO, number: 1, timestamp, parent_hash },
            l1_origin: BlockNumHash { hash: parent_hash, number: 1 },
            seq_num: 0,
        };
        (builder, BlockNumHash { hash: epoch_hash, number: 2 }, l2_parent)
    }

    /// Advances to the next L2 parent within the same L1 origin.
    fn next_l2_parent(prev: &L2BlockInfo, cfg: &RollupConfig, epoch: BlockNumHash) -> L2BlockInfo {
        let number = prev.block_info.number + 1;
        L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::ZERO,
                number,
                timestamp: cfg.l2_block_timestamp(number),
                parent_hash: prev.block_info.hash,
            },
            l1_origin: epoch,
            seq_num: prev.seq_num + 1,
        }
    }

    #[tokio::test]
    async fn test_system_config_carried_forward_within_epoch() {
        let (mut builder, epoch, l2_parent) = transition_setup();

        // Epoch transition: reads the EL once for the parent origin's config.
        builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        assert_eq!(builder.config_fetcher.system_config_calls, vec![1]);

        // Same-epoch blocks reuse the carried-forward config even when the EL can no longer
        // serve it.
        builder.config_fetcher.clear();
        let cfg = Arc::clone(&builder.rollup_cfg);
        let l2_parent = next_l2_parent(&l2_parent, &cfg, epoch);
        builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        let l2_parent = next_l2_parent(&l2_parent, &cfg, epoch);
        builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        assert_eq!(builder.config_fetcher.system_config_calls, vec![1]);
    }

    #[tokio::test]
    async fn test_system_config_cache_ignores_stale_el_value() {
        let (mut builder, epoch, l2_parent) = transition_setup();

        builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        let cached_gas_limit = SystemConfig::default().gas_limit;

        // The EL now reports a different config for the next block; the carried-forward config
        // must win because it already includes the new epoch's updates.
        let stale = SystemConfig { gas_limit: cached_gas_limit + 1, ..Default::default() };
        builder.config_fetcher.insert(2, stale);

        let cfg = Arc::clone(&builder.rollup_cfg);
        let l2_parent = next_l2_parent(&l2_parent, &cfg, epoch);
        let payload = builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        assert_eq!(
            payload.gas_limit,
            Some(u64::from_be_bytes(alloy_primitives::U64::from(cached_gas_limit).to_be_bytes()))
        );
        assert_eq!(builder.config_fetcher.system_config_calls, vec![1]);
    }

    #[tokio::test]
    async fn test_unknown_l1_origin_falls_back_to_el() {
        let (mut builder, epoch, l2_parent) = transition_setup();
        builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        assert_eq!(builder.config_fetcher.system_config_calls, vec![1]);

        // A reorged L1 origin (same number, unknown hash) is not in the cache: the config is
        // read from the EL.
        let reorged_header = Header { number: 2, timestamp: 100, ..Default::default() };
        let reorged_hash = reorged_header.hash_slow();
        builder.receipts_fetcher.insert_header(reorged_hash, reorged_header);
        builder.config_fetcher.insert(2, SystemConfig::default());

        let cfg = Arc::clone(&builder.rollup_cfg);
        let mut l2_parent =
            next_l2_parent(&l2_parent, &cfg, BlockNumHash { hash: reorged_hash, number: 2 });
        l2_parent.l1_origin = BlockNumHash { hash: reorged_hash, number: 2 };
        let epoch = BlockNumHash { hash: reorged_hash, number: 2 };
        builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        assert_eq!(builder.config_fetcher.system_config_calls, vec![1, 2]);
    }

    #[test]
    fn test_system_config_cache_evicts_least_recently_used() {
        let mut builder = StatefulAttributesBuilder::new(
            Arc::new(RollupConfig::default()),
            Arc::new(Sepolia::l1_config()),
            TestSystemConfigL2Fetcher::default(),
            TestChainProvider::default(),
        );
        let origins: Vec<B256> = (0..10).map(|i| B256::from([i as u8; 32])).collect();
        for &origin in &origins {
            builder.cache_system_config(origin, SystemConfig::default());
        }
        assert_eq!(builder.system_configs.len(), MAX_SYSTEM_CONFIG_CACHE_ENTRIES);
        assert!(builder.cached_system_config(&origins[0]).is_none());
        assert!(builder.cached_system_config(&origins[9]).is_some());

        // A hit promotes the entry, evicting the next-oldest on the following insert.
        assert!(builder.cached_system_config(&origins[2]).is_some());
        builder.cache_system_config(B256::from([10_u8; 32]), SystemConfig::default());
        assert!(builder.cached_system_config(&origins[2]).is_some());
        assert!(builder.cached_system_config(&origins[3]).is_none());
    }

    /// Crossing a fork that changes the system config encoding (here Holocene, which moves the
    /// EIP-1559 parameters into the header's `extra_data`) must force one EL re-read: the
    /// pre-fork cached config has no EIP-1559 parameters, and carrying it forward would emit
    /// zeroed params where the EL decodes the persisted values from the parent block.
    #[tokio::test]
    async fn test_fork_boundary_forces_el_reseed() {
        let block_time = 2_u64;
        // Blocks: 1 -> ts 100 (pre-Holocene), 2 -> ts 102 (first Holocene), 3 -> ts 104.
        let cfg = Arc::new(RollupConfig {
            block_time,
            genesis: ChainGenesis { l2_time: 98, ..Default::default() },
            upgrades: UpgradeConfig { holocene_time: Some(102), ..Default::default() },
            ..Default::default()
        });
        let l1_cfg = Arc::new(Sepolia::l1_config());
        let mut fetcher = TestSystemConfigL2Fetcher::default();
        fetcher.insert(1, SystemConfig::default());
        let mut provider = TestChainProvider::default();
        let origin_header = Header { number: 1, timestamp: 100, ..Default::default() };
        let origin_hash = origin_header.hash_slow();
        provider.insert_header(origin_hash, origin_header);
        let mut builder =
            StatefulAttributesBuilder::new(Arc::clone(&cfg), l1_cfg, fetcher, provider);
        let epoch = BlockNumHash { hash: origin_hash, number: 1 };

        // Build block 2 (the Holocene transition block) on the pre-fork parent: seeds the
        // cache from the EL and emits the zeroed transition sentinel.
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::ZERO,
                number: 1,
                timestamp: 100,
                ..Default::default()
            },
            l1_origin: epoch,
            seq_num: 0,
        };
        let payload = builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        assert_eq!(payload.eip_1559_params, Some(B64::ZERO));
        assert_eq!(builder.config_fetcher.system_config_calls, vec![1]);

        // The EL's view of block 2 decodes the persisted EIP-1559 parameters from its
        // `extra_data`; the pre-fork cached config has neither.
        let post_fork = SystemConfig {
            eip1559_denominator: Some(250),
            eip1559_elasticity: Some(6),
            ..Default::default()
        };
        builder.config_fetcher.insert(2, post_fork);

        // Build block 3 on the post-fork parent: the transition cleared the cache, so the EL is
        // re-read, yielding the persisted parameters instead of zeros.
        let l2_parent = next_l2_parent(&l2_parent, &cfg, epoch);
        let payload = builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        assert_eq!(
            payload.eip_1559_params,
            Some(B64::from_slice(&[250_u32.to_be_bytes(), 6_u32.to_be_bytes()].concat()))
        );
        assert_eq!(builder.config_fetcher.system_config_calls, vec![1, 2]);

        // Build block 4 without another fork, served from the re-seeded cache.
        let l2_parent = next_l2_parent(&l2_parent, &cfg, epoch);
        builder.prepare_payload_attributes(l2_parent, epoch).await.unwrap();
        assert_eq!(builder.config_fetcher.system_config_calls, vec![1, 2]);
    }

    /// A Reset-class error (here a broken time invariant) must clear the carried-forward
    /// configs: after the pipeline resets, the chain may differ, so the EL is re-read.
    #[tokio::test]
    async fn test_reset_error_clears_config_cache() {
        let (mut builder, _epoch, l2_parent) = transition_setup();
        let origin = l2_parent.l1_origin;

        // Same-epoch build seeds the cache.
        builder.prepare_payload_attributes(l2_parent, origin).await.unwrap();
        assert_eq!(builder.config_fetcher.system_config_calls, vec![1]);

        // An epoch transition to an origin whose timestamp breaks the time invariant returns a
        // Reset error and clears the cache.
        let bad_header =
            Header { number: 2, timestamp: 1_000, parent_hash: origin.hash, ..Default::default() };
        let bad_hash = bad_header.hash_slow();
        builder.receipts_fetcher.insert_header(bad_hash, bad_header);
        builder.receipts_fetcher.insert_receipts(bad_hash, vec![]);
        let bad_epoch = BlockNumHash { hash: bad_hash, number: 2 };
        let err = builder.prepare_payload_attributes(l2_parent, bad_epoch).await.unwrap_err();
        assert!(matches!(err, PipelineErrorKind::Reset(_)));
        assert!(builder.system_configs.is_empty());

        // The next build re-reads the EL instead of trusting a pre-reset config.
        builder.prepare_payload_attributes(l2_parent, origin).await.unwrap();
        assert_eq!(builder.config_fetcher.system_config_calls, vec![1, 1]);
    }
}
