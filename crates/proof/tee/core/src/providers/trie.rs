//! Trie root computation utilities.
//!
//! This module provides functions to compute Merkle Patricia Trie roots
//! for receipts and transactions, matching Ethereum's `DeriveSha` algorithm.

use alloy_consensus::ReceiptEnvelope;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::B256;
use alloy_rlp::Encodable;
use alloy_trie::{EMPTY_ROOT_HASH, HashBuilder, Nibbles};
use base_alloy_consensus::OpReceiptEnvelope;

/// Computes the receipt root from a list of receipts.
///
/// This matches Go's `types.DeriveSha(receipts, trie.NewStackTrie(nil))`.
///
/// The algorithm:
/// 1. For each receipt at index i, the key is RLP(i)
/// 2. The value is the RLP-encoded receipt (envelope format for typed receipts)
/// 3. Keys are sorted and inserted into a Merkle Patricia Trie
/// 4. Returns the root hash
#[must_use]
pub fn compute_receipt_root(receipts: &[OpReceiptEnvelope]) -> B256 {
    if receipts.is_empty() {
        return EMPTY_ROOT_HASH;
    }

    // Build key-value pairs: (RLP(index), EIP-2718 encoded receipt)
    // Note: We use encode_2718() to match Go's EncodeIndex behavior, which produces
    // type_byte || rlp(receipt_data) for typed receipts, without an outer RLP wrapper.
    let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = receipts
        .iter()
        .enumerate()
        .map(|(i, receipt)| {
            let key = encode_index(i);
            let mut value = Vec::new();
            receipt.encode_2718(&mut value);
            (key, value)
        })
        .collect();

    // Sort by key (lexicographic order for proper trie construction)
    pairs.sort_by(|a, b| a.0.cmp(&b.0));

    // Build the trie using HashBuilder
    let mut builder = HashBuilder::default();
    for (key, value) in pairs {
        let nibbles = Nibbles::unpack(&key);
        builder.add_leaf(nibbles, &value);
    }

    builder.root()
}

/// Computes the receipt root from L1 receipts (supports all Ethereum receipt types including EIP-4844).
///
/// This is used for verifying L1 receipt roots which may contain blob transaction receipts.
#[must_use]
pub fn compute_l1_receipt_root(receipts: &[ReceiptEnvelope]) -> B256 {
    if receipts.is_empty() {
        return EMPTY_ROOT_HASH;
    }

    let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = receipts
        .iter()
        .enumerate()
        .map(|(i, receipt)| {
            let key = encode_index(i);
            let mut value = Vec::new();
            receipt.encode_2718(&mut value);
            (key, value)
        })
        .collect();

    pairs.sort_by(|a, b| a.0.cmp(&b.0));

    let mut builder = HashBuilder::default();
    for (key, value) in pairs {
        let nibbles = Nibbles::unpack(&key);
        builder.add_leaf(nibbles, &value);
    }

    builder.root()
}

/// Computes the transaction root from a list of RLP-encoded transactions.
///
/// This matches Go's `types.DeriveSha(txs, trie.NewStackTrie(nil))`.
#[must_use]
pub fn compute_tx_root(txs_rlp: &[Vec<u8>]) -> B256 {
    if txs_rlp.is_empty() {
        return EMPTY_ROOT_HASH;
    }

    // Build key-value pairs: (RLP(index), tx_rlp)
    let mut pairs: Vec<(Vec<u8>, &[u8])> =
        txs_rlp.iter().enumerate().map(|(i, tx)| (encode_index(i), tx.as_slice())).collect();

    // Sort by key (lexicographic order for proper trie construction)
    pairs.sort_by(|a, b| a.0.cmp(&b.0));

    // Build the trie using HashBuilder
    let mut builder = HashBuilder::default();
    for (key, value) in pairs {
        let nibbles = Nibbles::unpack(&key);
        builder.add_leaf(nibbles, value);
    }

    builder.root()
}

