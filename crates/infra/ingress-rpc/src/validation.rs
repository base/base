use alloy_consensus::private::alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_consensus::{Transaction, Typed2718, constants::KECCAK_EMPTY, transaction::Recovered};
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_mev::EthSendBundle;
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use op_alloy_consensus::interop::CROSS_L2_INBOX_ADDRESS;
use op_alloy_network::Optimism;
use op_revm::{OpSpecId, l1block::L1BlockInfo};
use reth_optimism_evm::extract_l1_info_from_tx;
use reth_rpc_eth_types::{EthApiError, RpcInvalidTransactionError, SignError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::warn;

// TODO: make this configurable
const MAX_BUNDLE_GAS: u64 = 30_000_000;

/// Account info for a given address
pub struct AccountInfo {
    pub balance: U256,
    pub nonce: u64,
    pub code_hash: B256,
}

/// Interface for fetching account info for a given address
#[async_trait]
pub trait AccountInfoLookup: Send + Sync {
    async fn fetch_account_info(&self, address: Address) -> RpcResult<AccountInfo>;
}

/// Implementation of the `AccountInfoLookup` trait for the `RootProvider`
#[async_trait]
impl AccountInfoLookup for RootProvider<Optimism> {
    async fn fetch_account_info(&self, address: Address) -> RpcResult<AccountInfo> {
        let account = self
            .get_account(address)
            .await
            .map_err(|_| EthApiError::Signing(SignError::NoAccount))?;
        Ok(AccountInfo {
            balance: account.balance,
            nonce: account.nonce,
            code_hash: account.code_hash,
        })
    }
}

/// Interface for fetching L1 block info for a given block number
#[async_trait]
pub trait L1BlockInfoLookup: Send + Sync {
    async fn fetch_l1_block_info(&self) -> RpcResult<L1BlockInfo>;
}

/// Implementation of the `L1BlockInfoLookup` trait for the `RootProvider`
#[async_trait]
impl L1BlockInfoLookup for RootProvider<Optimism> {
    async fn fetch_l1_block_info(&self) -> RpcResult<L1BlockInfo> {
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

        let txs = block.transactions.clone();
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

/// Helper function to validate a transaction. A valid transaction must satisfy the following criteria:
/// - If the transaction is not EIP-4844
/// - If the transaction is not a cross chain tx
/// - If the transaction is a 7702 tx, then the account is a 7702 account
/// - If the transaction's nonce is the latest
/// - If the transaction's execution cost is less than the account's balance
/// - If the transaction's L1 gas cost is less than the account's balance
pub async fn validate_tx<T: Transaction>(
    account: AccountInfo,
    txn: &Recovered<T>,
    data: &[u8],
    l1_block_info: &mut L1BlockInfo,
) -> RpcResult<()> {
    // skip eip4844 transactions
    if txn.is_eip4844() {
        warn!(message = "EIP-4844 transactions are not supported");
        return Err(RpcInvalidTransactionError::TxTypeNotSupported.into_rpc_err());
    }

    // from: https://github.com/paradigmxyz/reth/blob/3b0d98f3464b504d96154b787a860b2488a61b3e/crates/optimism/txpool/src/supervisor/client.rs#L76-L84
    // it returns `None` if a tx is not cross chain, which is when `inbox_entries` is empty in the snippet above.
    // we can do something similar where if the inbox_entries is non-empty then it is a cross chain tx and it's something we don't support
    if let Some(access_list) = txn.access_list() {
        let inbox_entries = access_list
            .iter()
            .filter(|entry| entry.address == CROSS_L2_INBOX_ADDRESS);
        if inbox_entries.count() > 0 {
            warn!(message = "Interop transactions are not supported");
            return Err(RpcInvalidTransactionError::TxTypeNotSupported.into_rpc_err());
        }
    }

    // error if account is 7702 but tx is not 7702
    if account.code_hash != KECCAK_EMPTY && !txn.is_eip7702() {
        return Err(EthApiError::InvalidTransaction(
            RpcInvalidTransactionError::AuthorizationListInvalidFields,
        )
        .into_rpc_err());
    }

    // error if tx nonce is not equal to or greater than the latest on chain
    // https://github.com/paradigmxyz/reth/blob/a047a055ab996f85a399f5cfb2fe15e350356546/crates/transaction-pool/src/validate/eth.rs#L611
    if txn.nonce() < account.nonce {
        return Err(
            EthApiError::InvalidTransaction(RpcInvalidTransactionError::NonceTooLow {
                tx: txn.nonce(),
                state: account.nonce,
            })
            .into_rpc_err(),
        );
    }

    // For EIP-1559 transactions: `max_fee_per_gas * gas_limit + tx_value`.
    // ref: https://github.com/paradigmxyz/reth/blob/main/crates/transaction-pool/src/traits.rs#L1186
    let max_fee = txn
        .max_fee_per_gas()
        .saturating_mul(txn.gas_limit() as u128);
    let txn_cost = txn.value().saturating_add(U256::from(max_fee));

    // error if execution cost costs more than balance
    if txn_cost > account.balance {
        warn!(message = "Insufficient funds for transfer");
        return Err(EthApiError::InvalidTransaction(
            RpcInvalidTransactionError::InsufficientFundsForTransfer,
        )
        .into_rpc_err());
    }

    // op-checks to see if sender can cover L1 gas cost
    // from: https://github.com/paradigmxyz/reth/blob/6aa73f14808491aae77fc7c6eb4f0aa63bef7e6e/crates/optimism/txpool/src/validator.rs#L219
    let l1_cost_addition = l1_block_info.calculate_tx_l1_cost(data, OpSpecId::ISTHMUS);
    let l1_cost = txn_cost.saturating_add(l1_cost_addition);
    if l1_cost > account.balance {
        warn!(message = "Insufficient funds for L1 gas");
        return Err(EthApiError::InvalidTransaction(
            RpcInvalidTransactionError::InsufficientFundsForTransfer,
        )
        .into_rpc_err());
    }
    Ok(())
}

/// Helper function to validate propeties of a bundle. A bundle is valid if it satisfies the following criteria:
/// - The bundle's max_timestamp is not more than 1 hour in the future
/// - The bundle's gas limit is not greater than the maximum allowed gas limit
pub fn validate_bundle(bundle: &EthSendBundle, bundle_gas: u64) -> RpcResult<()> {
    // Don't allow bundles to be submitted over 1 hour into the future
    // TODO: make the window configurable
    let valid_timestamp_window = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
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
        return Err(
            EthApiError::InvalidParams("Bundle gas limit exceeds maximum allowed".into())
                .into_rpc_err(),
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::SignableTransaction;
    use alloy_consensus::{Transaction, constants::KECCAK_EMPTY, transaction::SignerRecoverable};
    use alloy_consensus::{TxEip1559, TxEip4844, TxEip7702};
    use alloy_primitives::Bytes;
    use alloy_primitives::{bytes, keccak256};
    use alloy_signer_local::PrivateKeySigner;
    use op_alloy_consensus::OpTxEnvelope;
    use op_alloy_network::TxSignerSync;
    use op_alloy_network::eip2718::Encodable2718;
    use revm_context_interface::transaction::{AccessList, AccessListItem};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn create_account(nonce: u64, balance: U256) -> AccountInfo {
        AccountInfo {
            balance,
            nonce,
            code_hash: KECCAK_EMPTY,
        }
    }

    fn create_7702_account() -> AccountInfo {
        AccountInfo {
            balance: U256::from(1000000000000000000u128),
            nonce: 0,
            code_hash: keccak256(bytes!("1234567890")),
        }
    }

    fn create_l1_block_info() -> L1BlockInfo {
        L1BlockInfo::default()
    }

    #[tokio::test]
    async fn test_valid_tx() {
        // Create a sample EIP-1559 transaction
        let signer = PrivateKeySigner::random();
        let mut tx = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 20000000000u128,
            max_priority_fee_per_gas: 1000000000u128,
            to: Address::random().into(),
            value: U256::from(10000000000000u128),
            access_list: Default::default(),
            input: bytes!("").clone(),
        };

        let account = create_account(0, U256::from(1000000000000000000u128));
        let mut l1_block_info = create_l1_block_info();

        let data = tx.input().to_vec();
        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();
        assert!(
            validate_tx(account, &recovered_tx, &data, &mut l1_block_info)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_valid_7702_tx() {
        let signer = PrivateKeySigner::random();
        let mut tx = TxEip7702 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 20000000000u128,
            max_priority_fee_per_gas: 1000000000u128,
            to: Address::random(),
            value: U256::from(10000000000000u128),
            authorization_list: Default::default(),
            access_list: Default::default(),
            input: bytes!("").clone(),
        };

        let account = create_7702_account();
        let mut l1_block_info = create_l1_block_info();

        let data = tx.input().to_vec();
        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip7702(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();
        assert!(
            validate_tx(account, &recovered_tx, &data, &mut l1_block_info)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_err_interop_tx() {
        let signer = PrivateKeySigner::random();

        let access_list = AccessList::from(vec![AccessListItem {
            address: CROSS_L2_INBOX_ADDRESS,
            storage_keys: vec![],
        }]);

        let mut tx = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 20000000000u128,
            max_priority_fee_per_gas: 1000000000u128,
            to: Address::random().into(),
            value: U256::from(10000000000000u128),
            access_list,
            input: bytes!("").clone(),
        };

        let account = create_account(0, U256::from(1000000000000000000u128));
        let mut l1_block_info = create_l1_block_info();

        let data = tx.input().to_vec();
        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();

        assert_eq!(
            validate_tx(account, &recovered_tx, &data, &mut l1_block_info).await,
            Err(RpcInvalidTransactionError::TxTypeNotSupported.into_rpc_err())
        );
    }

    #[tokio::test]
    async fn test_err_eip4844_tx() {
        let signer = PrivateKeySigner::random();
        let mut tx = TxEip4844 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 20000000000u128,
            max_priority_fee_per_gas: 1000000000u128,
            to: Address::random(),
            value: U256::from(10000000000000u128),
            access_list: Default::default(),
            input: bytes!("").clone(),
            blob_versioned_hashes: Default::default(),
            max_fee_per_blob_gas: 20000000000u128,
        };

        let account = create_account(0, U256::from(1000000000000000000u128));
        let mut l1_block_info = create_l1_block_info();

        let data = tx.input().to_vec();
        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let recovered_tx = tx
            .into_signed(signature)
            .try_into_recovered()
            .expect("failed to recover tx");
        assert_eq!(
            validate_tx(account, &recovered_tx, &data, &mut l1_block_info).await,
            Err(RpcInvalidTransactionError::TxTypeNotSupported.into_rpc_err())
        );
    }

    #[tokio::test]
    async fn test_err_tx_not_7702() {
        let signer = PrivateKeySigner::random();

        let mut tx = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 20000000000u128,
            max_priority_fee_per_gas: 1000000000u128,
            to: Address::random().into(),
            value: U256::from(10000000000000u128),
            access_list: Default::default(),
            input: bytes!("").clone(),
        };

        // account is 7702
        let account = create_7702_account();
        let mut l1_block_info = create_l1_block_info();

        let data = tx.input().to_vec();
        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();

        assert_eq!(
            validate_tx(account, &recovered_tx, &data, &mut l1_block_info).await,
            Err(EthApiError::InvalidTransaction(
                RpcInvalidTransactionError::AuthorizationListInvalidFields,
            )
            .into_rpc_err())
        );
    }

    #[tokio::test]
    async fn test_err_tx_nonce_too_low() {
        let signer = PrivateKeySigner::random();
        let mut tx = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 20000000000u128,
            max_priority_fee_per_gas: 1000000000u128,
            to: Address::random().into(),
            value: U256::from(10000000000000u128),
            access_list: Default::default(),
            input: bytes!("").clone(),
        };

        let account = create_account(1, U256::from(1000000000000000000u128));
        let mut l1_block_info = create_l1_block_info();

        let nonce = account.nonce;
        let tx_nonce = tx.nonce();

        let data = tx.input().to_vec();
        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();
        assert_eq!(
            validate_tx(account, &recovered_tx, &data, &mut l1_block_info).await,
            Err(
                EthApiError::InvalidTransaction(RpcInvalidTransactionError::NonceTooLow {
                    tx: tx_nonce,
                    state: nonce,
                })
                .into_rpc_err()
            )
        );
    }

    #[tokio::test]
    async fn test_err_tx_insufficient_funds() {
        let signer = PrivateKeySigner::random();
        let mut tx = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 20000000000u128,
            max_priority_fee_per_gas: 10000000000000u128,
            to: Address::random().into(),
            value: U256::from(10000000000000u128),
            access_list: Default::default(),
            input: bytes!("").clone(),
        };

        let account = create_account(0, U256::from(1000000u128));
        let mut l1_block_info = create_l1_block_info();

        let data = tx.input().to_vec();
        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();
        assert_eq!(
            validate_tx(account, &recovered_tx, &data, &mut l1_block_info).await,
            Err(EthApiError::InvalidTransaction(
                RpcInvalidTransactionError::InsufficientFundsForTransfer,
            )
            .into_rpc_err())
        );
    }

    #[tokio::test]
    async fn test_err_tx_insufficient_funds_for_l1_gas() {
        let signer = PrivateKeySigner::random();
        let mut tx = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 200000u128,
            max_priority_fee_per_gas: 100000u128,
            to: Address::random().into(),
            value: U256::from(1000000u128),
            access_list: Default::default(),
            input: bytes!("").clone(),
        };

        // fund the account with enough funds to cover the txn cost but not enough to cover the l1 cost
        let account = create_account(0, U256::from(4201000000u128));
        let mut l1_block_info = create_l1_block_info();
        l1_block_info.tx_l1_cost = Some(U256::from(1000000u128));

        let data = tx.input().to_vec();
        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();

        assert_eq!(
            validate_tx(account, &recovered_tx, &data, &mut l1_block_info).await,
            Err(EthApiError::InvalidTransaction(
                RpcInvalidTransactionError::InsufficientFundsForTransfer
            )
            .into_rpc_err())
        );
    }

    #[tokio::test]
    async fn test_err_bundle_max_timestamp_too_far_in_the_future() {
        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let too_far_in_the_future = current_time + 3601;
        let bundle = EthSendBundle {
            txs: vec![],
            max_timestamp: Some(too_far_in_the_future),
            ..Default::default()
        };
        assert_eq!(
            validate_bundle(&bundle, 0),
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

        // Create transactions that collectively exceed MAX_BUNDLE_GAS (30M)
        // Each transaction uses 4M gas, so 8 transactions = 32M gas > 30M limit
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

            // Encode the transaction
            let mut encoded = vec![];
            envelope.encode_2718(&mut encoded);
            encoded_txs.push(Bytes::from(encoded));
        }

        let bundle = EthSendBundle {
            txs: encoded_txs,
            block_number: 0,
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: vec![],
            ..Default::default()
        };

        // Test should fail due to exceeding gas limit
        let result = validate_bundle(&bundle, total_gas);
        assert!(result.is_err());
        if let Err(e) = result {
            let error_message = format!("{e:?}");
            assert!(error_message.contains("Bundle gas limit exceeds maximum allowed"));
        }
    }
}
