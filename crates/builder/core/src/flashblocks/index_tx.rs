//! EIP-1559 transaction builder for the flashblock index `setIndex(uint256)` call.

use alloy_consensus::TxEip1559;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TxSignerSync;
use alloy_primitives::{Bytes, TxKind, U256};
use alloy_sol_types::{SolCall, sol};
use base_alloy_consensus::OpTypedTransaction;
use base_alloy_flz::tx_estimated_size_fjord_bytes;
use base_execution_primitives::OpTransactionSigned;
use reth_node_api::PayloadBuilderError;
use reth_primitives::Recovered;

use super::FlashblockIndexConfig;

sol! {
    /// The `setIndex` function on the `FlashblockIndex` contract.
    function setIndex(uint256 index);
}

/// Gas limit for the `setIndex` call (a single SSTORE costs ~25k gas).
const SET_INDEX_GAS_LIMIT: u64 = 50_000;

/// Builds a signed EIP-1559 transaction calling `setIndex(flashblock_index)`.
///
/// Returns the recovered signed transaction and its DA size metrics
/// (compressed DA size, uncompressed size).
pub(super) fn build_flashblock_index_tx(
    config: &FlashblockIndexConfig,
    chain_id: u64,
    nonce: u64,
    base_fee: u64,
    flashblock_index: u64,
) -> Result<(Recovered<OpTransactionSigned>, u64, u64), PayloadBuilderError> {
    let calldata = setIndexCall { index: U256::from(flashblock_index) }.abi_encode();

    let tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: SET_INDEX_GAS_LIMIT,
        max_fee_per_gas: base_fee as u128,
        max_priority_fee_per_gas: 0,
        to: TxKind::Call(config.contract_address),
        value: U256::ZERO,
        input: Bytes::from(calldata),
        access_list: Default::default(),
    };

    let mut op_tx = OpTypedTransaction::Eip1559(tx);

    let signature =
        config.signer.sign_transaction_sync(&mut op_tx).map_err(PayloadBuilderError::other)?;

    let signed = OpTransactionSigned::new_unhashed(op_tx, signature);
    let encoded = signed.encoded_2718();
    let da_size = tx_estimated_size_fjord_bytes(encoded.as_slice());
    let uncompressed_size = encoded.len() as u64;

    Ok((Recovered::new_unchecked(signed, config.signer.address()), da_size, uncompressed_size))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use alloy_signer_local::PrivateKeySigner;

    use super::*;

    fn test_config() -> FlashblockIndexConfig {
        let signer: PrivateKeySigner =
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".parse().unwrap();
        FlashblockIndexConfig {
            signer,
            contract_address: address!("0x4200000000000000000000000000000000000024"),
        }
    }

    #[test]
    fn builds_valid_tx() {
        let config = test_config();
        let (recovered, da_size, uncompressed_size) =
            build_flashblock_index_tx(&config, 8453, 0, 1_000_000_000, 3).unwrap();

        assert_eq!(recovered.signer(), config.signer.address());
        assert!(da_size > 0);
        assert!(uncompressed_size > 0);
    }

    #[test]
    fn selector_matches() {
        let calldata = setIndexCall { index: U256::from(42) }.abi_encode();
        // First 4 bytes are the function selector.
        assert_eq!(calldata.len(), 4 + 32);
    }

    #[test]
    fn deterministic_output() {
        let config = test_config();
        let (a, _, _) = build_flashblock_index_tx(&config, 8453, 5, 1_000_000_000, 0).unwrap();
        let (b, _, _) = build_flashblock_index_tx(&config, 8453, 5, 1_000_000_000, 0).unwrap();

        assert_eq!(a.tx_hash(), b.tx_hash());
    }
}
