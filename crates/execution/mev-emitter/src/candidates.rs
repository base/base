//! C-2 execution loop (part): candidate-holder extraction from ERC-20 Transfer
//! logs.
//!
//! Reverse-mapping a token's changed storage slot to a holder
//! ([`crate::state_diff`]) needs the set of candidate holder addresses. The
//! reliable source is the transaction's ERC-20 `Transfer(from, to, value)`
//! events: their indexed `from`/`to` are exactly the accounts whose balances
//! moved. This module decodes those parties from raw log topics — pure logic,
//! independent of any node type, so it is unit-tested directly and reused by the
//! ExEx loop (which pulls topics off the committed receipts).

use alloy_primitives::{Address, B256, b256};

/// `keccak256("Transfer(address,address,uint256)")` — the ERC-20 Transfer topic0.
pub const ERC20_TRANSFER_TOPIC0: B256 =
    b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");

/// Decode the `(from, to)` of an ERC-20 `Transfer` from its indexed topics, or
/// `None` if the log is not a standard Transfer (wrong topic0, or too few topics
/// — e.g. a non-indexed or ERC-721-style layout we don't treat as a balance move).
pub fn erc20_transfer_parties(topics: &[B256]) -> Option<(Address, Address)> {
    if topics.len() < 3 || topics[0] != ERC20_TRANSFER_TOPIC0 {
        return None;
    }
    Some((address_from_topic(&topics[1]), address_from_topic(&topics[2])))
}

/// Collect the de-duplicated candidate holders (Transfer `from`/`to`) across a
/// transaction's logs, given each log's topics. First-seen order is preserved.
pub fn transfer_candidates<'a>(log_topics: impl IntoIterator<Item = &'a [B256]>) -> Vec<Address> {
    let mut out: Vec<Address> = Vec::new();
    for topics in log_topics {
        if let Some((from, to)) = erc20_transfer_parties(topics) {
            for a in [from, to] {
                if !out.contains(&a) {
                    out.push(a);
                }
            }
        }
    }
    out
}

/// The low 20 bytes of a 32-byte indexed-address topic.
fn address_from_topic(topic: &B256) -> Address {
    Address::from_slice(&topic.as_slice()[12..32])
}

// --- pool-slot path: candidate POOLS from Swap logs --------------------------
//
// Mid-block pool PRICE state (UniV3 `slot0`/`liquidity`, reserve `reserve0/1`)
// lives in pool-contract storage, not in token-balance slots — so the balance
// reverse-mapping in [`crate::state_diff`] cannot recover it. Instead we collect
// the set of POOL addresses that swapped this tx (a swap is the only way pool
// price state moves) and emit their CHANGED raw storage slots; the TS consumer
// decodes slot→PoolState field per protocol (it owns the pool layout registry).
//
// A swap is identified by its emitter address + a known Swap `topic0`. The
// emitter (the pool) is the log's `address`, NOT a topic — so this path needs
// `(address, topics)` per log, unlike [`transfer_candidates`] which needs only
// topics. Topic0s verified via viem `toEventSelector`.

/// `keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)")` —
/// Uniswap V3 / Aerodrome Slipstream (concentrated-liquidity) Swap topic0.
pub const UNIV3_SWAP_TOPIC0: B256 =
    b256!("c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67");

/// `keccak256("Swap(address,uint256,uint256,uint256,uint256,address)")` —
/// Uniswap-V2-style Swap topic0 (amount0In, amount1In, amount0Out, amount1Out, to).
pub const UNIV2_SWAP_TOPIC0: B256 =
    b256!("d78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822");

/// `keccak256("Swap(address,address,uint256,uint256,uint256,uint256)")` —
/// Aerodrome / Velodrome-v2 / Solidly volatile+stable AMM Swap topic0.
pub const AERODROME_SWAP_TOPIC0: B256 =
    b256!("b3e2773606abfd36b5bd91394b3a54d1398336c65005baf7bf7a05efeffaf75b");

