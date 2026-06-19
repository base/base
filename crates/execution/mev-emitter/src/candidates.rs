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

use alloy_primitives::{b256, Address, B256};

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
pub fn transfer_candidates<'a>(
    log_topics: impl IntoIterator<Item = &'a [B256]>,
) -> Vec<Address> {
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
    fn collects_dedup_candidates_in_order() {
        let t1 = [ERC20_TRANSFER_TOPIC0, topic_addr(0x11), topic_addr(0x22)];
        let t2 = [ERC20_TRANSFER_TOPIC0, topic_addr(0x22), topic_addr(0x33)]; // 0x22 repeats
        let not_transfer = [B256::ZERO, topic_addr(0x99), topic_addr(0x99)];
        let logs: Vec<&[B256]> = vec![&t1, &t2, &not_transfer];
        let cands = transfer_candidates(logs);
        assert_eq!(
            cands,
            vec![
                Address::from([0x11; 20]),
                Address::from([0x22; 20]),
                Address::from([0x33; 20]),
            ]
        );
    }
}
