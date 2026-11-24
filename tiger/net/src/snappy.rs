//! Snappy block compression, the format RLPx uses for every message after Hello (devp2p v5).
//!
//! The format is: a varint holding the uncompressed length, then a sequence of elements, each a
//! literal run or a back-reference copy. The decoder accepts every valid stream. The encoder is a
//! greedy single-pass matcher over 4-byte hashes; its output is valid Snappy, not byte-identical
//! to the reference compressor (the format does not require that).

/// Errors from decoding a Snappy block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnappyError {
    /// The stream ended inside an element or the length varint.
    Truncated,
    /// The length prefix exceeds the caller's limit.
    TooLarge,
    /// A copy refers before the start of the output, has offset zero, or the output length does
    /// not match the length prefix.
    Corrupt,
}

/// Largest back-reference offset the encoder emits (fits the 2-byte copy form).
const MAX_OFFSET: usize = 65_535;
/// Hash table size of the encoder, in entries.
const HASH_BITS: u32 = 14;

/// Returns the uncompressed length announced by a Snappy block, checking it against `max`.
pub fn decompressed_len(input: &[u8], max: usize) -> Result<usize, SnappyError> {
    let (len, _) = read_varint(input)?;
    if len > max as u64 {
        return Err(SnappyError::TooLarge);
    }
    Ok(len as usize)
}

/// Decodes a Snappy block whose uncompressed length is at most `max`.
pub fn decompress(input: &[u8], max: usize) -> Result<Vec<u8>, SnappyError> {
    let (len, mut pos) = read_varint(input)?;
    if len > max as u64 {
        return Err(SnappyError::TooLarge);
    }
    let len = len as usize;
    let mut out = Vec::with_capacity(len);
    while pos < input.len() {
        let tag = input[pos];
        pos += 1;
        match tag & 3 {
            0 => {
                let mut n = (tag >> 2) as usize;
                if n >= 60 {
                    let width = n - 59;
                    let bytes = input.get(pos..pos + width).ok_or(SnappyError::Truncated)?;
                    n = 0;
                    for (i, b) in bytes.iter().enumerate() {
                        n |= (*b as usize) << (8 * i);
                    }
                    pos += width;
                }
                let n = n + 1;
                let lit = input.get(pos..pos + n).ok_or(SnappyError::Truncated)?;
                if out.len() + n > len {
                    return Err(SnappyError::Corrupt);
                }
                out.extend_from_slice(lit);
                pos += n;
            }
            kind => {
                let (n, offset) = match kind {
                    1 => {
                        let b = *input.get(pos).ok_or(SnappyError::Truncated)? as usize;
                        pos += 1;
                        (4 + ((tag >> 2) & 7) as usize, (((tag >> 5) as usize) << 8) | b)
                    }
                    2 => {
                        let b = input.get(pos..pos + 2).ok_or(SnappyError::Truncated)?;
                        pos += 2;
                        (1 + (tag >> 2) as usize, u16::from_le_bytes([b[0], b[1]]) as usize)
                    }
                    _ => {
                        let b = input.get(pos..pos + 4).ok_or(SnappyError::Truncated)?;
                        pos += 4;
                        let offset = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
                        (1 + (tag >> 2) as usize, offset)
                    }
                };
                if offset == 0 || offset > out.len() || out.len() + n > len {
                    return Err(SnappyError::Corrupt);
                }
                let start = out.len() - offset;
                // Copies may overlap their own output (run-length style), so copy byte by byte.
                for i in 0..n {
                    let b = out[start + i];
                    out.push(b);
                }
            }
        }
    }
    if out.len() != len {
        return Err(SnappyError::Corrupt);
    }
    Ok(out)
}

/// Encodes `input` as one Snappy block.
pub fn compress(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + input.len() / 6 + 16);
    write_varint(&mut out, input.len() as u64);
    let mut table = vec![u32::MAX; 1 << HASH_BITS];
    let mut literal_start = 0;
    let mut pos = 0;
    while pos + 4 <= input.len() {
        let word = u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);
        let slot = (word.wrapping_mul(0x1e35_a7bd) >> (32 - HASH_BITS)) as usize;
        let candidate = table[slot];
        table[slot] = pos as u32;
        if candidate != u32::MAX {
            let candidate = candidate as usize;
            let offset = pos - candidate;
            if offset <= MAX_OFFSET && input[candidate..candidate + 4] == input[pos..pos + 4] {
                let mut n = 4;
                while pos + n < input.len() && input[candidate + n] == input[pos + n] {
                    n += 1;
                }
                emit_literal(&mut out, &input[literal_start..pos]);
                emit_copy(&mut out, offset, n);
                pos += n;
                literal_start = pos;
                continue;
            }
        }
        pos += 1;
    }
    emit_literal(&mut out, &input[literal_start..]);
    out
}