/// RLP-encodes an index for use as a trie key.
///
/// For index 0, returns [0x80] (RLP encoding of empty byte sequence).
/// For other indices, returns the minimal RLP encoding.
fn encode_index(index: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    index.encode(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use std::slice;

    use alloy_consensus::{Eip658Value, Receipt, ReceiptWithBloom};
    use alloy_primitives::{Bloom, Log, b256};

    use super::*;

    /// Creates a test receipt for use in test vectors.
    ///
    /// # Arguments
    /// * `tx_type` - The transaction type (0=Legacy, 1=EIP-2930, 2=EIP-1559, 126=Deposit)
    /// * `success` - Whether the receipt indicates success
    /// * `cumulative_gas` - The cumulative gas used
    fn create_test_receipt(tx_type: u8, success: bool, cumulative_gas: u64) -> OpReceiptEnvelope {
        let inner = Receipt::<Log> {
            status: Eip658Value::Eip658(success),
            cumulative_gas_used: cumulative_gas,
            logs: vec![],
        };

        let receipt_with_bloom = ReceiptWithBloom::new(inner, Bloom::default());

        match tx_type {
            0 => OpReceiptEnvelope::Legacy(receipt_with_bloom),
            1 => OpReceiptEnvelope::Eip2930(receipt_with_bloom),
            2 => OpReceiptEnvelope::Eip1559(receipt_with_bloom),
            126 => OpReceiptEnvelope::Deposit(base_alloy_consensus::OpDepositReceiptWithBloom {
                receipt: base_alloy_consensus::OpDepositReceipt {
                    inner: Receipt::<Log> {
                        status: Eip658Value::Eip658(success),
                        cumulative_gas_used: cumulative_gas,
                        logs: vec![],
                    },
                    deposit_nonce: None,
                    deposit_receipt_version: None,
                },
                logs_bloom: Bloom::default(),
            }),
            _ => panic!("unsupported tx type"),
        }
    }

    #[test]
    fn test_compute_receipt_root_matches_go_single() {
        // Test vector generated from Go types.DeriveSha()
        // Single EIP-1559 receipt with cumulative gas 21000
        let receipts = vec![create_test_receipt(2, true, 21000)];

        let computed = compute_receipt_root(&receipts);

        // Expected root from Go: types.DeriveSha(receipts, trie.NewStackTrie(nil))
        let expected = b256!("f78dfb743fbd92ade140711c8bbc542b5e307f0ab7984eff35d751969fe57efa");

        assert_eq!(computed, expected, "Receipt root must match Go DeriveSha for single receipt");
    }

    #[test]
    fn test_compute_receipt_root_matches_go_multiple() {
        // Test vector generated from Go types.DeriveSha()
        // Two EIP-1559 receipts with known cumulative gas values
        let receipts =
            vec![create_test_receipt(2, true, 21000), create_test_receipt(2, true, 42000)];

        let computed = compute_receipt_root(&receipts);

        // Expected root from Go: types.DeriveSha(receipts, trie.NewStackTrie(nil))
        let expected = b256!("75308898d571eafb5cd8cde8278bf5b3d13c5f6ec074926de3bb895b519264e1");

        assert_eq!(
            computed, expected,
            "Receipt root must match Go DeriveSha for multiple receipts"
        );
    }

    #[test]
    fn test_compute_receipt_root_empty() {
        let result = compute_receipt_root(&[]);
        assert_eq!(result, EMPTY_ROOT_HASH);
    }

    #[test]
    fn test_compute_tx_root_empty() {
        let result = compute_tx_root(&[]);
        assert_eq!(result, EMPTY_ROOT_HASH);
    }

    #[test]
    fn test_encode_index_zero() {
        let encoded = encode_index(0);
        // RLP encoding of 0 is 0x80 (empty byte sequence)
        assert_eq!(encoded, vec![0x80]);
    }

    #[test]
    fn test_encode_index_small() {
        // RLP encoding of small integer (< 128) is the integer itself
        let encoded = encode_index(127);
        assert_eq!(encoded, vec![127]);
    }

    #[test]
    fn test_encode_index_larger() {
        // RLP encoding of 128 is 0x8180 (string prefix + value)
        let encoded = encode_index(128);
        assert_eq!(encoded, vec![0x81, 0x80]);
    }

    #[test]
    fn test_compute_receipt_root_single_receipt() {
        let inner_receipt = Receipt::<Log> {
            status: Eip658Value::Eip658(true),
            cumulative_gas_used: 21000,
            logs: vec![],
        };

        let receipt = OpReceiptEnvelope::Legacy(ReceiptWithBloom::new(
            inner_receipt,
            alloy_primitives::Bloom::default(),
        ));

        let root = compute_receipt_root(slice::from_ref(&receipt));

        // Root should be deterministic and non-empty
        assert_ne!(root, EMPTY_ROOT_HASH);

        // Same input produces same output
        let root2 = compute_receipt_root(slice::from_ref(&receipt));
        assert_eq!(root, root2);
    }

    #[test]
    fn test_compute_receipt_root_multiple_receipts() {
        let receipts: Vec<OpReceiptEnvelope> = (0..3)
            .map(|i| {
                let inner_receipt = Receipt::<Log> {
                    status: Eip658Value::Eip658(true),
                    cumulative_gas_used: 21000 * (i + 1) as u64,
                    logs: vec![],
                };
                OpReceiptEnvelope::Legacy(ReceiptWithBloom::new(
                    inner_receipt,
                    alloy_primitives::Bloom::default(),
                ))
            })
            .collect();

        let root = compute_receipt_root(&receipts);
        assert_ne!(root, EMPTY_ROOT_HASH);

        // Different order or count should produce different roots
        let root_single = compute_receipt_root(&receipts[..1]);
        assert_ne!(root, root_single);
    }

    #[test]
    fn test_compute_tx_root_single() {
        // Simple RLP-encoded transaction-like data
        let tx_rlp = vec![0xc0]; // Empty RLP list

        let root = compute_tx_root(slice::from_ref(&tx_rlp));
        assert_ne!(root, EMPTY_ROOT_HASH);

        // Same input produces same output
        let root2 = compute_tx_root(&[tx_rlp]);
        assert_eq!(root, root2);
    }

    #[test]
    fn test_compute_tx_root_multiple() {
        let txs: Vec<Vec<u8>> = (0..3).map(|i| vec![0xc0 + i as u8]).collect();

        let root = compute_tx_root(&txs);
        assert_ne!(root, EMPTY_ROOT_HASH);

        // Different count produces different root
        let root_single = compute_tx_root(&txs[..1]);
        assert_ne!(root, root_single);
    }
}
