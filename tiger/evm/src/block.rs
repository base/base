//! Block assembly: transaction decoding and the header fields a block's execution determines.

use std::sync::{Arc, Mutex, OnceLock, PoisonError, mpsc};

use alloy_consensus::{
    EMPTY_OMMER_ROOT_HASH, Header, constants::EMPTY_WITHDRAWALS, proofs, transaction::Recovered,
    transaction::SignerRecoverable,
};
use alloy_eips::{
    eip2718::{Decodable2718, Encodable2718},
    eip7685::EMPTY_REQUESTS_HASH,
};
use alloy_primitives::{B64, B256, Bytes, U256, logs_bloom};
use alloy_trie::root::ordered_trie_root_with_encoder;
use base_common_consensus::{BaseReceiptEnvelope, BaseTxEnvelope as SignedTx};
use base_common_genesis::BaseFeature;
use evm2::ethereum::TxEnvelope;

use crate::{BaseTxEnvelope, BlockEnvInput, ChainSpec, ComputedHeader};

/// Most threads [`Transactions::decode_all`] recovers a block's senders on: the caller and
/// the decode threads.
const DECODE_THREADS: usize = 8;
/// Fewest transactions a [`Transactions::decode_all`] thread decodes: below that, handing them
/// to a thread costs more than it saves.
const DECODE_CHUNK: usize = 4;

/// One decoded transaction: the signed transaction and its execution form.
type Decoded = Result<(SignedTx, Recovered<BaseTxEnvelope>), String>;
/// A chunk of a block's transactions for a decode thread: its index, the transactions, and where
/// the decoded chunk goes (or the decoding's panic).
type DecodeJob = (usize, Vec<Bytes>, mpsc::Sender<(usize, std::thread::Result<Vec<Decoded>>)>);

/// The decode threads, started on first use and kept, so a block does not start and join
/// threads (which cost as much as the recovery they saved).
fn decode_threads() -> &'static mpsc::Sender<DecodeJob> {
    static THREADS: OnceLock<mpsc::Sender<DecodeJob>> = OnceLock::new();
    THREADS.get_or_init(|| {
        let (jobs, receiver) = mpsc::channel::<DecodeJob>();
        let receiver = Arc::new(Mutex::new(receiver));
        for _ in 1..DECODE_THREADS {
            let receiver = Arc::clone(&receiver);
            let decode = move || {
                loop {
                    let job = receiver.lock().unwrap_or_else(PoisonError::into_inner).recv();
                    let Ok((index, txs, done)) = job else { return };
                    let decoded = std::panic::catch_unwind(|| txs.iter().map(Transactions::decode).collect());
                    let _ = done.send((index, decoded));
                }
            };
            std::thread::Builder::new().name("tiger-decode".into()).spawn(decode).expect("decode thread");
        }
        jobs
    })
}

/// Decodes block-body transactions and recovers their senders.
#[derive(Clone, Copy, Debug, Default)]
pub struct Transactions;

impl Transactions {
    /// Decodes one EIP-2718 transaction (canonical encoding, the whole input) and recovers its
    /// signer, returning the signed transaction and its execution form.
    pub fn decode(raw: &Bytes) -> Result<(SignedTx, Recovered<BaseTxEnvelope>), String> {
        let mut buf = raw.as_ref();
        let signed = SignedTx::decode_2718(&mut buf).map_err(|error| format!("decoding: {error}"))?;
        if !buf.is_empty() {
            return Err("decoding: trailing bytes".into());
        }
        if signed.encoded_2718().as_slice() != raw.as_ref() {
            return Err("decoding: non-canonical encoding".into());
        }
        let signer = signed.recover_signer().map_err(|error| format!("recovering signer: {error}"))?;
        let tx = Self::execution_form(&signed, raw.clone());
        Ok((signed, Recovered::new_unchecked(tx, signer)))
    }

    /// Decodes a block's transactions as [`Self::decode`] does, in order, recovering senders on
    /// up to [`DECODE_THREADS`] threads: signature recovery is most of a block's decoding.
    pub fn decode_all(raw: &[Bytes]) -> Vec<Decoded> {
        let chunk = raw.len().div_ceil(DECODE_THREADS).max(DECODE_CHUNK);
        if raw.len() <= chunk {
            return raw.iter().map(Self::decode).collect();
        }
        let (done, finished) = mpsc::channel();
        let chunks = raw[chunk..].chunks(chunk).len();
        for (index, txs) in raw[chunk..].chunks(chunk).enumerate() {
            decode_threads().send((index, txs.to_vec(), done.clone())).expect("decode threads run");
        }
        let mut decoded: Vec<_> = raw[..chunk].iter().map(Self::decode).collect();
        let mut rest: Vec<Option<Vec<Decoded>>> = vec![None; chunks];
        for (index, chunk) in finished.iter().take(chunks) {
            rest[index] = Some(chunk.unwrap_or_else(|panic| std::panic::resume_unwind(panic)));
        }
        decoded.extend(rest.into_iter().flat_map(|chunk| chunk.expect("every chunk decoded")));
        decoded
    }

