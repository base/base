use alloy_consensus::{Transaction, transaction::Recovered};
use alloy_eips::Encodable2718;
use alloy_primitives::U256;
use derive_more::Display;
use op_revm::{OpSpecId, l1block::L1BlockInfo};
use reth_primitives_traits::{Account, Bytecode};
use tracing::warn;

/// Errors that can occur when validating a transaction.
#[derive(Debug, PartialEq, Eq, Display)]
pub enum TxValidationError {
    /// Signer account has non-EIP-7702 bytecode (i.e., it's a contract, not an EOA)
    #[display("Signer account has bytecode that is not EIP-7702 delegation")]
    SignerAccountHasBytecode,
    /// EIP-7702 transaction has empty authorization list
    #[display("EIP-7702 transaction has empty authorization list")]
    AuthorizationListIsEmpty,
    /// Transaction nonce is too low
    #[display("Transaction nonce: {_0} is too low, account nonce: {_1}")]
    TransactionNonceTooLow(u64, u64),
    /// Insufficient funds for transfer
    #[display("Insufficient funds for transfer: {_0}, account balance: {_1}")]
    InsufficientFundsForTransfer(U256, U256),
    /// Insufficient funds for L1 gas
    #[display("Insufficient funds for L1 gas: {_0}, account balance: {_1}")]
    InsufficientFundsForL1Gas(U256, U256),
}

