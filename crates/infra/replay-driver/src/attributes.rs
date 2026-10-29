use alloy_consensus::Block;
use alloy_eips::Encodable2718;
use alloy_primitives::B64;
use alloy_rpc_types_engine::PayloadAttributes;
use base_common_chains::Upgrades;
use base_common_consensus::{
    BaseTxEnvelope, EIP1559ParamError, HoloceneExtraData, JovianExtraData,
};
use base_common_rpc_types_engine::BasePayloadAttributes;

/// Derives builder inputs from a trusted canonical block.
#[derive(Debug)]
pub struct CanonicalAttributes;

impl CanonicalAttributes {
    /// Reconstructs canonical attributes while enabling transaction-pool selection.
    ///
    /// # Errors
    /// Returns an error when fork-specific extra data cannot be decoded.
    pub fn derive(
        block: &Block<BaseTxEnvelope>,
        chain: &impl Upgrades,
    ) -> Result<BasePayloadAttributes, EIP1559ParamError> {
        let header = &block.header;
        let (eip_1559_params, min_base_fee) =
            if chain.is_jovian_active_at_timestamp(header.timestamp) {
                let (_, _, minimum) = JovianExtraData::decode(&header.extra_data)?;
                (Some(B64::from_slice(&header.extra_data[1..9])), Some(minimum))
            } else if chain.is_holocene_active_at_timestamp(header.timestamp) {
                HoloceneExtraData::decode(&header.extra_data)?;
                (Some(B64::from_slice(&header.extra_data[1..9])), None)
            } else {
                (None, None)
            };
        Ok(BasePayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: header.timestamp,
                prev_randao: header.mix_hash,
                suggested_fee_recipient: header.beneficiary,
                withdrawals: chain.is_canyon_active_at_timestamp(header.timestamp).then(Vec::new),
                parent_beacon_block_root: header.parent_beacon_block_root,
                ..Default::default()
            },
            transactions: Some(
                block
                    .body
                    .transactions
                    .iter()
                    .take_while(|tx| tx.is_deposit())
                    .map(|tx| tx.encoded_2718().into())
                    .collect(),
            ),
            no_tx_pool: Some(false),
            gas_limit: Some(header.gas_limit),
            eip_1559_params,
            min_base_fee,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Block, Header, Sealable, SignableTransaction, TxLegacy};
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Address, B64, B256, Bytes, Signature};
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};
    use base_common_genesis::{RollupConfig, UpgradeConfig};

    use super::CanonicalAttributes;

    fn chain() -> RollupConfig {
        RollupConfig {
            upgrades: UpgradeConfig {
                canyon_time: Some(20),
                ecotone_time: Some(30),
                holocene_time: Some(100),
                isthmus_time: Some(150),
                jovian_time: Some(200),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn copies_header_when_deriving_canonical_attributes() {
        let block = Block::<BaseTxEnvelope> {
            header: Header {
                timestamp: 1,
                mix_hash: B256::repeat_byte(7),
                beneficiary: Address::repeat_byte(8),
                gas_limit: 42_000_000,
                ..Default::default()
            },
            ..Default::default()
        };

        let attrs = CanonicalAttributes::derive(&block, &chain()).unwrap();

        assert_eq!(attrs.payload_attributes.timestamp, 1);
        assert_eq!(attrs.payload_attributes.prev_randao, block.header.mix_hash);
        assert_eq!(attrs.payload_attributes.suggested_fee_recipient, block.header.beneficiary);
        assert_eq!(attrs.gas_limit, Some(42_000_000));
        assert_eq!(attrs.no_tx_pool, Some(false));
    }

    #[test]
    fn preserves_fee_parameters_when_crossing_fork_boundaries() {
        for (timestamp, extra, params, minimum) in [
            (19, "abcd", None, None),
            (20, "abcd", None, None),
            (29, "abcd", None, None),
            (30, "abcd", None, None),
            (99, "abcd", None, None),
            (100, "00000000fa00000006", Some(B64::from(250_u64 << 32 | 6)), None),
            (199, "00000000fa00000006", Some(B64::from(250_u64 << 32 | 6)), None),
            (
                200,
                "01000000fa00000006000000000000012c",
                Some(B64::from(250_u64 << 32 | 6)),
                Some(300),
            ),
            (
                201,
                "01000000fa000000060000000000000000",
                Some(B64::from(250_u64 << 32 | 6)),
                Some(0),
            ),
        ] {
            let block = Block::<BaseTxEnvelope> {
                header: Header {
                    timestamp,
                    extra_data: Bytes::from(alloy_primitives::hex::decode(extra).unwrap()),
                    parent_beacon_block_root: (timestamp >= 30).then_some(B256::repeat_byte(9)),
                    ..Default::default()
                },
                ..Default::default()
            };

            let attrs = CanonicalAttributes::derive(&block, &chain()).unwrap();

            assert_eq!(attrs.eip_1559_params, params, "timestamp {timestamp}");
            assert_eq!(attrs.min_base_fee, minimum, "timestamp {timestamp}");
            assert_eq!(attrs.payload_attributes.withdrawals, (timestamp >= 20).then(Vec::new));
            assert_eq!(
                attrs.payload_attributes.parent_beacon_block_root,
                block.header.parent_beacon_block_root
            );
        }
    }

    #[test]
    fn keeps_withdrawals_empty_when_crossing_isthmus_with_nonempty_header_root() {
        for timestamp in [149, 150] {
            let block = Block::<BaseTxEnvelope> {
                header: Header {
                    timestamp,
                    extra_data: Bytes::from_static(&[0, 0, 0, 0, 250, 0, 0, 0, 6]),
                    withdrawals_root: Some(B256::repeat_byte(10)),
                    ..Default::default()
                },
                ..Default::default()
            };

            let attrs = CanonicalAttributes::derive(&block, &chain()).unwrap();

            assert_eq!(attrs.payload_attributes.withdrawals, Some(vec![]), "timestamp {timestamp}");
        }
    }

    #[test]
    fn rejects_extra_data_when_fork_encoding_is_malformed() {
        for (timestamp, extra) in [
            (100, ""),
            (100, "01000000fa00000006"),
            (200, "00000000fa00000006"),
            (200, "00000000fa00000006000000000000012c"),
        ] {
            let block = Block::<BaseTxEnvelope> {
                header: Header {
                    timestamp,
                    extra_data: Bytes::from(alloy_primitives::hex::decode(extra).unwrap()),
                    ..Default::default()
                },
                ..Default::default()
            };

            let result = CanonicalAttributes::derive(&block, &chain());

            assert!(result.is_err(), "timestamp {timestamp}, extra {extra}");
        }
    }

    #[test]
    fn forces_only_leading_deposits_when_pool_transactions_follow() {
        let first = BaseTxEnvelope::Deposit(TxDeposit::default().seal_slow());
        let second = BaseTxEnvelope::Deposit(
            TxDeposit { source_hash: B256::repeat_byte(1), ..Default::default() }.seal_slow(),
        );
        let pool =
            BaseTxEnvelope::Legacy(TxLegacy::default().into_signed(Signature::test_signature()));
        let mut block = Block::<BaseTxEnvelope>::default();
        block.body.transactions = vec![first.clone(), second.clone(), pool, first.clone()];

        let attrs = CanonicalAttributes::derive(&block, &chain()).unwrap();

        assert_eq!(
            attrs.transactions,
            Some(vec![first.encoded_2718().into(), second.encoded_2718().into()])
        );
    }

    #[test]
    fn forces_empty_list_when_canonical_block_has_no_deposits() {
        let block = Block::<BaseTxEnvelope>::default();

        let attrs = CanonicalAttributes::derive(&block, &chain()).unwrap();

        assert_eq!(attrs.transactions, Some(vec![]));
    }
}