    /// The evm2 form of a signed transaction, carrying its encoding for the L1 fee.
    pub fn execution_form(signed: &SignedTx, raw: Bytes) -> BaseTxEnvelope {
        match signed {
            SignedTx::Legacy(tx) => {
                BaseTxEnvelope::standard(TxEnvelope::Legacy(tx.tx().clone()), raw)
            }
            SignedTx::Eip2930(tx) => {
                BaseTxEnvelope::standard(TxEnvelope::Eip2930(tx.tx().clone()), raw)
            }
            SignedTx::Eip1559(tx) => {
                BaseTxEnvelope::standard(TxEnvelope::Eip1559(tx.tx().clone()), raw)
            }
            SignedTx::Eip7702(tx) => BaseTxEnvelope::standard(
                TxEnvelope::Eip7702(evm2::ethereum::LazyTxEip7702::from_signed_authorizations(
                    tx.tx().clone(),
                )),
                raw,
            ),
            SignedTx::Deposit(tx) => BaseTxEnvelope::Deposit(tx.inner().clone()),
            SignedTx::Base(tx) => BaseTxEnvelope::base(tx.clone(), raw),
        }
    }
}

/// The receipts root, with the Regolith quirk: between Regolith and Canyon, deposit receipts are
/// hashed without their deposit nonce, as op-geth did.
pub fn receipts_root(chain: &ChainSpec, timestamp: u64, receipts: &[BaseReceiptEnvelope]) -> B256 {
    let strip_nonce = chain.is_active(BaseFeature::DepositNonce, timestamp)
        && !chain.is_active(BaseFeature::DepositReceiptVersion, timestamp);
    if !strip_nonce {
        return ordered_trie_root_with_encoder(receipts, |receipt, buf| receipt.encode_2718(buf));
    }
    let stripped: Vec<BaseReceiptEnvelope> = receipts
        .iter()
        .map(|receipt| {
            let mut receipt = receipt.clone();
            if let BaseReceiptEnvelope::Deposit(deposit) = &mut receipt {
                deposit.receipt.deposit_nonce = None;
            }
            receipt
        })
        .collect();
    ordered_trie_root_with_encoder(&stripped, |receipt, buf| receipt.encode_2718(buf))
}

/// Computes the header fields of a block from its execution results. `withdrawals_root` is the
/// `L2ToL1MessagePasser` storage root after the block, used from Isthmus.
pub fn computed_header(
    chain: &ChainSpec,
    timestamp: u64,
    transactions: &[SignedTx],
    result: &crate::BlockExecutionResult,
    state_root: B256,
    message_passer_root: impl FnOnce() -> Result<B256, String>,
) -> Result<ComputedHeader, String> {
    let withdrawals_root = if chain.is_active(BaseFeature::L2ToL1MessageRoot, timestamp) {
        Some(message_passer_root()?)
    } else if chain.is_active(BaseFeature::Withdrawals, timestamp) {
        Some(EMPTY_WITHDRAWALS)
    } else {
        None
    };
    let blob_gas_used = if chain.is_active(BaseFeature::DaFootprint, timestamp) {
        Some(result.blob_gas_used)
    } else if chain.is_active(BaseFeature::BlobHeader, timestamp) {
        Some(0)
    } else {
        None
    };
    Ok(ComputedHeader {
        state_root,
        receipts_root: receipts_root(chain, timestamp, &result.receipts),
        transactions_root: proofs::calculate_transaction_root(transactions),
        logs_bloom: logs_bloom(result.receipts.iter().flat_map(|receipt| receipt.logs())),
        gas_used: result.gas_used,
        blob_gas_used,
        withdrawals_root,
        requests_hash: chain
            .is_active(BaseFeature::EmptyRequestsHash, timestamp)
            .then_some(EMPTY_REQUESTS_HASH),
    })
}

/// Assembles the full header of a built block. `excess_blob_gas` is zero from Ecotone unless the
/// caller supplies another value.
pub fn assemble_header(
    chain: &ChainSpec,
    env: &BlockEnvInput,
    extra_data: Bytes,
    excess_blob_gas: Option<u64>,
    computed: &ComputedHeader,
) -> Header {
    let ecotone = chain.is_active(BaseFeature::BlobHeader, env.timestamp);
    Header {
        parent_hash: env.parent_hash,
        ommers_hash: EMPTY_OMMER_ROOT_HASH,
        beneficiary: env.coinbase,
        state_root: computed.state_root,
        transactions_root: computed.transactions_root,
        receipts_root: computed.receipts_root,
        logs_bloom: computed.logs_bloom,
        difficulty: U256::ZERO,
        number: env.number,
        gas_limit: env.gas_limit,
        gas_used: computed.gas_used,
        timestamp: env.timestamp,
        extra_data,
        mix_hash: env.prev_randao,
        nonce: B64::ZERO,
        base_fee_per_gas: Some(env.base_fee),
        withdrawals_root: computed.withdrawals_root,
        blob_gas_used: computed.blob_gas_used,
        excess_blob_gas: ecotone.then(|| excess_blob_gas.unwrap_or(0)),
        parent_beacon_block_root: env.parent_beacon_block_root,
        requests_hash: computed.requests_hash,
        block_access_list_hash: None,
        slot_number: None,
    }
}
