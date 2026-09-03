//! Per-sender routing and predicate resolution for validity transactions.
//!
//! Cohort assignment is deterministic per sender: a sender's entire nonce
//! stream flows through a single submission origin (plain vs. validity), which
//! keeps nonces contiguous and single-origin under congestion. Predicate
//! templates are resolved into concrete [`ValidityPredicate`] values against
//! each transaction's `from`/`to` at prepare time.

use alloy_primitives::{Address, B256, U256, keccak256};
use base_execution_txpool::ValidityPredicate;

use super::{
    BlockNumberBound, LoadConfig, PredicateAddress, SlotTemplate, SubmitCohort,
    ValidityPredicateTemplate,
};

/// Salt mixed into the per-sender validity routing hash.
const VALIDITY_SENDER_SALT: u64 = 0x7661_6c69_6469_7479; // "validity"
/// Salt mixed into the priority-lead validity sub-cohort hash.
const PRIORITY_LEAD_SENDER_SALT: u64 = 0x6c65_6164_2d74_6970; // "lead-tip"

/// Routes transactions onto submission cohorts and resolves validity predicates.
#[derive(Debug, Clone)]
pub struct ValidityRouter {
    ratio: f64,
    priority_lead_ratio: f64,
    predicates: Vec<ValidityPredicateTemplate>,
    seed: u64,
}

impl ValidityRouter {
    /// Builds a router from the runtime load configuration.
    pub fn new(config: &LoadConfig) -> Self {
        Self {
            ratio: config.validity_ratio,
            priority_lead_ratio: config.validity_priority_lead_ratio,
            predicates: config.validity_predicates.clone(),
            seed: config.seed,
        }
    }

    /// Returns true when no transaction will ever be routed to the validity path.
    pub fn is_disabled(&self) -> bool {
        self.ratio <= 0.0
    }

    /// Returns true when resolving this router's predicates requires the current
    /// chain height, i.e. at least one [`BlockNumberBound::Offset`] template is
    /// configured on the validity path. Absolute, balance, storage, and
    /// flashblock-index predicates never read the current height, so absolute-only
    /// configurations avoid the per-round latest-block fetch entirely.
    pub fn needs_current_block(&self) -> bool {
        !self.is_disabled()
            && self.predicates.iter().any(|template| {
                matches!(
                    template,
                    ValidityPredicateTemplate::BlockNumber {
                        bound: BlockNumberBound::Offset(_),
                        ..
                    }
                )
            })
    }

    /// Determines the submission cohort for a sender.
    ///
    /// The decision is deterministic in `(seed, sender)` so a sender's entire
    /// nonce stream shares one origin across the run.
    pub fn cohort_for_sender(&self, sender: Address) -> SubmitCohort {
        if self.is_disabled()
            || !sender_in_fraction(sender, self.seed, VALIDITY_SENDER_SALT, self.ratio)
        {
            return SubmitCohort::Plain;
        }
        if sender_in_fraction(
            sender,
            self.seed,
            PRIORITY_LEAD_SENDER_SALT,
            self.priority_lead_ratio,
        ) {
            SubmitCohort::ValidityPassPriorityLead
        } else {
            SubmitCohort::ValidityPass
        }
    }

    /// Resolves the configured predicate templates into concrete predicates for
    /// a transaction. Returns an empty vector for the plain cohort, which carries
    /// no predicates.
    ///
    /// `current_block` is the latest chain height at prepare time, used to
    /// resolve any [`BlockNumberBound::Offset`] into an absolute block number
    /// (`current_block + offset`). `nonce` makes [`SlotTemplate::SenderNonce`]
    /// storage locations unique to the transaction being prepared.
    pub fn predicates_for(
        &self,
        cohort: SubmitCohort,
        current_block: u64,
        nonce: u64,
        from: Address,
        to: Option<Address>,
    ) -> Vec<ValidityPredicate> {
        match cohort {
            SubmitCohort::ValidityPass | SubmitCohort::ValidityPassPriorityLead => self
                .predicates
                .iter()
                .map(|template| Self::resolve(template, current_block, nonce, from, to))
                .collect(),
            SubmitCohort::Plain => Vec::new(),
        }
    }

