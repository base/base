//! State transition of a validated native BLS transaction.

use alloy_primitives::{Address, U256};
use base_common_consensus::{BlsConstants, BlsPayload};
use revm::{
    Database,
    context_interface::{JournalTr, journaled_state::account::JournaledAccountTr},
};

use crate::{BlsRegistry, BlsValidationError};

/// Applies the state transition of a native BLS transaction that already passed
/// [`crate::BlsValidator`] against the same journal.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlsExecutor;

impl BlsExecutor {
    /// Applies `payload`, signed by the outer `signer`, to `journal`.
    ///
    /// A registration stores the key under `signer` and gives the registry account its
    /// EIP-161-proof nonce. A batch debits every sender and increments its nonce, then credits
    /// every recipient. The caller must discard the journal on error; validation guarantees the
    /// balance and nonce errors cannot occur.
    pub fn apply<J: JournalTr>(
        journal: &mut J,
        signer: Address,
        payload: &BlsPayload,
    ) -> Result<(), BlsValidationError<<J::Database as Database>::Error>> {
        match payload {
            BlsPayload::Registration { pubkey, .. } => {
                let mut registry = journal
                    .load_account_mut(BlsConstants::REGISTRY_ADDRESS)
                    .map_err(BlsValidationError::Database)?
                    .data;
                if registry.nonce() == 0 {
                    registry.set_nonce(BlsRegistry::ACCOUNT_NONCE);
                }
                drop(registry);
                let slots = BlsRegistry::key_slots(signer);
                for (slot, word) in slots.into_iter().zip(BlsRegistry::key_words(pubkey)) {
                    journal
                        .sstore(BlsConstants::REGISTRY_ADDRESS, slot.into(), word)
                        .map_err(BlsValidationError::Database)?;
                }
            }
            BlsPayload::Batch { transfers, .. } => {
                for transfer in transfers {
                    let sender = transfer.sender;
                    let value = U256::from(transfer.value);
                    let mut account = journal
                        .load_account_mut(sender)
                        .map_err(BlsValidationError::Database)?
                        .data;
                    let balance = *account.balance();
                    if !account.decr_balance(value) {
                        return Err(BlsValidationError::InsufficientBalance {
                            sender,
                            balance,
                            value,
                        });
                    }
                    if !account.bump_nonce() {
                        return Err(BlsValidationError::NonceOverflow { sender });
                    }
                }
                for transfer in transfers {
                    journal
                        .balance_incr(transfer.recipient, U256::from(transfer.value))
                        .map_err(BlsValidationError::Database)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::{collections::BTreeMap, vec, vec::Vec};
    use core::convert::Infallible;

    use alloy_consensus::{SignableTransaction, TxEip1559};
    use alloy_eips::Encodable2718;
    use alloy_evm::FromTxWithEncoded;
    use alloy_primitives::{
        Address, B256, Bytes, FixedBytes, Signature, TxKind, U256, address, aliases::U96, keccak256,
    };
    use ark_bls12_381::{Fq, Fq2, G1Affine, G2Affine};
    use ark_ec::{AffineRepr, CurveGroup};
    use ark_ff::One;
    use ark_serialize::CanonicalSerialize;
    use base_common_consensus::{
        BaseTxEnvelope, BlsConstants, BlsPayload, BlsTransfer, Predeploys, TxBls,
    };
    use revm::{
        Context, ExecuteCommitEvm,
        bytecode::Bytecode,
        context::{BlockEnv, CfgEnv, TxEnv},
        context_interface::result::{EVMError, ExecutionResult, InvalidTransaction},
        database::InMemoryDB,
        state::AccountInfo,
    };

    use crate::{
        BaseHaltReason, BaseSpecId, BaseTransaction, BaseTransactionError, BaseUpgrade,
        BlsRegistry, BlsValidationError, BlsValidator, Builder, DefaultBase, L1BlockInfo,
        test_utils::BlsTestKey,
    };

    const BASE_FEE: u64 = 1_000_000;
    const PRIORITY_FEE: u128 = 7;
    const MAX_FEE: u128 = 2 * BASE_FEE as u128;
    const OPERATOR_FEE_SCALAR: u64 = 2_000_000;
    const OPERATOR_FEE_CONSTANT: u64 = 1_000;
    const COINBASE: Address = address!("0x00000000000000000000000000000000000c0b0e");
    const RELAYER: Address = address!("0x000000000000000000000000000000000000e1a1");
    const ALICE: Address = address!("0x00000000000000000000000000000000000a11ce");
    const BOB: Address = address!("0x0000000000000000000000000000000000000b0b");
    const CAROL: Address = address!("0x0000000000000000000000000000000000ca401e");
    const ALICE_SEED: u64 = 1;
    const BOB_SEED: u64 = 2;

    type TxResult =
        Result<ExecutionResult<BaseHaltReason>, EVMError<Infallible, BaseTransactionError>>;

    fn ether(amount: u64) -> U256 {
        U256::from(amount) * U256::from(10u64).pow(U256::from(18))
    }

    /// Observable state: every existing account's balance, nonce, code hash and non-zero storage.
    type Snapshot = BTreeMap<Address, (U256, u64, B256, BTreeMap<U256, U256>)>;

    struct Harness {
        db: InMemoryDB,
        chain_id: u64,
    }

    impl Harness {
        fn new() -> Self {
            let mut db = InMemoryDB::default();
            for address in [RELAYER, ALICE, BOB] {
                db.insert_account_info(
                    address,
                    AccountInfo { balance: ether(10), ..Default::default() },
                );
            }
            Self { db, chain_id: BlsConstants::CHAIN_ID }
        }

        fn l1_block_info() -> L1BlockInfo {
            L1BlockInfo {
                l2_block: Some(U256::from(1)),
                l1_base_fee: U256::from(30_000_000_000u64),
                l1_base_fee_scalar: U256::from(1_368),
                l1_blob_base_fee: Some(U256::from(5)),
                l1_blob_base_fee_scalar: Some(U256::from(810_949)),
                operator_fee_scalar: Some(U256::from(OPERATOR_FEE_SCALAR)),
                operator_fee_constant: Some(U256::from(OPERATOR_FEE_CONSTANT)),
                ..Default::default()
            }
        }

        fn transact(&mut self, tx: BaseTransaction<TxEnv>) -> TxResult {
            let mut cfg = CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Isthmus));
            cfg.chain_id = self.chain_id;
            let block = BlockEnv {
                number: U256::from(1),
                beneficiary: COINBASE,
                basefee: BASE_FEE,
                gas_limit: 30_000_000,
                ..Default::default()
            };
            let mut evm = Context::base()
                .with_db(core::mem::take(&mut self.db))
                .with_chain(Self::l1_block_info())
                .with_cfg(cfg)
                .with_block(block)
                .build_base();
            let result = evm.transact_commit(tx);
            self.db = evm.into_context().journaled_state.database;
            result
        }

        fn account(&self, address: Address) -> AccountInfo {
            self.db
                .cache
                .accounts
                .get(&address)
                .map(|account| account.info.clone())
                .unwrap_or_default()
        }

        fn balance(&self, address: Address) -> U256 {
            self.account(address).balance
        }

        fn nonce(&self, address: Address) -> u64 {
            self.account(address).nonce
        }

        fn snapshot(&self) -> Snapshot {
            self.db
                .cache
                .accounts
                .iter()
                .filter(|(_, account)| {
                    !account.info.is_empty() || account.storage.values().any(|v| !v.is_zero())
                })
                .map(|(address, account)| {
                    let storage = account
                        .storage
                        .iter()
                        .filter(|(_, value)| !value.is_zero())
                        .map(|(slot, value)| (*slot, *value))
                        .collect();
                    (
                        *address,
                        (account.info.balance, account.info.nonce, account.info.code_hash, storage),
                    )
                })
                .collect()
        }

        fn bls_tx(&self, signer: Address, payload: &BlsPayload) -> BaseTransaction<TxEnv> {
            self.bls_tx_with(signer, payload.encode(), 5_000_000)
        }

        fn bls_tx_with(
            &self,
            signer: Address,
            input: Bytes,
            gas_limit: u64,
        ) -> BaseTransaction<TxEnv> {
            let tx = TxBls {
                chain_id: BlsConstants::CHAIN_ID,
                nonce: self.nonce(signer),
                max_priority_fee_per_gas: PRIORITY_FEE,
                max_fee_per_gas: MAX_FEE,
                gas_limit,
                input,
            };
            Self::envelope_tx(signer, BaseTxEnvelope::Bls(tx.into_signed(signature())))
        }

        fn envelope_tx(signer: Address, envelope: BaseTxEnvelope) -> BaseTransaction<TxEnv> {
            let encoded = envelope.encoded_2718().into();
            BaseTransaction::from_encoded_tx(&envelope, signer, encoded)
        }

        fn register(&mut self, owner: Address, seed: u64) {
            let key = BlsTestKey::from_seed(seed);
            let payload = BlsPayload::Registration {
                pubkey: key.public_key(),
                pop: key.proof_of_possession(),
            };
            let result =
                self.transact(self.bls_tx(owner, &payload)).expect("registration executes");
            assert!(result.is_success());
        }

        /// Asserts `tx` is rejected as an invalid BLS transaction without changing any state.
        fn assert_rejected(&mut self, tx: BaseTransaction<TxEnv>, reason: &str) {
            let before = self.snapshot();
            let error = self.transact(tx).expect_err("transaction must be rejected");
            match &error {
                EVMError::Transaction(BaseTransactionError::Bls(message)) => {
                    assert!(message.contains(reason), "expected `{reason}`, got `{message}`");
                }
                other => panic!("expected a BLS rejection, got {other:?}"),
            }
            assert_eq!(self.snapshot(), before, "a rejected transaction must not change state");
        }
    }

    fn signature() -> Signature {
        Signature::new(U256::from(1), U256::from(1), false)
    }

    fn transfer(sender: Address, nonce: u64, recipient: Address, value: U256) -> BlsTransfer {
        BlsTransfer { sender, nonce, recipient, value: U96::from(value) }
    }

    /// Builds a batch signed by the given key seeds, ordering transfers by sender.
    fn batch(mut transfers: Vec<(BlsTransfer, u64)>) -> BlsPayload {
        transfers.sort_by_key(|(transfer, _)| transfer.sender);
        let signatures: Vec<_> = transfers
            .iter()
            .map(|(transfer, seed)| {
                BlsTestKey::from_seed(*seed).sign(transfer.signing_digest(BlsConstants::CHAIN_ID))
            })
            .collect();
        BlsPayload::Batch {
            transfers: transfers.into_iter().map(|(transfer, _)| transfer).collect(),
            signature: BlsTestKey::aggregate(&signatures),
        }
    }

    /// A compressed G1 point on the curve but outside the prime-order subgroup.
    fn non_subgroup_g1() -> FixedBytes<48> {
        let point = (1u64..)
            .filter_map(|x| G1Affine::get_point_from_x_unchecked(Fq::from(x), true))
            .find(|point| !point.is_in_correct_subgroup_assuming_on_curve())
            .unwrap();
        let mut out = FixedBytes::ZERO;
        point.serialize_compressed(out.as_mut_slice()).unwrap();
        out
    }

    /// A compressed G2 point on the curve but outside the prime-order subgroup.
    fn non_subgroup_g2() -> FixedBytes<96> {
        let point = (1u64..)
            .filter_map(|x| {
                G2Affine::get_point_from_x_unchecked(Fq2::new(Fq::from(x), Fq::one()), true)
            })
            .find(|point| !point.is_in_correct_subgroup_assuming_on_curve())
            .unwrap();
        let mut out = FixedBytes::ZERO;
        point.serialize_compressed(out.as_mut_slice()).unwrap();
        out
    }

    #[test]
    fn registration_and_batch_move_native_balances_and_charge_relayer_fees() {
        let mut h = Harness::new();
        h.register(ALICE, ALICE_SEED);
        h.register(BOB, BOB_SEED);

        // The registry persists in ordinary state with an EIP-161-proof nonce.
        let registry = h.db.cache.accounts.get(&BlsConstants::REGISTRY_ADDRESS).unwrap();
        assert_eq!(registry.info.nonce, BlsRegistry::ACCOUNT_NONCE);
        let alice_key = BlsTestKey::from_seed(ALICE_SEED).public_key();
        let words = BlsRegistry::key_slots(ALICE)
            .map(|slot| registry.storage[&U256::from_be_bytes(slot.0)]);
        assert_eq!(BlsRegistry::key_from_words(words), Some(alice_key.0));
        // Registration consumes the owner's outer nonce like any transaction.
        assert_eq!((h.nonce(ALICE), h.nonce(BOB)), (1, 1));

        let before = h.snapshot();
        let (alice, bob, carol, relayer) =
            (h.balance(ALICE), h.balance(BOB), h.balance(CAROL), h.balance(RELAYER));
        let vaults = [
            COINBASE,
            Predeploys::BASE_FEE_VAULT,
            Predeploys::L1_FEE_VAULT,
            Predeploys::OPERATOR_FEE_VAULT,
        ]
        .map(|vault| h.balance(vault));

        let payload = batch(vec![
            (transfer(ALICE, 1, CAROL, ether(1)), ALICE_SEED),
            (transfer(BOB, 1, ALICE, ether(2)), BOB_SEED),
        ]);
        let tx = h.bls_tx(RELAYER, &payload);
        let enveloped = tx.enveloped_tx.clone().unwrap();
        let result = h.transact(tx).expect("batch executes");
        assert!(result.is_success());
        assert!(result.logs().is_empty());

        // Inner transfers spend and bump native balances and nonces.
        assert_eq!(h.balance(ALICE), alice - ether(1) + ether(2));
        assert_eq!(h.balance(BOB), bob - ether(2));
        assert_eq!(h.balance(CAROL), carol + ether(1));
        assert_eq!((h.nonce(ALICE), h.nonce(BOB)), (2, 2));
        assert_eq!(h.nonce(RELAYER), 1);

        // The relayer pays ordinary gas, L1 data and operator fees for exactly the deterministic gas.
        let gas_used = result.tx_gas_used();
        assert_eq!(gas_used, 42_000);
        let l1_cost = Harness::l1_block_info()
            .calculate_tx_l1_cost(&enveloped, BaseSpecId::new(BaseUpgrade::Isthmus));
        assert!(!l1_cost.is_zero());
        let operator_fee =
            U256::from(gas_used * OPERATOR_FEE_SCALAR / 1_000_000 + OPERATOR_FEE_CONSTANT);
        let gas_fee = U256::from(gas_used) * U256::from(BASE_FEE as u128 + PRIORITY_FEE);
        assert_eq!(h.balance(RELAYER), relayer - gas_fee - l1_cost - operator_fee);
        assert_eq!(h.balance(COINBASE), vaults[0] + U256::from(gas_used as u128 * PRIORITY_FEE));
        assert_eq!(
            h.balance(Predeploys::BASE_FEE_VAULT),
            vaults[1] + U256::from(gas_used * BASE_FEE)
        );
        assert_eq!(h.balance(Predeploys::L1_FEE_VAULT), vaults[2] + l1_cost);
        assert_eq!(h.balance(Predeploys::OPERATOR_FEE_VAULT), vaults[3] + operator_fee);

        // No ether is created or destroyed.
        let total = |s: &Snapshot| s.values().fold(U256::ZERO, |sum, account| sum + account.0);
        assert_eq!(total(&h.snapshot()), total(&before));
    }

    #[test]
    fn admission_validator_matches_execution() {
        let mut h = Harness::new();
        h.register(ALICE, ALICE_SEED);
        let payload = batch(vec![(transfer(ALICE, 1, CAROL, ether(1)), ALICE_SEED)]);
        let tx = TxBls {
            chain_id: BlsConstants::CHAIN_ID,
            nonce: 0,
            max_priority_fee_per_gas: PRIORITY_FEE,
            max_fee_per_gas: MAX_FEE,
            gas_limit: 5_000_000,
            input: payload.encode(),
        };

        let validated =
            BlsValidator::validate(&mut h.db, BlsConstants::CHAIN_ID, RELAYER, &tx).unwrap();
        assert_eq!(validated.payload, payload);
        assert_eq!(Ok(validated.intrinsic_gas), BlsValidator::intrinsic_gas(&tx));
        assert_eq!(
            BlsValidator::validate(&mut h.db, 8453, RELAYER, &tx),
            Err(BlsValidationError::UnsupportedChain { chain_id: 8453 })
        );
        assert_eq!(
            BlsValidator::validate(&mut h.db, BlsConstants::CHAIN_ID, ALICE, &tx),
            Err(BlsValidationError::SignerIsInnerSender { sender: ALICE })
        );

        let result = h.transact(h.bls_tx(RELAYER, &payload)).unwrap();
        assert_eq!(result.tx_gas_used(), validated.intrinsic_gas);
        // Replaying the batch fails validation against the new state.
        assert_eq!(
            BlsValidator::validate(&mut h.db, BlsConstants::CHAIN_ID, RELAYER, &tx),
            Err(BlsValidationError::NonceMismatch { sender: ALICE, expected: 2, nonce: 1 })
        );
    }

    #[test]
    fn rejects_registration_without_valid_proof_of_possession() {
        let mut h = Harness::new();
        let alice = BlsTestKey::from_seed(ALICE_SEED);
        let bob = BlsTestKey::from_seed(BOB_SEED);

        // A proof of possession made by another key.
        let payload =
            BlsPayload::Registration { pubkey: alice.public_key(), pop: bob.proof_of_possession() };
        h.assert_rejected(h.bls_tx(ALICE, &payload), "invalid BLS proof of possession");

        // A signature over the key under the transfer DST instead of the PoP DST.
        let wrong_dst = alice.sign(keccak256(alice.public_key()));
        let payload = BlsPayload::Registration { pubkey: alice.public_key(), pop: wrong_dst };
        h.assert_rejected(h.bls_tx(ALICE, &payload), "invalid BLS proof of possession");
    }

    #[test]
    fn rejects_rogue_key_registration_and_its_forged_batch() {
        let mut h = Harness::new();
        h.register(ALICE, ALICE_SEED);
        let victim = BlsTestKey::from_seed(ALICE_SEED);
        let attacker = BlsTestKey::from_seed(BOB_SEED);

        // Rogue key: pk_rogue = pk_attacker - pk_victim, so pk_rogue + pk_victim = pk_attacker and
        // an attacker-only signature would verify as a joint aggregate if the key were accepted.
        let decode = |pk: FixedBytes<96>| crate::BlsCrypto::public_key(&pk.0).unwrap();
        let rogue = (decode(attacker.public_key()).into_group() - decode(victim.public_key()))
            .into_affine();
        let mut rogue_pk = FixedBytes::<96>::ZERO;
        rogue.serialize_compressed(rogue_pk.as_mut_slice()).unwrap();
        // The attacker cannot produce a valid PoP for the rogue key without its secret.
        let rogue_pop = (crate::BlsCrypto::hash_to_g1(BlsConstants::POP_DST, rogue_pk.as_slice())
            .unwrap()
            * attacker.secret)
            .into_affine();
        let mut pop = FixedBytes::<48>::ZERO;
        rogue_pop.serialize_compressed(pop.as_mut_slice()).unwrap();
        let payload = BlsPayload::Registration { pubkey: rogue_pk, pop };
        h.assert_rejected(h.bls_tx(BOB, &payload), "invalid BLS proof of possession");

        // The forged batch spending the victim's balance therefore cannot verify.
        let forged = BlsPayload::Batch {
            transfers: vec![transfer(BOB, 0, CAROL, ether(1)), transfer(ALICE, 1, CAROL, ether(1))],
            signature: attacker.sign(B256::ZERO),
        };
        h.assert_rejected(h.bls_tx(RELAYER, &forged), "has no registered key");
    }

    #[test]
    fn rejects_replay_missing_signer_and_wrong_domain() {
        let mut h = Harness::new();
        h.register(ALICE, ALICE_SEED);
        let payload = batch(vec![(transfer(ALICE, 1, CAROL, ether(1)), ALICE_SEED)]);
        assert!(h.transact(h.bls_tx(RELAYER, &payload)).unwrap().is_success());

        // Replay of the executed batch.
        h.assert_rejected(h.bls_tx(RELAYER, &payload), "does not match account nonce");

        // A sender without a registered key.
        let unregistered = batch(vec![
            (transfer(ALICE, 2, CAROL, ether(1)), ALICE_SEED),
            (transfer(BOB, 0, CAROL, ether(1)), BOB_SEED),
        ]);
        h.assert_rejected(h.bls_tx(RELAYER, &unregistered), "has no registered key");

        // Signatures over the wrong chain id, and over a digest without the domain.
        let record = transfer(ALICE, 2, CAROL, ether(1));
        let key = BlsTestKey::from_seed(ALICE_SEED);
        let mut no_domain = [0u8; 92];
        U256::from(BlsConstants::CHAIN_ID)
            .to_be_bytes::<32>()
            .iter()
            .zip(no_domain.iter_mut())
            .for_each(|(b, o)| *o = *b);
        no_domain[32..].copy_from_slice(&record.encode());
        for signature in [key.sign(record.signing_digest(8453)), key.sign(keccak256(no_domain))] {
            let payload = BlsPayload::Batch { transfers: vec![record], signature };
            h.assert_rejected(h.bls_tx(RELAYER, &payload), "invalid BLS aggregate signature");
        }
    }

    #[test]
    fn rejects_non_subgroup_and_non_canonical_points() {
        let mut h = Harness::new();
        let key = BlsTestKey::from_seed(ALICE_SEED);

        let payload =
            BlsPayload::Registration { pubkey: non_subgroup_g2(), pop: key.proof_of_possession() };
        h.assert_rejected(h.bls_tx(ALICE, &payload), "invalid BLS public key");
        let payload = BlsPayload::Registration { pubkey: key.public_key(), pop: non_subgroup_g1() };
        h.assert_rejected(h.bls_tx(ALICE, &payload), "invalid BLS signature point");

        // An x coordinate that has no point on the curve.
        let off_curve = (1u64..)
            .find(|x| G1Affine::get_point_from_x_unchecked(Fq::from(*x), true).is_none())
            .unwrap();
        let mut pop = FixedBytes::<48>::ZERO;
        pop[40..].copy_from_slice(&off_curve.to_be_bytes());
        pop[0] |= 0x80;
        let payload = BlsPayload::Registration { pubkey: key.public_key(), pop };
        h.assert_rejected(h.bls_tx(ALICE, &payload), "invalid BLS signature point");

        // A coordinate at the field modulus and the encoded identity are malformed payloads.
        let mut input =
            BlsPayload::Registration { pubkey: key.public_key(), pop: key.proof_of_possession() }
                .encode()
                .to_vec();
        input[97..].copy_from_slice(&BlsConstants::FIELD_MODULUS);
        input[97] |= 0x80;
        h.assert_rejected(h.bls_tx_with(ALICE, input.clone().into(), 5_000_000), "non-canonical");
        input[97..].fill(0);
        input[97] = 0xc0;
        h.assert_rejected(h.bls_tx_with(ALICE, input.into(), 5_000_000), "non-canonical");
    }

    #[test]
    fn rejects_invalid_batches_against_state() {
        let mut h = Harness::new();
        h.register(ALICE, ALICE_SEED);
        h.register(BOB, BOB_SEED);

        // Circular funding: Alice can only afford her transfer with Bob's credit from the same
        // batch, which pre-batch balance validation refuses.
        let alice_balance = h.balance(ALICE);
        let circular = batch(vec![
            (transfer(ALICE, 1, CAROL, alice_balance + ether(1)), ALICE_SEED),
            (transfer(BOB, 1, ALICE, ether(2)), BOB_SEED),
        ]);
        h.assert_rejected(h.bls_tx(RELAYER, &circular), "is below value");

        // The relayer may not be an inner sender.
        let payload = batch(vec![(transfer(ALICE, 1, CAROL, ether(1)), ALICE_SEED)]);
        h.assert_rejected(h.bls_tx(ALICE, &payload), "is also a transfer sender");

        // Recipients must be code-free and not the registry.
        let mut contract = AccountInfo::default();
        contract.set_code(Bytecode::new_raw(Bytes::from_static(&[0x00])));
        h.db.insert_account_info(CAROL, contract);
        h.assert_rejected(h.bls_tx(RELAYER, &payload), "has code");
        let to_registry =
            batch(vec![(transfer(ALICE, 1, BlsConstants::REGISTRY_ADDRESS, ether(1)), ALICE_SEED)]);
        h.assert_rejected(h.bls_tx(RELAYER, &to_registry), "recipient is the registry");

        // Keys are immutable.
        let key = BlsTestKey::from_seed(BOB_SEED);
        let payload =
            BlsPayload::Registration { pubkey: key.public_key(), pop: key.proof_of_possession() };
        h.assert_rejected(h.bls_tx(ALICE, &payload), "already registered");
    }

    #[test]
    fn rejects_off_chain_and_underfunded_gas() {
        let mut h = Harness::new();
        let key = BlsTestKey::from_seed(ALICE_SEED);
        let payload =
            BlsPayload::Registration { pubkey: key.public_key(), pop: key.proof_of_possession() };

        // Any chain other than the development chain, even for an otherwise valid transaction.
        h.chain_id = 8453;
        h.assert_rejected(h.bls_tx(ALICE, &payload), "disabled on this chain");
        h.chain_id = BlsConstants::CHAIN_ID;

        // The gas limit must cover the deterministic gas.
        let gas =
            BlsValidator::intrinsic_gas(&TxBls { input: payload.encode(), ..Default::default() })
                .unwrap();
        let before = h.snapshot();
        let error = h.transact(h.bls_tx_with(ALICE, payload.encode(), gas - 1)).unwrap_err();
        assert!(matches!(
            error,
            EVMError::Transaction(BaseTransactionError::Base(
                InvalidTransaction::CallGasCostMoreThanGasLimit { .. }
            ))
        ));
        assert_eq!(h.snapshot(), before);
        assert_eq!(
            h.transact(h.bls_tx_with(ALICE, payload.encode(), gas)).unwrap().tx_gas_used(),
            gas
        );

        // Nonempty native input must still execute at exactly 21,000, not EVM calldata gas.
        let payload = batch(vec![(transfer(ALICE, 1, CAROL, ether(1)), ALICE_SEED)]);
        let before = h.snapshot();
        assert!(matches!(
            h.transact(h.bls_tx_with(RELAYER, payload.encode(), 20_999)),
            Err(EVMError::Transaction(BaseTransactionError::Base(
                InvalidTransaction::CallGasCostMoreThanGasLimit { .. }
            )))
        ));
        assert_eq!(h.snapshot(), before);
        assert_eq!(
            h.transact(h.bls_tx_with(RELAYER, payload.encode(), 21_000)).unwrap().tx_gas_used(),
            21_000
        );
    }

    #[test]
    fn ordinary_call_to_registry_does_not_register() {
        let mut h = Harness::new();
        let key = BlsTestKey::from_seed(ALICE_SEED);
        let payload =
            BlsPayload::Registration { pubkey: key.public_key(), pop: key.proof_of_possession() };
        let tx = TxEip1559 {
            chain_id: BlsConstants::CHAIN_ID,
            nonce: 0,
            gas_limit: 100_000,
            max_fee_per_gas: MAX_FEE,
            max_priority_fee_per_gas: PRIORITY_FEE,
            to: TxKind::Call(BlsConstants::REGISTRY_ADDRESS),
            input: payload.encode(),
            ..Default::default()
        };
        let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(signature()));
        let result = h.transact(Harness::envelope_tx(ALICE, envelope)).unwrap();
        assert!(result.is_success());
        assert_eq!(h.nonce(ALICE), 1);
        let tx = TxBls {
            chain_id: BlsConstants::CHAIN_ID,
            input: payload.encode(),
            ..Default::default()
        };
        // The key is still unregistered, so a real registration succeeds afterwards.
        assert!(BlsValidator::validate(&mut h.db, BlsConstants::CHAIN_ID, ALICE, &tx).is_ok());
    }
}
