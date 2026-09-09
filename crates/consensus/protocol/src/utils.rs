//! Utility methods used by protocol types.

use alloc::string::ToString;

use alloy_primitives::{B256, Bytes, U256};
use base_common_types_chain::{BaseBlock, BaseTxEnvelope, JovianExtraData, Transaction, Typed2718};
use base_common_genesis::{RollupConfig, SystemConfig};
use base_common_types_payload::BaseExecutionPayload;

use crate::{
    BaseBlockConversionError, L1BlockInfoBedrockOnlyFields as _, L1BlockInfoEcotoneBaseFields as _,
    L1BlockInfoTx,
};

/// Converts the [`BaseBlock`] to a partial [`SystemConfig`].
pub fn to_system_config(
    block: &BaseBlock,
    rollup_config: &RollupConfig,
) -> Result<SystemConfig, BaseBlockConversionError> {
    let block_hash = block.header.hash_slow();
    if let Some(config) = genesis_system_config(block.header.number, block_hash, rollup_config)? {
        return Ok(config);
    }

    let first_tx = block
        .body
        .transactions
        .first()
        .ok_or(BaseBlockConversionError::EmptyTransactions(block_hash))?;
    system_config_from_transaction(
        first_tx,
        block.header.timestamp,
        block.header.gas_limit,
        &block.header.extra_data,
        rollup_config,
    )
}

/// Converts the [`BaseExecutionPayload`] to a partial [`SystemConfig`].
pub fn to_system_config_from_payload(
    payload: &BaseExecutionPayload,
    rollup_config: &RollupConfig,
) -> Result<SystemConfig, BaseBlockConversionError> {
    let block_hash = payload.block_hash();
    if let Some(config) = genesis_system_config(payload.block_number(), block_hash, rollup_config)?
    {
        return Ok(config);
    }

    let first_tx = payload
        .decoded_transactions::<BaseTxEnvelope>()
        .next()
        .ok_or(BaseBlockConversionError::EmptyTransactions(block_hash))?
        .map_err(|error| BaseBlockConversionError::InvalidTransactionEncoding(error.to_string()))?;
    system_config_from_transaction(
        &first_tx,
        payload.timestamp(),
        payload.gas_limit(),
        &payload.as_v1().extra_data,
        rollup_config,
    )
}

fn genesis_system_config(
    block_number: u64,
    block_hash: B256,
    rollup_config: &RollupConfig,
) -> Result<Option<SystemConfig>, BaseBlockConversionError> {
    if block_number != rollup_config.genesis.l2.number {
        return Ok(None);
    }
    if block_hash != rollup_config.genesis.l2.hash {
        return Err(BaseBlockConversionError::InvalidGenesisHash(
            rollup_config.genesis.l2.hash,
            block_hash,
        ));
    }
    rollup_config
        .genesis
        .system_config
        .map(Some)
        .ok_or(BaseBlockConversionError::MissingSystemConfigGenesis)
}

