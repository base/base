//! C-2 wiring: bridge revm's per-tx execution output (`EvmState`) to
//! [`StateDiffEvent`]s via the [`crate::state_diff`] accumulator.
//!
//! revm produces a `ResultAndState` per executed transaction; its `state`
//! (`EvmState`) carries every changed storage slot (original + present value)
//! on every touched account. For each TRUSTED token contract we reverse-map its
//! changed slots to holders (candidates supplied from the tx's ERC-20 Transfer
//! logs) and net them into per-`(account, token)` [`StateDiffEvent`]s.
//!
//! The block-re-execution loop that produces the `EvmState` per tx (running each
//! tx with the Base EVM inside the `ExEx`) is the remaining integration step.

use alloy_primitives::{Address, U256, B256};
use revm::state::EvmState;
use revm::Database;

use crate::state_diff::{signed_delta, BalanceSlotRegistry, TxStateDiffAccumulator};
use crate::{PoolSlotDiffEvent, StateDiffEvent, NATIVE_SENTINEL};

/// Convert a single transaction's revm [`EvmState`] into net [`StateDiffEvent`]s.
///
/// `candidate_holders` are the addresses to reverse-map storage slots against —
/// typically the `from`/`to` of the tx's ERC-20 Transfer logs.
pub fn state_diffs_from_evm_state(
    state: &EvmState,
    registry: &BalanceSlotRegistry,
    candidate_holders: &[Address],
    tx_hash: B256,
    block_number: u64,
    flashblock_index: u32,
    payload_id: String,
) -> Vec<StateDiffEvent> {
    let mut acc = TxStateDiffAccumulator::new(registry);
    for (token, account) in state {
        if !registry.is_trusted(token) {
            continue; // untrusted token layout — storage delta not reliable
        }
        for (slot, change) in account.changed_storage_slots() {
            acc.record_sstore(
                token,
                *slot,
                change.original_value,
                change.present_value,
                candidate_holders,
            );
        }
    }
    acc.into_events(tx_hash, block_number, flashblock_index, payload_id)
}

/// Convert a single transaction's revm [`EvmState`] into native-ETH
/// [`StateDiffEvent`]s — one per touched account whose native balance moved,
/// tagged with [`NATIVE_SENTINEL`] as the token.
///
/// This is the native sibling of [`state_diffs_from_evm_state`]. Unlike the
/// ERC-20 path (which reverse-maps trusted-token storage against a Transfer-log
/// candidate slice), native value moves leave NO Transfer log, so we scan the
/// FULL touched set (`out.state`) — coinbase bribes are native-only and would
/// otherwise be missed. The emitter does NOT know arb classification, so it does
/// NO participant/fee filtering: it emits RAW deltas and the TS consumer
/// classifies them later.
///
/// For each touched account the delta is `post − pre`:
/// - `post` = `state[addr].info.balance` (present, after this tx executed);
/// - `pre`  = `db.basic(addr)?.balance`, defaulting to `U256::ZERO` for a fresh
///   account the db has never seen (`None`).
///
/// COMMIT-ORDERING PRECONDITION: `db` MUST still hold the PRE-tx state — i.e.
/// this is called BEFORE `db.commit(out.state)` for this tx. `db.basic()` reads
/// the committed (prior-tx) balance, which is the correct pre-tx baseline;
/// `Account::original_info()` is NOT usable here (private / BAL-only). Net-zero
/// deltas are dropped. `internal_calls` is always `None`.
pub fn native_balance_diffs_from_evm_state<DB: Database>(
    state: &EvmState,
    db: &mut DB,
    tx_hash: B256,
    block_number: u64,
    flashblock_index: u32,
    payload_id: String,
) -> Result<Vec<StateDiffEvent>, DB::Error> {
    let mut events = Vec::new();
    for (addr, account) in state {
        let post = account.info.balance;
        // PRE-tx baseline: db still holds the prior-commit state at the call
        // site (precondition above). A fresh account the db never saw ⇒ ZERO.
        let pre = db.basic(*addr)?.map_or(U256::ZERO, |info| info.balance);
        let Some(delta) = signed_delta(pre, post) else {
            continue; // magnitude overflow — not expected for real balances
        };
        if delta.is_zero() {
            continue; // net-zero native move — drop
        }
        events.push(StateDiffEvent {
            protocol_version: crate::PROTOCOL_VERSION,
            tx_hash,
            block_number,
            flashblock_index,
            payload_id: payload_id.clone(),
            account: *addr,
            token: NATIVE_SENTINEL,
            balance_delta_raw: delta,
            internal_calls: None,
        });
    }
    Ok(events)
}

