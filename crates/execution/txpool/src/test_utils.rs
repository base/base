//! Fixtures shared by the unit tests of this crate.

use std::sync::Arc;

use alloy_consensus::{
    SignableTransaction, TxEip1559,
    transaction::{Recovered, SignerRecoverable},
};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_chains::ChainConfig;
use base_common_consensus::{
    BasePooledTransaction as ConsensusPooledTransaction, BasePrimitives, BaseTransactionSigned,
    BaseTxEnvelope, Eip8130Constants, Eip8130Signed, TxDeposit, TxEip8130,
};
use base_execution_chainspec::BaseChainSpec;
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_transaction_pool::PoolTransaction;

use crate::BasePooledTransaction;

/// Builders for the signed transactions and funded accounts the unit tests pool.
#[derive(Debug)]
pub struct TxFixtures;

impl TxFixtures {
    /// The gas limit of the transactions built by [`Self::eip8130_tx`] and [`Self::eip1559`].
    pub const GAS_LIMIT: u64 = 50_000;

    /// The chain id every fixture transaction is bound to.
    pub fn chain_id() -> u64 {
        ChainConfig::mainnet().chain_id
    }

    /// An account holding one ether, enough to pay the fees of any fixture transaction.
    pub fn funded_account() -> ExtendedAccount {
        ExtendedAccount::new(0, U256::from(1_000_000_000_000_000_000u64))
    }

    /// Seeds `account` with [`Self::funded_account`] in `client`.
    pub fn fund(client: &MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>, account: Address) {
        client.add_account(account, Self::funded_account());
    }

    /// An unsigned EIP-8130 transaction on the test chain with no calls and no payer.
    pub fn eip8130_tx(nonce_key: U256, nonce_sequence: u64) -> TxEip8130 {
        TxEip8130 {
            chain_id: Self::chain_id(),
            nonce_key,
            nonce_sequence,
            gas_limit: Self::GAS_LIMIT,
            ..Default::default()
        }
    }

    /// Signs `tx` as `signer` and, when `payer` is set, sponsors it with that payer.
    pub fn sign_eip8130(
        signer: &PrivateKeySigner,
        payer: Option<&PrivateKeySigner>,
        mut tx: TxEip8130,
    ) -> BasePooledTransaction {
        tx.payer = payer.map(PrivateKeySigner::address);
        let sender_auth = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let payer_auth = payer.map_or_else(Vec::new, |payer| {
            let payer_sig =
                payer.sign_hash_sync(&tx.payer_signature_hash(signer.address())).unwrap();
            let mut payer_auth = Eip8130Constants::K1_AUTHENTICATOR.to_vec();
            payer_auth.extend_from_slice(&payer_sig.as_bytes());
            payer_auth
        });
        let signed = Eip8130Signed::new(
            tx,
            Bytes::from(sender_auth.as_bytes().to_vec()),
            Bytes::from(payer_auth),
        );
        let pooled = ConsensusPooledTransaction::Eip8130(signed);
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(pooled, signer.address()))
    }

    /// Signs `tx` as `signer`.
    pub fn sign_eip1559(signer: &PrivateKeySigner, tx: TxEip1559) -> BasePooledTransaction {
        let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
        let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(signature));
        let encoded_length = envelope.encode_2718_len();
        BasePooledTransaction::new(envelope.try_into_recovered().unwrap(), encoded_length)
    }

    /// A signed EIP-1559 transaction on the test chain that pays `max_priority_fee_per_gas`.
    pub fn eip1559(
        signer: &PrivateKeySigner,
        nonce: u64,
        max_priority_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        Self::sign_eip1559(
            signer,
            TxEip1559 {
                chain_id: Self::chain_id(),
                nonce,
                gas_limit: Self::GAS_LIMIT,
                max_fee_per_gas: 1_000,
                max_priority_fee_per_gas,
                to: TxKind::Call(Address::repeat_byte(0xEE)),
                ..Default::default()
            },
        )
    }

    /// Replaces the arrival time of `transaction` with `received_at`, in milliseconds.
    pub fn received_at(
        transaction: BasePooledTransaction,
        received_at: u128,
    ) -> BasePooledTransaction {
        let encoded_length = transaction.encoded_length();
        BasePooledTransaction::new_with_received_at(
            transaction.into_consensus(),
            encoded_length,
            received_at,
        )
    }

    /// A deposit transaction carrying `input`, as the sequencer's L1 attributes transaction.
    pub fn l1_info_deposit(input: impl Into<Bytes>) -> BaseTransactionSigned {
        TxDeposit { to: TxKind::Create, input: input.into(), ..Default::default() }.into()
    }

    /// A pooled deposit transaction, which the pool never admits.
    pub fn deposit() -> BasePooledTransaction {
        let signed = Self::l1_info_deposit(Bytes::new());
        let encoded_length = signed.encode_2718_len();
        BasePooledTransaction::new(Recovered::new_unchecked(signed, Address::ZERO), encoded_length)
    }
}