fn system_config_from_transaction(
    first_tx: &BaseTxEnvelope,
    _timestamp: u64,
    gas_limit: u64,
    extra_data: &Bytes,
    _rollup_config: &RollupConfig,
) -> Result<SystemConfig, BaseBlockConversionError> {
    let Some(tx) = first_tx.as_deposit() else {
        return Err(BaseBlockConversionError::InvalidTxType(first_tx.ty()));
    };

    let l1_info = L1BlockInfoTx::decode_calldata(tx.input().as_ref())?;
    let l1_fee_scalar = match l1_info {
        L1BlockInfoTx::Bedrock(block_info) => block_info.l1_fee_scalar(),
        L1BlockInfoTx::Ecotone(block_info) => {
            encode_scalar(block_info.blob_base_fee_scalar(), block_info.base_fee_scalar())
        }
        L1BlockInfoTx::Isthmus(block_info) => {
            encode_scalar(block_info.blob_base_fee_scalar(), block_info.base_fee_scalar())
        }
        L1BlockInfoTx::Jovian(block_info) => {
            encode_scalar(block_info.blob_base_fee_scalar(), block_info.base_fee_scalar())
        }
    };

    let mut cfg = SystemConfig {
        batcher_address: l1_info.batcher_address(),
        overhead: l1_info.l1_fee_overhead(),
        scalar: l1_fee_scalar,
        gas_limit,
        ..Default::default()
    };

    // After holocene's activation, the EIP-1559 parameters are stored in the block header's nonce.

    let (elasticity, denominator, min_base_fee) = JovianExtraData::decode(extra_data)?;
    cfg.eip1559_denominator = Some(denominator);
    cfg.eip1559_elasticity = Some(elasticity);
    cfg.min_base_fee = Some(min_base_fee);

    cfg.operator_fee_scalar = Some(l1_info.operator_fee_scalar());
    cfg.operator_fee_constant = Some(l1_info.operator_fee_constant());

    if let Some(da_footprint) = l1_info.da_footprint() {
        cfg.da_footprint_gas_scalar = Some(da_footprint);
    }

    Ok(cfg)
}

