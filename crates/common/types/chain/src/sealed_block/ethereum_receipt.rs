use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::B256;
use base_common_types_chain::{EthereumReceipt, TxTy};

use alloy_trie::root::ordered_trie_root_with_encoder;

/// Calculates the receipt root for a header for the reference type of [`EthereumReceipt`].
///
/// NOTE: Prefer `proofs::calculate_receipt_root` if you have log blooms memoized.
#[derive(Debug)]
pub struct EthereumReceiptRoot;

impl EthereumReceiptRoot {
    /// Computes the root from receipts without memoized blooms.
    pub fn calculate<T: TxTy>(receipts: &[EthereumReceipt<T>]) -> B256 {
        ordered_trie_root_with_encoder(receipts, |r, buf| {
            base_common_types_chain::TxReceipt::with_bloom_ref(r).encode_2718(buf)
        })
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloy_eips::eip2718::Encodable2718;
    #[cfg(feature = "reth-codec")]
    use alloy_primitives::Bytes;
    use alloy_primitives::{
        Address, Bloom, Log, LogData, address, b256, bloom, bytes, hex_literal::hex,
    };
    use alloy_rlp::{Decodable, Encodable};
    #[cfg(feature = "reth-codec")]
    use base_common_types_chain::Compact;
    use base_common_types_chain::{EthereumTxEnvelope, ReceiptWithBloom, TxEip4844, TxType};

    use super::*;
    use crate::proofs::{
        calculate_receipt_root, calculate_transaction_root, calculate_withdrawals_root,
    };

    #[test]
    #[cfg(feature = "reth-codec")]
    fn test_decode_receipt() {
        base_common_types_chain::test_utils::test_decode::<EthereumReceipt<TxType>>(&hex!(
            "c428b52ffd23fc42696156b10200f034792b6a94c3850215c2fef7aea361a0c31b79d9a32652eefc0d4e2e730036061cff7344b6fc6132b50cda0ed810a991ae58ef013150c12b2522533cb3b3a8b19b7786a8b5ff1d3cdc84225e22b02def168c8858df"
        ));
    }

    // Test vector from: https://eips.ethereum.org/EIPS/eip-2481
    #[test]
    fn encode_legacy_receipt() {
        let expected = hex!(
            "f901668001b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f85ff85d940000000000000000000000000000000000000011f842a0000000000000000000000000000000000000000000000000000000000000deada0000000000000000000000000000000000000000000000000000000000000beef830100ff"
        );

        let mut data = Vec::with_capacity(expected.length());
        let receipt = ReceiptWithBloom {
            receipt: EthereumReceipt::<TxType> {
                tx_type: TxType::Legacy,
                cumulative_gas_used: 0x1u64,
                logs: vec![Log::new_unchecked(
                    address!("0x0000000000000000000000000000000000000011"),
                    vec![
                        b256!("0x000000000000000000000000000000000000000000000000000000000000dead"),
                        b256!("0x000000000000000000000000000000000000000000000000000000000000beef"),
                    ],
                    bytes!("0100ff"),
                )],
                success: false,
            },
            logs_bloom: [0; 256].into(),
        };

        receipt.encode(&mut data);

        // check that the rlp length equals the length of the expected rlp
        assert_eq!(receipt.length(), expected.len());
        assert_eq!(data, expected);
    }

    // Test vector from: https://eips.ethereum.org/EIPS/eip-2481
    #[test]
    fn decode_legacy_receipt() {
        let data = hex!(
            "f901668001b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f85ff85d940000000000000000000000000000000000000011f842a0000000000000000000000000000000000000000000000000000000000000deada0000000000000000000000000000000000000000000000000000000000000beef830100ff"
        );

        // EIP658Receipt
        let expected = ReceiptWithBloom {
            receipt: EthereumReceipt::<TxType> {
                tx_type: TxType::Legacy,
                cumulative_gas_used: 0x1u64,
                logs: vec![Log::new_unchecked(
                    address!("0x0000000000000000000000000000000000000011"),
                    vec![
                        b256!("0x000000000000000000000000000000000000000000000000000000000000dead"),
                        b256!("0x000000000000000000000000000000000000000000000000000000000000beef"),
                    ],
                    bytes!("0100ff"),
                )],
                success: false,
            },
            logs_bloom: [0; 256].into(),
        };

        let receipt = ReceiptWithBloom::decode(&mut &data[..]).unwrap();
        assert_eq!(receipt, expected);
    }

    #[test]
    #[cfg(feature = "reth-codec")]
    fn gigantic_receipt() {
        let receipt = EthereumReceipt::<TxType> {
            cumulative_gas_used: 16747627,
            success: true,
            tx_type: TxType::Legacy,
            logs: vec![
                Log::new_unchecked(
                    address!("0x4bf56695415f725e43c3e04354b604bcfb6dfb6e"),
                    vec![b256!(
                        "0xc69dc3d7ebff79e41f525be431d5cd3cc08f80eaf0f7819054a726eeb7086eb9"
                    )],
                    Bytes::from(vec![1; 0xffffff]),
                ),
                Log::new_unchecked(
                    address!("0xfaca325c86bf9c2d5b413cd7b90b209be92229c2"),
                    vec![b256!(
                        "0x8cca58667b1e9ffa004720ac99a3d61a138181963b294d270d91c53d36402ae2"
                    )],
                    Bytes::from(vec![1; 0xffffff]),
                ),
            ],
        };

        let mut data = vec![];
        receipt.to_compact(&mut data);
        let (decoded, _) = EthereumReceipt::<TxType>::from_compact(&data[..], data.len());
        assert_eq!(decoded, receipt);
    }

    #[test]
    fn test_encode_2718_length() {
        let receipt = ReceiptWithBloom {
            receipt: EthereumReceipt::<TxType> {
                tx_type: TxType::Eip1559,
                success: true,
                cumulative_gas_used: 21000,
                logs: vec![],
            },
            logs_bloom: Bloom::default(),
        };

        let encoded = receipt.encoded_2718();
        assert_eq!(
            encoded.len(),
            receipt.encode_2718_len(),
            "Encoded length should match the actual encoded data length"
        );

        // Test for legacy receipt as well
        let legacy_receipt = ReceiptWithBloom {
            receipt: EthereumReceipt::<TxType> {
                tx_type: TxType::Legacy,
                success: true,
                cumulative_gas_used: 21000,
                logs: vec![],
            },
            logs_bloom: Bloom::default(),
        };

        let legacy_encoded = legacy_receipt.encoded_2718();
        assert_eq!(
            legacy_encoded.len(),
            legacy_receipt.encode_2718_len(),
            "Encoded length for legacy receipt should match the actual encoded data length"
        );
    }

    #[test]
    fn check_transaction_root() {
        let data = &hex!(
            "f90262f901f9a092230ce5476ae868e98c7979cfc165a93f8b6ad1922acf2df62e340916efd49da01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347942adc25665018aa1fe0e6bc666dac8fc2697ff9baa02307107a867056ca33b5087e77c4174f47625e48fb49f1c70ced34890ddd88f3a08151d548273f6683169524b66ca9fe338b9ce42bc3540046c828fd939ae23bcba0c598f69a5674cae9337261b669970e24abc0b46e6d284372a239ec8ccbf20b0ab901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000083020000018502540be40082a8618203e800a00000000000000000000000000000000000000000000000000000000000000000880000000000000000f863f861800a8405f5e10094100000000000000000000000000000000000000080801ba07e09e26678ed4fac08a249ebe8ed680bf9051a5e14ad223e4b2b9d26e0208f37a05f6e3f188e3e6eab7d7d3b6568f5eac7d687b08d307d3154ccd8c87b4630509bc0"
        );
        let block_rlp = &mut data.as_slice();
        let block =
            base_common_types_chain::Block::<EthereumTxEnvelope<TxEip4844>>::decode(block_rlp)
                .unwrap();

        let tx_root = calculate_transaction_root(&block.body.transactions);
        assert_eq!(block.transactions_root, tx_root, "Must be the same");
    }

    #[test]
    fn check_withdrawals_root() {
        // Single withdrawal, amount 0
        // https://github.com/ethereum/tests/blob/9760400e667eba241265016b02644ef62ab55de2/BlockchainTests/EIPTests/bc4895-withdrawals/amountIs0.json
        let data = &hex!(
            "f90238f90219a0151934ad9b654c50197f37018ee5ee9bb922dec0a1b5e24a6d679cb111cdb107a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347942adc25665018aa1fe0e6bc666dac8fc2697ff9baa0046119afb1ab36aaa8f66088677ed96cd62762f6d3e65642898e189fbe702d51a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421b90100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008001887fffffffffffffff8082079e42a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b42188000000000000000009a048a703da164234812273ea083e4ec3d09d028300cd325b46a6a75402e5a7ab95c0c0d9d8808094c94f5374fce5edbc8e2a8697c15331677e6ebf0b80"
        );
        let block = base_common_types_chain::Block::<EthereumTxEnvelope<TxEip4844>>::decode(
            &mut data.as_slice(),
        )
        .unwrap();
        assert!(block.body.withdrawals.is_some());
        let withdrawals = block.body.withdrawals.as_ref().unwrap();
        assert_eq!(withdrawals.len(), 1);
        let withdrawals_root = calculate_withdrawals_root(withdrawals);
        assert_eq!(block.withdrawals_root, Some(withdrawals_root));

        // 4 withdrawals, identical indices
        // https://github.com/ethereum/tests/blob/9760400e667eba241265016b02644ef62ab55de2/BlockchainTests/EIPTests/bc4895-withdrawals/twoIdenticalIndex.json
        let data = &hex!(
            "f9028cf90219a0151934ad9b654c50197f37018ee5ee9bb922dec0a1b5e24a6d679cb111cdb107a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347942adc25665018aa1fe0e6bc666dac8fc2697ff9baa0ccf7b62d616c2ad7af862d67b9dcd2119a90cebbff8c3cd1e5d7fc99f8755774a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421b90100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008001887fffffffffffffff8082079e42a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b42188000000000000000009a0a95b9a7b58a6b3cb4001eb0be67951c5517141cb0183a255b5cae027a7b10b36c0c0f86cda808094c94f5374fce5edbc8e2a8697c15331677e6ebf0b822710da028094c94f5374fce5edbc8e2a8697c15331677e6ebf0b822710da018094c94f5374fce5edbc8e2a8697c15331677e6ebf0b822710da028094c94f5374fce5edbc8e2a8697c15331677e6ebf0b822710"
        );
        let block = base_common_types_chain::Block::<EthereumTxEnvelope<TxEip4844>>::decode(
            &mut data.as_slice(),
        )
        .unwrap();
        assert!(block.body.withdrawals.is_some());
        let withdrawals = block.body.withdrawals.as_ref().unwrap();
        assert_eq!(withdrawals.len(), 4);
        let withdrawals_root = calculate_withdrawals_root(withdrawals);
        assert_eq!(block.withdrawals_root, Some(withdrawals_root));
    }
    #[test]
    fn check_receipt_root_optimism() {
        let logs = vec![Log {
            address: Address::ZERO,
            data: LogData::new_unchecked(vec![], Default::default()),
        }];
        let bloom = bloom!(
            "00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001"
        );
        let receipt = ReceiptWithBloom {
            receipt: EthereumReceipt::<TxType> {
                tx_type: TxType::Eip2930,
                success: true,
                cumulative_gas_used: 102068,
                logs,
            },
            logs_bloom: bloom,
        };
        let receipt = vec![receipt];
        let root = calculate_receipt_root(&receipt);
        assert_eq!(
            root,
            b256!("0xfe70ae4a136d98944951b2123859698d59ad251a381abc9960fa81cae3d0d4a0")
        );
    }

    // Ensures that reth and alloy receipts encode to the same JSON
}