    /// Resolves a single template into a concrete predicate.
    fn resolve(
        template: &ValidityPredicateTemplate,
        current_block: u64,
        nonce: u64,
        from: Address,
        to: Option<Address>,
    ) -> ValidityPredicate {
        match template {
            ValidityPredicateTemplate::Balance { address, op, value } => {
                ValidityPredicate::Balance {
                    address: resolve_address(address, from, to),
                    op: *op,
                    value: *value,
                }
            }
            ValidityPredicateTemplate::Storage { address, slot, mask, op, value } => {
                ValidityPredicate::Storage {
                    address: resolve_address(address, from, to),
                    slot: resolve_slot(slot, nonce, from, to),
                    mask: mask.unwrap_or_else(ValidityPredicate::default_mask),
                    op: *op,
                    value: value.resolve(from),
                }
            }
            // Position predicates read the build context, not `from`/`to`. An
            // absolute bound resolves identically for every transaction; an
            // offset bound resolves against the current chain height.
            ValidityPredicateTemplate::BlockNumber { op, bound } => {
                let value = match bound {
                    BlockNumberBound::Absolute(value) => *value,
                    BlockNumberBound::Offset(offset) => {
                        U256::from(current_block).saturating_add(*offset)
                    }
                };
                ValidityPredicate::BlockNumber { op: *op, value }
            }
            ValidityPredicateTemplate::FlashblockIndex { op, value } => {
                ValidityPredicate::FlashblockIndex { op: *op, value: *value }
            }
        }
    }
}

/// Resolves a predicate address source against a transaction's `from`/`to`.
///
/// A contract-creation transaction (no `to`) falls back to the sender.
fn resolve_address(source: &PredicateAddress, from: Address, to: Option<Address>) -> Address {
    match source {
        PredicateAddress::Sender => from,
        PredicateAddress::Recipient => to.unwrap_or(from),
        PredicateAddress::Fixed(addr) => *addr,
    }
}

/// Resolves a storage slot source against the transaction being prepared.
fn resolve_slot(slot: &SlotTemplate, nonce: u64, from: Address, to: Option<Address>) -> U256 {
    match slot {
        SlotTemplate::Fixed(value) => *value,
        SlotTemplate::Mapping { mapping_slot, key } => {
            mapping_slot_for(resolve_address(key, from, to), *mapping_slot)
        }
        SlotTemplate::SenderNonce { salt } => {
            let mut preimage = [0u8; 96];
            preimage[12..32].copy_from_slice(from.as_slice());
            preimage[56..64].copy_from_slice(&nonce.to_be_bytes());
            preimage[64..96].copy_from_slice(&salt.to_be_bytes::<32>());
            U256::from_be_bytes::<32>(keccak256(preimage).0)
        }
    }
}

/// Computes the Solidity mapping storage slot `keccak256(key ++ mapping_slot)`
/// for a 20-byte address key left-padded to 32 bytes.
fn mapping_slot_for(key: Address, mapping_slot: U256) -> U256 {
    let mut preimage = [0u8; 64];
    preimage[12..32].copy_from_slice(key.as_slice());
    preimage[32..64].copy_from_slice(&mapping_slot.to_be_bytes::<32>());
    U256::from_be_bytes::<32>(keccak256(preimage).0)
}

/// Returns true when a sender falls within `fraction` of the sender space for a
/// given salt, using a deterministic hash of `(seed, salt, sender)`.
fn sender_in_fraction(sender: Address, seed: u64, salt: u64, fraction: f64) -> bool {
    if fraction <= 0.0 {
        return false;
    }
    if fraction >= 1.0 {
        return true;
    }
    let mut preimage = [0u8; 36];
    preimage[..8].copy_from_slice(&seed.to_be_bytes());
    preimage[8..16].copy_from_slice(&salt.to_be_bytes());
    preimage[16..36].copy_from_slice(sender.as_slice());
    let digest: B256 = keccak256(preimage);
    // Map the leading 8 bytes to a uniform [0, 1) value.
    let bucket = u64::from_be_bytes(digest.0[..8].try_into().expect("8 bytes"));
    (bucket as f64) / (u64::MAX as f64) < fraction
}

#[cfg(test)]
mod tests {
    use base_execution_txpool::ValidityOperator;

    use super::*;
    use crate::PredicateValue;

    fn router(ratio: f64, predicates: Vec<ValidityPredicateTemplate>) -> ValidityRouter {
        ValidityRouter { ratio, priority_lead_ratio: 0.0, predicates, seed: 12345 }
    }

    #[test]
    fn disabled_router_routes_everything_plain() {
        let r = router(0.0, Vec::new());
        assert!(r.is_disabled());
        assert_eq!(r.cohort_for_sender(Address::repeat_byte(0x11)), SubmitCohort::Plain);
    }

