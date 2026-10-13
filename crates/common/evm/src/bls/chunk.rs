//! Authentication of the native BLS transfers in an ordered chunk of derived blocks.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    vec::Vec,
};

use alloy_consensus::transaction::SignerRecoverable;
use alloy_eips::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, Bytes, FixedBytes};
use ark_bls12_381::G2Affine;
use base_common_consensus::{
    BLS_TRANSFER_TX_TYPE_ID, BLS_TX_TYPE_ID, BaseTxEnvelope, BlsConstants, BlsPayload,
    TxBlsTransfer,
};

use crate::{BlsCrypto, BlsRegistry, BlsState};

/// Reasons [`BlsChunkVerifier::verify`] rejects a chunk. `block` and `index` locate the offending
/// transaction within the chunk.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlsChunkError<E> {
    /// Native BLS transactions only execute on [`BlsConstants::CHAIN_ID`].
    #[error("native BLS transactions are disabled on chain {chain_id}")]
    UnsupportedChain {
        /// The verifying chain.
        chain_id: u64,
    },
    /// The chunk has more than [`BlsChunkVerifier::MAX_BLOCKS`] blocks.
    #[error("BLS chunk has {blocks} blocks")]
    TooManyBlocks {
        /// Number of blocks in the chunk.
        blocks: usize,
    },
    /// The chunk has more than [`BlsChunkVerifier::MAX_TRANSFERS`] transfers.
    #[error("BLS chunk has more than {max} transfers", max = BlsChunkVerifier::MAX_TRANSFERS)]
    TooManyTransfers {
        /// Number of transfers counted when the bound was exceeded.
        transfers: usize,
    },
    /// A native BLS transaction does not decode, or its payload is malformed.
    #[error("malformed native BLS transaction at block {block} index {index}")]
    Malformed {
        /// Block position in the chunk.
        block: usize,
        /// Transaction position in the block.
        index: usize,
    },
    /// A native BLS transaction does not re-encode to its exact bytes.
    #[error("non-canonical native BLS transaction at block {block} index {index}")]
    NonCanonical {
        /// Block position in the chunk.
        block: usize,
        /// Transaction position in the block.
        index: usize,
    },
    /// A native BLS transaction is for another chain.
    #[error("native BLS transaction at block {block} index {index} has chain id {tx_chain_id}")]
    ChainIdMismatch {
        /// Block position in the chunk.
        block: usize,
        /// Transaction position in the block.
        index: usize,
        /// The transaction's chain id.
        tx_chain_id: u64,
    },
    /// The outer ECDSA signature of a registration does not recover an owner.
    #[error("BLS registration at block {block} index {index} has an invalid signer")]
    InvalidRegistrationSigner {
        /// Block position in the chunk.
        block: usize,
        /// Transaction position in the block.
        index: usize,
    },
    /// A registered public key is not a valid non-identity subgroup point.
    #[error("invalid BLS public key at block {block} index {index}")]
    InvalidPublicKey {
        /// Block position in the chunk.
        block: usize,
        /// Transaction position in the block.
        index: usize,
    },
    /// A registration's proof of possession is not a valid point or does not verify.
    #[error("invalid BLS proof of possession at block {block} index {index}")]
    InvalidProofOfPossession {
        /// Block position in the chunk.
        block: usize,
        /// Transaction position in the block.
        index: usize,
    },
    /// A registration for an owner that is already registered, in the parent state or earlier in
    /// the chunk. Keys are immutable.
    #[error("duplicate BLS registration for {owner} at block {block} index {index}")]
    DuplicateRegistration {
        /// Block position in the chunk.
        block: usize,
        /// Transaction position in the block.
        index: usize,
        /// The registration owner.
        owner: Address,
    },
    /// A transfer sender has no key registered in the parent state or earlier in the chunk.
    #[error("BLS transfer sender {sender} at block {block} index {index} has no registered key")]
    UnregisteredSender {
        /// Block position in the chunk.
        block: usize,
        /// Transaction position in the block.
        index: usize,
        /// The transfer sender.
        sender: Address,
    },
    /// The same transfer appears more than once in the chunk.
    #[error("duplicate BLS transfer at block {block} index {index}")]
    DuplicateTransfer {
        /// Block position in the chunk.
        block: usize,
        /// Transaction position in the block.
        index: usize,
    },
    /// The chunk has transfers but no aggregate signature.
    #[error("BLS chunk has transfers but no aggregate signature")]
    MissingAggregate,
    /// The chunk has an aggregate signature but no transfers.
    #[error("BLS chunk has an aggregate signature but no transfers")]
    UnexpectedAggregate,
    /// The aggregate signature is not a canonical non-identity subgroup point.
    #[error("invalid BLS chunk aggregate signature point")]
    InvalidAggregatePoint,
    /// The aggregate signature does not verify over the chunk's transfers.
    #[error("invalid BLS chunk aggregate signature")]
    InvalidAggregate,
    /// State access failed.
    #[error("BLS chunk state access failed: {0}")]
    Database(E),
}

