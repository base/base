use std::{
    collections::HashSet,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_consensus::private::alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{Provider, RootProvider};
use async_trait::async_trait;
use base_alloy_network::Base;
use base_bundles::Bundle;
use base_revm::L1BlockInfo;
use jsonrpsee::core::RpcResult;
use reth_optimism_evm::extract_l1_info_from_tx;
use reth_rpc_eth_types::{EthApiError, SignError};
use tokio::time::Instant;
use tracing::warn;

use crate::metrics::record_histogram;

const MAX_BUNDLE_GAS: u64 = 25_000_000;

/// Account info for a given address.
#[derive(Debug)]
pub struct AccountInfo {
    /// Account balance in wei.
    pub balance: U256,
    /// Account transaction nonce.
    pub nonce: u64,
    /// Hash of the account's code.
    pub code_hash: B256,
}

/// Interface for fetching account info for a given address.
#[async_trait]
pub trait AccountInfoLookup: Send + Sync {
    /// Fetches account info for the given address.
    async fn fetch_account_info(&self, address: Address) -> RpcResult<AccountInfo>;
}

/// Implementation of the `AccountInfoLookup` trait for the `RootProvider`
#[async_trait]
impl AccountInfoLookup for RootProvider<Base> {
    async fn fetch_account_info(&self, address: Address) -> RpcResult<AccountInfo> {
        let start = Instant::now();
        let account = self
            .get_account(address)
            .await
            .map_err(|_| EthApiError::Signing(SignError::NoAccount))?;
        record_histogram(start.elapsed(), "eth_getAccount".to_string());

        Ok(AccountInfo {
            balance: account.balance,
            nonce: account.nonce,
            code_hash: account.code_hash,
        })
    }
}

/// Interface for fetching L1 block info for a given block number.
#[async_trait]
pub trait L1BlockInfoLookup: Send + Sync {
    /// Fetches the L1 block info from the latest L2 block.
    async fn fetch_l1_block_info(&self) -> RpcResult<L1BlockInfo>;
}

/// Implementation of the `L1BlockInfoLookup` trait for the `RootProvider`
#[async_trait]
impl L1BlockInfoLookup for RootProvider<Base> {
    async fn fetch_l1_block_info(&self) -> RpcResult<L1BlockInfo> {
        let start = Instant::now();
        let block = self
            .get_block(BlockId::Number(BlockNumberOrTag::Latest))
            .full()
            .await
            .map_err(|e| {
                warn!(message = "failed to fetch latest block", err = %e);
                EthApiError::InternalEthError.into_rpc_err()
            })?
            .ok_or_else(|| {
                warn!(message = "empty latest block returned");
                EthApiError::InternalEthError.into_rpc_err()
            })?;
        record_histogram(start.elapsed(), "eth_getBlockByNumber".to_string());

        let txs = block.transactions;
        let first_tx = txs.first_transaction().ok_or_else(|| {
            warn!(message = "block contains no transactions");
            EthApiError::InternalEthError.into_rpc_err()
        })?;

        Ok(extract_l1_info_from_tx(&first_tx.clone()).map_err(|e| {
            warn!(message = "failed to extract l1_info from tx", err = %e);
            EthApiError::InternalEthError.into_rpc_err()
        })?)
    }
}

/// Helper function to validate properties of a bundle. A bundle is valid if it satisfies the following criteria:
/// - The bundle's `max_timestamp` is not more than 1 hour in the future
/// - The bundle's gas limit is not greater than the maximum allowed gas limit
/// - The bundle can only contain 3 transactions at once
/// - Partial transaction dropping is not supported, `dropping_tx_hashes` must be empty
/// - revert protection is not supported, all transaction hashes must be in `reverting_tx_hashes`
pub fn validate_bundle(bundle: &Bundle, bundle_gas: u64, tx_hashes: Vec<B256>) -> RpcResult<()> {
    // Don't allow bundles to be submitted over 1 hour into the future
    // TODO: make the window configurable
    let valid_timestamp_window = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
        + Duration::from_secs(3600).as_secs();
    if let Some(max_timestamp) = bundle.max_timestamp
        && max_timestamp > valid_timestamp_window
    {
        return Err(EthApiError::InvalidParams(
            "Bundle cannot be more than 1 hour in the future".into(),
        )
        .into_rpc_err());
    }

    // Check max gas limit for the entire bundle
    if bundle_gas > MAX_BUNDLE_GAS {
        return Err(EthApiError::InvalidParams("Bundle gas limit exceeds maximum allowed".into())
            .into_rpc_err());
    }

    // Can only provide 3 transactions at once
    if bundle.txs.len() > 3 {
        return Err(EthApiError::InvalidParams("Bundle can only contain 3 transactions".into())
            .into_rpc_err());
    }

    // Partial transaction dropping is not supported, `dropping_tx_hashes` must be empty
    if !bundle.dropping_tx_hashes.is_empty() {
        return Err(EthApiError::InvalidParams(
            "Partial transaction dropping is not supported".into(),
        )
        .into_rpc_err());
    }

    // revert protection: all transaction hashes must be in `reverting_tx_hashes`
    let reverting_tx_hashes_set: HashSet<_> = bundle.reverting_tx_hashes.iter().collect();
    let tx_hashes_set: HashSet<_> = tx_hashes.iter().collect();
    if reverting_tx_hashes_set != tx_hashes_set {
        return Err(EthApiError::InvalidParams(
            "Revert protection is not supported. reverting_tx_hashes must include all hashes"
                .into(),
        )
        .into_rpc_err());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use alloy_consensus::{SignableTransaction, TxEip1559, transaction::SignerRecoverable};
    use alloy_primitives::{Address, B256, Bytes, U256, bytes};
    use alloy_signer_local::PrivateKeySigner;
    use base_alloy_consensus::OpTxEnvelope;
    use base_alloy_network::{
        TxSignerSync,
        eip2718::{Decodable2718, Encodable2718},
    };
    use base_bundles::Bundle;
    use reth_rpc_eth_types::EthApiError;

    use super::validate_bundle;

    #[tokio::test]
    async fn test_err_bundle_max_timestamp_too_far_in_the_future() {
        let current_time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let too_far_in_the_future = current_time + 3601;
        let bundle = Bundle {
            txs: vec![],
            max_timestamp: Some(too_far_in_the_future),
            ..Default::default()
        };
        assert_eq!(
            validate_bundle(&bundle, 0, vec![]),
            Err(EthApiError::InvalidParams(
                "Bundle cannot be more than 1 hour in the future".into()
            )
            .into_rpc_err())
        );
    }

    #[tokio::test]
    async fn test_err_bundle_max_gas_limit_too_high() {
        let signer = PrivateKeySigner::random();
        let mut encoded_txs = vec![];
        let mut tx_hashes = vec![];

        // Create transactions that collectively exceed MAX_BUNDLE_GAS (25M)
        // Each transaction uses 4M gas, so 8 transactions = 32M gas > 25M limit
        let gas = 4_000_000;
        let mut total_gas = 0u64;
        for _ in 0..8 {
            let mut tx = TxEip1559 {
                chain_id: 1,
                nonce: 0,
                gas_limit: gas,
                max_fee_per_gas: 200000u128,
                max_priority_fee_per_gas: 100000u128,
                to: Address::random().into(),
                value: U256::from(1000000u128),
                access_list: Default::default(),
                input: bytes!("").clone(),
            };
            total_gas = total_gas.saturating_add(gas);

            let signature = signer.sign_transaction_sync(&mut tx).unwrap();
            let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
            let tx_hash = envelope.clone().try_into_recovered().unwrap().tx_hash();
            tx_hashes.push(tx_hash);

            // Encode the transaction
            let mut encoded = vec![];
            envelope.encode_2718(&mut encoded);
            encoded_txs.push(Bytes::from(encoded));
        }

        let bundle = Bundle {
            txs: encoded_txs,
            block_number: 0,
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: vec![],
            ..Default::default()
        };

        // Test should fail due to exceeding gas limit
        let result = validate_bundle(&bundle, total_gas, tx_hashes);
        assert!(result.is_err());
        if let Err(e) = result {
            let error_message = format!("{e:?}");
            assert!(error_message.contains("Bundle gas limit exceeds maximum allowed"));
        }
    }

    #[tokio::test]
    async fn test_err_bundle_too_many_transactions() {
        let signer = PrivateKeySigner::random();
        let mut encoded_txs = vec![];
        let mut tx_hashes = vec![];

        let gas = 4_000_000;
        let mut total_gas = 0u64;
        for _ in 0..4 {
            let mut tx = TxEip1559 {
                chain_id: 1,
                nonce: 0,
                gas_limit: gas,
                max_fee_per_gas: 200000u128,
                max_priority_fee_per_gas: 100000u128,
                to: Address::random().into(),
                value: U256::from(1000000u128),
                access_list: Default::default(),
                input: bytes!("").clone(),
            };
            total_gas = total_gas.saturating_add(gas);

            let signature = signer.sign_transaction_sync(&mut tx).unwrap();
            let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
            let tx_hash = envelope.clone().try_into_recovered().unwrap().tx_hash();
            tx_hashes.push(tx_hash);

            // Encode the transaction
            let mut encoded = vec![];
            envelope.encode_2718(&mut encoded);
            encoded_txs.push(Bytes::from(encoded));
        }

        let bundle = Bundle {
            txs: encoded_txs,
            block_number: 0,
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: vec![],
            ..Default::default()
        };

        // Test should fail due to exceeding gas limit
        let result = validate_bundle(&bundle, total_gas, tx_hashes);
        assert!(result.is_err());
        if let Err(e) = result {
            let error_message = format!("{e:?}");
            assert!(error_message.contains("Bundle can only contain 3 transactions"));
        }
    }

    #[tokio::test]
    async fn test_err_bundle_partial_transaction_dropping_not_supported() {
        let bundle =
            Bundle { txs: vec![], dropping_tx_hashes: vec![B256::random()], ..Default::default() };
        assert_eq!(
            validate_bundle(&bundle, 0, vec![]),
            Err(EthApiError::InvalidParams("Partial transaction dropping is not supported".into())
                .into_rpc_err())
        );
    }

    #[tokio::test]
    async fn test_err_bundle_not_all_tx_hashes_in_reverting_tx_hashes() {
        let signer = PrivateKeySigner::random();
        let mut encoded_txs = vec![];
        let mut tx_hashes = vec![];

        let gas = 4_000_000;
        let mut total_gas = 0u64;
        for _ in 0..3 {
            let mut tx = TxEip1559 {
                chain_id: 1,
                nonce: 0,
                gas_limit: gas,
                max_fee_per_gas: 200000u128,
                max_priority_fee_per_gas: 100000u128,
                to: Address::random().into(),
                value: U256::from(1000000u128),
                access_list: Default::default(),
                input: bytes!("").clone(),
            };
            total_gas = total_gas.saturating_add(gas);

            let signature = signer.sign_transaction_sync(&mut tx).unwrap();
            let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
            let tx_hash = envelope.clone().try_into_recovered().unwrap().tx_hash();
            tx_hashes.push(tx_hash);

            // Encode the transaction
            let mut encoded = vec![];
            envelope.encode_2718(&mut encoded);
            encoded_txs.push(Bytes::from(encoded));
        }

        let bundle = Bundle {
            txs: encoded_txs,
            block_number: 0,
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: tx_hashes[..2].to_vec(),
            ..Default::default()
        };

        // Test should fail due to mismatched reverting_tx_hashes
        let result = validate_bundle(&bundle, total_gas, tx_hashes);
        assert!(result.is_err());
        if let Err(e) = result {
            let error_message = format!("{e:?}");
            assert!(error_message.contains("reverting_tx_hashes must include all hashes"));
        }
    }

    #[tokio::test]
    async fn test_decode_tx_rejects_empty_bytes() {
        // Test that empty bytes fail to decode
        let empty_bytes = Bytes::new();
        let result = OpTxEnvelope::decode_2718(&mut empty_bytes.as_ref());
        assert!(result.is_err(), "Empty bytes should fail decoding");
    }

    #[tokio::test]
    async fn test_decode_tx_rejects_invalid_bytes() {
        // Test that malformed bytes fail to decode
        let invalid_bytes = Bytes::from(vec![0x01, 0x02, 0x03]);
        let result = OpTxEnvelope::decode_2718(&mut invalid_bytes.as_ref());
        assert!(result.is_err(), "Invalid bytes should fail decoding");
    }
}