    #[test]
    fn full_ratio_routes_all_senders_to_validity() {
        let r = router(1.0, Vec::new());
        for i in 0..50u8 {
            assert_eq!(
                r.cohort_for_sender(Address::repeat_byte(i)),
                SubmitCohort::ValidityPass,
                "sender {i} must route to validity at ratio 1.0"
            );
        }
    }

    #[test]
    fn cohort_is_stable_per_sender() {
        let r = router(0.5, Vec::new());
        let sender = Address::repeat_byte(0x22);
        let first = r.cohort_for_sender(sender);
        for _ in 0..10 {
            assert_eq!(r.cohort_for_sender(sender), first);
        }
    }

    #[test]
    fn ratio_selects_roughly_the_configured_fraction() {
        let r = router(0.3, Vec::new());
        let total = 2_000u32;
        let validity = (0..total)
            .filter(|i| {
                let mut bytes = [0u8; 20];
                bytes[..4].copy_from_slice(&i.to_be_bytes());
                r.cohort_for_sender(Address::from(bytes)).is_validity()
            })
            .count();
        let fraction = validity as f64 / total as f64;
        assert!((fraction - 0.3).abs() < 0.05, "fraction {fraction} should be near 0.3");
    }

    #[test]
    fn priority_lead_ratio_selects_a_stable_validity_sub_cohort() {
        let mut r = router(0.8, Vec::new());
        r.priority_lead_ratio = 0.25;
        let total = 4_000u32;
        let cohorts: Vec<SubmitCohort> = (0..total)
            .map(|i| {
                let mut bytes = [0u8; 20];
                bytes[..4].copy_from_slice(&i.to_be_bytes());
                r.cohort_for_sender(Address::from(bytes))
            })
            .collect();
        let validity = cohorts.iter().filter(|cohort| cohort.is_validity()).count();
        let priority_lead = cohorts
            .iter()
            .filter(|cohort| **cohort == SubmitCohort::ValidityPassPriorityLead)
            .count();

        assert!((validity as f64 / total as f64 - 0.8).abs() < 0.05);
        assert!((priority_lead as f64 / validity as f64 - 0.25).abs() < 0.05);
    }

    #[test]
    fn pass_cohort_resolves_sender_and_recipient_addresses() {
        let templates = vec![
            ValidityPredicateTemplate::Balance {
                address: PredicateAddress::Sender,
                op: ValidityOperator::GreaterThanOrEqual,
                value: U256::ZERO,
            },
            ValidityPredicateTemplate::Balance {
                address: PredicateAddress::Recipient,
                op: ValidityOperator::LessThanOrEqual,
                value: U256::MAX,
            },
        ];
        let r = router(1.0, templates);
        let from = Address::repeat_byte(0xaa);
        let to = Address::repeat_byte(0xbb);
        let predicates = r.predicates_for(SubmitCohort::ValidityPass, 0, 0, from, Some(to));
        assert_eq!(predicates.len(), 2);
        match &predicates[0] {
            ValidityPredicate::Balance { address, .. } => assert_eq!(*address, from),
            other => panic!("expected balance, got {other:?}"),
        }
        match &predicates[1] {
            ValidityPredicate::Balance { address, .. } => assert_eq!(*address, to),
            other => panic!("expected balance, got {other:?}"),
        }
    }

    #[test]
    fn position_predicates_resolve_independent_of_addresses() {
        let templates = vec![
            ValidityPredicateTemplate::BlockNumber {
                op: ValidityOperator::GreaterThanOrEqual,
                bound: BlockNumberBound::Absolute(U256::from(100)),
            },
            ValidityPredicateTemplate::FlashblockIndex {
                op: ValidityOperator::Equal,
                value: U256::from(2),
            },
        ];
        let r = router(1.0, templates);
        let predicates = r.predicates_for(
            SubmitCohort::ValidityPass,
            0,
            0,
            Address::repeat_byte(0xaa),
            Some(Address::repeat_byte(0xbb)),
        );
        assert_eq!(
            predicates,
            vec![
                ValidityPredicate::BlockNumber {
                    op: ValidityOperator::GreaterThanOrEqual,
                    value: U256::from(100),
                },
                ValidityPredicate::FlashblockIndex {
                    op: ValidityOperator::Equal,
                    value: U256::from(2),
                },
            ],
        );
    }

    #[test]
    fn absolute_block_number_bound_ignores_current_block() {
        let templates = vec![ValidityPredicateTemplate::BlockNumber {
            op: ValidityOperator::GreaterThanOrEqual,
            bound: BlockNumberBound::Absolute(U256::from(12345)),
        }];
        let r = router(1.0, templates);
        let predicates = r.predicates_for(
            SubmitCohort::ValidityPass,
            9_000,
            0,
            Address::repeat_byte(0xaa),
            None,
        );
        assert_eq!(
            predicates,
            vec![ValidityPredicate::BlockNumber {
                op: ValidityOperator::GreaterThanOrEqual,
                value: U256::from(12345),
            }],
        );
    }