/// Convert a single transaction's revm [`EvmState`] into [`PoolSlotDiffEvent`]s —
/// the RAW changed storage slots of every account in `candidate_pools` (the
/// Swap-log emitters for this tx, from [`crate::candidates::swap_pool_candidates`]).
///
/// This is the pool-PRICE sibling of [`state_diffs_from_evm_state`]: a swap moves
/// a pool's `slot0`/`liquidity` (UniV3) or `reserve0/1` (reserve pools), which are
/// packed pool-contract storage words — NOT token-balance slots — so the balance
/// reverse-mapping cannot recover them. The emitter has no pool-layout knowledge,
/// so it emits the raw `(slot, post-value)` words and the TS consumer decodes them
/// per protocol (mirrors the native-ETH RAW policy: emit truth, classify in TS).
///
/// Only genuinely-changed slots are emitted (`original != present`), in revm's
/// storage iteration order. Accounts not in `candidate_pools` are skipped, so a
/// router/token that merely had storage touched does not flood the stream.
pub fn pool_slot_diffs_from_evm_state(
    state: &EvmState,
    candidate_pools: &[Address],
    tx_hash: B256,
    block_number: u64,
    flashblock_index: u32,
    payload_id: String,
) -> Vec<PoolSlotDiffEvent> {
    let mut events = Vec::new();
    for (addr, account) in state {
        if !candidate_pools.contains(addr) {
            continue; // not a pool that swapped this tx
        }
        for (slot, change) in account.changed_storage_slots() {
            if change.original_value == change.present_value {
                continue; // unchanged — skip
            }
            events.push(PoolSlotDiffEvent {
                protocol_version: crate::PROTOCOL_VERSION,
                tx_hash,
                block_number,
                flashblock_index,
                payload_id: payload_id.clone(),
                pool: *addr,
                slot: *slot,
                value: change.present_value,
            });
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_diff::balance_slot_key;
    use alloy_primitives::{I256, U256};
    use revm::state::{Account, EvmStorageSlot};

    #[test]
    fn bridges_trusted_token_storage_change_to_event() {
        let reg = BalanceSlotRegistry::base_priority();
        let weth: Address = "0x4200000000000000000000000000000000000006".parse().unwrap();
        let holder = Address::from([0x11; 20]);
        let slot = balance_slot_key(&holder, 3); // WETH balance slot index = 3

        let mut account = Account::default();
        account
            .storage
            .insert(slot, EvmStorageSlot::new_changed(U256::from(100), U256::from(175), Default::default()));
        let mut state = EvmState::default();
        state.insert(weth, account);

        let events = state_diffs_from_evm_state(
            &state,
            &reg,
            &[holder],
            B256::from([0x22; 32]),
            1,
            0,
            "0x04".into(),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].account, holder);
        assert_eq!(events[0].token, weth);
        assert_eq!(events[0].balance_delta_raw, I256::try_from(75).unwrap());
    }

    #[test]
    fn emits_changed_pool_slots_for_candidate_pools_only() {
        let pool: Address = "0x6c561b446416e1a00e8e93e221854d6ea4171372".parse().unwrap();
        let other = Address::from([0x99; 20]); // touched but NOT a swap candidate
        let slot0 = U256::ZERO; // UniV3 slot0 storage index
        let liq_slot = U256::from(4u64);

        let mut pool_acct = Account::default();
        pool_acct
            .storage
            .insert(slot0, EvmStorageSlot::new_changed(U256::from(1), U256::from(2), Default::default()));
        pool_acct
            .storage
            .insert(liq_slot, EvmStorageSlot::new_changed(U256::from(10), U256::from(20), Default::default()));
        // an unchanged slot must NOT be emitted
        pool_acct
            .storage
            .insert(U256::from(7u64), EvmStorageSlot::new_changed(U256::from(5), U256::from(5), Default::default()));

        let mut other_acct = Account::default();
        other_acct
            .storage
            .insert(slot0, EvmStorageSlot::new_changed(U256::from(1), U256::from(2), Default::default()));

        let mut state = EvmState::default();
        state.insert(pool, pool_acct);
        state.insert(other, other_acct);

        let events = pool_slot_diffs_from_evm_state(
            &state,
            &[pool], // only `pool` is a swap candidate
            B256::from([0x22; 32]),
            47_620_296,
            1,
            "0x033ff020c315fa4a".into(),
        );
        // 2 changed slots from `pool`; `other` skipped; unchanged slot dropped.
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.pool == pool));
        let by_slot: std::collections::HashMap<U256, U256> =
            events.iter().map(|e| (e.slot, e.value)).collect();
        assert_eq!(by_slot.get(&slot0), Some(&U256::from(2)));
        assert_eq!(by_slot.get(&liq_slot), Some(&U256::from(20)));
        assert!(!by_slot.contains_key(&U256::from(7u64)));
    }

    #[test]
    fn ignores_untrusted_token_account() {
        let reg = BalanceSlotRegistry::base_priority();
        let untrusted = Address::from([0xEE; 20]);
        let holder = Address::from([0x11; 20]);
        let slot = balance_slot_key(&holder, 3);

        let mut account = Account::default();
        account
            .storage
            .insert(slot, EvmStorageSlot::new_changed(U256::ZERO, U256::from(1), Default::default()));
        let mut state = EvmState::default();
        state.insert(untrusted, account);

        let events =
            state_diffs_from_evm_state(&state, &reg, &[holder], B256::ZERO, 1, 0, "0x04".into());
        assert!(events.is_empty());
    }

    // --- WS-E E1: native-ETH balance diffs ----------------------------------

    use std::collections::HashMap;
    use std::convert::Infallible;

    use revm::primitives::{AddressMap, StorageKey, StorageValue};
    use revm::state::{AccountInfo, Bytecode};
    use revm::{Database, DatabaseCommit};

    /// In-memory `Database` mock holding only per-account native balances. It is
    /// the PRE-tx baseline source: `basic()` returns the currently committed
    /// balance, and `commit()` advances those balances from `info.balance`, so a
    /// sequence of (`compute-native-diff` → `commit`) calls mirrors the emitter
    /// tx loop.
    #[derive(Default)]
    struct BalanceDb {
        balances: HashMap<Address, U256>,
        /// Counts `basic()` lookups so a test can assert the emit-only invariant
        /// (one lookup per touched account, no extra EVM execution pass).
        basic_calls: std::cell::Cell<usize>,
    }

    impl BalanceDb {
        fn with(addr: Address, bal: u64) -> Self {
            let mut db = Self::default();
            db.balances.insert(addr, U256::from(bal));
            db
        }
    }

    impl Database for BalanceDb {
        type Error = Infallible;

        fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            self.basic_calls.set(self.basic_calls.get() + 1);
            Ok(self.balances.get(&address).map(|b| AccountInfo::from_balance(*b)))
        }

        fn code_by_hash(&mut self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
            Ok(Bytecode::default())
        }

        fn storage(
            &mut self,
            _address: Address,
            _index: StorageKey,
        ) -> Result<StorageValue, Self::Error> {
            Ok(U256::ZERO)
        }

        fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
    }

    impl DatabaseCommit for BalanceDb {
        fn commit(&mut self, changes: AddressMap<Account>) {
            for (addr, account) in changes {
                self.balances.insert(addr, account.info.balance);
            }
        }
    }

    /// Build an `EvmState` whose single account reports `post` as its post-tx
    /// native balance (everything else default).
    fn state_with_balance(addr: Address, post: u64) -> EvmState {
        let mut account = Account::default();
        account.info = AccountInfo::from_balance(U256::from(post));
        let mut state = EvmState::default();
        state.insert(addr, account);
        state
    }

    #[test]
    fn pre_tx_baseline_uses_db_basic_not_zero() {
        // Account already holds 100 wei pre-tx; tx ends it at 175 → +75 delta,
        // proving the baseline is `db.basic()` (100), NOT a 0 baseline (which
        // would yield +175). `Account::original_info()` is private/BAL-only and
        // is not consulted here.
        let acct = Address::from([0x11; 20]);
        let mut db = BalanceDb::with(acct, 100);
        let state = state_with_balance(acct, 175);

        let events = native_balance_diffs_from_evm_state(
            &state,
            &mut db,
            B256::from([0x22; 32]),
            1,
            0,
            "0x04".into(),
        )
        .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].account, acct);
        assert_eq!(events[0].token, NATIVE_SENTINEL);
        assert_eq!(events[0].balance_delta_raw, I256::try_from(75).unwrap());
        assert!(events[0].internal_calls.is_none());
    }

    #[test]
    fn sequential_txs_baseline_is_prior_commit() {
        // tx1: 100 → 150 (+50), THEN commit advances the db to 150. tx2's
        // baseline MUST be the post-tx1-commit balance (150), so 150 → 220 = +70
        // (NOT 220−100=+120). This pins the commit-ordering precondition: the
        // native read happens BEFORE each commit.
        let acct = Address::from([0x11; 20]);
        let mut db = BalanceDb::with(acct, 100);

        let s1 = state_with_balance(acct, 150);
        let e1 = native_balance_diffs_from_evm_state(&s1, &mut db, B256::ZERO, 1, 0, "0x04".into())
            .unwrap();
        assert_eq!(e1[0].balance_delta_raw, I256::try_from(50).unwrap());
        db.commit(s1); // advance to post-tx1 state (mirrors exex commit)

        let s2 = state_with_balance(acct, 220);
        let e2 = native_balance_diffs_from_evm_state(&s2, &mut db, B256::ZERO, 1, 0, "0x04".into())
            .unwrap();
        assert_eq!(e2[0].balance_delta_raw, I256::try_from(70).unwrap());
    }

    #[test]
    fn full_touched_set_captures_native_only_account() {
        // A coinbase-bribe recipient that moved native ETH with NO ERC-20
        // Transfer log still yields a native sentinel row. The fresh account is
        // unknown to the db (basic → None ⇒ ZERO baseline), gaining 1_000 wei.
        let bribe_recipient = Address::from([0xC0; 20]);
        let mut db = BalanceDb::default(); // recipient unseen → pre = ZERO
        let state = state_with_balance(bribe_recipient, 1_000);

        let events =
            native_balance_diffs_from_evm_state(&state, &mut db, B256::ZERO, 1, 0, "0x04".into())
                .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].account, bribe_recipient);
        assert_eq!(events[0].token, NATIVE_SENTINEL);
        assert_eq!(events[0].balance_delta_raw, I256::try_from(1_000).unwrap());
    }

    #[test]
    fn net_zero_native_delta_dropped() {
        // Balance unchanged across the tx (100 → 100) → no row.
        let acct = Address::from([0x11; 20]);
        let mut db = BalanceDb::with(acct, 100);
        let state = state_with_balance(acct, 100);

        let events =
            native_balance_diffs_from_evm_state(&state, &mut db, B256::ZERO, 1, 0, "0x04".into())
                .unwrap();
        assert!(events.is_empty(), "net-zero native delta must be dropped");
    }

    #[test]
    fn negative_native_delta_when_balance_decreases() {
        // A sender that paid out native ETH yields a negative delta.
        let acct = Address::from([0x11; 20]);
        let mut db = BalanceDb::with(acct, 500);
        let state = state_with_balance(acct, 200);

        let events =
            native_balance_diffs_from_evm_state(&state, &mut db, B256::ZERO, 1, 0, "0x04".into())
                .unwrap();
        assert_eq!(events[0].balance_delta_raw, I256::try_from(-300).unwrap());
    }

    #[test]
    fn emit_only_one_db_lookup_per_touched_account() {
        // Emit-only invariant: exactly one `db.basic()` per touched account and
        // NO new EVM execution pass (the fn never calls `transact`; it only reads
        // the supplied `out.state` + one baseline lookup per account).
        let a = Address::from([0x11; 20]);
        let b = Address::from([0x22; 20]);
        let mut db = BalanceDb::with(a, 10);
        db.balances.insert(b, U256::from(20));
        let mut state = state_with_balance(a, 15);
        state.insert(b, {
            let mut acct = Account::default();
            acct.info = AccountInfo::from_balance(U256::from(25));
            acct
        });

        let events =
            native_balance_diffs_from_evm_state(&state, &mut db, B256::ZERO, 1, 0, "0x04".into())
                .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(db.basic_calls.get(), 2, "exactly one basic() per touched account");
    }

    #[test]
    fn native_sentinel_matches_ts_literal() {
        // The on-wire token for native rows is byte-identical to the TS
        // NATIVE_SENTINEL (lowercased) in packages/node-protocol/src/index.ts.
        let acct = Address::from([0x11; 20]);
        let mut db = BalanceDb::with(acct, 1);
        let state = state_with_balance(acct, 2);
        let events =
            native_balance_diffs_from_evm_state(&state, &mut db, B256::ZERO, 1, 0, "0x04".into())
                .unwrap();
        let ev = crate::NodeEvent::StateDiff(events[0].clone());
        assert!(crate::encode_event(&ev)
            .contains(r#""token":"0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee""#));
    }
}
