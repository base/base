use alloy_primitives::TxHash;
use serde::Serialize;

/// A block-level budget that a candidate transaction did not fit in.
///
/// Whether a candidate fits depends on what is already in the payload, so each of these stops
/// further inclusion of larger candidates until another transaction is included or the scan
/// ends. Limits intrinsic to one transaction, such as the per-transaction DA size, are not
/// constraints: they reject the same transaction in every payload and are journaled per
/// transaction as `BUILDER_REJECTED`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceConstraint {
    /// The block (or flashblock) gas budget, including EIP-8130 payer authentication gas.
    BlockGas,
    /// The operator-configured block DA byte budget.
    BlockDaSize,
    /// The post-Jovian DA footprint budget.
    BlockDaFootprint,
    /// The cumulative uncompressed (EIP-2718 encoded) block size budget.
    BlockUncompressedSize,
    /// A block-scope resource metering budget.
    ResourceMeteringBlockBudget,
}

impl ResourceConstraint {
    /// Stable snake-case code used in event data and event IDs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BlockGas => "block_gas",
            Self::BlockDaSize => "block_da_size",
            Self::BlockDaFootprint => "block_da_footprint",
            Self::BlockUncompressedSize => "block_uncompressed_size",
            Self::ResourceMeteringBlockBudget => "resource_metering_block_budget",
        }
    }
}

/// One candidate's miss against a [`ResourceConstraint`].
///
/// `limit`, `used`, and `required` share the constraint's unit (gas or bytes). They are `None`
/// when the builder does not expose a single scalar for the constraint, as for resource metering
/// budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceLimitHit {
    /// The constraint that was hit.
    pub constraint: ResourceConstraint,
    /// The budget for this payload or flashblock.
    pub limit: Option<u64>,
    /// The amount of the budget already used when the candidate was checked.
    pub used: Option<u64>,
    /// The amount the candidate needed.
    pub required: Option<u64>,
}

/// Summary of every candidate one [`ResourceConstraint`] rejected between two inclusions.
///
/// This is the `data` of a `BUILDER_RESOURCE_LIMIT_REACHED` event, apart from the builder
/// context fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceLimitReached {
    /// The constraint that rejected the candidates.
    pub constraint: ResourceConstraint,
    /// The budget for this payload or flashblock.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// The amount of the budget already used. Nothing is included during an interval, so this
    /// is the same for every candidate it summarizes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used: Option<u64>,
    /// The smallest amount any rejected candidate needed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_required: Option<u64>,
    /// The largest amount any rejected candidate needed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_required: Option<u64>,
    /// Candidates this constraint rejected during the interval. A candidate re-yielded by a
    /// later scan is counted again in that scan's interval.
    pub rejected_count: u64,
    /// One-based scan ordering position of the first rejected candidate.
    pub first_ordering_position: u64,
    /// One-based scan ordering position of the last rejected candidate.
    pub last_ordering_position: u64,
    /// Number of transactions already in the payload when the interval began.
    pub after_tx_index: u64,
    /// Hash of the last transaction included before the interval, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_tx_hash: Option<TxHash>,
    /// The first [`ResourceLimitRejections::SAMPLE_SIZE`] rejected hashes in scan order.
    pub sampled_tx_hashes: Vec<TxHash>,
}

impl ResourceLimitReached {
    fn new(hit: ResourceLimitHit, tx_hash: TxHash, ordering_position: u64) -> Self {
        Self {
            constraint: hit.constraint,
            limit: hit.limit,
            used: hit.used,
            min_required: hit.required,
            max_required: hit.required,
            rejected_count: 1,
            first_ordering_position: ordering_position,
            last_ordering_position: ordering_position,
            after_tx_index: 0,
            after_tx_hash: None,
            sampled_tx_hashes: vec![tx_hash],
        }
    }

    fn add(&mut self, hit: ResourceLimitHit, tx_hash: TxHash, ordering_position: u64) {
        self.rejected_count += 1;
        self.last_ordering_position = ordering_position;
        self.min_required = Self::merge(self.min_required, hit.required, u64::min);
        self.max_required = Self::merge(self.max_required, hit.required, u64::max);
        if self.sampled_tx_hashes.len() < ResourceLimitRejections::SAMPLE_SIZE {
            self.sampled_tx_hashes.push(tx_hash);
        }
    }

    fn merge(current: Option<u64>, next: Option<u64>, pick: fn(u64, u64) -> u64) -> Option<u64> {
        match (current, next) {
            (Some(current), Some(next)) => Some(pick(current, next)),
            (current, next) => current.or(next),
        }
    }
}

/// Resource-constraint rejections since the last inclusion in the current scan.
///
/// Builders record every candidate a [`ResourceConstraint`] rejects and call
/// [`Self::take`] before journaling the next included transaction and when the scan ends.
/// That journals at most one `BUILDER_RESOURCE_LIMIT_REACHED` per constraint per interval
/// between inclusions, instead of one `BUILDER_REJECTED` per rejected candidate.
#[derive(Debug, Default)]
pub struct ResourceLimitRejections {
    open: Vec<ResourceLimitReached>,
}

impl ResourceLimitRejections {
    /// Rejected hashes kept per constraint per interval, so a lookup for a specific
    /// transaction can sometimes be answered directly at bounded cost.
    pub const SAMPLE_SIZE: usize = 8;

