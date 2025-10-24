use anyhow::{anyhow, Result};
use op_succinct_host_utils::fetcher::BlockInfo;
use std::{collections::HashMap, ops::Range};

/// Identifies gaps not covered by the given sub-ranges in the overall range.
///
/// This function takes an overall block range and a list of sorted sub-ranges.
/// It identifies and returns the gaps between these sub-ranges in the overall range.
///
/// # Example
///
/// ```
/// use op_succinct_validity::find_gaps;
///
/// let overall_start = 1;
/// let overall_end = 10;
/// let ranges = [(2, 5), (7, 9)];
///
/// let gaps = find_gaps(overall_start, overall_end, &ranges);
/// assert_eq!(gaps, [1..2, 5..7, 9..10]);
/// ```
pub fn find_gaps(overall_start: i64, overall_end: i64, ranges: &[(i64, i64)]) -> Vec<Range<i64>> {
    let mut gaps = Vec::new();
    let mut current_start = overall_start;

    for &(start, end) in ranges {
        if current_start < start {
            gaps.push(current_start..start);
        }
        current_start = end.max(current_start); // Ensure current_start doesn't move backward
    }

    if current_start < overall_end {
        gaps.push(current_start..overall_end);
    }

    merge_ranges(gaps)
}

/// Determines the block ranges to be proven based on a set of ranges and a gas limit.
///
/// Given a set of block ranges that overlap at most on the boundaries and a gas limit, this
/// function calculates and returns the specific block ranges that need to be proven based on
/// the gas used in each block. Ensures that all disjoint ranges are fully covered by creating
/// ranges that don't exceed the gas limit.
///
/// # Arguments
/// * `disjoint_ranges` - The ranges to split based on gas usage
/// * `gas_limit` - Maximum gas allowed per range
/// * `block_infos` - Map of block number to BlockInfo for all blocks in the ranges
///
/// # Example
///
/// ```ignore
/// use op_succinct_validity::get_ranges_to_prove_by_gas;
/// use std::collections::HashMap;
///
/// let disjoint_ranges = [(0, 4)];
/// let gas_limit = 100_000_000; // 100 million gas
/// let mut block_infos = HashMap::new();
/// // Add BlockInfo for blocks 1-4...
///
/// // If blocks 1-4 use 30M gas each:
/// let ranges_to_prove = get_ranges_to_prove_by_gas(&disjoint_ranges, gas_limit, &block_infos)?;
/// // Result: [(0, 3), (3, 4)] - first 3 blocks (90M gas), then last block (30M gas)
/// ```
pub fn get_ranges_to_prove_by_gas(
    disjoint_ranges: &[Range<i64>],
    evm_gas_limit: u64,
    range_proof_interval: i64,
    block_infos: &HashMap<i64, BlockInfo>,
) -> Result<Vec<Range<i64>>> {
    let mut ranges = Vec::new();

    for &Range { start, end } in disjoint_ranges {
        let mut current_start = start;
        let mut accumulated_gas = 0u64;

        // Process blocks in the range (start+1 to end inclusive)
        // Note: We don't prove the start block, only use its hash as starting point
        for block_num in (start + 1)..=end {
            let block_info = block_infos.get(&(block_num)).ok_or(anyhow!(
                "Missing BlockInfo for block {} in range ({}, {})",
                block_num,
                start,
                end
            ))?;

            // Validate block number consistency
            if block_info.block_number as i64 != block_num {
                return Err(anyhow!(
                    "BlockInfo has inconsistent block number: expected {}, got {}",
                    block_num,
                    block_info.block_number
                ));
            }

            // Check if adding this block would exceed gas limit or block count limit
            if (accumulated_gas > 0 && accumulated_gas + block_info.gas_used > evm_gas_limit) ||
                (range_proof_interval > 0 && block_num - current_start > range_proof_interval)
            {
                // Create a range from current_start to the previous block
                ranges.push(current_start..block_num - 1);
                current_start = block_num - 1;
                accumulated_gas = block_info.gas_used;
            } else {
                accumulated_gas += block_info.gas_used;
            }
        }

        // Handle remaining blocks
        if current_start < end {
            ranges.push(current_start..end);
        }
    }

    // Remove the last range if it's empty
    if let Some(r) = ranges.last() {
        if r.is_empty() {
            ranges.pop();
        }
    }

    Ok(ranges)
}

