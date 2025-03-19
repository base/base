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
/// assert_eq!(gaps, [(1, 2), (5, 7), (9, 10)]);
/// ```
///
pub fn find_gaps(overall_start: i64, overall_end: i64, ranges: &[(i64, i64)]) -> Vec<(i64, i64)> {
    let mut gaps = Vec::new();
    let mut current_start = overall_start;

    for &(start, end) in ranges {
        if current_start < start {
            gaps.push((current_start, start));
        }
        current_start = end.max(current_start); // Ensure current_start doesn't move backward
    }

    if current_start < overall_end {
        gaps.push((current_start, overall_end));
    }

    gaps
}

/// Determines the block ranges to be proven based on disjoint ranges and a specified interval.
///
/// Given a set of disjoint block ranges and a range proof interval, this function calculates
/// and returns the specific block ranges that need to be proven. Ensures that all disjoint ranges
/// are fully covered by conditionally inserting a single range smaller than the range proof interval
/// if necessary.
///
/// # Example
///
/// ```
/// use op_succinct_validity::get_ranges_to_prove;
///
/// let disjoint_ranges = [(0, 50), (100, 200), (200, 210)];
/// let range_proof_interval = 25;
///
/// let ranges_to_prove = get_ranges_to_prove(&disjoint_ranges, range_proof_interval);
/// assert_eq!(ranges_to_prove, [(0, 25), (25, 50), (100, 125), (125, 150), (150, 175), (175, 200), (200, 210)]);
/// ```
///
pub fn get_ranges_to_prove(
    disjoint_ranges: &[(i64, i64)],
    range_proof_interval: i64,
) -> Vec<(i64, i64)> {
    let mut ranges = Vec::new();

    for &(start, end) in disjoint_ranges {
        let mut current_start = start;
        while current_start < end {
            let current_end = std::cmp::min(current_start + range_proof_interval, end);
            ranges.push((current_start, current_end));
            current_start = current_end;
        }
    }

    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! test_find_gaps {
        ($name:ident, $overall_start:expr, $overall_end:expr, $ranges:expr, $expected:expr) => {
            #[test]
            fn $name() {
                let gaps = find_gaps($overall_start, $overall_end, $ranges);
                assert_eq!(gaps, $expected);
            }
        };
    }

    test_find_gaps!(test_find_gaps_no_gaps, 1, 4, &[(1, 2), (2, 3), (3, 4)], &[]);
    test_find_gaps!(
        test_find_gaps_with_gaps,
        1,
        10,
        &[(1, 2), (4, 5), (7, 8)],
        &[(2, 4), (5, 7), (8, 10)]
    );
    test_find_gaps!(
        test_find_gaps_at_start,
        1,
        6,
        &[(3, 4), (5, 6)],
        &[(1, 3), (4, 5)]
    );
    test_find_gaps!(test_find_gaps_at_end, 1, 5, &[(1, 2), (2, 3)], &[(3, 5)]);
    test_find_gaps!(test_find_gaps_empty_ranges, 1, 5, &[], &[(1, 5)]);
    test_find_gaps!(
        test_find_gaps_single_range,
        1,
        5,
        &[(2, 4)],
        &[(1, 2), (4, 5)]
    );

    macro_rules! test_get_ranges_to_prove {
        ($name:ident, $disjoint_ranges:expr, $range_proof_interval:expr, $expected:expr) => {
            #[test]
            fn $name() {
                let result = get_ranges_to_prove($disjoint_ranges, $range_proof_interval);
                assert_eq!(result, $expected);
            }
        };
    }

    test_get_ranges_to_prove!(
        test_get_ranges_to_prove_case_1,
        &[(0, 50), (100, 200), (200, 210)],
        25,
        &[
            (0, 25),
            (25, 50),
            (100, 125),
            (125, 150),
            (150, 175),
            (175, 200),
            (200, 210)
        ]
    );

    test_get_ranges_to_prove!(
        test_get_ranges_to_prove_case_2,
        &[(0, 30), (40, 70)],
        10,
        &[(0, 10), (10, 20), (20, 30), (40, 50), (50, 60), (60, 70)]
    );

    test_get_ranges_to_prove!(
        test_get_ranges_to_prove_case_3,
        &[(0, 100)],
        20,
        &[(0, 20), (20, 40), (40, 60), (60, 80), (80, 100)]
    );

    test_get_ranges_to_prove!(
        test_get_ranges_to_prove_case_4,
        &[(0, 15), (20, 35)],
        5,
        &[(0, 5), (5, 10), (10, 15), (20, 25), (25, 30), (30, 35)]
    );

    test_get_ranges_to_prove!(
        test_get_ranges_to_prove_case_5,
        &[(0, 5), (10, 15), (20, 25)],
        3,
        &[(0, 3), (3, 5), (10, 13), (13, 15), (20, 23), (23, 25)]
    );

    test_get_ranges_to_prove!(
        test_get_ranges_to_prove_case_interval_larger_than_range,
        &[(0, 5), (10, 15), (20, 25)],
        30,
        &[(0, 5), (10, 15), (20, 25)]
    );
}
