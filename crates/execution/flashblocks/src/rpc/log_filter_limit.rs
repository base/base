//! Parameter-level limits for `eth_getLogs` filter inputs.
//!
//! Each address in a `getLogs` request is a separate index lookup the EL must
//! resolve per matched block, and each per-slot topic entry multiplies the
//! per-block work. Without an upper bound, a single unauthenticated request can
//! spend tens of seconds on the executor thread the EL also uses for the engine
//! API, which can cascade into missed sequencer block production windows. Same
//! `DoS` class as `eth_getProof` storage keys (capped in #2596 via
//! `MAX_PROOF_KEYS = 1024`).
//!
//! These caps reject pathological filters at the parameter layer, before any DB
//! access. Legitimate dApp queries — which typically address a handful of
//! contracts and a few topic values — are unaffected. Block-range capping is
//! handled separately by reth's existing filter settings and is intentionally
//! out of scope here.

use alloy_rpc_types_eth::Filter;
use jsonrpsee_types::{ErrorObjectOwned, error::INVALID_PARAMS_CODE};

/// Maximum number of addresses accepted in a single `eth_getLogs` request.
pub(super) const MAX_LOG_ADDRESSES: usize = 1000;

/// Maximum number of topic values accepted per topic slot (4 slots total).
pub(super) const MAX_LOG_TOPICS_PER_SLOT: usize = 1000;

/// Validates that an `eth_getLogs` filter's address and topic sets fit within
/// the configured caps.
#[derive(Debug)]
pub(super) struct LogFilterLimit;

impl LogFilterLimit {
    /// Returns an `InvalidParams` error when the filter exceeds the address or
    /// per-slot topic caps.
    pub(super) fn check(filter: &Filter) -> Result<(), ErrorObjectOwned> {
        let address_count = filter.address.len();
        if address_count > MAX_LOG_ADDRESSES {
            return Err(ErrorObjectOwned::owned(
                INVALID_PARAMS_CODE,
                format!("too many filter addresses: max {MAX_LOG_ADDRESSES}, got {address_count}"),
                None::<()>,
            ));
        }

        for (slot, topic_set) in filter.topics.iter().enumerate() {
            let topic_count = topic_set.len();
            if topic_count > MAX_LOG_TOPICS_PER_SLOT {
                return Err(ErrorObjectOwned::owned(
                    INVALID_PARAMS_CODE,
                    format!(
                        "too many filter topics in slot {slot}: max {MAX_LOG_TOPICS_PER_SLOT}, got {topic_count}"
                    ),
                    None::<()>,
                ));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256};
    use alloy_rpc_types_eth::Filter;

    use super::{LogFilterLimit, MAX_LOG_ADDRESSES, MAX_LOG_TOPICS_PER_SLOT};

    fn make_addr(i: usize) -> Address {
        let mut bytes = [0u8; 20];
        bytes[16..20].copy_from_slice(&(i as u32).to_be_bytes());
        Address::from(bytes)
    }

    fn make_topic(i: usize) -> B256 {
        let mut bytes = [0u8; 32];
        bytes[28..32].copy_from_slice(&(i as u32).to_be_bytes());
        B256::from(bytes)
    }

    fn filter_with_n_addresses(n: usize) -> Filter {
        let addrs: Vec<Address> = (0..n).map(make_addr).collect();
        Filter::new().address(addrs)
    }

    fn filter_with_n_topics_in_slot(slot: usize, n: usize) -> Filter {
        let topics: Vec<B256> = (0..n).map(make_topic).collect();
        let f = Filter::new();
        match slot {
            0 => f.event_signature(topics),
            1 => f.topic1(topics),
            2 => f.topic2(topics),
            3 => f.topic3(topics),
            _ => unreachable!("topic slot must be 0..4"),
        }
    }

    #[test]
    fn accepts_empty_filter() {
        assert!(LogFilterLimit::check(&Filter::new()).is_ok());
    }

    #[test]
    fn accepts_at_address_limit() {
        let f = filter_with_n_addresses(MAX_LOG_ADDRESSES);
        assert!(LogFilterLimit::check(&f).is_ok());
    }

    #[test]
    fn rejects_above_address_limit() {
        let f = filter_with_n_addresses(MAX_LOG_ADDRESSES + 1);
        let err = LogFilterLimit::check(&f).expect_err("must reject");
        assert!(
            err.message().contains("too many filter addresses"),
            "unexpected error message: {}",
            err.message()
        );
    }

    #[test]
    fn accepts_at_topic_limit_in_each_slot() {
        for slot in 0..4 {
            let f = filter_with_n_topics_in_slot(slot, MAX_LOG_TOPICS_PER_SLOT);
            assert!(
                LogFilterLimit::check(&f).is_ok(),
                "filter at topic limit in slot {slot} must be accepted",
            );
        }
    }

    #[test]
    fn rejects_above_topic_limit_in_each_slot() {
        for slot in 0..4 {
            let f = filter_with_n_topics_in_slot(slot, MAX_LOG_TOPICS_PER_SLOT + 1);
            let err = LogFilterLimit::check(&f).expect_err("must reject");
            assert!(
                err.message().contains(&format!("topics in slot {slot}")),
                "expected slot-{slot} error message, got: {}",
                err.message()
            );
        }
    }
}