    /// Records that `hit.constraint` rejected `tx_hash` at `ordering_position`.
    pub fn record(&mut self, hit: ResourceLimitHit, tx_hash: TxHash, ordering_position: u64) {
        match self.open.iter_mut().find(|reached| reached.constraint == hit.constraint) {
            Some(reached) => reached.add(hit, tx_hash, ordering_position),
            None => self.open.push(ResourceLimitReached::new(hit, tx_hash, ordering_position)),
        }
    }

    /// Returns `true` when no constraint has rejected a candidate in the current interval.
    pub const fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    /// Closes the current interval and returns one summary per constraint hit during it, in
    /// the order each constraint was first hit.
    ///
    /// `after_tx_index` and `after_tx_hash` describe the payload position the interval
    /// followed: the number of transactions already included and the last one's hash.
    pub fn take(
        &mut self,
        after_tx_index: u64,
        after_tx_hash: Option<TxHash>,
    ) -> Vec<ResourceLimitReached> {
        let mut closed = core::mem::take(&mut self.open);
        for reached in &mut closed {
            reached.after_tx_index = after_tx_index;
            reached.after_tx_hash = after_tx_hash;
        }
        closed
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::*;

    const fn gas_hit(required: u64) -> ResourceLimitHit {
        ResourceLimitHit {
            constraint: ResourceConstraint::BlockGas,
            limit: Some(100_000),
            used: Some(90_000),
            required: Some(required),
        }
    }

    #[test]
    fn rejections_by_one_constraint_between_inclusions_are_summarized_once() {
        let mut rejections = ResourceLimitRejections::default();
        rejections.record(gas_hit(21_000), B256::repeat_byte(1), 4);
        rejections.record(gas_hit(50_000), B256::repeat_byte(2), 5);
        rejections.record(gas_hit(30_000), B256::repeat_byte(3), 7);

        let closed = rejections.take(3, Some(B256::repeat_byte(9)));

        assert_eq!(
            closed,
            [ResourceLimitReached {
                constraint: ResourceConstraint::BlockGas,
                limit: Some(100_000),
                used: Some(90_000),
                min_required: Some(21_000),
                max_required: Some(50_000),
                rejected_count: 3,
                first_ordering_position: 4,
                last_ordering_position: 7,
                after_tx_index: 3,
                after_tx_hash: Some(B256::repeat_byte(9)),
                sampled_tx_hashes: vec![
                    B256::repeat_byte(1),
                    B256::repeat_byte(2),
                    B256::repeat_byte(3)
                ],
            }]
        );
    }

    #[test]
    fn each_constraint_hit_in_an_interval_gets_its_own_summary() {
        let mut rejections = ResourceLimitRejections::default();
        rejections.record(gas_hit(21_000), B256::repeat_byte(1), 1);
        rejections.record(
            ResourceLimitHit {
                constraint: ResourceConstraint::BlockDaSize,
                limit: Some(1_000),
                used: Some(900),
                required: Some(200),
            },
            B256::repeat_byte(2),
            2,
        );
        rejections.record(gas_hit(21_000), B256::repeat_byte(3), 3);

        let constraints: Vec<_> = rejections
            .take(0, None)
            .into_iter()
            .map(|reached| (reached.constraint, reached.rejected_count))
            .collect();

        assert_eq!(
            constraints,
            [(ResourceConstraint::BlockGas, 2), (ResourceConstraint::BlockDaSize, 1)]
        );
    }

    #[test]
    fn an_inclusion_starts_a_new_interval() {
        let mut rejections = ResourceLimitRejections::default();
        rejections.record(gas_hit(21_000), B256::repeat_byte(1), 1);
        assert_eq!(rejections.take(1, None).len(), 1);

        assert!(rejections.take(2, None).is_empty(), "an interval with no rejections is silent");

        rejections.record(gas_hit(21_000), B256::repeat_byte(1), 3);
        let reopened = rejections.take(2, None);
        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened[0].after_tx_index, 2);
        assert_eq!(reopened[0].rejected_count, 1);
    }

    #[test]
    fn the_hash_sample_is_bounded() {
        let mut rejections = ResourceLimitRejections::default();
        for position in 0..100_u8 {
            rejections.record(gas_hit(21_000), B256::repeat_byte(position), position.into());
        }

        let closed = rejections.take(0, None);

        assert_eq!(closed[0].rejected_count, 100);
        assert_eq!(closed[0].sampled_tx_hashes.len(), ResourceLimitRejections::SAMPLE_SIZE);
        assert_eq!(closed[0].sampled_tx_hashes[0], B256::repeat_byte(0));
    }

    #[test]
    fn summaries_serialize_with_stable_constraint_codes() {
        let mut rejections = ResourceLimitRejections::default();
        rejections.record(
            ResourceLimitHit {
                constraint: ResourceConstraint::ResourceMeteringBlockBudget,
                limit: None,
                used: None,
                required: None,
            },
            B256::repeat_byte(1),
            1,
        );

        let data = serde_json::to_value(&rejections.take(0, None)[0]).unwrap();

        assert_eq!(data["constraint"], "resource_metering_block_budget");
        assert_eq!(data["constraint"], ResourceConstraint::ResourceMeteringBlockBudget.as_str());
        assert!(data.get("limit").is_none());
        assert!(data.get("after_tx_hash").is_none());
        assert_eq!(data["rejected_count"], 1);
    }
}