fn encode_scalar(blob_base_fee_scalar: u32, base_fee_scalar: u32) -> U256 {
    // Translate Ecotone values back into encoded scalar if needed.
    // We do not know if it was derived from a v0 or v1 scalar,
    // but v1 is fine, a 0 blob base fee has the same effect.
    let mut buf = B256::ZERO;
    buf[0] = 0x01;
    buf[24..28].copy_from_slice(blob_base_fee_scalar.to_be_bytes().as_ref());
    buf[28..32].copy_from_slice(base_fee_scalar.to_be_bytes().as_ref());
    buf.into()
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_eips::eip1898::BlockNumHash;
    use alloy_primitives::{U256, address, bytes};
    use base_common_genesis::{ChainGenesis, UpgradeConfig};

    use super::*;
    use crate::L1BlockInfoJovian;

    #[test]
    fn test_to_system_config_invalid_genesis_hash() {
        let block = BaseBlock::default();
        let rollup_config = RollupConfig::default();
        let err = to_system_config(&block, &rollup_config).unwrap_err();
        assert_eq!(
            err,
            BaseBlockConversionError::InvalidGenesisHash(
                rollup_config.genesis.l2.hash,
                block.header.hash_slow(),
            )
        );
    }

    #[test]
    fn test_to_system_config_missing_system_config_genesis() {
        let block = BaseBlock::default();
        let block_hash = block.header.hash_slow();
        let rollup_config = RollupConfig {
            genesis: ChainGenesis {
                l2: BlockNumHash { hash: block_hash, ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        };
        let err = to_system_config(&block, &rollup_config).unwrap_err();
        assert_eq!(err, BaseBlockConversionError::MissingSystemConfigGenesis);
    }

    #[test]
    fn test_to_system_config_from_genesis() {
        let block = BaseBlock::default();
        let block_hash = block.header.hash_slow();
        let rollup_config = RollupConfig {
            genesis: ChainGenesis {
                l2: BlockNumHash { hash: block_hash, ..Default::default() },
                system_config: Some(SystemConfig::default()),
                ..Default::default()
            },
            ..Default::default()
        };
        let config = to_system_config(&block, &rollup_config).unwrap();
        assert_eq!(config, SystemConfig::default());
    }

    #[test]
    fn test_to_system_config_empty_txs() {
        let block = BaseBlock {
            header: base_common_types_chain::Header { number: 1, ..Default::default() },
            ..Default::default()
        };
        let block_hash = block.header.hash_slow();
        let rollup_config = RollupConfig {
            genesis: ChainGenesis {
                l2: BlockNumHash { hash: block_hash, ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        };
        let err = to_system_config(&block, &rollup_config).unwrap_err();
        assert_eq!(err, BaseBlockConversionError::EmptyTransactions(block_hash));

        let (payload, _) = BaseExecutionPayload::from_block_slow(&block);
        let err = to_system_config_from_payload(&payload, &rollup_config).unwrap_err();
        assert_eq!(err, BaseBlockConversionError::EmptyTransactions(block_hash));
    }

    #[test]
    fn test_to_system_config_non_deposit() {
        let block = BaseBlock {
            header: base_common_types_chain::Header { number: 1, ..Default::default() },
            body: base_common_types_chain::BlockBody {
                transactions: vec![base_common_types_chain::BaseTxEnvelope::Legacy(
                    base_common_types_chain::Signed::new_unchecked(
                        base_common_types_chain::TxLegacy {
                            chain_id: Some(1),
                            nonce: 1,
                            gas_price: 1,
                            gas_limit: 1,
                            to: alloy_primitives::TxKind::Create,
                            value: U256::ZERO,
                            input: alloy_primitives::Bytes::new(),
                        },
                        alloy_primitives::Signature::new(U256::ZERO, U256::ZERO, false),
                        Default::default(),
                    ),
                )],
                ..Default::default()
            },
        };
        let block_hash = block.header.hash_slow();
        let rollup_config = RollupConfig {
            genesis: ChainGenesis {
                l2: BlockNumHash { hash: block_hash, ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        };
        let err = to_system_config(&block, &rollup_config).unwrap_err();
        assert_eq!(err, BaseBlockConversionError::InvalidTxType(0));

        let (payload, _) = BaseExecutionPayload::from_block_slow(&block);
        let err = to_system_config_from_payload(&payload, &rollup_config).unwrap_err();
        assert_eq!(err, BaseBlockConversionError::InvalidTxType(0));
    }

    #[test]
    fn test_to_system_config_malformed_payload_transaction() {
        let block = BaseBlock {
            header: base_common_types_chain::Header { number: 1, ..Default::default() },
            ..Default::default()
        };
        let (mut payload, _) = BaseExecutionPayload::from_block_slow(&block);
        payload.transactions_mut().push(bytes!("ff"));

        let err = to_system_config_from_payload(&payload, &RollupConfig::default()).unwrap_err();
        let BaseBlockConversionError::InvalidTransactionEncoding(error) = err else {
            panic!("expected invalid transaction encoding error");
        };
        assert!(!error.is_empty());
    }

    #[test]
    fn test_constructs_jovian_system_config_from_payload() {
        let block = BaseBlock {
            header: base_common_types_chain::Header {
                number: 1,
                extra_data: bytes!("010000beef0000babe0000000000000123"),
                ..Default::default()
            },
            body: base_common_types_chain::BlockBody {
                transactions: vec![BaseTxEnvelope::Deposit(alloy_primitives::Sealed::new(
                    base_common_types_chain::TxDeposit {
                        input: L1BlockInfoJovian::new(
                            1,
                            2,
                            3,
                            B256::ZERO,
                            4,
                            address!("6887246668a3b87f54deb3b94ba47a6f63f32985"),
                            5,
                            6,
                            7,
                            8,
                            9,
                            10,
                        )
                        .encode_calldata(),
                        ..Default::default()
                    },
                ))],
                ..Default::default()
            },
        };
        let rollup_config = RollupConfig {
            upgrades: UpgradeConfig {
                isthmus_time: Some(0),
                jovian_time: Some(0),
                ..Default::default()
            },
            ..Default::default()
        };

        let config = to_system_config(&block, &rollup_config).unwrap();
        assert_eq!(config.min_base_fee, Some(0x123));
        assert_eq!(config.da_footprint_gas_scalar, Some(10));

        let (payload, _) = BaseExecutionPayload::from_block_slow(&block);
        assert_eq!(to_system_config_from_payload(&payload, &rollup_config).unwrap(), config);
    }
}