/// Helper function to validate a transaction. A valid transaction must satisfy the following criteria:
/// - The transaction is not EIP-4844
/// - If the account has bytecode, it MUST be EIP-7702 bytecode
/// - The transaction's nonce is the latest
/// - The transaction's execution cost is less than the account's balance
/// - The transaction's L1 gas cost is less than the account's balance
///   
/// Note: We don't need to check for EIP-4844 because bundle transactions are Recovered<OpTxEnvelope>
/// which only includes Legacy, Eip2930, Eip1559, Eip7702, and Deposit.
pub fn validate_tx<T: Transaction + Encodable2718>(
    account: Account,
    sender_code: Option<&Bytecode>,
    txn: &Recovered<T>,
    l1_block_info: &mut L1BlockInfo,
) -> Result<(), TxValidationError> {
    let data = txn.encoded_2718();

    // If an account has bytecode, it MUST be EIP-7702 bytecode
    if let Some(bytecode) = sender_code
        && !bytecode.is_eip7702()
    {
        return Err(TxValidationError::SignerAccountHasBytecode);
    }

    // If tx is 7702 type, it needs a valid authorization list
    // https://github.com/paradigmxyz/reth/blob/68e4ff1f7d9f5b40bc02ba7433077dcba0456783/crates/transaction-pool/src/validate/eth.rs#L476
    if txn.is_eip7702() && txn.authorization_list().is_none_or(|l| l.is_empty()) {
        return Err(TxValidationError::AuthorizationListIsEmpty);
    }

    // Return error if tx nonce is not equal to or greater than the latest on chain
    // https://github.com/paradigmxyz/reth/blob/a047a055ab996f85a399f5cfb2fe15e350356546/crates/transaction-pool/src/validate/eth.rs#L611
    if txn.nonce() < account.nonce {
        return Err(TxValidationError::TransactionNonceTooLow(txn.nonce(), account.nonce));
    }

    // For EIP-1559 transactions: `max_fee_per_gas * gas_limit + tx_value`.
    let max_fee = txn.max_fee_per_gas().saturating_mul(txn.gas_limit() as u128);
    let txn_cost = txn.value().saturating_add(U256::from(max_fee));

    // Return error if execution cost exceeds balance
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
    use std::sync::Arc;

    use alloy_consensus::{
        SignableTransaction, Transaction, TxEip1559, TxEip7702, transaction::SignerRecoverable,
    };
    use alloy_eips::eip7702::Authorization;
    use alloy_primitives::{Address, Bytes, U256, bytes};
    use base_alloy_consensus::OpTxEnvelope;
    use base_alloy_network::TxSignerSync;
    use base_client_node::test_utils::{Account as BaseAccount, SignerSync};
    use revm_bytecode::eip7702::Eip7702Bytecode;

    use super::*;

    fn create_account(nonce: u64, balance: U256) -> Account {
        Account { balance, nonce, bytecode_hash: None }
    }

    fn create_eip7702_bytecode() -> Bytecode {
        Bytecode(revm_bytecode::Bytecode::Eip7702(Arc::new(
            Eip7702Bytecode::new(Address::random()),
        )))
    }

    fn create_contract_bytecode() -> Bytecode {
        Bytecode::new_raw(Bytes::from_static(&[0x60, 0x80, 0x60, 0x40, 0x52]))
    }

    fn create_l1_block_info() -> L1BlockInfo {
        L1BlockInfo::default()
    }

    fn create_signed_authorization(
        chain_id: u64,
        contract_address: Address,
        nonce: u64,
        account: &BaseAccount,
    ) -> alloy_eips::eip7702::SignedAuthorization {
        let auth =
            Authorization { chain_id: U256::from(chain_id), address: contract_address, nonce };
        let signature = account.signer().sign_hash_sync(&auth.signature_hash()).unwrap();
        auth.into_signed(signature)
    }

    #[test]
    fn test_valid_tx_no_bytecode() {
        // Create a sample EIP-1559 transaction
        let signer = BaseAccount::Alice.signer();
        let mut tx = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 20000000000u128,
            max_priority_fee_per_gas: 1000000000u128,
            to: Address::random().into(),
            value: U256::from(10000000000000u128),
            access_list: Default::default(),
            input: bytes!(""),
        };

        let account = create_account(0, U256::from(1000000000000000000u128));
        let mut l1_block_info = create_l1_block_info();

        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();
        assert!(validate_tx(account, None, &recovered_tx, &mut l1_block_info).is_ok());
    }

    #[test]
    fn test_valid_eip1559_tx_from_delegated_account() {
        let signer = BaseAccount::Alice.signer();
        let mut tx = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 20000000000u128,
            max_priority_fee_per_gas: 1000000000u128,
            to: Address::random().into(),
            value: U256::from(10000000000000u128),
            access_list: Default::default(),
            input: bytes!(""),
        };

        let account = create_account(0, U256::from(1000000000000000000u128));
        let eip7702_code = create_eip7702_bytecode();
        let mut l1_block_info = create_l1_block_info();

        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();
        assert!(
            validate_tx(account, Some(&eip7702_code), &recovered_tx, &mut l1_block_info).is_ok()
        );
    }

    #[test]
    fn test_valid_7702_tx_from_delegated_account() {
        let signer = BaseAccount::Alice.signer();
        let delegate_to = Address::random();
        let authorization = create_signed_authorization(1, delegate_to, 0, &BaseAccount::Alice);

        let mut tx = TxEip7702 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 20000000000u128,
            max_priority_fee_per_gas: 1000000000u128,
            to: Address::random(),
            value: U256::from(10000000000000u128),
            authorization_list: vec![authorization],
            access_list: Default::default(),
            input: bytes!(""),
        };

        let account = create_account(0, U256::from(1000000000000000000u128));
        let eip7702_code = create_eip7702_bytecode();
        let mut l1_block_info = create_l1_block_info();

        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip7702(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();
        assert!(
            validate_tx(account, Some(&eip7702_code), &recovered_tx, &mut l1_block_info).is_ok()
        );
    }

    #[test]
    fn test_err_signer_has_contract_bytecode() {
        let signer = BaseAccount::Alice.signer();

        let mut tx = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 20000000000u128,
            max_priority_fee_per_gas: 1000000000u128,
            to: Address::random().into(),
            value: U256::from(10000000000000u128),
            access_list: Default::default(),
            input: bytes!(""),
        };

        let account = create_account(0, U256::from(1000000000000000000u128));
        let contract_code = create_contract_bytecode();
        let mut l1_block_info = create_l1_block_info();

        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();

        assert_eq!(
            validate_tx(account, Some(&contract_code), &recovered_tx, &mut l1_block_info),
            Err(TxValidationError::SignerAccountHasBytecode)
        );
    }

    #[test]
    fn test_err_tx_nonce_too_low() {
        let signer = BaseAccount::Alice.signer();
        let mut tx = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 20000000000u128,
            max_priority_fee_per_gas: 1000000000u128,
            to: Address::random().into(),
            value: U256::from(10000000000000u128),
            access_list: Default::default(),
            input: bytes!(""),
        };

        let account = create_account(1, U256::from(1000000000000000000u128));
        let mut l1_block_info = create_l1_block_info();

        let tx_nonce = tx.nonce();
        let nonce = account.nonce;

        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();
        assert_eq!(
            validate_tx(account, None, &recovered_tx, &mut l1_block_info),
            Err(TxValidationError::TransactionNonceTooLow(tx_nonce, nonce))
        );
    }

    #[test]
    fn test_err_tx_insufficient_funds() {
        let signer = BaseAccount::Alice.signer();
        let mut tx = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 20000000000u128,
            max_priority_fee_per_gas: 10000000000000u128,
            to: Address::random().into(),
            value: U256::from(10000000000000u128),
            access_list: Default::default(),
            input: bytes!(""),
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
            validate_tx(account, None, &recovered_tx, &mut l1_block_info),
            Err(TxValidationError::InsufficientFundsForTransfer(txn_cost, account_balance))
        );
    }

    #[test]
    fn test_err_tx_insufficient_funds_for_l1_gas() {
        let signer = BaseAccount::Alice.signer();
        let mut tx = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 200000u128,
            max_priority_fee_per_gas: 100000u128,
            to: Address::random().into(),
            value: U256::from(1000000u128),
            access_list: Default::default(),
            input: bytes!(""),
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
            validate_tx(account, None, &recovered_tx, &mut l1_block_info),
            Err(TxValidationError::InsufficientFundsForL1Gas(l1_cost, account_balance))
        );
    }

    #[test]
    fn test_valid_7702_tx_from_eoa() {
        let signer = BaseAccount::Alice.signer();
        let delegate_to = Address::random();
        let authorization = create_signed_authorization(1, delegate_to, 0, &BaseAccount::Alice);

        let mut tx = TxEip7702 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 20000000000u128,
            max_priority_fee_per_gas: 1000000000u128,
            to: Address::random(),
            value: U256::from(10000000000000u128),
            authorization_list: vec![authorization],
            access_list: Default::default(),
            input: bytes!(""),
        };

        let account = create_account(0, U256::from(1000000000000000000u128));
        let mut l1_block_info = create_l1_block_info();

        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip7702(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();
        assert!(
            validate_tx(account, None, &recovered_tx, &mut l1_block_info).is_ok(),
            "EOA setting up delegation for the first time should be valid"
        );
    }

    #[test]
    fn test_err_7702_tx_with_empty_authorization_list() {
        let signer = BaseAccount::Alice.signer();
        let mut tx = TxEip7702 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21000,
            max_fee_per_gas: 20000000000u128,
            max_priority_fee_per_gas: 1000000000u128,
            to: Address::random(),
            value: U256::from(10000000000000u128),
            authorization_list: vec![],
            access_list: Default::default(),
            input: bytes!(""),
        };

        let account = create_account(0, U256::from(1000000000000000000u128));
        let mut l1_block_info = create_l1_block_info();

        let signature = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = OpTxEnvelope::Eip7702(tx.into_signed(signature));
        let recovered_tx = envelope.try_into_recovered().unwrap();

        assert_eq!(
            validate_tx(account, None, &recovered_tx, &mut l1_block_info),
            Err(TxValidationError::AuthorizationListIsEmpty)
        );
    }
}
