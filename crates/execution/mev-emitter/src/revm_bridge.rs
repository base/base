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

use alloy_primitives::{Address, B256};
use revm::state::EvmState;

use crate::state_diff::{BalanceSlotRegistry, TxStateDiffAccumulator};
use crate::StateDiffEvent;

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
}