/// Determines the block ranges to be proven based on a set of ranges and a specified interval.
///
/// Given a set block ranges that overlap at most on the boundaries and a range proof interval, this
/// function calculates and returns the specific block ranges that need to be proven. Ensures that
/// all disjoint ranges are fully covered by conditionally inserting a single range smaller than the
/// range proof interval if necessary.
///
/// # Example
///
/// ```
/// use op_succinct_validity::get_ranges_to_prove_by_blocks;
///
/// let disjoint_ranges = [0..50, 100..200, 200..210];
/// let range_proof_interval = 25;
///
/// let ranges_to_prove = get_ranges_to_prove_by_blocks(&disjoint_ranges, range_proof_interval);
/// assert_eq!(ranges_to_prove, [0..25, 25..50, 100..125, 125..150, 150..175, 175..200]);
/// ```
pub fn get_ranges_to_prove_by_blocks(
    disjoint_ranges: &[Range<i64>],
    range_proof_interval: i64,
) -> Vec<Range<i64>> {
    let mut ranges = Vec::new();

    for &Range { start, end } in disjoint_ranges {
        let mut current_start = start;
        while current_start < end {
            let current_end = std::cmp::min(current_start + range_proof_interval, end);
            ranges.push(current_start..current_end);
            current_start = current_end;
        }
    }

    // For the last range, remove it if it's less than range_proof_interval. This is to ensure when
    // inserting the ranges near the tip, only requests of size range_proof_interval are
    // inserted.
    if let Some(r) = ranges.last() {
        if r.end - r.start < range_proof_interval {
            ranges.pop();
        }
    }

    ranges
}