/// True when a log's `topic0` is one of the recognized AMM Swap signatures, i.e.
/// the log's emitter is a pool whose price state moved this tx.
pub fn is_swap_topic0(topic0: &B256) -> bool {
    *topic0 == UNIV3_SWAP_TOPIC0 || *topic0 == UNIV2_SWAP_TOPIC0 || *topic0 == AERODROME_SWAP_TOPIC0
}

/// Collect the de-duplicated POOL addresses (log emitters) that emitted a Swap
/// event across a transaction's logs, given each log's `(address, topics)`.
/// First-seen order is preserved (deterministic, mirrors [`transfer_candidates`]).
/// A log with no topics, or a non-Swap `topic0`, is skipped.
pub fn swap_pool_candidates<'a>(
    logs: impl IntoIterator<Item = (Address, &'a [B256])>,
) -> Vec<Address> {
    let mut out: Vec<Address> = Vec::new();
    for (address, topics) in logs {
        let Some(topic0) = topics.first() else { continue };
        if is_swap_topic0(topic0) && !out.contains(&address) {
            out.push(address);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topic_addr(b: u8) -> B256 {
        let mut t = [0u8; 32];
        t[12..32].copy_from_slice(&[b; 20]);
        B256::from(t)
    }

    #[test]
    fn decodes_transfer_parties() {
        let topics = [ERC20_TRANSFER_TOPIC0, topic_addr(0xAA), topic_addr(0xBB)];
        let (from, to) = erc20_transfer_parties(&topics).unwrap();
        assert_eq!(from, Address::from([0xAA; 20]));
        assert_eq!(to, Address::from([0xBB; 20]));
    }

    #[test]
    fn rejects_non_transfer_and_short_topics() {
        assert!(erc20_transfer_parties(&[B256::ZERO, topic_addr(1), topic_addr(2)]).is_none());
        assert!(erc20_transfer_parties(&[ERC20_TRANSFER_TOPIC0, topic_addr(1)]).is_none());
    }

    #[test]
    fn collects_swap_pool_candidates_dedup_in_order() {
        let pool_a = Address::from([0xA1; 20]);
        let pool_b = Address::from([0xB2; 20]);
        let router = Address::from([0xCC; 20]);
        let v3 = [UNIV3_SWAP_TOPIC0];
        let v2 = [UNIV2_SWAP_TOPIC0];
        let aero = [AERODROME_SWAP_TOPIC0];
        let transfer = [ERC20_TRANSFER_TOPIC0, topic_addr(0x11), topic_addr(0x22)];
        // pool_a swaps (v3) then again (aero) -> dedup; pool_b swaps (v2); router
        // only emits a Transfer -> excluded (not a pool price move).
        let logs: Vec<(Address, &[B256])> =
            vec![(pool_a, &v3), (router, &transfer), (pool_b, &v2), (pool_a, &aero)];
        let cands = swap_pool_candidates(logs);
        assert_eq!(cands, vec![pool_a, pool_b]);
    }

    #[test]
    fn swap_pool_candidates_skips_empty_and_non_swap_topics() {
        let pool = Address::from([0xA1; 20]);
        let empty: [B256; 0] = [];
        let non_swap = [B256::ZERO];
        let logs: Vec<(Address, &[B256])> = vec![(pool, &empty), (pool, &non_swap)];
        assert!(swap_pool_candidates(logs).is_empty());
    }

    #[test]
    fn collects_dedup_candidates_in_order() {
        let t1 = [ERC20_TRANSFER_TOPIC0, topic_addr(0x11), topic_addr(0x22)];
        let t2 = [ERC20_TRANSFER_TOPIC0, topic_addr(0x22), topic_addr(0x33)]; // 0x22 repeats
        let not_transfer = [B256::ZERO, topic_addr(0x99), topic_addr(0x99)];
        let logs: Vec<&[B256]> = vec![&t1, &t2, &not_transfer];
        let cands = transfer_candidates(logs);
        assert_eq!(
            cands,
            vec![Address::from([0x11; 20]), Address::from([0x22; 20]), Address::from([0x33; 20]),]
        );
    }
}
