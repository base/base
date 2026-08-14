//! The [`AttributesBuilder`] and it's default implementation.

use alloc::{boxed::Box, fmt::Debug, string::ToString, sync::Arc, vec, vec::Vec};

use alloy_consensus::{Eip658Value, Receipt};
use alloy_eips::{BlockNumHash, eip2718::Encodable2718};
use alloy_genesis::ChainConfig;
use alloy_primitives::{Address, B256, Bytes, Sealed};
use alloy_rlp::Encodable;
use alloy_rpc_types_engine::PayloadAttributes;
use async_trait::async_trait;
use base_common_consensus::{BaseBlock, Predeploys};
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_consensus_upgrades::{Upgrade, Upgrades};
use base_protocol::{BaseTimeUpdateTx, Deposits, L1BlockInfoTx, L2BlockInfo, to_system_config};
use tracing::warn;

use crate::{
    AttributesBuilder, BuilderError, ChainProvider, L2ChainProvider, PipelineEncodingError,
    PipelineError, PipelineErrorKind, PipelineResult,
};

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
        }
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
        parent_block: Option<&Sealed<BaseBlock>>,
    ) -> PipelineResult<BasePayloadAttributes> {
        let l1_header;
        let deposit_transactions: Vec<Bytes>;

        let next_l2_block_number = l2_parent.block_info.number + 1;
        let (next_l2_time, next_l2_timestamp_millis_part) =
            self.rollup_cfg.l2_block_timestamp_parts(next_l2_block_number);

        // Prefer the parent payload the caller already has (the sequencer just built it).
        // Hash must match so a stashed block from before a reorg cannot be used. Decode
        // failure falls through to the EL.
        let mut sys_config = match parent_block
            .filter(|block| block.hash() == l2_parent.block_info.hash)
            .and_then(|block| match to_system_config(block.inner(), &self.rollup_cfg) {
                Ok(config) => Some(config),
                Err(err) => {
                    warn!(
                        target: "attributes",
                        error = ?err,
                        number = block.header.number,
                        "Failed to decode system config from parent payload"
                    );
                    None
                }
            }) {
            Some(config) => config,
            None => self
                .config_fetcher
                .system_config_by_number(l2_parent.block_info.number, Arc::clone(&self.rollup_cfg))
                .await
                .map_err(Into::into)?,
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
    use base_protocol::{BlockInfo, DepositDecodeError, test_utils::RAW_BEDROCK_INFO_TX};

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
        let err = builder.prepare_payload_attributes(l2_parent, epoch, None).await.unwrap_err();
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
        let err = builder.prepare_payload_attributes(l2_parent, epoch, None).await.unwrap_err();
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
        let err = builder.prepare_payload_attributes(l2_parent, epoch, None).await.unwrap_err();
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
        let payload = builder.prepare_payload_attributes(l2_parent, epoch, None).await.unwrap();
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
        let payload = builder.prepare_payload_attributes(l2_parent, epoch, None).await.unwrap();
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

        let payload = builder.prepare_payload_attributes(l2_parent, epoch, None).await.unwrap();
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
        let payload = builder.prepare_payload_attributes(l2_parent, epoch, None).await.unwrap();
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
        let payload = builder.prepare_payload_attributes(l2_parent, epoch, None).await.unwrap();
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
        let payload = builder.prepare_payload_attributes(l2_parent, epoch, None).await.unwrap();
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
        assert!(builder.prepare_payload_attributes(l2_parent, epoch, None).await.is_ok());
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

    /// Memoizes a block's header hash for parent-payload tests.
    fn sealed_block(block: BaseBlock) -> Sealed<BaseBlock> {
        let hash = block.header.hash_slow();
        Sealed::new_unchecked(block, hash)
    }

    /// Registers a fallback [`SystemConfig`] for `header`'s block number and returns the
    /// header's hash to use as the L2 parent hash.
    fn fallback_parent(fetcher: &mut TestSystemConfigL2Fetcher, header: Header) -> B256 {
        fetcher.insert(header.number, SystemConfig::default());
        header.hash_slow()
    }

    #[tokio::test]
    async fn test_parent_block_skips_el_read() {
        let (mut builder, epoch, mut l2_parent) = transition_setup();
        let parent = sealed_block(seedable_block(Header {
            number: 1,
            timestamp: 100,
            ..Default::default()
        }));
        l2_parent.block_info.hash = parent.hash();
        builder.config_fetcher.clear();
        builder.prepare_payload_attributes(l2_parent, epoch, Some(&parent)).await.unwrap();
        assert!(builder.config_fetcher.system_config_calls.is_empty());
    }

    #[tokio::test]
    async fn test_missing_parent_block_falls_back_to_el() {
        let (mut builder, epoch, l2_parent) = transition_setup();
        builder.prepare_payload_attributes(l2_parent, epoch, None).await.unwrap();
        assert_eq!(builder.config_fetcher.system_config_calls, vec![1]);
    }

    #[tokio::test]
    async fn test_parent_block_hash_mismatch_falls_back_to_el() {
        let (mut builder, epoch, l2_parent) = transition_setup();
        let other = sealed_block(seedable_block(Header { number: 99, ..Default::default() }));
        builder.prepare_payload_attributes(l2_parent, epoch, Some(&other)).await.unwrap();
        assert_eq!(builder.config_fetcher.system_config_calls, vec![1]);
    }

    #[tokio::test]
    async fn test_parent_block_wins_over_el_value() {
        let (mut builder, epoch, mut l2_parent) = transition_setup();
        let parent = sealed_block(seedable_block(Header {
            number: 1,
            timestamp: 100,
            gas_limit: 40_000_000,
            ..Default::default()
        }));
        l2_parent.block_info.hash = parent.hash();
        let payload =
            builder.prepare_payload_attributes(l2_parent, epoch, Some(&parent)).await.unwrap();
        assert_eq!(payload.gas_limit, Some(40_000_000));
        assert!(builder.config_fetcher.system_config_calls.is_empty());
    }

    #[tokio::test]
    async fn test_undecodable_parent_block_falls_back_to_el() {
        let (mut builder, epoch, l2_parent) = transition_setup();
        // Same header hash as the parent, but no L1 info tx, so `to_system_config` fails.
        let undecodable = sealed_block(BaseBlock {
            header: Header { number: 1, timestamp: 100, ..Default::default() },
            body: BlockBody::default(),
        });
        assert_eq!(undecodable.hash(), l2_parent.block_info.hash);
        builder.prepare_payload_attributes(l2_parent, epoch, Some(&undecodable)).await.unwrap();
        assert_eq!(builder.config_fetcher.system_config_calls, vec![1]);
    }

    /// Crossing a fork that changes the system config encoding (here Holocene, which moves the
    /// EIP-1559 parameters into the header's `extra_data`) needs no EL read: the first
    /// post-fork block is passed in as the parent payload, so the next build decodes the new
    /// fields directly from it.
    #[tokio::test]
    async fn test_fork_transition_parent_block_skips_el_read() {
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
        // persists denominator 250 and elasticity 6.
        let inserted = sealed_block(seedable_block(Header {
            number: 2,
            timestamp: 102,
            extra_data: bytes!("00000000fa00000006"),
            ..Default::default()
        }));

        // Build block 3 on the post-fork parent payload: the persisted parameters come from
        // the payload and the EL is never consulted.
        let l2_parent = L2BlockInfo {
            block_info: BlockInfo {
                hash: inserted.hash(),
                number: 2,
                timestamp: 102,
                parent_hash: B256::ZERO,
            },
            l1_origin: epoch,
            seq_num: 1,
        };
        let payload =
            builder.prepare_payload_attributes(l2_parent, epoch, Some(&inserted)).await.unwrap();
        assert_eq!(
            payload.eip_1559_params,
            Some(B64::from_slice(&[250_u32.to_be_bytes(), 6_u32.to_be_bytes()].concat()))
        );
        assert!(builder.config_fetcher.system_config_calls.is_empty());
    }
}
