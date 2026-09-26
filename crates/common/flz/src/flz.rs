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

/// Number of slots in the `FastLZ` hash table (13-bit hash).
const HTAB_LEN: usize = 8192;

/// Inputs up to this length reuse a per-thread hash table instead of zeroing a fresh one.
/// Zeroing dominates for typical (sub-KiB) transactions; above this size it is noise and the
/// stale-slot check in the hot loop costs more than it saves.
#[cfg(feature = "std")]
const REUSED_HTAB_MAX_INPUT_LEN: usize = 32 * 1024;

#[cfg(feature = "std")]
std::thread_local! {
    /// Reusable hash table and the offset ("base") stamped into slots by the current call.
    /// Slots written by earlier calls hold values below `base` and read as empty.
    static REUSED_HTAB: core::cell::RefCell<(u32, std::boxed::Box<[u32; HTAB_LEN]>)> =
        core::cell::RefCell::new((0, std::boxed::Box::new([0; HTAB_LEN])));
}

/// Returns the length of the data after compression through `FastLZ`.
///
/// The u32s match op-geth's Go port.
///
/// <https://github.com/Vectorized/solady/blob/5315d937d79b335c668896d7533ac603adac5315/js/solady.js>
/// <https://github.com/ethereum-optimism/op-geth/blob/647c346e2bef36219cc7b47d76b1cb87e7ca29e4/core/types/rollup_cost.go#L411>
pub fn flz_compress_len(input: &[u8]) -> u32 {
    // Positions are u32 like op-geth's, which only addresses the first `len mod 2^32` bytes.
    // Truncating keeps the unchecked loads below in bounds for inputs of 4 GiB and more.
    let input = &input[..input.len() as u32 as usize];
    #[cfg(feature = "std")]
    if input.len() <= REUSED_HTAB_MAX_INPUT_LEN {
        // Falls back to a fresh table if the thread-local is being destroyed or already borrowed.
        let reused = REUSED_HTAB.try_with(|cell| {
            let (base, htab) = &mut *cell.try_borrow_mut().ok()?;
            // Stamps range over `base..=base + len`; start over from a zeroed table on overflow.
            let len = input.len() as u32;
            if base.checked_add(len + 1).is_none() {
                htab.fill(0);
                *base = 0;
            }
            let size = compress_len(input, htab, *base);
            *base += len + 1;
            Some(size)
        });
        if let Ok(Some(size)) = reused {
            return size;
        }
    }
    compress_len(input, &mut [0; HTAB_LEN], 0)
}

