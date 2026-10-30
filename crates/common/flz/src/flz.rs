#[cfg(feature = "std")]
use std::cell::RefCell;

/// Cost per zero byte.
pub const ZERO_BYTE_COST: u64 = 4;

/// Cost per non-zero byte.
pub const NON_ZERO_BYTE_COST: u64 = 16;

/// <https://github.com/ethereum-optimism/op-geth/blob/647c346e2bef36219cc7b47d76b1cb87e7ca29e4/core/types/rollup_cost.go#L79>
pub const L1_COST_FASTLZ_COEF: u64 = 836_500;

/// <https://github.com/ethereum-optimism/op-geth/blob/647c346e2bef36219cc7b47d76b1cb87e7ca29e4/core/types/rollup_cost.go#L78>
/// Inverted to be used with `saturating_sub`.
pub const L1_COST_INTERCEPT: u64 = 42_585_600;

/// <https://github.com/ethereum-optimism/op-geth/blob/647c346e2bef36219cc7b47d76b1cb87e7ca29e4/core/types/rollup_cost.go#L82>
pub const MIN_TX_SIZE_SCALED: u64 = 100 * 1_000_000;

/// Number of `FastLZ` hash-table slots, one per 13-bit hash of a 3-byte sequence.
const HTAB_SLOTS: usize = 8192;

#[cfg(feature = "std")]
std::thread_local! {
    /// Per-thread `FastLZ` hash table, reused across calls so that a call costs time
    /// proportional to its input rather than zeroing a 32 KiB table first.
    ///
    /// The `u32` counts calls (the epoch). Each slot packs the epoch that wrote it in its
    /// high 32 bits and the input position in its low 32 bits, so a slot written by an
    /// earlier call reads as position 0, exactly like the zeroed table op-geth starts with.
    static HTAB: RefCell<(u32, [u64; HTAB_SLOTS])> =
        const { RefCell::new((0, [0; HTAB_SLOTS])) };
}

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
    tx_estimated_size_fjord_from_fastlz_size(flz_compress_len(input))
}

/// [`tx_estimated_size_fjord`] for an input whose [`flz_compress_len`] is `fastlz_size`.
pub fn tx_estimated_size_fjord_from_fastlz_size(fastlz_size: u32) -> u64 {
    (fastlz_size as u64)
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
    #[cfg(feature = "std")]
    {
        HTAB.with_borrow_mut(|(epoch, slots)| {
            *epoch = epoch.wrapping_add(1);
            if *epoch == 0 {
                // Slots written 2^32 calls ago carry the reused epoch values, so clear them.
                *slots = [0; HTAB_SLOTS];
                *epoch = 1;
            }
            let epoch = *epoch;
            compress_len(input, |hash, idx| {
                let slot = u64::from(epoch) << 32 | u64::from(idx);
                let prev = core::mem::replace(&mut slots[hash as usize], slot);
                if (prev >> 32) as u32 == epoch { prev as u32 } else { 0 }
            })
        })
    }
    #[cfg(not(feature = "std"))]
    {
        let mut slots = [0; HTAB_SLOTS];
        compress_len(input, |hash, idx| core::mem::replace(&mut slots[hash as usize], idx))
    }
}

