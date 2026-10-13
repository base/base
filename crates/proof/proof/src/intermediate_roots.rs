//! Sampling of the intermediate output roots committed by an aggregate proof.

use alloc::vec::Vec;
use core::num::NonZeroU64;

use alloy_primitives::B256;
use thiserror::Error;

/// Error returned by [`IntermediateRootSampler::finish`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum IntermediateRootSamplerError {
    /// No block was derived, so there is no root to commit.
    #[error("cannot sample intermediate roots from an empty block range")]
    EmptyRange,
    /// The derived range does not end on an intermediate checkpoint.
    #[error(
        "{blocks} derived block(s) is not a multiple of the intermediate block interval {interval}"
    )]
    PartialInterval {
        /// Number of blocks fed to the sampler.
        blocks: u64,
        /// Number of blocks between two intermediate roots.
        interval: u64,
    },
}

/// Samples the intermediate output roots of a proven block range.
///
/// Fed the output root of every derived block in order, it keeps the root of every
/// `interval`-th block. The range must span a non-zero multiple of the interval so that the last
/// sampled root is the range's final root, which the `AggregateVerifier` requires to be the claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntermediateRootSampler {
    interval: NonZeroU64,
    blocks: u64,
    roots: Vec<B256>,
}

impl IntermediateRootSampler {
    /// Creates a sampler that keeps the root of every `interval`-th block.
    pub const fn new(interval: NonZeroU64) -> Self {
        Self { interval, blocks: 0, roots: Vec::new() }
    }

    /// Records the output root of the next derived block.
    pub fn push(&mut self, output_root: B256) {
        self.blocks += 1;
        if self.blocks.is_multiple_of(self.interval.get()) {
            self.roots.push(output_root);
        }
    }

    /// Returns the sampled roots, or an error if the range is empty or does not end on an
    /// intermediate checkpoint.
    pub fn finish(self) -> Result<Vec<B256>, IntermediateRootSamplerError> {
        if self.blocks == 0 {
            return Err(IntermediateRootSamplerError::EmptyRange);
        }
        if !self.blocks.is_multiple_of(self.interval.get()) {
            return Err(IntermediateRootSamplerError::PartialInterval {
                blocks: self.blocks,
                interval: self.interval.get(),
            });
        }
        Ok(self.roots)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn root(block: u64) -> B256 {
        B256::left_padding_from(&block.to_be_bytes())
    }

    #[rstest]
    #[case::pre_denim_game(600, 30, Ok(20))]
    #[case::post_denim_game(6000, 300, Ok(20))]
    #[case::challenge_segment(30, 30, Ok(1))]
    #[case::non_dividing_interval(
        6000,
        7,
        Err(IntermediateRootSamplerError::PartialInterval { blocks: 6000, interval: 7 })
    )]
    #[case::empty_range(0, 30, Err(IntermediateRootSamplerError::EmptyRange))]
    fn samples_every_interval_block(
        #[case] blocks: u64,
        #[case] interval: u64,
        #[case] expected: Result<u64, IntermediateRootSamplerError>,
    ) {
        let mut sampler = IntermediateRootSampler::new(NonZeroU64::new(interval).unwrap());
        for block in 1..=blocks {
            sampler.push(root(block));
        }

        let expected =
            expected.map(|count| (1..=count).map(|i| root(i * interval)).collect::<Vec<_>>());
        let roots = sampler.finish();
        assert_eq!(roots, expected);
        if let Ok(roots) = roots {
            assert_eq!(roots.last(), Some(&root(blocks)));
        }
    }
}