/// `FastLZ` compressed length using `htab`, whose slots hold `base + position`. Slots below
/// `base` are treated as empty (position 0), exactly like a freshly zeroed table.
#[inline(always)]
fn compress_len(input: &[u8], htab: &mut [u32; HTAB_LEN], base: u32) -> u32 {
    let mut idx: u32 = 2;

    let idx_limit: u32 = if input.len() < 13 { 0 } else { input.len() as u32 - 13 };

    let mut anchor = 0;

    let mut size = 0;

    while idx < idx_limit {
        let mut r: u32;
        let mut distance: u32;

        loop {
            let seq = u24(input, idx);
            let hash = hash(seq);
            r = htab[hash as usize].saturating_sub(base);
            htab[hash as usize] = base + idx;
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

        idx = set_next_hash(htab, input, idx + len, base);
        idx = set_next_hash(htab, input, idx, base);
        anchor = idx;
    }

    literals(input.len() as u32 - anchor, size)
}

const fn literals(r: u32, size: u32) -> u32 {
    let size = size + 0x21 * (r / 0x20);
    let r = r % 0x20;
    if r != 0 { size + r + 1 } else { size }
}

/// Number of bytes compared at `p` and `q` up to and including the first mismatch, capped at
/// `r - q`. Compares eight bytes at a time; `r` never exceeds `input.len() - 4`.
fn cmp(input: &[u8], p: u32, q: u32, r: u32) -> u32 {
    let (p, q, n) = (p as usize, q as usize, (r - q) as usize);
    let mut l = 0;
    while l + 8 <= n {
        let diff = u64_at(input, p + l) ^ u64_at(input, q + l);
        if diff != 0 {
            return (l + diff.trailing_zeros() as usize / 8 + 1) as u32;
        }
        l += 8;
    }
    while l < n {
        if input[p + l] != input[q + l] {
            return (l + 1) as u32;
        }
        l += 1;
    }
    n as u32
}

const fn flz_match(l: u32, size: u32) -> u32 {
    let l = l - 1;
    let size = size + (3 * (l / 262));
    if l % 262 >= 6 { size + 3 } else { size + 2 }
}

fn set_next_hash(htab: &mut [u32; HTAB_LEN], input: &[u8], idx: u32, base: u32) -> u32 {
    htab[hash(u24(input, idx)) as usize] = base + idx;
    idx + 1
}

const fn hash(v: u32) -> u16 {
    // The masked bits 19..32 of the product only depend on its low 32 bits.
    (v.wrapping_mul(2654435769) >> 19) as u16 & 0x1fff
}

/// Little-endian 24-bit value at `idx`, read as one unchecked 4-byte load.
///
/// Every call site reads at `idx <= input.len() - 6`: scan positions stay at or below
/// `idx_limit = len - 13`, match candidates `r` are earlier written positions (or 0), and `cmp`
/// stops at `len - 4`, so the two post-match positions are at most `len - 7` and `len - 6`.
fn u24(input: &[u8], idx: u32) -> u32 {
    let idx = idx as usize;
    debug_assert!(idx + 4 <= input.len());
    // SAFETY: `idx + 4 <= input.len()` at every call site (see above); the read is unaligned.
    let word = unsafe { input.as_ptr().add(idx).cast::<u32>().read_unaligned() };
    u32::from_le(word) & 0x00ff_ffff
}

/// Little-endian 8 bytes at `idx`, read as one unchecked load. `cmp` only reads 8-byte words that
/// end at or before its bound, which never exceeds `input.len() - 4`.
fn u64_at(input: &[u8], idx: usize) -> u64 {
    debug_assert!(idx + 8 <= input.len());
    // SAFETY: `idx + 8 <= input.len()` at every call site (see above); the read is unaligned.
    let word = unsafe { input.as_ptr().add(idx).cast::<u64>().read_unaligned() };
    u64::from_le(word)
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;
    use proptest::prelude::*;
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

    #[rstest]
    #[case(&[1, 2, 3, 4, 5])]
    #[case(&[0xFA, 0xCA, 0xDE])]
    fn test_fjord_estimates_nonzero(#[case] input: &[u8]) {
        assert!(tx_estimated_size_fjord_bytes(input) > 0);
        assert!(data_gas_fjord(input) > 0);
    }

    /// The straightforward byte-at-a-time port (op-geth / solady) with a fresh table per call.
    fn reference_compress_len(input: &[u8]) -> u32 {
        let byte_u24 = |i: u32| {
            let i = i as usize;
            u32::from(input[i]) | u32::from(input[i + 1]) << 8 | u32::from(input[i + 2]) << 16
        };
        let mut htab = [0u32; HTAB_LEN];
        let mut idx: u32 = 2;
        let idx_limit = if input.len() < 13 { 0 } else { input.len() as u32 - 13 };
        let (mut anchor, mut size) = (0, 0);
        while idx < idx_limit {
            let mut r: u32;
            loop {
                let seq = byte_u24(idx);
                let h = hash(seq) as usize;
                r = htab[h];
                htab[h] = idx;
                let distance = idx - r;
                if idx >= idx_limit {
                    break;
                }
                idx += 1;
                if distance < 8192 && seq == byte_u24(r) {
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
            let (p, q) = (r + 3, idx + 3);
            let mut len = 0;
            while len < idx_limit + 9 - q {
                len += 1;
                if input[(p + len - 1) as usize] != input[(q + len - 1) as usize] {
                    break;
                }
            }
            size = flz_match(len, size);
            idx += len;
            for _ in 0..2 {
                htab[hash(byte_u24(idx)) as usize] = idx;
                idx += 1;
            }
            anchor = idx;
        }
        literals(input.len() as u32 - anchor, size)
    }

    /// Deterministic inputs mixing random bytes, zero runs, and repeated fragments, so both
    /// literal and match paths (including long word-compared matches) are exercised.
    fn mixed_input(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed | 1;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let run = (next() % 80) as usize + 1;
            match next() % 3 {
                0 => out.extend((0..run).map(|_| next() as u8)),
                1 => out.extend(core::iter::repeat_n(0, run)),
                _ if out.len() > run => {
                    let start = (next() as usize) % (out.len() - run);
                    out.extend_from_within(start..start + run);
                }
                _ => out.push(next() as u8),
            }
        }
        out.truncate(len);
        out
    }

    /// Builds an input from segments of random bytes, byte runs, and (possibly overlapping)
    /// copies of earlier output, which drive both literal and match paths.
    fn build_input(segments: &[(u8, usize, u64)]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        for &(kind, len, seed) in segments {
            let mut state = seed | 1;
            match kind {
                0 => out.extend((0..len).map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })),
                1 => out
                    .extend(core::iter::repeat_n(if seed % 4 == 0 { 0 } else { seed as u8 }, len)),
                _ if !out.is_empty() => {
                    let start = seed as usize % out.len();
                    for i in 0..len {
                        out.push(out[start + i]);
                    }
                }
                _ => {}
            }
        }
        out
    }

    proptest! {
        /// A short sequence of inputs per case also exercises slots left by earlier calls.
        #[test]
        fn prop_flz_compress_len_matches_reference(
            inputs in prop::collection::vec(
                prop::collection::vec((0u8..3, 1usize..400, any::<u64>()), 0..120)
                    .prop_map(|segments| build_input(&segments)),
                1..4,
            )
        ) {
            for input in &inputs {
                prop_assert_eq!(flz_compress_len(input), reference_compress_len(input));
            }
        }
    }

    #[test]
    fn test_flz_compress_len_matches_reference() {
        // Sequential calls on one thread also exercise stale slots left by earlier inputs.
        let lens = (0..600).chain([4095, 8191, 8192, 8193, 32 * 1024, 32 * 1024 + 1, 70_000]);
        for (seed, len) in lens.enumerate() {
            let input = mixed_input(seed as u64, len);
            assert_eq!(flz_compress_len(&input), reference_compress_len(&input), "len {len}");
        }
        for len in [1000, 20_000, 131_072] {
            let input = vec![7; len];
            assert_eq!(flz_compress_len(&input), reference_compress_len(&input), "len {len}");
        }
    }

    #[test]
    #[cfg(feature = "std")]
    fn test_flz_compress_len_reused_table_overflow() {
        let input = mixed_input(42, 2000);
        let expected = reference_compress_len(&input);
        REUSED_HTAB.with_borrow_mut(|(base, htab)| {
            htab.fill(u32::MAX - 10);
            *base = u32::MAX - 1000;
        });
        assert_eq!(flz_compress_len(&input), expected);
        assert_eq!(flz_compress_len(&input), expected);
    }

    #[test]
    #[cfg(feature = "std")]
    fn test_flz_compress_len_falls_back_while_table_borrowed() {
        let input = mixed_input(7, 2000);
        let expected = reference_compress_len(&input);
        REUSED_HTAB.with_borrow(|_| assert_eq!(flz_compress_len(&input), expected));
    }
}
