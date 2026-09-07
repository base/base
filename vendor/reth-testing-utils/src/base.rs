//! Base block and transaction fixtures for execution and storage tests.

use std::ops::RangeInclusive;

use alloy_consensus::{SignableTransaction, Transaction as _};
use alloy_primitives::{B256, BlockNumber};
use base_common_consensus::{
    BaseBlock, BaseReceipt, BaseTxEnvelope, BaseTypedTransaction, OpTxType,
};
use rand::Rng;
use reth_primitives_traits::{SealedBlock, crypto::secp256k1::sign_message};
use secp256k1::Keypair;

use crate::generators::{self, BlockParams, BlockRangeParams};

/// Generates signed Base transactions and blocks for tests.
#[derive(Debug)]
pub struct BaseTestData;

impl BaseTestData {
    /// Generates a legacy transaction supported by Base.
    pub fn random_tx<R: Rng>(rng: &mut R) -> BaseTypedTransaction {
        let tx = generators::random_tx(rng);
        BaseTypedTransaction::Legacy(tx.legacy().unwrap().clone())
    }

    /// Generates a signed legacy transaction supported by Base.
    pub fn random_signed_tx<R: Rng>(rng: &mut R) -> BaseTxEnvelope {
        BaseTxEnvelope::try_from(alloy_consensus::TxEnvelope::from(generators::random_signed_tx(
            rng,
        )))
        .unwrap()
    }

    /// Signs a transaction with the supplied key.
    pub fn sign_tx_with_key_pair(key_pair: Keypair, tx: BaseTypedTransaction) -> BaseTxEnvelope {
        let signature = sign_message(
            B256::from(key_pair.secret_bytes()),
            tx.checked_signature_hash().expect("fixture must support ECDSA signatures"),
        )
        .unwrap();
        tx.into_signed(signature).into()
    }

    /// Signs a transaction with a generated key.
    pub fn sign_tx_with_random_key_pair<R: Rng>(
        rng: &mut R,
        tx: BaseTypedTransaction,
    ) -> BaseTxEnvelope {
        Self::sign_tx_with_key_pair(generators::generate_key(rng), tx)
    }

    /// Generates a sealed block with signed legacy transactions.
    pub fn random_block<R: Rng>(
        rng: &mut R,
        number: u64,
        params: BlockParams,
    ) -> SealedBlock<BaseBlock> {
        let (block, hash) = generators::random_block(rng, number, params).split();
        SealedBlock::new_unchecked(
            block.map_transactions(|tx| {
                BaseTxEnvelope::try_from(alloy_consensus::TxEnvelope::from(tx)).unwrap()
            }),
            hash,
        )
    }

    /// Generates parent-linked blocks with signed legacy transactions.
    pub fn random_block_range<R: Rng>(
        rng: &mut R,
        numbers: RangeInclusive<BlockNumber>,
        params: BlockRangeParams,
    ) -> Vec<SealedBlock<BaseBlock>> {
        generators::random_block_range(rng, numbers, params)
            .into_iter()
            .map(|block| {
                let (block, hash) = block.split();
                SealedBlock::new_unchecked(
                    block.map_transactions(|tx| {
                        BaseTxEnvelope::try_from(alloy_consensus::TxEnvelope::from(tx)).unwrap()
                    }),
                    hash,
                )
            })
            .collect()
    }

    /// Generates a receipt matching a signed transaction's type.
    pub fn random_receipt<R: Rng>(
        rng: &mut R,
        transaction: &BaseTxEnvelope,
        logs_count: Option<u8>,
        topics_count: Option<u8>,
    ) -> BaseReceipt {
        let success = rng.random::<bool>();
        let logs_count = logs_count.unwrap_or_else(|| rng.random());
        let receipt = alloy_consensus::Receipt {
            status: success.into(),
            cumulative_gas_used: rng.random_range(0..=transaction.gas_limit()),
            logs: if success {
                (0..logs_count).map(|_| generators::random_log(rng, None, topics_count)).collect()
            } else {
                vec![]
            },
        };
        match transaction.tx_type() {
            OpTxType::Legacy => BaseReceipt::Legacy(receipt),
            OpTxType::Eip2930 => BaseReceipt::Eip2930(receipt),
            OpTxType::Eip1559 => BaseReceipt::Eip1559(receipt),
            OpTxType::Eip7702 => BaseReceipt::Eip7702(receipt),
            OpTxType::Deposit => BaseReceipt::Deposit(base_common_consensus::DepositReceipt {
                inner: receipt,
                deposit_nonce: None,
                deposit_receipt_version: None,
            }),
            OpTxType::Eip8130 => BaseReceipt::Eip8130(base_common_consensus::Eip8130Receipt {
                inner: receipt,
                ..Default::default()
            }),
        }
    }
}
