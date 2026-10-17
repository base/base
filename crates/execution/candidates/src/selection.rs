//! Score-read context v1 results and winner choice (design §3.4, §5.4).

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::SolCall;

alloy_sol_types::sol! {
    /// ERC-20 balance read used by score-read context v1.
    function balanceOf(address account) external view returns (uint256);
}

/// Default probe gas cap `G_probe` (design proposes 100k; TBD from measurement).
pub const DEFAULT_PROBE_GAS: u64 = 100_000;

/// Why a sibling is ineligible in one round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ineligibility {
    /// Static, DA or gas limits do not fit this block.
    BlockLimits,
    /// Simulated resource admission excluded it.
    SimulatedResource {
        /// Whether the exclusion is transaction-scope permanent.
        permanent: bool,
    },
    /// Executed resource admission excluded it.
    ExecutedResource {
        /// Whether the exclusion is transaction-scope permanent.
        permanent: bool,
    },
    /// Pre-EVM transaction validation failed.
    Invalid,
    /// The trial reverted.
    Reverted,
    /// The trial halted.
    Halted,
    /// A balance probe reverted.
    ScoreReverted,
    /// A balance probe halted, including out of gas.
    ScoreHalted,
    /// A balance probe did not return exactly 32 bytes.
    ScoreMalformed,
    /// The recipient's balance decreased by this magnitude.
    NegativeDelta(U256),
    /// The delta is below the signed `minDelta`.
    BelowMinDelta,
    /// A previous round recorded a transaction-scope permanent exclusion.
    PermanentlyExcluded,
    /// The round stopped before this sibling ran (cancellation or deadline).
    NotRun,
    /// Executed, but the round stopped before its score was read. Its work is charged; it never
    /// wins.
    Interrupted,
    /// An enforced resource-metering dimension prices operations only a `meterBundle` sample
    /// reports, and the sibling has none (design §5.2 "Missing metering"). Clears when a sample
    /// arrives.
    Unmetered,
}

impl Ineligibility {
    /// Returns a stable metric label (`candidates_sibling_excluded_total{reason}`).
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::BlockLimits => "static",
            Self::SimulatedResource { .. } => "simulated_resource",
            Self::ExecutedResource { .. } => "executed_resource",
            Self::Invalid => "invalid",
            Self::Reverted => "reverted",
            Self::Halted => "halted",
            Self::ScoreReverted => "score_reverted",
            Self::ScoreHalted => "score_halted",
            Self::ScoreMalformed => "score_malformed",
            Self::NegativeDelta(_) => "negative_delta",
            Self::BelowMinDelta => "below_min_delta",
            Self::PermanentlyExcluded => "permanently_excluded",
            Self::NotRun => "not_run",
            Self::Interrupted => "interrupted",
            Self::Unmetered => "unmetered",
        }
    }

    /// Returns `true` for exclusions that can never clear for this sibling.
    pub const fn is_permanent(self) -> bool {
        matches!(
            self,
            Self::SimulatedResource { permanent: true }
                | Self::ExecutedResource { permanent: true }
                | Self::PermanentlyExcluded
        )
    }

    /// Returns `true` for exclusions caused by this block's remaining capacity only.
    pub const fn is_block_scope(self) -> bool {
        matches!(
            self,
            Self::BlockLimits
                | Self::SimulatedResource { permanent: false }
                | Self::ExecutedResource { permanent: false }
        )
    }
}

/// Result of one sibling's trial in a round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrialOutcome {
    /// Eligible with its score and tie-break gas.
    Eligible {
        /// Recipient balance increase.
        delta: U256,
        /// The builder's transaction gas-used measure.
        gas_used: u64,
    },
    /// Ineligible this round.
    Ineligible(Ineligibility),
}

/// Raw result of one balance probe's top-level call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    /// The call returned.
    Success(Bytes),
    /// The call reverted.
    Revert,
    /// The call halted, including out of gas.
    Halt,
}

/// Score-read context v1 helpers.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScoreRead;

impl ScoreRead {
    /// Returns the `balanceOf(recipient)` calldata.
    pub fn balance_of_calldata(recipient: Address) -> Bytes {
        balanceOfCall { account: recipient }.abi_encode().into()
    }

    /// Decodes a probe: success with exactly 32 bytes is a big-endian `uint256`.
    pub fn decode_balance(result: &ProbeResult) -> Result<U256, Ineligibility> {
        match result {
            ProbeResult::Success(output) if output.len() == 32 => {
                Ok(U256::from_be_slice(output.as_ref()))
            }
            ProbeResult::Success(_) => Err(Ineligibility::ScoreMalformed),
            ProbeResult::Revert => Err(Ineligibility::ScoreReverted),
            ProbeResult::Halt => Err(Ineligibility::ScoreHalted),
        }
    }

