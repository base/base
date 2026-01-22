use alloy_consensus::{Transaction, Typed2718, transaction::Recovered};
use alloy_primitives::U256;
use alloy_eips::Encodable2718;
use reth_primitives_traits::Account;
use op_alloy_consensus::interop::CROSS_L2_INBOX_ADDRESS;
use op_revm::{OpSpecId, l1block::L1BlockInfo};
use tracing::warn;

/// Errors that can occur when validating a transaction.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum TxValidationError {
    /// Interop transactions are not supported
    #[error("Interop transactions are not supported")]
    InteropNotSupported,
    /// Account is 7702 but tx is not 7702
    #[error("Account is 7702 but tx is not 7702")]
    AccountIs7702ButTxIsNot7702,
    /// Transaction nonce is too low
    #[error("Transaction nonce is too low")]
    TransactionNonceTooLow(u64, u64),
    /// Insufficient funds for transfer
    #[error("Insufficient funds for transfer")]
    InsufficientFundsForTransfer(U256, U256),
    /// Insufficient funds for L1 gas
    #[error("Insufficient funds for L1 gas")]
    InsufficientFundsForL1Gas(U256, U256),
}

/// Helper function to validate a transaction. A valid transaction must satisfy the following criteria:
/// - If the transaction is not EIP-4844
/// - If the transaction is not a cross chain tx
/// - If the transaction is a 7702 tx, then the account is a 7702 account
/// - If the transaction's nonce is the latest
/// - If the transaction's execution cost is less than the account's balance
/// - If the transaction's L1 gas cost is less than the account's balance
/// Note: We don't need to check for EIP-4844 because bundle transactions are Recovered<OpTxEnvelope>
/// which only Legacy, Eip2930, Eip1559, Eip7702, and Deposit.
pub fn validate_tx<T: Transaction + Encodable2718>(
    account: Account,
    txn: &Recovered<T>,
    l1_block_info: &mut L1BlockInfo,
) -> Result<(), TxValidationError> {
    let data = txn.encoded_2718();

    // from: https://github.com/paradigmxyz/reth/blob/3b0d98f3464b504d96154b787a860b2488a61b3e/crates/optimism/txpool/src/supervisor/client.rs#L76-L84
    // it returns `None` if a tx is not cross chain, which is when `inbox_entries` is empty in the snippet above.
    // we can do something similar where if the inbox_entries is non-empty then it is a cross chain tx and it's something we don't support
    if let Some(access_list) = txn.access_list() {
        let inbox_entries =
            access_list.iter().filter(|entry| entry.address == CROSS_L2_INBOX_ADDRESS);
        if inbox_entries.count() > 0 {
            warn!(message = "Interop transactions are not supported");
            return Err(TxValidationError::InteropNotSupported);
        }
    }

    // error if account is 7702 but tx is not 7702
    if account.bytecode_hash.is_some() && !txn.is_eip7702() {
        return Err(TxValidationError::AccountIs7702ButTxIsNot7702);
    }

    // error if tx nonce is not equal to or greater than the latest on chain
    // https://github.com/paradigmxyz/reth/blob/a047a055ab996f85a399f5cfb2fe15e350356546/crates/transaction-pool/src/validate/eth.rs#L611
    if txn.nonce() < account.nonce {
        return Err(TxValidationError::TransactionNonceTooLow(txn.nonce(), account.nonce));
    }

    // For EIP-1559 transactions: `max_fee_per_gas * gas_limit + tx_value`.
    // ref: https://github.com/paradigmxyz/reth/blob/main/crates/transaction-pool/src/traits.rs#L1186
    let max_fee = txn.max_fee_per_gas().saturating_mul(txn.gas_limit() as u128);
    let txn_cost = txn.value().saturating_add(U256::from(max_fee));

    // error if execution cost costs more than balance
    if txn_cost > account.balance {
        warn!(message = "Insufficient funds for transfer");
        return Err(TxValidationError::InsufficientFundsForTransfer(txn_cost, account.balance));
    }

    // op-checks to see if sender can cover L1 gas cost
    // from: https://github.com/paradigmxyz/reth/blob/6aa73f14808491aae77fc7c6eb4f0aa63bef7e6e/crates/optimism/txpool/src/validator.rs#L219
    let l1_cost_addition = l1_block_info.calculate_tx_l1_cost(&data, OpSpecId::ISTHMUS);
    let l1_cost = txn_cost.saturating_add(l1_cost_addition);
    if l1_cost > account.balance {
        warn!(message = "Insufficient funds for L1 gas");
        return Err(TxValidationError::InsufficientFundsForL1Gas(l1_cost, account.balance));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{
        SignableTransaction, Transaction, TxEip1559, TxEip7702, constants::KECCAK_EMPTY,
        transaction::SignerRecoverable,
    };
    use alloy_primitives::{Address, bytes, keccak256};
    use alloy_signer_local::PrivateKeySigner;
    use op_alloy_consensus::OpTxEnvelope;
    use op_alloy_network::TxSignerSync;
    use revm_context_interface::transaction::{AccessList, AccessListItem};

    use super::*;

    fn create_account(nonce: u64, balance: U256) -> Account {
        Account { balance, nonce, bytecode_hash: Some(KECCAK_EMPTY) }
    }

    fn create_7702_account() -> Account {
        Account {
            balance: U256::from(1000000000000000000u128),
            nonce: 0,
            bytecode_hash: Some(keccak256(bytes!("1234567890"))),
        }
    }

    fn create_l1_block_info() -> L1BlockInfo {
        L1BlockInfo::default()
    }

    #[test]
    fn test_valid_tx() {
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

        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();
        assert!(validate_tx(account, &recovered_tx, &mut l1_block_info).is_ok());
    }

    #[test]
    fn test_valid_7702_tx() {
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

        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip7702(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();
        assert!(validate_tx(account, &recovered_tx, &mut l1_block_info).is_ok());
    }

    #[test]
    fn test_err_interop_tx() {
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

        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();

        assert_eq!(
            validate_tx(account, &recovered_tx, &mut l1_block_info),
            Err(TxValidationError::InteropNotSupported)
        );
    }

    #[test]
    fn test_err_tx_not_7702() {
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

        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();

        assert_eq!(
            validate_tx(account, &recovered_tx, &mut l1_block_info),
            Err(TxValidationError::AccountIs7702ButTxIsNot7702)
        );
    }

    #[test]
    fn test_err_tx_nonce_too_low() {
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

        let tx_nonce = tx.nonce();
        let nonce = account.nonce;

        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();
        assert_eq!(
            validate_tx(account, &recovered_tx, &mut l1_block_info),
            Err(TxValidationError::TransactionNonceTooLow(tx_nonce, nonce))
        );
    }

    #[test]
    fn test_err_tx_insufficient_funds() {
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

        let account_balance = U256::from(1000000u128);
        let account = create_account(0, account_balance);
        let mut l1_block_info = create_l1_block_info();

        let max_fee = tx.max_fee_per_gas().saturating_mul(tx.gas_limit() as u128);
        let txn_cost = tx.value().saturating_add(U256::from(max_fee));

        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();
        assert_eq!(
            validate_tx(account, &recovered_tx, &mut l1_block_info),
            Err(TxValidationError::InsufficientFundsForTransfer(txn_cost, account_balance))
        );
    }

    #[test]
    fn test_err_tx_insufficient_funds_for_l1_gas() {
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
        let account_balance = U256::from(4201000000u128);
        let account = create_account(0, account_balance);
        let mut l1_block_info = create_l1_block_info();
        l1_block_info.tx_l1_cost = Some(U256::from(1000000u128));

        let max_fee = tx.max_fee_per_gas().saturating_mul(tx.gas_limit() as u128);
        let txn_cost = tx.value().saturating_add(U256::from(max_fee));
        let l1_cost_addition = l1_block_info.calculate_tx_l1_cost(tx.input(), OpSpecId::ISTHMUS);
        let l1_cost = txn_cost.saturating_add(l1_cost_addition);

        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();

        assert_eq!(
            validate_tx(account, &recovered_tx, &mut l1_block_info),
            Err(TxValidationError::InsufficientFundsForL1Gas(l1_cost, account_balance))
        );
    }
}
