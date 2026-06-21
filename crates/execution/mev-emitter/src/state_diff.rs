//! C-2 core: trusted balance-slot reverse-mapping + net per-tx token deltas.
//!
//! An ERC-20 `balanceOf` is `mapping(address => uint256)` at a token-specific
//! slot index, so a holder's balance lives at
//! `keccak256(pad32(holder) ++ pad32(slotIndex))`. This module reverse-maps an
//! observed SSTORE (token contract, slot, old, new) back to `(holder, delta)` —
//! but ONLY for tokens in [`BalanceSlotRegistry`] (verified standard layouts).
//! Proxy / rebasing / fee-on-transfer tokens are NOT trusted from storage alone
//! (mirrors `packages/node-protocol/src/balance-slot-registry.ts`).
//!
//! Pure logic (no revm) so it is unit-tested directly; the revm `Inspector`
//! (node feature) is a thin wrapper that feeds SSTOREs in and emits the
//! resulting [`StateDiffEvent`]s.

use std::collections::HashMap;

use alloy_primitives::{Address, B256, I256, U256, keccak256};

use crate::StateDiffEvent;

/// A token whose `balanceOf` mapping slot index is known and standard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BalanceSlotEntry {
    /// ERC-20 token contract.
    pub token: Address,
    /// Storage slot index of the `balanceOf` mapping.
    pub balance_slot: u64,
}

/// Registry of tokens whose storage-derived balances can be trusted. State-diffs
/// for tokens NOT in the registry are untrusted and must be excluded from
/// classifier truth until live reconciliation verifies them.
#[derive(Debug, Clone, Default)]
pub struct BalanceSlotRegistry {
    by_token: HashMap<Address, u64>,
}

impl BalanceSlotRegistry {
    /// Build a registry from explicit entries.
    pub fn new(entries: impl IntoIterator<Item = BalanceSlotEntry>) -> Self {
        Self { by_token: entries.into_iter().map(|e| (e.token, e.balance_slot)).collect() }
    }

    /// The provisional Base-mainnet priority tokens (WETH/USDC/cbBTC/cbETH/AERO).
    /// Slot indices mirror the TS registry and are unverified until live
    /// reconciliation confirms them.
    pub fn base_priority() -> Self {
        Self::new([
            BalanceSlotEntry {
                token: addr("0x4200000000000000000000000000000000000006"),
                balance_slot: 3,
            }, // WETH
            BalanceSlotEntry {
                token: addr("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"),
                balance_slot: 9,
            }, // USDC
            BalanceSlotEntry {
                token: addr("0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf"),
                balance_slot: 9,
            }, // cbBTC
            BalanceSlotEntry {
                token: addr("0x2ae3f1ec7f1f5012cfeab0185bfc7aa3cf0dec22"),
                balance_slot: 51,
            }, // cbETH (OZ upgradeable: _balances at slot 51, verified live)
            BalanceSlotEntry {
                token: addr("0x940181a94a35a4569e4529a3cdfb74e38fd98631"),
                balance_slot: 0,
            }, // AERO
        ])
    }

    /// The trusted balance-slot index for a token, if registered.
    pub fn balance_slot(&self, token: &Address) -> Option<u64> {
        self.by_token.get(token).copied()
    }

    /// Whether a token's storage-derived balance is trusted.
    pub fn is_trusted(&self, token: &Address) -> bool {
        self.by_token.contains_key(token)
    }
}

/// The storage slot holding `holder`'s balance in a `mapping(address => uint256)`
/// at `slot_index`: `keccak256(pad32(holder) ++ pad32(slot_index))`, as a `U256`
/// (revm keys storage by `U256`).
pub fn balance_slot_key(holder: &Address, slot_index: u64) -> U256 {
    let mut preimage = [0u8; 64];
    // 32-byte left-padded holder, then 32-byte big-endian slot index.
    preimage[12..32].copy_from_slice(holder.as_slice());
    preimage[32..64].copy_from_slice(&U256::from(slot_index).to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(preimage).0)
}

/// Net signed delta of `new - old` as an `I256`, or `None` if the magnitude
/// overflows `I256` (not expected for real balances). `pub(crate)` so the
/// native-ETH bridge ([`crate::revm_bridge::native_balance_diffs_from_evm_state`])
/// reuses the SAME signed-delta arithmetic as the ERC-20 storage path.
pub(crate) fn signed_delta(old: U256, new: U256) -> Option<I256> {
    let (mag, neg) = if new >= old { (new - old, false) } else { (old - new, true) };
    let signed = I256::try_from(mag).ok()?;
    Some(if neg { -signed } else { signed })
}

/// Accumulates net `(account, token)` balance deltas for a single transaction
/// from observed SSTOREs on trusted token contracts.
#[derive(Debug, Clone)]
pub struct TxStateDiffAccumulator<'a> {
    registry: &'a BalanceSlotRegistry,
    /// Net delta keyed by `(account, token)`, insertion-ordered for determinism.
    deltas: Vec<((Address, Address), I256)>,
    index: HashMap<(Address, Address), usize>,
}

impl<'a> TxStateDiffAccumulator<'a> {
    /// Create an accumulator over `registry`.
    pub fn new(registry: &'a BalanceSlotRegistry) -> Self {
        Self { registry, deltas: Vec::new(), index: HashMap::new() }
    }