    #[test]
    fn offset_block_number_bound_resolves_against_current_block() {
        let templates = vec![ValidityPredicateTemplate::BlockNumber {
            op: ValidityOperator::GreaterThanOrEqual,
            bound: BlockNumberBound::Offset(U256::from(10)),
        }];
        let r = router(1.0, templates);
        let predicates = r.predicates_for(
            SubmitCohort::ValidityPass,
            1_000,
            0,
            Address::repeat_byte(0xaa),
            None,
        );
        assert_eq!(
            predicates,
            vec![ValidityPredicate::BlockNumber {
                op: ValidityOperator::GreaterThanOrEqual,
                value: U256::from(1_010),
            }],
        );
    }

    #[test]
    fn offset_block_number_bound_saturates_instead_of_wrapping() {
        let templates = vec![ValidityPredicateTemplate::BlockNumber {
            op: ValidityOperator::GreaterThanOrEqual,
            bound: BlockNumberBound::Offset(U256::MAX),
        }];
        let r = router(1.0, templates);
        let predicates = r.predicates_for(
            SubmitCohort::ValidityPass,
            1_000,
            0,
            Address::repeat_byte(0xaa),
            None,
        );
        assert_eq!(
            predicates,
            vec![ValidityPredicate::BlockNumber {
                op: ValidityOperator::GreaterThanOrEqual,
                value: U256::MAX,
            }],
        );
    }

    #[test]
    fn needs_current_block_only_with_an_offset_bound() {
        let offset = router(
            1.0,
            vec![ValidityPredicateTemplate::BlockNumber {
                op: ValidityOperator::GreaterThanOrEqual,
                bound: BlockNumberBound::Offset(U256::from(10)),
            }],
        );
        assert!(offset.needs_current_block(), "an offset bound requires the current height");

        let absolute = router(
            1.0,
            vec![ValidityPredicateTemplate::BlockNumber {
                op: ValidityOperator::GreaterThanOrEqual,
                bound: BlockNumberBound::Absolute(U256::from(12345)),
            }],
        );
        assert!(!absolute.needs_current_block(), "an absolute bound must not trigger the fetch");

        let balance = router(
            1.0,
            vec![ValidityPredicateTemplate::Balance {
                address: PredicateAddress::Sender,
                op: ValidityOperator::GreaterThanOrEqual,
                value: U256::ZERO,
            }],
        );
        assert!(
            !balance.needs_current_block(),
            "non-position predicates must not trigger the fetch"
        );
    }

    #[test]
    fn disabled_router_never_needs_current_block() {
        let r = router(
            0.0,
            vec![ValidityPredicateTemplate::BlockNumber {
                op: ValidityOperator::GreaterThanOrEqual,
                bound: BlockNumberBound::Offset(U256::from(10)),
            }],
        );
        assert!(!r.needs_current_block(), "a disabled router routes nothing to the validity path");
    }

    #[test]
    fn flashblock_index_resolves_independent_of_current_block() {
        let templates = vec![ValidityPredicateTemplate::FlashblockIndex {
            op: ValidityOperator::Equal,
            value: U256::from(2),
        }];
        let r = router(1.0, templates);
        let low =
            r.predicates_for(SubmitCohort::ValidityPass, 0, 0, Address::repeat_byte(0xaa), None);
        let high = r.predicates_for(
            SubmitCohort::ValidityPass,
            9_999,
            0,
            Address::repeat_byte(0xaa),
            None,
        );
        assert_eq!(low, high);
        assert_eq!(
            low,
            vec![ValidityPredicate::FlashblockIndex {
                op: ValidityOperator::Equal,
                value: U256::from(2),
            }],
        );
    }

    #[test]
    fn plain_cohort_carries_no_predicates() {
        let templates = vec![ValidityPredicateTemplate::Balance {
            address: PredicateAddress::Sender,
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::ZERO,
        }];
        let r = router(1.0, templates);
        let from = Address::repeat_byte(0xaa);
        assert!(r.predicates_for(SubmitCohort::Plain, 0, 0, from, None).is_empty());
    }

