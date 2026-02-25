#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]

/// Cost per zero byte.
pub const ZERO_BYTE_COST: u64 = 4;

/// Cost per non-zero byte.
pub const NON_ZERO_BYTE_COST: u64 = 16;

/// <https://github.com/ethereum-optimism/op-geth/blob/647c346e2bef36219cc7b47d76b1cb87e7ca29e4/core/types/rollup_cost.go#L79>
pub const L1_COST_FASTLZ_COEF: u64 = 836_500;

/// <https://github.com/ethereum-optimism/op-geth/blob/647c346e2bef36219cc7b47d76b1cb87e7ca29e4/core/types/rollup_cost.go#L78>
/// Inverted to be used with `saturating_sub`.
pub const L1_COST_INTERCEPT: u64 = 42_585_600;

/// <https://github.com/ethereum-optimism/op-geth/blob/647c346e2bef36219cc7b47d76b1cb87e7ca29e4/core/types/rollup_cost.go#82>
pub const MIN_TX_SIZE_SCALED: u64 = 100 * 1_000_000;

/// Calculate the data gas for posting the transaction on L1.
///
/// In fjord, Calldata costs 16 gas per byte after compression.
pub fn data_gas_fjord(input: &[u8]) -> u64 {
    let estimated_size = tx_estimated_size_fjord(input);
    estimated_size.saturating_mul(NON_ZERO_BYTE_COST).wrapping_div(1_000_000)
}

/// Calculate the estimated compressed transaction size in bytes, scaled by 1e6.
/// This value is computed based on the following formula:
/// max(minTransactionSize, intercept + fastlzCoef*fastlzSize)
pub fn tx_estimated_size_fjord(input: &[u8]) -> u64 {
    let fastlz_size = flz_compress_len(input) as u64;

    fastlz_size
        .saturating_mul(L1_COST_FASTLZ_COEF)
        .saturating_sub(L1_COST_INTERCEPT)
        .max(MIN_TX_SIZE_SCALED)
}

/// Calculate the estimated compressed transaction size in bytes.
/// This value is computed based on the following formula:
/// max(minTransactionSize, intercept + fastlzCoef*fastlzSize) / 1e6
pub fn tx_estimated_size_fjord_bytes(input: &[u8]) -> u64 {
    let estimated_size = tx_estimated_size_fjord(input);
    estimated_size.wrapping_div(1_000_000)
}

/// Returns the length of the data after compression through `FastLZ`.
///
/// The u32s match op-geth's Go port.
///
/// <https://github.com/Vectorized/solady/blob/5315d937d79b335c668896d7533ac603adac5315/js/solady.js>
/// <https://github.com/ethereum-optimism/op-geth/blob/647c346e2bef36219cc7b47d76b1cb87e7ca29e4/core/types/rollup_cost.go#L411>
pub fn flz_compress_len(input: &[u8]) -> u32 {
    let mut idx: u32 = 2;

    let idx_limit: u32 = if input.len() < 13 { 0 } else { input.len() as u32 - 13 };

    let mut anchor = 0;

    let mut size = 0;

    let mut htab = [0; 8192];

    while idx < idx_limit {
        let mut r: u32;
        let mut distance: u32;

        loop {
            let seq = u24(input, idx);
            let hash = hash(seq);
            r = htab[hash as usize];
            htab[hash as usize] = idx;
            distance = idx - r;
            if idx >= idx_limit {
                break;
            }
            idx += 1;
            if distance < 8192 && seq == u24(input, r) {
                break;
            }
        }

        if idx >= idx_limit {
            break;
        }

        idx -= 1;

        if idx > anchor {
            size = literals(idx - anchor, size);
        }

        let len = cmp(input, r + 3, idx + 3, idx_limit + 9);
        size = flz_match(len, size);

        idx = set_next_hash(&mut htab, input, idx + len);
        idx = set_next_hash(&mut htab, input, idx);
        anchor = idx;
    }

    literals(input.len() as u32 - anchor, size)
}

const fn literals(r: u32, size: u32) -> u32 {
    let size = size + 0x21 * (r / 0x20);
    let r = r % 0x20;
    if r != 0 { size + r + 1 } else { size }
}

const fn cmp(input: &[u8], p: u32, q: u32, r: u32) -> u32 {
    let mut l = 0;
    let mut r = r - q;
    while l < r {
        if input[(p + l) as usize] != input[(q + l) as usize] {
            r = 0;
        }
        l += 1;
    }
    l
}

const fn flz_match(l: u32, size: u32) -> u32 {
    let l = l - 1;
    let size = size + (3 * (l / 262));
    if l % 262 >= 6 { size + 3 } else { size + 2 }
}

fn set_next_hash(htab: &mut [u32; 8192], input: &[u8], idx: u32) -> u32 {
    htab[hash(u24(input, idx)) as usize] = idx;
    idx + 1
}

const fn hash(v: u32) -> u16 {
    let hash = (v as u64 * 2654435769) >> 19;
    hash as u16 & 0x1fff
}

fn u24(input: &[u8], idx: u32) -> u32 {
    u32::from(input[idx as usize])
        + (u32::from(input[(idx + 1) as usize]) << 8)
        + (u32::from(input[(idx + 2) as usize]) << 16)
}

#[cfg(test)]
mod tests {
    use super::{data_gas_fjord, flz_compress_len, tx_estimated_size_fjord_bytes};

    #[test]
    fn test_flz_compress_len_empty() {
        assert_eq!(flz_compress_len(&[]), 0);
    }

    #[test]
    fn test_flz_compress_len_thousand_zeros() {
        assert_eq!(flz_compress_len(&[0; 1000]), 21);
    }

    #[test]
    fn test_flz_compress_len_thousand_forty_twos() {
        assert_eq!(flz_compress_len(&[42; 1000]), 21);
    }

    #[test]
    fn test_flz_compress_len_short_hex() {
        assert_eq!(flz_compress_len(&[0xFA, 0xCA, 0xDE]), 4);
    }

    #[test]
    fn test_flz_compress_len_no_repeats() {
        let mut input = Vec::new();
        let mut len = 0;

        for i in 0..256 {
            input.push(i as u8);
            let prev_len = len;
            len = flz_compress_len(&input);
            assert!(len > prev_len);
        }
    }

    #[test]
    fn test_tx_estimated_size_fjord_bytes_nonempty() {
        // Any non-empty input should produce a positive estimate
        assert!(tx_estimated_size_fjord_bytes(&[1, 2, 3, 4, 5]) > 0);
    }

    #[test]
    fn test_data_gas_fjord_nonempty() {
        assert!(data_gas_fjord(&[1, 2, 3, 4, 5]) > 0);
    }
}