fn emit_literal(out: &mut Vec<u8>, lit: &[u8]) {
    if lit.is_empty() {
        return;
    }
    let n = lit.len() - 1;
    if n < 60 {
        out.push((n as u8) << 2);
    } else {
        let width = (usize::BITS - n.leading_zeros()).div_ceil(8) as usize;
        assert!((1..=4).contains(&width));
        out.push(((59 + width) as u8) << 2);
        out.extend_from_slice(&(n as u32).to_le_bytes()[..width]);
    }
    out.extend_from_slice(lit);
}

fn emit_copy(out: &mut Vec<u8>, offset: usize, mut n: usize) {
    assert!(offset > 0 && offset <= MAX_OFFSET);
    while n > 0 {
        if (4..=11).contains(&n) && offset < 2048 {
            out.push(1 | (((n - 4) as u8) << 2) | (((offset >> 8) as u8) << 5));
            out.push(offset as u8);
            return;
        }
        // The 2-byte form copies 1..=64 bytes; keep at least 4 for a following short copy.
        let chunk = if n > 64 { if n - 64 < 4 { 60 } else { 64 } } else { n };
        out.push(2 | (((chunk - 1) as u8) << 2));
        out.extend_from_slice(&(offset as u16).to_le_bytes());
        n -= chunk;
    }
}

fn read_varint(input: &[u8]) -> Result<(u64, usize), SnappyError> {
    let mut value = 0u64;
    for (i, b) in input.iter().enumerate().take(5) {
        value |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            return Ok((value, i + 1));
        }
    }
    Err(SnappyError::Truncated)
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text compressed by the `snap` crate (a port of the reference compressor), through
    /// Python's `cramjam.snappy.compress_raw`.
    const REFERENCE_TEXT: &[u8] = b"Snappy is a compression/decompression library. It does not \
aim for maximum compression, or compatibility with any other compression library; instead, it \
aims for very high speeds and reasonable compression. ";
    const REFERENCE_COMPRESSED: &str = "ed0464536e61707079206973206120636f6d7072657373696f6e2f64\
651d0e90206c6962726172792e20497420646f6573206e6f742061696d20666f72206d6178696d756d2e3f000c2c20\
6f7205105861746962696c697479207769746820616e79206f746865091d0d6c115e303b20696e73746561642c2069\
74015e0073055f787665727920686967682073706565647320616e6420726561736f6e61626c6505670d4a042e20fe\
cf00fecf00fecf00fecf00fecf00fecf0076cf00";

    #[test]
    fn decodes_reference_compressor_output() {
        let compressed = alloy_primitives::hex::decode(REFERENCE_COMPRESSED).unwrap();
        let expected = REFERENCE_TEXT.repeat(3);
        assert_eq!(decompressed_len(&compressed, 1000).unwrap(), expected.len());
        assert_eq!(decompress(&compressed, 1000).unwrap(), expected);
    }

    #[test]
    fn decodes_hand_built_elements() {
        let empty = [0x00];
        assert_eq!(decompress(&empty, 10).unwrap(), b"");
        // "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" (32 bytes): literal 'a' then a 31-byte copy at
        // offset 1.
        let run = [0x20, 0x00, 0x61, 0x7a, 0x01, 0x00];
        assert_eq!(decompress(&run, 64).unwrap(), vec![b'a'; 32]);
        // From the snappy format description: copy-1 with offset in the tag's high bits.
        let mixed = [0x0a, 0x0c, b'a', b'b', b'c', b'd', 0x09, 0x04];
        assert_eq!(decompress(&mixed, 64).unwrap(), b"abcdabcdab");
    }

    #[test]
    fn rejects_corrupt_streams() {
        assert_eq!(decompress(&[0x05, 0x00, b'a'], 10), Err(SnappyError::Corrupt));
        assert_eq!(decompress(&[0x04, 0x05, 0x00, 0x00], 10), Err(SnappyError::Corrupt));
        assert_eq!(decompress(&[0x80], 10), Err(SnappyError::Truncated));
        assert_eq!(decompress(&[0x7f], 10), Err(SnappyError::TooLarge));
        assert_eq!(decompress(&[0x02, 0x04, b'a'], 10), Err(SnappyError::Truncated));
    }

    #[test]
    fn round_trips() {
        let mut seed = 7u64;
        let mut cases: Vec<Vec<u8>> = vec![vec![], b"a".to_vec(), vec![0u8; 100_000]];
        for len in [3usize, 60, 61, 300, 70_000, 200_000] {
            let mut v = Vec::with_capacity(len);
            for i in 0..len {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                // Low-entropy data so the matcher finds both short and long copies.
                v.push(if i % 7 < 3 { (seed >> 60) as u8 } else { b"tiger"[i % 5] });
            }
            cases.push(v);
        }
        for case in cases {
            let c = compress(&case);
            assert_eq!(decompress(&c, case.len()).unwrap(), case);
        }
        assert!(compress(&vec![0u8; 100_000]).len() < 6_000);
    }
}