    #[test]
    fn recipient_address_falls_back_to_sender_on_create() {
        let templates = vec![ValidityPredicateTemplate::Balance {
            address: PredicateAddress::Recipient,
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::ZERO,
        }];
        let r = router(1.0, templates);
        let from = Address::repeat_byte(0xaa);
        let predicates = r.predicates_for(SubmitCohort::ValidityPass, 0, 0, from, None);
        match &predicates[0] {
            ValidityPredicate::Balance { address, .. } => assert_eq!(*address, from),
            other => panic!("expected balance, got {other:?}"),
        }
    }

    #[test]
    fn mapping_slot_matches_solidity_layout() {
        // keccak256(pad32(key) ++ pad32(slot)) is the canonical mapping slot.
        let key = Address::repeat_byte(0x01);
        let templates = vec![ValidityPredicateTemplate::Storage {
            address: PredicateAddress::Fixed(Address::repeat_byte(0x99)),
            slot: SlotTemplate::Mapping {
                mapping_slot: U256::ZERO,
                key: PredicateAddress::Fixed(key),
            },
            mask: None,
            op: ValidityOperator::GreaterThanOrEqual,
            value: PredicateValue::Fixed(U256::ZERO),
        }];
        let r = router(1.0, templates);
        let predicates = r.predicates_for(SubmitCohort::ValidityPass, 0, 0, Address::ZERO, None);
        let expected = {
            let mut preimage = [0u8; 64];
            preimage[12..32].copy_from_slice(key.as_slice());
            // mapping_slot 0 => all zero high half
            U256::from_be_bytes::<32>(keccak256(preimage).0)
        };
        match &predicates[0] {
            ValidityPredicate::Storage { slot, mask, .. } => {
                assert_eq!(*slot, expected);
                assert_eq!(*mask, ValidityPredicate::default_mask());
            }
            other => panic!("expected storage, got {other:?}"),
        }
    }

    #[test]
    fn sender_nonce_slot_matches_known_vector_and_changes_with_each_input() {
        let sender = Address::repeat_byte(0x11);
        let salt = U256::from(7);
        let templates = vec![ValidityPredicateTemplate::Storage {
            address: PredicateAddress::Fixed(Address::repeat_byte(0x99)),
            slot: SlotTemplate::SenderNonce { salt },
            mask: None,
            op: ValidityOperator::GreaterThanOrEqual,
            value: PredicateValue::Fixed(U256::ZERO),
        }];
        let r = router(1.0, templates);

        let concrete_slot = |router: &ValidityRouter, sender, nonce| match &router.predicates_for(
            SubmitCohort::ValidityPass,
            0,
            nonce,
            sender,
            None,
        )[0]
        {
            ValidityPredicate::Storage { slot, .. } => *slot,
            other => panic!("expected storage predicate, got {other:?}"),
        };
        let slot = concrete_slot(&r, sender, 42);
        let expected = U256::from_str_radix(
            "da52651d545e6d5ecd593d763e061ef7a0521fafaac8f10fe44c58a420947341",
            16,
        )
        .unwrap();
        let different_salt = router(
            1.0,
            vec![ValidityPredicateTemplate::Storage {
                address: PredicateAddress::Fixed(Address::repeat_byte(0x99)),
                slot: SlotTemplate::SenderNonce { salt: salt + U256::from(1) },
                mask: None,
                op: ValidityOperator::GreaterThanOrEqual,
                value: PredicateValue::Fixed(U256::ZERO),
            }],
        );

        assert_eq!(slot, expected);
        assert_ne!(slot, concrete_slot(&r, Address::repeat_byte(0x12), 42));
        assert_ne!(slot, concrete_slot(&r, sender, 43));
        assert_ne!(slot, concrete_slot(&different_salt, sender, 42));
    }

    #[test]
    fn sender_parity_value_splits_storage_predicates_by_sender() {
        let templates = vec![ValidityPredicateTemplate::Storage {
            address: PredicateAddress::Fixed(Address::repeat_byte(0x99)),
            slot: SlotTemplate::Fixed(U256::ZERO),
            mask: Some(U256::from(1)),
            op: ValidityOperator::Equal,
            value: PredicateValue::SenderParity,
        }];
        let r = router(1.0, templates);

        for (sender, expected) in
            [(Address::repeat_byte(0x10), U256::ZERO), (Address::repeat_byte(0x11), U256::from(1))]
        {
            let predicates = r.predicates_for(SubmitCohort::ValidityPass, 0, 0, sender, None);
            match &predicates[0] {
                ValidityPredicate::Storage { value, .. } => assert_eq!(*value, expected),
                other => panic!("expected storage, got {other:?}"),
            }
        }
    }
}