/// Runs `FastLZ` over a hash table accessed only through `replace`, which stores `idx` in
/// the slot for `hash` and returns the position the slot held before (0 if unwritten).
#[inline(always)]
fn compress_len(input: &[u8], mut replace: impl FnMut(u16, u32) -> u32) -> u32 {
    let mut idx: u32 = 2;

    let idx_limit: u32 = if input.len() < 13 { 0 } else { input.len() as u32 - 13 };

    let mut anchor = 0;

    let mut size = 0;

    while idx < idx_limit {
        let mut r: u32;
        let mut distance: u32;

        loop {
            let seq = u24(input, idx);
            r = replace(hash(seq), idx);
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

        idx = set_next_hash(&mut replace, input, idx + len);
        idx = set_next_hash(&mut replace, input, idx);
        anchor = idx;
    }

    literals(input.len() as u32 - anchor, size)
}

const fn literals(r: u32, size: u32) -> u32 {
    let size = size + 0x21 * (r / 0x20);
    let r = r % 0x20;
    if r != 0 { size + r + 1 } else { size }
}

/// Returns one past the offset of the first mismatch between `input[p..]` and `input[q..r]`,
/// or `r - q` if they match throughout.
fn cmp(input: &[u8], p: u32, q: u32, r: u32) -> u32 {
    let (p, q, r) = (p as usize, q as usize, r as usize);
    input[q..r].iter().zip(&input[p..]).position(|(a, b)| a != b).map_or(r - q, |l| l + 1) as u32
}

const fn flz_match(l: u32, size: u32) -> u32 {
    let l = l - 1;
    let size = size + (3 * (l / 262));
    if l % 262 >= 6 { size + 3 } else { size + 2 }
}

fn set_next_hash(replace: &mut impl FnMut(u16, u32) -> u32, input: &[u8], idx: u32) -> u32 {
    replace(hash(u24(input, idx)), idx);
    idx + 1
}

const fn hash(v: u32) -> u16 {
    let hash = (v as u64 * 2654435769) >> 19;
    hash as u16 & 0x1fff
}

fn u24(input: &[u8], idx: u32) -> u32 {
    // One range check instead of three per-byte bounds checks.
    let [b0, b1, b2]: [u8; 3] = input[idx as usize..idx as usize + 3].try_into().unwrap();
    u32::from(b0) + (u32::from(b1) << 8) + (u32::from(b2) << 16)
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::empty(&[], 0)]
    #[case::thousand_zeros(&[0; 1000], 21)]
    #[case::thousand_forty_twos(&[42; 1000], 21)]
    #[case::short_hex(&hex!("FACADE"), 4)]
    #[case::sample_contract_call(&hex!("02f901550a758302df1483be21b88304743f94f80e51afb613d764fa61751affd3313c190a86bb870151bd62fd12adb8e41ef24f3f000000000000000000000000000000000000000000000000000000000000006e000000000000000000000000af88d065e77c8cc2239327c5edb3a432268e5831000000000000000000000000000000000000000000000000000000000003c1e5000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000148c89ed219d02f1a5be012c689b4f5b731827bebe000000000000000000000000c001a033fd89cb37c31b2cba46b6466e040c61fc9b2a3675a7f5f493ebd5ad77c497f8a07cdf65680e238392693019b4092f610222e71b7cec06449cb922b93b6a12744e"), 202)]
    #[case::base_0x5dadeb(&hex!("b9047c02f904788221050883036ee48409c6c87383037f6f941195cf65f83b3a5768f3c496d3a05ad6412c64b78644364c5bb000b90404d123b4d80000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000038000000000000000000000000000000000f6476f90447748c19248ccaa31e6b8bfda4eb9d830f5f47df7f0998f7c2123d9e6137761b75d3184efb0f788e3b14516000000000000000000000000000000000000000000000000000044364c5bb000000000000000000000000000f38e53bd45c8225a7c94b513beadaa7afe5d222d0000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000024000000000000000000000000000000000000000000000000000000000000002a000000000000000000000000000000000000000000000000000000000000002c000000000000000000000000000000000000000000000000000000000000002e0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000030000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000084d6574614d61736b0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000035697066733a2f2f516d656852577a743347745961776343347564745657557233454c587261436746434259416b66507331696f48610000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000cd0d83d9e840f8e27d5c2e365fd365ff1c05b2480000000000000000000000000000000000000000000000000000000000000ce40000000000000000000000000000000000000000000000000000000000000041e4480d358dbae20880960a0a464d63b06565a0c9f9b1b37aa94b522247b23ce149c81359bf4239d1a879eeb41047ec710c15f5c0f67453da59a383e6abd742971c00000000000000000000000000000000000000000000000000000000000000c001a0b57f0ff8516ea29cb26a44ac5055a5420847d1e16a8e7b03b70f0c02291ff2d5a00ad3771e5f39ccacfff0faa8c5d25ef7a1c179f79e66e828ffddcb994c8b512e"), 471)]
    fn test_flz_compress_len(#[case] input: &[u8], #[case] expected: u32) {
        assert_eq!(flz_compress_len(input), expected);
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

    /// The original port of op-geth's `FlzCompressLen`, which zeroes a fresh table on every
    /// call and bounds-checks each byte, kept verbatim as the reference for equivalence.
    fn reference_flz_compress_len(input: &[u8]) -> u32 {
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

        fn set_next_hash(htab: &mut [u32; 8192], input: &[u8], idx: u32) -> u32 {
            htab[hash(u24(input, idx)) as usize] = idx;
            idx + 1
        }

        fn u24(input: &[u8], idx: u32) -> u32 {
            u32::from(input[idx as usize])
                + (u32::from(input[(idx + 1) as usize]) << 8)
                + (u32::from(input[(idx + 2) as usize]) << 16)
        }

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

    /// Consensus requires bit-identical output to op-geth, so compare against the reference
    /// over many inputs on one thread, which also exercises reuse of the per-thread table
    /// across calls of different lengths and contents.
    #[test]
    fn test_flz_compress_len_matches_reference() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut check = |len: usize| {
            // Small alphabets and zero runs produce matches; 256 symbols mostly literals.
            let alphabet = [1, 2, 4, 16, 256][(next() % 5) as usize];
            let zero_bias = next() % 4;
            let input: Vec<u8> = (0..len)
                .map(|_| {
                    let x = next();
                    if x % 4 < zero_bias { 0 } else { ((x >> 8) % alphabet) as u8 }
                })
                .collect();
            assert_eq!(flz_compress_len(&input), reference_flz_compress_len(&input), "len {len}");
        };
        for len in 0..=600 {
            for _ in 0..16 {
                check(len);
            }
        }
        // Lengths around the 8192-byte match window, and beyond a 16-bit position.
        for len in [8191, 8192, 8193, 8205, 8206, 8207, 20_000, 70_000] {
            for _ in 0..4 {
                check(len);
            }
        }
    }

    #[rstest]
    #[case(&[1, 2, 3, 4, 5])]
    #[case(&[0xFA, 0xCA, 0xDE])]
    fn test_fjord_estimates_nonzero(#[case] input: &[u8]) {
        assert!(tx_estimated_size_fjord_bytes(input) > 0);
        assert!(data_gas_fjord(input) > 0);
    }
}