    /// Computes the eligible delta without signed overflow.
    ///
    /// Negative differences are ineligible and record their magnitude; nonnegative differences
    /// use checked `uint256` subtraction and must reach `min_delta`.
    pub fn delta(before: U256, after: U256, min_delta: U256) -> Result<U256, Ineligibility> {
        let Some(delta) = after.checked_sub(before) else {
            return Err(Ineligibility::NegativeDelta(before - after));
        };
        if delta < min_delta {
            return Err(Ineligibility::BelowMinDelta);
        }
        Ok(delta)
    }
}

/// Winner choice among a round's trials.
#[derive(Debug, Clone, Copy, Default)]
pub struct WinnerSelection;

impl WinnerSelection {
    /// Returns the winner: highest delta, then lowest gas used, then lowest index.
    pub fn choose(trials: &[TrialOutcome]) -> Option<usize> {
        let mut best: Option<(usize, U256, u64)> = None;
        for (index, trial) in trials.iter().enumerate() {
            let TrialOutcome::Eligible { delta, gas_used } = *trial else { continue };
            let better = best.is_none_or(|(_, best_delta, best_gas)| {
                delta > best_delta || (delta == best_delta && gas_used < best_gas)
            });
            if better {
                best = Some((index, delta, gas_used));
            }
        }
        best.map(|(index, _, _)| index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eligible(delta: u64, gas_used: u64) -> TrialOutcome {
        TrialOutcome::Eligible { delta: U256::from(delta), gas_used }
    }

    #[test]
    fn winner_prefers_delta_then_gas_then_index() {
        let ineligible = TrialOutcome::Ineligible(Ineligibility::Reverted);
        assert_eq!(WinnerSelection::choose(&[eligible(5, 10), eligible(7, 99)]), Some(1));
        assert_eq!(WinnerSelection::choose(&[eligible(7, 50), eligible(7, 40)]), Some(1));
        assert_eq!(WinnerSelection::choose(&[eligible(7, 40), eligible(7, 40)]), Some(0));
        assert_eq!(WinnerSelection::choose(&[ineligible, eligible(0, 1)]), Some(1));
        assert_eq!(WinnerSelection::choose(&[ineligible, ineligible]), None);
    }

    #[test]
    fn delta_handles_uint256_boundaries() {
        assert_eq!(ScoreRead::delta(U256::ZERO, U256::MAX, U256::ZERO), Ok(U256::MAX));
        assert_eq!(
            ScoreRead::delta(U256::MAX, U256::ZERO, U256::ZERO),
            Err(Ineligibility::NegativeDelta(U256::MAX))
        );
        assert_eq!(ScoreRead::delta(U256::from(3), U256::from(3), U256::ZERO), Ok(U256::ZERO));
        assert_eq!(
            ScoreRead::delta(U256::from(3), U256::from(4), U256::from(2)),
            Err(Ineligibility::BelowMinDelta)
        );
        assert_eq!(
            ScoreRead::delta(U256::from(3), U256::from(5), U256::from(2)),
            Ok(U256::from(2))
        );
    }

    #[test]
    fn probe_decoding_classifies_failures() {
        let word = Bytes::from(U256::from(42).to_be_bytes::<32>().to_vec());
        assert_eq!(ScoreRead::decode_balance(&ProbeResult::Success(word)), Ok(U256::from(42)));
        assert_eq!(
            ScoreRead::decode_balance(&ProbeResult::Success(Bytes::from(vec![0; 31]))),
            Err(Ineligibility::ScoreMalformed)
        );
        assert_eq!(
            ScoreRead::decode_balance(&ProbeResult::Success(Bytes::from(vec![0; 64]))),
            Err(Ineligibility::ScoreMalformed)
        );
        assert_eq!(
            ScoreRead::decode_balance(&ProbeResult::Revert),
            Err(Ineligibility::ScoreReverted)
        );
        assert_eq!(ScoreRead::decode_balance(&ProbeResult::Halt), Err(Ineligibility::ScoreHalted));
    }

    #[test]
    fn balance_of_calldata_matches_selector() {
        let calldata = ScoreRead::balance_of_calldata(Address::repeat_byte(0xaa));
        assert_eq!(&calldata[..4], &[0x70, 0xa0, 0x82, 0x31]);
        assert_eq!(&calldata[16..], Address::repeat_byte(0xaa).as_slice());
    }
}