    /// Record one SSTORE. `contract` is the storage owner (a token), `slot` the
    /// written key, `old`/`new` the values. `candidate_holders` are addresses
    /// touched by the tx; the slot is reverse-mapped against them. No-op when the
    /// token is untrusted, the slot matches no candidate, or the value is unchanged.
    pub fn record_sstore(
        &mut self,
        contract: &Address,
        slot: U256,
        old: U256,
        new: U256,
        candidate_holders: &[Address],
    ) {
        if old == new {
            return;
        }
        let Some(slot_index) = self.registry.balance_slot(contract) else {
            return; // untrusted token — storage delta not reliable
        };
        let Some(holder) =
            candidate_holders.iter().find(|h| balance_slot_key(h, slot_index) == slot)
        else {
            return; // could not reverse-map this slot to a known holder
        };
        let Some(delta) = signed_delta(old, new) else {
            return;
        };
        let key = (*holder, *contract);
        match self.index.get(&key) {
            Some(&i) => self.deltas[i].1 = self.deltas[i].1.saturating_add(delta),
            None => {
                self.index.insert(key, self.deltas.len());
                self.deltas.push((key, delta));
            }
        }
    }

    /// Materialize the accumulated net deltas as [`StateDiffEvent`]s, dropping any
    /// that netted to zero. Order is deterministic (first-touch order).
    pub fn into_events(
        self,
        tx_hash: B256,
        block_number: u64,
        flashblock_index: u32,
        payload_id: String,
    ) -> Vec<StateDiffEvent> {
        self.deltas
            .into_iter()
            .filter(|(_, d)| *d != I256::ZERO)
            .map(|((account, token), delta)| StateDiffEvent {
                protocol_version: crate::PROTOCOL_VERSION,
                tx_hash,
                block_number,
                flashblock_index,
                payload_id: payload_id.clone(),
                account,
                token,
                balance_delta_raw: delta,
                internal_calls: None,
            })
            .collect()
    }
}

/// Parse a `0x`-hex address literal (panics on malformed input — used for the
/// compile-time registry constants only).
fn addr(s: &str) -> Address {
    s.parse().expect("valid address literal")
}

#[cfg(test)]
mod tests {
    use super::*;

    const WETH: &str = "0x4200000000000000000000000000000000000006";

    fn holder(b: u8) -> Address {
        Address::from([b; 20])
    }

    #[test]
    fn balance_slot_key_matches_solidity_convention() {
        // keccak256(pad32(holder) ++ pad32(slot)) — independent recomputation.
        let h = holder(0xAB);
        let key = balance_slot_key(&h, 3);
        let mut preimage = Vec::with_capacity(64);
        preimage.extend_from_slice(&[0u8; 12]);
        preimage.extend_from_slice(h.as_slice());
        preimage.extend_from_slice(&U256::from(3u64).to_be_bytes::<32>());
        assert_eq!(key, U256::from_be_bytes(keccak256(&preimage).0));
    }

    #[test]
    fn reverse_maps_trusted_sstore_to_holder_delta() {
        let reg = BalanceSlotRegistry::base_priority();
        let weth: Address = WETH.parse().unwrap();
        let h = holder(0x11);
        let slot = balance_slot_key(&h, 3); // WETH balance slot index = 3
        let mut acc = TxStateDiffAccumulator::new(&reg);
        acc.record_sstore(&weth, slot, U256::from(100), U256::from(150), &[h]);
        let events = acc.into_events(B256::from([0x22; 32]), 1, 0, "0x04".into());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].account, h);
        assert_eq!(events[0].token, weth);
        assert_eq!(events[0].balance_delta_raw, I256::try_from(50).unwrap());
    }

    #[test]
    fn negative_delta_when_balance_decreases() {
        let reg = BalanceSlotRegistry::base_priority();
        let weth: Address = WETH.parse().unwrap();
        let h = holder(0x11);
        let slot = balance_slot_key(&h, 3);
        let mut acc = TxStateDiffAccumulator::new(&reg);
        acc.record_sstore(&weth, slot, U256::from(150), U256::from(40), &[h]);
        let events = acc.into_events(B256::ZERO, 1, 0, "0x04".into());
        assert_eq!(events[0].balance_delta_raw, I256::try_from(-110).unwrap());
    }

    #[test]
    fn net_aggregates_multiple_sstores_and_drops_zero_net() {
        let reg = BalanceSlotRegistry::base_priority();
        let weth: Address = WETH.parse().unwrap();
        let h = holder(0x11);
        let slot = balance_slot_key(&h, 3);
        let mut acc = TxStateDiffAccumulator::new(&reg);
        acc.record_sstore(&weth, slot, U256::from(100), U256::from(150), &[h]); // +50
        acc.record_sstore(&weth, slot, U256::from(150), U256::from(100), &[h]); // -50 -> net 0
        let events = acc.into_events(B256::ZERO, 1, 0, "0x04".into());
        assert!(events.is_empty(), "zero-net delta must be dropped");
    }

    #[test]
    fn untrusted_token_and_unmatched_slot_are_ignored() {
        let reg = BalanceSlotRegistry::base_priority();
        let untrusted = holder(0xEE); // not in registry
        let weth: Address = WETH.parse().unwrap();
        let h = holder(0x11);
        let mut acc = TxStateDiffAccumulator::new(&reg);
        // untrusted token contract -> ignored even with a plausible slot
        acc.record_sstore(&untrusted, balance_slot_key(&h, 3), U256::ZERO, U256::from(1), &[h]);
        // trusted token but slot does not reverse-map to any candidate holder
        acc.record_sstore(&weth, U256::from(99u64), U256::ZERO, U256::from(1), &[h]);
        let events = acc.into_events(B256::ZERO, 1, 0, "0x04".into());
        assert!(events.is_empty());
    }
}