/// Authenticates every native BLS transfer in an ordered chunk of blocks against one aggregate
/// signature, before the blocks are released for execution with
/// [`crate::BlsBlockAuthorization::Derived`].
///
/// This is pure authentication: it decides which key authorizes each transfer and that the
/// aggregate covers exactly the chunk's transfers. Nonces, balances, fees and code checks are
/// left to the ordinary ordered execution of the blocks.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlsChunkVerifier;

impl BlsChunkVerifier {
    /// Maximum number of blocks in one chunk.
    pub const MAX_BLOCKS: usize = 128;

    /// Maximum number of transfers in one chunk aggregate.
    pub const MAX_TRANSFERS: usize = 16_384;

    /// Verifies `aggregate` over the transfers of `blocks`, the raw EIP-2718 transactions of
    /// consecutive blocks in execution order, on `chain_id`, with `state` the parent state of
    /// the first block.
    ///
    /// Each transfer (type `0x7b`) is authorized by its sender's key from the parent registry or
    /// from a valid registration (type `0x7a`, verified owner recovery and proof of possession)
    /// earlier in the chunk; registrations later in the chunk do not count. Senders may repeat.
    /// Every native BLS transaction must decode canonically and be for `chain_id`; other
    /// transaction types are not inspected. A chunk with transfers requires an aggregate and a
    /// chunk without transfers rejects one.
    ///
    /// The block and transfer bounds are enforced before any decoding or cryptography.
    pub fn verify<S: BlsState>(
        state: &mut S,
        chain_id: u64,
        blocks: &[Vec<Bytes>],
        aggregate: Option<FixedBytes<48>>,
    ) -> Result<(), BlsChunkError<S::Error>> {
        if chain_id != BlsConstants::CHAIN_ID {
            return Err(BlsChunkError::UnsupportedChain { chain_id });
        }
        if blocks.len() > Self::MAX_BLOCKS {
            return Err(BlsChunkError::TooManyBlocks { blocks: blocks.len() });
        }
        let mut transfers = 0usize;
        for tx in blocks.iter().flatten() {
            if tx.first() == Some(&BLS_TRANSFER_TX_TYPE_ID) {
                transfers += 1;
                if transfers > Self::MAX_TRANSFERS {
                    return Err(BlsChunkError::TooManyTransfers { transfers });
                }
            }
        }
        match (aggregate.is_some(), transfers) {
            (false, 0) | (true, 1..) => {}
            (true, 0) => return Err(BlsChunkError::UnexpectedAggregate),
            (false, 1..) => return Err(BlsChunkError::MissingAggregate),
        }

        // Keys usable so far: registrations earlier in the chunk, and parent-registry keys
        // decoded on first use.
        let mut keys = BTreeMap::<Address, G2Affine>::new();
        let mut digests = BTreeSet::new();
        let mut signers = Vec::with_capacity(transfers);
        for (block, txs) in blocks.iter().enumerate() {
            for (index, raw) in txs.iter().enumerate() {
                let ty = raw.first().copied();
                if ty != Some(BLS_TX_TYPE_ID) && ty != Some(BLS_TRANSFER_TX_TYPE_ID) {
                    continue;
                }
                let envelope = BaseTxEnvelope::decode_2718_exact(raw)
                    .map_err(|_| BlsChunkError::Malformed { block, index })?;
                if envelope.encoded_2718() != raw.as_ref() {
                    return Err(BlsChunkError::NonCanonical { block, index });
                }
                match envelope {
                    BaseTxEnvelope::BlsTransfer(tx) => {
                        if tx.chain_id != chain_id {
                            return Err(BlsChunkError::ChainIdMismatch {
                                block,
                                index,
                                tx_chain_id: tx.chain_id,
                            });
                        }
                        let sender = tx.sender;
                        let key = match keys.get(&sender) {
                            Some(key) => *key,
                            None => {
                                let bytes = BlsRegistry::read_key(state, sender)
                                    .map_err(BlsChunkError::Database)?
                                    .ok_or(BlsChunkError::UnregisteredSender {
                                        block,
                                        index,
                                        sender,
                                    })?;
                                let key = BlsCrypto::public_key(&bytes)
                                    .ok_or(BlsChunkError::InvalidPublicKey { block, index })?;
                                keys.insert(sender, key);
                                key
                            }
                        };
                        let digest = TxBlsTransfer::signing_digest_for_hash(tx.hash());
                        if !digests.insert(digest) {
                            return Err(BlsChunkError::DuplicateTransfer { block, index });
                        }
                        signers.push((key, digest));
                    }
                    BaseTxEnvelope::Bls(tx) => {
                        if tx.tx().chain_id != chain_id {
                            return Err(BlsChunkError::ChainIdMismatch {
                                block,
                                index,
                                tx_chain_id: tx.tx().chain_id,
                            });
                        }
                        let payload = tx
                            .tx()
                            .payload()
                            .map_err(|_| BlsChunkError::Malformed { block, index })?;
                        // Batches carry their own aggregate, verified by execution.
                        let BlsPayload::Registration { pubkey, pop } = payload else {
                            continue;
                        };
                        // EIP-2 (low-s) recovery, as for every other signed transaction.
                        let owner = SignerRecoverable::recover_signer(&tx).map_err(|_| {
                            BlsChunkError::InvalidRegistrationSigner { block, index }
                        })?;
                        if keys.contains_key(&owner)
                            || BlsRegistry::read_key(state, owner)
                                .map_err(BlsChunkError::Database)?
                                .is_some()
                        {
                            return Err(BlsChunkError::DuplicateRegistration {
                                block,
                                index,
                                owner,
                            });
                        }
                        let key = BlsCrypto::public_key(&pubkey.0)
                            .ok_or(BlsChunkError::InvalidPublicKey { block, index })?;
                        let pop = BlsCrypto::signature(&pop.0)
                            .ok_or(BlsChunkError::InvalidProofOfPossession { block, index })?;
                        if !BlsCrypto::verify_proof_of_possession(&pubkey.0, key, pop) {
                            return Err(BlsChunkError::InvalidProofOfPossession { block, index });
                        }
                        keys.insert(owner, key);
                    }
                    // The type byte selected only native BLS transactions.
                    _ => return Err(BlsChunkError::Malformed { block, index }),
                }
            }
        }

        let Some(aggregate) = aggregate else {
            return Ok(());
        };
        let aggregate =
            BlsCrypto::signature(&aggregate.0).ok_or(BlsChunkError::InvalidAggregatePoint)?;
        if !BlsCrypto::aggregate_verify(&signers, aggregate) {
            return Err(BlsChunkError::InvalidAggregate);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use alloy_consensus::{Sealable, SignableTransaction, crypto::secp256k1::sign_message};
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Address, B256, Bytes, FixedBytes, Signature, U256, address, b256};
    use base_common_consensus::{
        BaseTxEnvelope, BlsConstants, BlsPayload, BlsTransferWithSignature, TxBls, TxBlsTransfer,
    };
    use revm::{Database, database::InMemoryDB, state::AccountInfo};

    use crate::{
        BlsChunkError, BlsChunkVerifier, BlsCrypto, BlsDatabaseState, BlsRegistry, BlsState,
        test_utils::BlsTestKey,
    };

    const ALICE: Address = address!("0x00000000000000000000000000000000000a11ce");
    const CAROL: Address = address!("0x0000000000000000000000000000000000ca401e");
    const ALICE_SEED: u64 = 1;
    const DAVE_SEED: u64 = 4;
    const DAVE_SECRET: B256 =
        b256!("0x00000000000000000000000000000000000000000000000000000000000d0d0e");
    const CHAIN: u64 = BlsConstants::CHAIN_ID;

    /// Parent state in which only Alice is registered.
    fn parent() -> InMemoryDB {
        let mut db = InMemoryDB::default();
        db.insert_account_info(BlsConstants::REGISTRY_ADDRESS, AccountInfo::default());
        let key = BlsTestKey::from_seed(ALICE_SEED).public_key();
        for (slot, word) in
            BlsRegistry::key_slots(ALICE).into_iter().zip(BlsRegistry::key_words(&key.0))
        {
            db.insert_account_storage(
                BlsConstants::REGISTRY_ADDRESS,
                U256::from_be_bytes(slot.0),
                word,
            )
            .unwrap();
        }
        db
    }

    /// An ECDSA-signed registration of `seed`'s key, its raw bytes and its recovered owner.
    fn registration(
        secret: B256,
        seed: u64,
        chain_id: u64,
        pop: Option<FixedBytes<48>>,
    ) -> (Bytes, Address) {
        let key = BlsTestKey::from_seed(seed);
        let payload = BlsPayload::Registration {
            pubkey: key.public_key(),
            pop: pop.unwrap_or_else(|| key.proof_of_possession()),
        };
        let tx = TxBls {
            chain_id,
            nonce: 0,
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: 2,
            gas_limit: 1_000_000,
            input: payload.encode(),
        };
        let signature = sign_message(secret, tx.signature_hash()).unwrap();
        let signed = tx.into_signed(signature);
        let owner =
            alloy_consensus::transaction::SignerRecoverable::recover_signer(&signed).unwrap();
        (BaseTxEnvelope::Bls(signed).encoded_2718().into(), owner)
    }

    fn dave() -> Address {
        registration(DAVE_SECRET, DAVE_SEED, CHAIN, None).1
    }

    /// A canonical transfer, its raw bytes and the sender's individual signature.
    fn transfer(sender: Address, nonce: u64, seed: u64) -> (TxBlsTransfer, Bytes, FixedBytes<48>) {
        let tx = TxBlsTransfer {
            chain_id: CHAIN,
            sender,
            nonce,
            recipient: CAROL,
            value: U256::from(1),
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: 2,
        };
        let raw = BaseTxEnvelope::BlsTransfer(tx.seal_slow()).encoded_2718().into();
        (tx, raw, BlsTestKey::from_seed(seed).sign(tx.signing_digest()))
    }

    fn aggregate(signatures: &[FixedBytes<48>]) -> FixedBytes<48> {
        BlsCrypto::aggregate_signatures(signatures).unwrap()
    }

    fn verify(
        blocks: &[Vec<Bytes>],
        aggregate: Option<FixedBytes<48>>,
    ) -> Result<(), BlsChunkError<core::convert::Infallible>> {
        BlsChunkVerifier::verify(&mut BlsDatabaseState(&mut parent()), CHAIN, blocks, aggregate)
    }

    /// Three blocks: a parent-key transfer and Dave's registration, then repeat transfers by
    /// Dave (key from earlier in the chunk) and Alice, then another from Dave. Non-BLS
    /// transactions are not inspected.
    fn chunk() -> (Vec<Vec<Bytes>>, Vec<FixedBytes<48>>) {
        let (reg, dave) = registration(DAVE_SECRET, DAVE_SEED, CHAIN, None);
        let (_, a0, s_a0) = transfer(ALICE, 0, ALICE_SEED);
        let (_, d0, s_d0) = transfer(dave, 1, DAVE_SEED);
        let (_, a1, s_a1) = transfer(ALICE, 1, ALICE_SEED);
        let (_, d1, s_d1) = transfer(dave, 2, DAVE_SEED);
        let other = Bytes::from_static(&[0x02, 0xff]);
        (vec![vec![other, a0, reg], vec![d0, a1], vec![d1]], vec![s_a0, s_d0, s_a1, s_d1])
    }

    #[test]
    fn authenticates_ordered_chunk_with_reaggregated_witness() {
        let (blocks, signatures) = chunk();
        assert_eq!(verify(&blocks, Some(aggregate(&signatures))), Ok(()));
        // Producers may aggregate per block and re-aggregate per chunk.
        let per_block = [
            aggregate(&signatures[..1]),
            aggregate(&signatures[1..3]),
            aggregate(&signatures[3..]),
        ];
        assert_eq!(verify(&blocks, Some(aggregate(&per_block))), Ok(()));
        // A chunk without transfers needs no witness.
        assert_eq!(
            verify(&[vec![registration(DAVE_SECRET, DAVE_SEED, CHAIN, None).0], vec![]], None),
            Ok(())
        );
    }

    #[test]
    fn rejects_missing_extraneous_and_mismatched_witnesses() {
        let (mut blocks, signatures) = chunk();
        let full = aggregate(&signatures);
        assert_eq!(verify(&blocks, None), Err(BlsChunkError::MissingAggregate));
        assert_eq!(verify(&[vec![], vec![]], Some(full)), Err(BlsChunkError::UnexpectedAggregate));
        // Witness omits a record's signature.
        assert_eq!(
            verify(&blocks, Some(aggregate(&signatures[1..]))),
            Err(BlsChunkError::InvalidAggregate)
        );
        let mut infinity = FixedBytes::<48>::ZERO;
        infinity[0] = 0xc0;
        assert_eq!(verify(&blocks, Some(infinity)), Err(BlsChunkError::InvalidAggregatePoint));

        // A record omitted from the chunk while the witness still covers it.
        let omitted = blocks[2].pop().unwrap();
        assert_eq!(verify(&blocks, Some(full)), Err(BlsChunkError::InvalidAggregate));
        // A duplicated record is rejected before any pairing, even with a matching witness.
        blocks[2].push(omitted.clone());
        blocks[2].push(omitted);
        let doubled = aggregate(&[signatures.as_slice(), &signatures[3..]].concat());
        assert_eq!(
            verify(&blocks, Some(doubled)),
            Err(BlsChunkError::DuplicateTransfer { block: 2, index: 1 })
        );
    }

    #[test]
    fn rejects_keys_used_before_publication_and_invalid_registrations() {
        let (blocks, signatures) = chunk();
        let full = Some(aggregate(&signatures));
        let dave = dave();

        // Reordered: Dave's first transfer precedes his registration.
        let mut reordered = blocks.clone();
        reordered[0].swap(1, 2);
        assert_eq!(verify(&reordered, full), Ok(()), "registration still precedes Dave's use");
        let mut early = blocks.clone();
        let reg = early[0].remove(2);
        early[1].push(reg);
        assert_eq!(
            verify(&early, full),
            Err(BlsChunkError::UnregisteredSender { block: 1, index: 0, sender: dave })
        );

        // Keys are immutable: Dave cannot register a second key later in the chunk, nor
        // register at all if the parent registry already holds a key for him.
        let (second_key, owner) = registration(DAVE_SECRET, ALICE_SEED, CHAIN, None);
        assert_eq!(owner, dave, "the owner is the recovered ECDSA signer, not the BLS key");
        let mut twice = blocks.clone();
        twice[1].insert(0, second_key);
        assert_eq!(
            verify(&twice, full),
            Err(BlsChunkError::DuplicateRegistration { block: 1, index: 0, owner: dave })
        );
        let mut registered = parent();
        let key = BlsTestKey::from_seed(DAVE_SEED).public_key();
        for (slot, word) in
            BlsRegistry::key_slots(dave).into_iter().zip(BlsRegistry::key_words(&key.0))
        {
            registered
                .insert_account_storage(
                    BlsConstants::REGISTRY_ADDRESS,
                    U256::from_be_bytes(slot.0),
                    word,
                )
                .unwrap();
        }
        assert_eq!(
            BlsChunkVerifier::verify(&mut BlsDatabaseState(&mut registered), CHAIN, &blocks, full),
            Err(BlsChunkError::DuplicateRegistration { block: 0, index: 2, owner: dave })
        );

        // A proof of possession by another key, a wrong-chain registration and an unrecoverable
        // owner signature.
        let foreign_pop = BlsTestKey::from_seed(ALICE_SEED).proof_of_possession();
        let mut bad = blocks;
        bad[0][2] = registration(DAVE_SECRET, DAVE_SEED, CHAIN, Some(foreign_pop)).0;
        assert_eq!(
            verify(&bad, full),
            Err(BlsChunkError::InvalidProofOfPossession { block: 0, index: 2 })
        );
        bad[0][2] = registration(DAVE_SECRET, DAVE_SEED, 8453, None).0;
        assert_eq!(
            verify(&bad, full),
            Err(BlsChunkError::ChainIdMismatch { block: 0, index: 2, tx_chain_id: 8453 })
        );
        let key = BlsTestKey::from_seed(DAVE_SEED);
        let unsigned = TxBls {
            chain_id: CHAIN,
            input: BlsPayload::Registration {
                pubkey: key.public_key(),
                pop: key.proof_of_possession(),
            }
            .encode(),
            ..Default::default()
        };
        let forged = unsigned.into_signed(Signature::new(U256::from(1), U256::MAX >> 1, false));
        bad[0][2] = BaseTxEnvelope::Bls(forged).encoded_2718().into();
        assert_eq!(
            verify(&bad, full),
            Err(BlsChunkError::InvalidRegistrationSigner { block: 0, index: 2 })
        );
    }

    #[test]
    fn rejects_offchain_malformed_and_unbounded_chunks() {
        let (tx, raw, signature) = transfer(ALICE, 0, ALICE_SEED);
        let witness = Some(signature);
        assert_eq!(verify(&[vec![raw.clone()]], witness), Ok(()));
        assert_eq!(
            BlsChunkVerifier::verify(
                &mut BlsDatabaseState(&mut parent()),
                8453,
                &[vec![raw.clone()]],
                witness
            ),
            Err(BlsChunkError::UnsupportedChain { chain_id: 8453 })
        );
        let offchain = TxBlsTransfer { chain_id: 8453, ..tx };
        let offchain_raw = BaseTxEnvelope::BlsTransfer(offchain.seal_slow()).encoded_2718().into();
        assert_eq!(
            verify(&[vec![offchain_raw]], witness),
            Err(BlsChunkError::ChainIdMismatch { block: 0, index: 0, tx_chain_id: 8453 })
        );
        // Truncated, trailing bytes and the pooled witness form are not canonical transactions.
        let pooled: Bytes =
            BlsTransferWithSignature::new(tx.seal_slow(), signature).unwrap().encoded_2718().into();
        let trailing: Bytes = [raw.as_ref(), &[0]].concat().into();
        for bytes in [raw.slice(..raw.len() - 1), trailing, pooled] {
            assert_eq!(
                verify(&[vec![bytes]], witness),
                Err(BlsChunkError::Malformed { block: 0, index: 0 })
            );
        }

        // Bounds are enforced before decoding: bogus transfers report the bound, not decoding.
        assert_eq!(verify(&vec![vec![]; BlsChunkVerifier::MAX_BLOCKS], None), Ok(()));
        assert_eq!(
            verify(&vec![vec![]; BlsChunkVerifier::MAX_BLOCKS + 1], None),
            Err(BlsChunkError::TooManyBlocks { blocks: BlsChunkVerifier::MAX_BLOCKS + 1 })
        );
        let bogus = vec![
            Bytes::from_static(&[BlsConstants::TRANSFER_TX_TYPE]);
            BlsChunkVerifier::MAX_TRANSFERS / 2 + 1
        ];
        assert_eq!(
            verify(&[bogus.clone(), bogus], witness),
            Err(BlsChunkError::TooManyTransfers { transfers: BlsChunkVerifier::MAX_TRANSFERS + 1 })
        );
    }

    /// Like the proof trie database, serves an account's storage only after the account was
    /// loaded with `basic`; earlier storage reads return zero. Hand-rolled because the
    /// behavior depends on the order of calls across both trait methods.
    struct LazyStorageState {
        db: InMemoryDB,
        loaded: Vec<Address>,
    }

    impl BlsState for LazyStorageState {
        type Error = core::convert::Infallible;

        fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            self.loaded.push(address);
            self.db.basic(address)
        }

        fn storage(&mut self, address: Address, slot: B256) -> Result<U256, Self::Error> {
            if !self.loaded.contains(&address) {
                return Ok(U256::ZERO);
            }
            self.db.storage(address, slot.into())
        }
    }

    #[test]
    fn reads_registry_from_states_that_load_storage_lazily() {
        let (blocks, signatures) = chunk();
        let mut state = LazyStorageState { db: parent(), loaded: Vec::new() };
        assert_eq!(
            BlsChunkVerifier::verify(&mut state, CHAIN, &blocks, Some(aggregate(&signatures))),
            Ok(())
        );
    }
}