/// Merges overlapping or contiguous ranges into a single range.
///
/// Takes a vector of ranges and combines any that overlap or touch, returning
/// a sorted vector of non-overlapping ranges.
fn merge_ranges(mut ranges: Vec<Range<i64>>) -> Vec<Range<i64>> {
    if ranges.is_empty() {
        return ranges;
    }

    // Sort by start, then end if starts are equal.
    ranges.sort_unstable_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));

    let mut merged = Vec::with_capacity(ranges.len());
    let mut current = ranges[0].clone();

    for range in ranges.into_iter().skip(1) {
        if range.start <= current.end {
            // Ranges overlap or are contiguous, merge them
            current.end = current.end.max(range.end);
        } else {
            // No overlap, push current and start new range
            merged.push(current);
            current = range;
        }
    }

    merged.push(current);
    merged
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;
    use op_succinct_host_utils::fetcher::BlockInfo;
    use std::collections::HashMap;

    // Tests for find_gaps
    macro_rules! test_find_gaps {
        ($name:ident, $overall_start:expr, $overall_end:expr, $ranges:expr, $expected:expr) => {
            #[test]
            fn $name() {
                let gaps = find_gaps($overall_start, $overall_end, $ranges);
                assert_eq!(gaps, $expected);
            }
        };
    }

    macro_rules! test_merge_ranges {
        ($name:ident, $disjoint_ranges:expr, $expected:expr) => {
            #[test]
            fn $name() {
                let ranges = merge_ranges($disjoint_ranges);
                assert_eq!(ranges, $expected);
            }
        };
    }

    test_find_gaps!(test_find_gaps_no_gaps, 1, 4, &[(1, 2), (2, 3), (3, 4)], &[]);
    test_find_gaps!(
        test_find_gaps_with_gaps,
        1,
        10,
        &[(1, 2), (4, 5), (7, 8)],
        &[2..4, 5..7, 8..10]
    );
    test_find_gaps!(test_find_gaps_at_start, 1, 6, &[(3, 4), (5, 6)], &[1..3, 4..5]);
    test_find_gaps!(test_find_gaps_at_end, 1, 5, &[(1, 2), (2, 3)], &[3..5]);
    test_find_gaps!(test_find_gaps_empty_ranges, 1, 5, &[], &[1..5]);
    test_find_gaps!(test_find_gaps_single_range, 1, 5, &[(2, 4)], &[1..2, 4..5]);

    test_merge_ranges!(test_merge_ranges_contiguous, vec![3..4, 4..7], vec![3..7]);
    test_merge_ranges!(test_merge_ranges_overlap, vec![3..6, 4..7], vec![3..7]);
    test_merge_ranges!(test_merge_ranges_disjoint, vec![3..5, 7..9], vec![3..5, 7..9]);

    // Tests for get_ranges_to_prove_by_blocks
    macro_rules! test_get_ranges_to_prove_by_blocks {
        ($name:ident, $disjoint_ranges:expr, $range_proof_interval:expr, $expected:expr) => {
            #[test]
            fn $name() {
                let result = get_ranges_to_prove_by_blocks($disjoint_ranges, $range_proof_interval);
                assert_eq!(result, $expected);
            }
        };
    }

    test_get_ranges_to_prove_by_blocks!(
        test_get_ranges_to_prove_case_1,
        &[0..50, 100..200, 200..210],
        25,
        &[0..25, 25..50, 100..125, 125..150, 150..175, 175..200]
    );

    test_get_ranges_to_prove_by_blocks!(
        test_get_ranges_to_prove_case_2,
        &[0..30, 40..70],
        10,
        &[0..10, 10..20, 20..30, 40..50, 50..60, 60..70]
    );

    test_get_ranges_to_prove_by_blocks!(
        test_get_ranges_to_prove_case_3,
        &[0..100],
        20,
        &[0..20, 20..40, 40..60, 60..80, 80..100]
    );

    test_get_ranges_to_prove_by_blocks!(
        test_get_ranges_to_prove_case_4,
        &[0..15, 20..35],
        5,
        &[0..5, 5..10, 10..15, 20..25, 25..30, 30..35]
    );

    test_get_ranges_to_prove_by_blocks!(
        test_get_ranges_to_prove_case_5,
        &[0..5, 10..15, 20..25],
        3,
        &[0..3, 3..5, 10..13, 13..15, 20..23]
    );

    test_get_ranges_to_prove_by_blocks!(
        test_get_ranges_to_prove_case_interval_larger_than_range,
        &[0..5, 10..15, 20..25],
        30,
        &[0..5, 10..15]
    );

    // Tests for get_ranges_to_prove_by_gas
    fn create_block_infos(block_data: Vec<(i64, i64)>) -> HashMap<i64, BlockInfo> {
        block_data
            .into_iter()
            .map(|(block_number, gas_used)| {
                (
                    block_number,
                    BlockInfo {
                        block_number: block_number as u64,
                        transaction_count: 10,
                        gas_used: gas_used as u64,
                        total_l1_fees: 1000,
                        total_tx_fees: 2000,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn test_basic_gas_splitting() {
        // Test case: 4 blocks of 30M gas each with 100M limit
        // Expected: First 3 blocks (90M gas), then last block (30M gas)
        let block_infos = create_block_infos(vec![
            (1, 30_000_000),
            (2, 30_000_000),
            (3, 30_000_000),
            (4, 30_000_000),
        ]);

        let result = get_ranges_to_prove_by_gas(&[0..4], 100_000_000, 600, &block_infos).unwrap();
        assert_eq!(result, vec![0..3, 3..4]);
    }

    #[test]
    fn test_varying_gas_amounts() {
        // Test case: blocks with varying gas amounts
        let block_infos = create_block_infos(vec![
            (1, 50_000_000), // 50M
            (2, 20_000_000), // 70M total - still under limit
            (3, 40_000_000), // 110M total - would exceed, so create range (0, 2)
            (4, 10_000_000), // 50M total with block 3
            (5, 60_000_000), // 110M total - would exceed, so create range (2, 4)
        ]);

        let result = get_ranges_to_prove_by_gas(&[0..5], 100_000_000, 600, &block_infos).unwrap();
        assert_eq!(result, vec![0..2, 2..4, 4..5]);
    }

    #[test]
    fn test_exact_gas_limit_match() {
        // Test case: blocks that exactly match gas limit
        let block_infos = create_block_infos(vec![
            (1, 50_000_000),  // 50M
            (2, 50_000_000),  // 100M total - exactly at limit
            (3, 100_000_000), // Would be 200M total - exceeds limit
        ]);

        let result = get_ranges_to_prove_by_gas(&[0..3], 100_000_000, 600, &block_infos).unwrap();
        assert_eq!(result, vec![0..2, 2..3]);
    }

    #[test]
    fn test_multiple_ranges() {
        // Test case: multiple disjoint ranges
        let block_infos = create_block_infos(vec![
            // First range
            (1, 40_000_000), // 40M
            (2, 40_000_000), // 80M total
            (3, 40_000_000), // 120M total - exceeds limit
            // Gap from 4-5
            // Second range
            (6, 60_000_000), // 60M
            (7, 60_000_000), // 120M total - exceeds limit
        ]);

        let result =
            get_ranges_to_prove_by_gas(&[0..3, 5..7], 100_000_000, 600, &block_infos).unwrap();
        assert_eq!(result, vec![0..2, 2..3, 5..6, 6..7]);
    }

    #[test]
    fn test_max_interval_and_some_empty_blocks() {
        // Test case: max interval and some empty blocks
        let block_infos = create_block_infos(vec![
            // First range
            (1, 40_000_000), // 40M
            (2, 40_000_000), // 80M total
            // Second range
            (3, 40_000_000), // 40M
            (4, 0),
            (5, 0),
            // Third range
            (6, 0),
            (7, 60_000_000), // 60M
        ]);

        let result = get_ranges_to_prove_by_gas(&[0..7], 100_000_000, 3, &block_infos).unwrap();
        assert_eq!(result, vec![0..2, 2..5, 5..7]);
    }

    #[test]
    fn test_max_interval_and_small_blocks() {
        // Test case: multiple small blocks
        let block_infos = create_block_infos(vec![
            // First range
            (1, 1_000),
            (2, 1_000),
            (3, 1_000),
            // Second range
            (4, 1_000),
            (5, 1_000),
            (6, 1_000),
            // Third range
            (7, 1_000),
            (8, 1_000),
        ]);

        let result = get_ranges_to_prove_by_gas(&[0..8], 100_000_000, 3, &block_infos).unwrap();
        assert_eq!(result, vec![0..3, 3..6, 6..8]);
    }

    #[test]
    fn test_zero_max_interval() {
        // Test case: multiple small blocks
        let block_infos = create_block_infos(vec![
            // First range
            (1, 40_000),
            (2, 20_000),
            (3, 20_000),
            // Second range
            (4, 40_000),
            (5, 20_000),
            (6, 20_000),
            // Third range
            (7, 40_000),
            (8, 40_000),
        ]);

        let result = get_ranges_to_prove_by_gas(&[0..8], 100_000, 0, &block_infos).unwrap();
        assert_eq!(result, vec![0..3, 3..6, 6..8]);
    }

    #[test]
    fn test_single_large_block() {
        // Test case: single block exceeding gas limit
        let block_infos = create_block_infos(vec![(1, 150_000_000)]);

        let result = get_ranges_to_prove_by_gas(&[0..1], 100_000_000, 600, &block_infos).unwrap();
        assert_eq!(result, vec![0..1]);
    }

    #[test]
    fn test_single_small_block() {
        // Test case: single block not exceeding gas limit
        let block_infos = create_block_infos(vec![(1, 10_000)]);

        let result = get_ranges_to_prove_by_gas(&[0..1], 100_000_000, 600, &block_infos).unwrap();
        assert_eq!(result, vec![0..1]);
    }

    #[test]
    fn test_empty_range() {
        // Test case: empty range (start == end)
        let block_infos = HashMap::new();

        let result = get_ranges_to_prove_by_gas(&[0..0], 100_000_000, 600, &block_infos).unwrap();
        assert_eq!(result, vec![]);
    }

    #[test]
    fn test_missing_block_info_validation() {
        // Test case: missing block info should return error
        let block_infos = create_block_infos(vec![(1, 30_000_000)]); // Missing block 2

        let result = get_ranges_to_prove_by_gas(&[0..2], 100_000_000, 600, &block_infos);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Missing BlockInfo for block 2"));
    }

    #[test]
    fn test_inconsistent_block_number_validation() {
        // Test case: block info with wrong block number should return error
        let mut block_infos = HashMap::new();
        block_infos.insert(
            1,
            BlockInfo {
                block_number: 99, // Wrong block number
                transaction_count: 10,
                gas_used: 30_000_000,
                total_l1_fees: 1000,
                total_tx_fees: 2000,
            },
        );

        let result = get_ranges_to_prove_by_gas(&[0..1], 100_000_000, 600, &block_infos);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("inconsistent block number"));
    }

    // Block-based splitting tests

    #[test]
    fn test_basic_block_splitting() {
        let result = get_ranges_to_prove_by_blocks(&[0..50, 100..200, 200..210], 25);
        assert_eq!(result, vec![0..25, 25..50, 100..125, 125..150, 150..175, 175..200]);
    }

    #[test]
    fn test_block_splitting_exact_intervals() {
        let result = get_ranges_to_prove_by_blocks(&[0..30, 40..70], 10);
        assert_eq!(result, vec![0..10, 10..20, 20..30, 40..50, 50..60, 60..70]);
    }

    #[test]
    fn test_block_splitting_removes_small_last_range() {
        let result = get_ranges_to_prove_by_blocks(&[0..5, 10..15, 20..25], 30);
        assert_eq!(result, vec![0..5, 10..15]);
    }
}
