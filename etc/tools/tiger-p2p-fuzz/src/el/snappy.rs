//! Snappy target: tiger's decoder against the `snap` crate as base-reth's `P2PStream` runs it
//! (declared length checked against 16 `MiB`, then decompressed into a buffer of exactly that
//! length), plus each side's compressor read back by the other.

use alloy_primitives::{hex, keccak256};

use super::{
    decode::{Seed, Verdict, byte_mutate},
    rng::Rng,
};

/// Largest uncompressed message on both sides (tiger's `MAX_MESSAGE_SIZE`, reth's
/// `MAX_PAYLOAD_SIZE`).
pub(crate) const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

fn digest(data: &[u8]) -> String {
    if data.len() <= 64 {
        hex::encode(data)
    } else {
        format!("len={} keccak={}", data.len(), keccak256(data))
    }
}

/// Plaintext with repeats, so the compressors emit copies as well as literals.
pub(crate) fn plaintext(rng: &mut Rng) -> Vec<u8> {
    let len = match rng.below(5) {
        0 => rng.below(4),
        1 => rng.below(70),
        2 => rng.below(600),
        3 => 60_000 + rng.below(10_000),
        _ => rng.below(5_000),
    };
    let alphabet = 1 + rng.below(8) as u8;
    (0..len)
        .map(|i| if rng.one_in(5) { rng.next_u64() as u8 } else { b"tiger"[i % 5] % alphabet })
        .collect()
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn read_varint(input: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, b) in input.iter().enumerate().take(10) {
        value |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

pub(crate) fn generate(rng: &mut Rng) -> Seed {
    let plain = plaintext(rng);
    match rng.below(3) {
        0 => Seed::raw(
            "decompress/snap-encoded",
            snap::raw::Encoder::new().compress_vec(&plain).unwrap(),
        ),
        1 => Seed::raw("decompress/tiger-encoded", base_tiger_net::snappy_compress(&plain)),
        _ => Seed::raw("roundtrip", plain),
    }
}

/// Snappy-specific mutations on top of the byte-level ones: the declared length, oversize
/// declarations, overlong output, bad copies.
pub(crate) fn mutate(rng: &mut Rng, seed: &Seed) -> (Vec<u8>, String) {
    if seed.case == "roundtrip" {
        return byte_mutate(rng, &seed.bytes);
    }
    let input = &seed.bytes;
    let Some((len, used)) = read_varint(input) else { return byte_mutate(rng, input) };
    let body = &input[used..];
    let with_len = |declared: u64, body: &[u8]| {
        let mut out = Vec::new();
        write_varint(&mut out, declared);
        out.extend_from_slice(body);
        out
    };
    let (out, name) = match rng.below(10) {
        0 => (with_len(len + 1 + rng.below(3) as u64, body), "declared-longer"),
        1 => (with_len(len.saturating_sub(1 + rng.below(3) as u64), body), "declared-shorter"),
        2 => {
            let declared = *rng.pick(&[
                MAX_PAYLOAD as u64,
                MAX_PAYLOAD as u64 + 1,
                u32::MAX as u64,
                u32::MAX as u64 + 1,
                1 << 34,
                u64::MAX,
            ]);
            (with_len(declared, body), "declared-oversize")
        }
        3 => {
            // A non-minimal varint for the same length.
            let mut out = Vec::new();
            write_varint(&mut out, len);
            let last = out.len() - 1;
            out[last] |= 0x80;
            out.push(0);
            out.extend_from_slice(body);
            (out, "varint-non-minimal")
        }
        4 => {
            // An extra literal past the declared length.
            let mut out = input.clone();
            out.extend_from_slice(&[0x00, b'x']);
            (out, "overlong-output")
        }
        5 => {
            // A copy with offset 0, or one reaching before the output.
            let mut out = input.clone();
            if rng.one_in(2) {
                out.extend_from_slice(&[0x01, 0x00]);
            } else {
                out.extend_from_slice(&[0x02, 0xff, 0xff]);
            }
            (with_len(len + 4, &out[used..]), "bad-copy")
        }
        6 => (Vec::new(), "empty"),
        7 => (with_len(0, &[]), "empty-declared-zero"),
        8 => {
            // A long literal whose length field runs past the input.
            let mut out = with_len(len, body);
            out.extend_from_slice(&[0xf0, 0xff, 0xff, 0xff, 0x00]);
            (out, "literal-length-past-end")
        }
        _ => return byte_mutate(rng, input),
    };
    (out, name.to_string())
}

/// base-reth's inbound path: `decompress_len` against the cap, then a buffer of that length.
fn reth_decompress(input: &[u8]) -> Result<Vec<u8>, String> {
    let len = snap::raw::decompress_len(input).map_err(|e| format!("decompress_len: {e:?}"))?;
    if len > MAX_PAYLOAD {
        return Err("MessageTooBig".to_string());
    }
    let mut out = vec![0u8; len];
    snap::raw::Decoder::new().decompress(input, &mut out).map_err(|e| format!("{e:?}"))?;
    Ok(out)
}

pub(crate) fn check(case: &str, input: &[u8]) -> (Verdict, Verdict) {
    if case == "roundtrip" {
        // tiger's compressor read by base-reth, and base-reth's compressor read by tiger.
        let tiger_out = base_tiger_net::snappy_compress(input);
        let tiger = match reth_decompress(&tiger_out) {
            Ok(back) if back == input => Verdict::ok(digest(input)),
            Ok(back) => Verdict::ok(format!("MISMATCH {}", digest(&back))),
            Err(e) => Verdict::err(format!("snap cannot read tiger's output: {e}")),
        };
        let reth_out = snap::raw::Encoder::new().compress_vec(input).unwrap();
        let reth = match base_tiger_net::snappy_decompress(&reth_out, MAX_PAYLOAD) {
            Ok(back) if back == input => Verdict::ok(digest(input)),
            Ok(back) => Verdict::ok(format!("MISMATCH {}", digest(&back))),
            Err(e) => Verdict::err(format!("tiger cannot read snap's output: {e:?}")),
        };
        return (tiger, reth);
    }
    let tiger = match base_tiger_net::snappy_decompress(input, MAX_PAYLOAD) {
        Ok(out) => Verdict::ok(digest(&out)),
        Err(e) => Verdict::err(e),
    };
    let reth = match reth_decompress(input) {
        Ok(out) => Verdict::ok(digest(&out)),
        Err(e) => Verdict::err(e),
    };
    (tiger, reth)
}
