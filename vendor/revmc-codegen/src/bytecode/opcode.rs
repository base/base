use std::{fmt, slice};

use revm_bytecode::{opcode as op, opcode::OPCODE_INFO};
use revm_primitives::hardfork::SpecId;

use crate::{OpcodeInfo, op_info_map};

/// A bytecode iterator that yields opcodes and their immediate data, alongside the program counter.
///
/// Created by calling [`OpcodesIter::with_pc`].
#[derive(Debug)]
pub struct OpcodesIterWithPc<'a> {
    iter: OpcodesIter<'a>,
    pc: usize,
}

impl OpcodesIterWithPc<'_> {
    /// Rewinds the iterator by `n` bytes so that the next call to [`Iterator::next`] re-yields
    /// them as fresh opcodes.
    ///
    /// # Safety
    ///
    /// `n` must not exceed the number of bytes already consumed.
    pub(crate) unsafe fn rewind(&mut self, n: usize) {
        // SAFETY: the caller guarantees n ≤ bytes consumed, so `remaining` was originally
        // part of a larger contiguous slice that started at least `n` bytes earlier.
        unsafe { self.iter.rewind(n) };
        self.pc -= n;
    }
}

impl<'a> Iterator for OpcodesIterWithPc<'a> {
    type Item = (usize, Opcode<'a>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(|opcode| {
            let pc = self.pc;
            self.pc += 1;
            if let Some(imm) = opcode.immediate {
                self.pc += imm.len();
            }
            (pc, opcode)
        })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}

impl std::iter::FusedIterator for OpcodesIterWithPc<'_> {}

/// An iterator that yields opcodes and their immediate data.
///
/// If the bytecode is not well-formed, the iterator will still yield opcodes, but the immediate
/// data may be incorrect. For example, if the bytecode is `PUSH2 0x69`, the iterator will yield
/// `PUSH2, None`.
#[derive(Clone, Debug)]
pub struct OpcodesIter<'a> {
    iter: slice::Iter<'a, u8>,
    info: &'static [OpcodeInfo; 256],
}

impl fmt::Display for OpcodesIter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, op) in self.clone().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{op}")?;
        }
        Ok(())
    }
}

impl<'a> OpcodesIter<'a> {
    /// Create a new iterator over the given bytecode slice.
    #[inline]
    pub fn new(slice: &'a [u8], spec_id: SpecId) -> Self {
        Self { iter: slice.iter(), info: op_info_map(spec_id) }
    }

    /// Returns a new iterator that also yields the program counter alongside the opcode and
    /// immediate data.
    #[inline]
    pub fn with_pc(self) -> OpcodesIterWithPc<'a> {
        OpcodesIterWithPc { iter: self, pc: 0 }
    }

    /// Returns the inner iterator.
    #[inline]
    pub fn inner(&self) -> &slice::Iter<'a, u8> {
        &self.iter
    }

    /// Returns the inner iterator.
    #[inline]
    pub fn inner_mut(&mut self) -> &mut slice::Iter<'a, u8> {
        &mut self.iter
    }

    /// Returns the inner iterator.
    #[inline]
    pub fn into_inner(self) -> slice::Iter<'a, u8> {
        self.iter
    }

    /// Rewinds the iterator by `n` bytes so that the next call to [`Iterator::next`] re-yields
    /// them as fresh opcodes.
    ///
    /// # Safety
    ///
    /// `n` must not exceed the number of bytes already consumed.
    pub(crate) unsafe fn rewind(&mut self, n: usize) {
        let inner = self.inner_mut();
        let remaining = inner.as_slice();
        // SAFETY: the caller guarantees n ≤ bytes consumed, so `remaining` was originally
        // part of a larger contiguous slice that started at least `n` bytes earlier.
        let rewound =
            unsafe { std::slice::from_raw_parts(remaining.as_ptr().sub(n), remaining.len() + n) };
        *inner = rewound.iter();
    }
}

impl<'a> Iterator for OpcodesIter<'a> {
    type Item = Opcode<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(|&opcode| {
            let info = self.info[opcode as usize];
            if info.is_unknown() || info.is_disabled() {
                return Opcode { opcode, immediate: None };
            }

            let len = min_imm_len(opcode) as usize;
            let immediate = if len > 0 {
                let r = self.iter.as_slice().get(..len);
                // TODO: Use `advance_by` when stable.
                self.iter.by_ref().take(len).for_each(drop);
                r
            } else {
                None
            };
            Opcode { opcode, immediate }
        })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.iter.len();
        ((len != 0) as usize, Some(len))
    }
}

impl std::iter::FusedIterator for OpcodesIter<'_> {}

/// An opcode and its immediate data. Returned by [`OpcodesIter`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Opcode<'a> {
    /// The opcode.
    pub opcode: u8,
    /// The immediate data, if any.
    pub immediate: Option<&'a [u8]>,
}

impl fmt::Debug for Opcode<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Opcode<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match OPCODE_INFO[self.opcode as usize] {
            Some(s) => f.write_str(s.name()),
            None => write!(f, "UNKNOWN(0x{:02x})", self.opcode),
        }?;
        match self.immediate {
            Some(imm) => write!(f, " {}", revm_primitives::hex::encode_prefixed(imm)),
            None => Ok(()),
        }
    }
}

/// Returns the length of the immediate data for the given opcode, or `0` if none.
#[inline]
pub const fn min_imm_len(op: u8) -> u8 {
    if let Some(info) = &OPCODE_INFO[op as usize] { info.immediate_size() } else { 0 }
}

/// Returns the number of input and output stack elements of the given opcode.
#[inline]
pub const fn stack_io(op: u8) -> (u8, u8) {
    if let Some(info) = &OPCODE_INFO[op as usize] {
        (info.inputs(), info.outputs())
    } else {
        (0, 0)
    }
}

/// Returns the real `(inputs, outputs)` stack I/O for an opcode, decoding the immediate for
/// `DUPN`, `SWAPN`, and `EXCHANGE` whose opcode-table entries are placeholders.
pub(crate) fn compute_stack_io(op: u8, immediate: Option<&[u8]>) -> (u8, u8) {
    let imm_u8 = || immediate.map_or(0, |b| b[0]);
    match op {
        op::DUPN => decode_single(imm_u8()).map(|n| (n, n + 1)),
        op::SWAPN => decode_single(imm_u8()).map(|n| (n + 1, n + 1)),
        op::EXCHANGE => decode_pair(imm_u8()).map(|(_n, m)| (m + 1, m + 1)),
        _ => return stack_io(op),
    }
    .unwrap_or((0, 0)) // Invalid immediate.
}

/// Decodes a DUPN/SWAPN immediate byte into a stack index.
///
/// Returns `None` if the immediate is in the invalid range `[91, 127]`.
pub fn decode_single(x: u8) -> Option<u8> {
    if x <= 90 || x >= 128 { Some(x.wrapping_add(145)) } else { None }
}

/// Encodes a stack index into a DUPN/SWAPN immediate byte.
///
/// Returns `None` if the value is outside the valid range.
pub fn encode_single(n: u8) -> Option<u8> {
    let x = n.wrapping_sub(145);
    if decode_single(x) == Some(n) { Some(x) } else { None }
}

/// Decodes an EXCHANGE immediate byte into a pair of stack indices `(n, m)`.
///
/// Returns `None` if the value is outside the valid range.
pub fn decode_pair(x: u8) -> Option<(u8, u8)> {
    if x > 81 && x < 128 {
        return None;
    }
    let k = (x ^ 143) as u16;
    let q = (k / 16) as u8;
    let r = (k % 16) as u8;
    if q < r { Some((q + 1, r + 1)) } else { Some((r + 1, 29 - q)) }
}

/// Encodes a pair of stack indices into an EXCHANGE immediate byte.
///
/// Returns `None` if the pair cannot be encoded.
pub fn encode_pair(n: u8, m: u8) -> Option<u8> {
    if n == 0 || m == 0 {
        return None;
    }
    // Try both (q,r) orderings and check round-trip.
    let try_k = |q: u8, r: u8| -> Option<u8> {
        if q >= 16 || r >= 16 {
            return None;
        }
        let k = q as u16 * 16 + r as u16;
        let x = (k as u8) ^ 143;
        if decode_pair(x) == Some((n, m)) { Some(x) } else { None }
    };

    // Case 1: q < r → n = q+1, m = r+1.
    if n < m {
        try_k(n - 1, m - 1)
    } else {
        // Case 2: q >= r → n = r+1, m = 29-q.
        if let Some(q) = 29u8.checked_sub(m) { try_k(q, n - 1) } else { None }
    }
}

/// Returns a string representation of the given bytecode.
pub fn format_bytecode(bytecode: &[u8], spec_id: SpecId) -> String {
    let mut w = String::new();
    format_bytecode_to(bytecode, spec_id, &mut w).unwrap();
    w
}

/// Formats an EVM bytecode to the given writer.
pub fn format_bytecode_to<W: fmt::Write + ?Sized>(
    bytecode: &[u8],
    spec_id: SpecId,
    w: &mut W,
) -> fmt::Result {
    write!(w, "{}", OpcodesIter::new(bytecode, spec_id))
}

#[cfg(test)]
mod tests {
    use revm_bytecode::opcode as op;

    use super::*;

    const DEF_SPEC: SpecId = SpecId::LONDON;

    #[test]
    fn iter_basic() {
        let bytecode = [0x01, 0x02, 0x03, 0x04, 0x05];
        let mut iter = OpcodesIter::new(&bytecode, DEF_SPEC);

        assert_eq!(iter.next(), Some(Opcode { opcode: 0x01, immediate: None }));
        assert_eq!(iter.next(), Some(Opcode { opcode: 0x02, immediate: None }));
        assert_eq!(iter.next(), Some(Opcode { opcode: 0x03, immediate: None }));
        assert_eq!(iter.next(), Some(Opcode { opcode: 0x04, immediate: None }));
        assert_eq!(iter.next(), Some(Opcode { opcode: 0x05, immediate: None }));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn iter_with_imm() {
        let bytecode = [op::PUSH0, op::PUSH1, 0x69, op::PUSH2, 0x01, 0x02];
        let mut iter = OpcodesIter::new(&bytecode, DEF_SPEC);

        assert_eq!(iter.next(), Some(Opcode { opcode: op::PUSH0, immediate: None }));
        assert_eq!(iter.next(), Some(Opcode { opcode: op::PUSH1, immediate: Some(&[0x69]) }));
        assert_eq!(iter.next(), Some(Opcode { opcode: op::PUSH2, immediate: Some(&[0x01, 0x02]) }));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn iter_with_imm_too_short() {
        let bytecode = [op::PUSH2, 0x69];
        let mut iter = OpcodesIter::new(&bytecode, DEF_SPEC);

        assert_eq!(iter.next(), Some(Opcode { opcode: op::PUSH2, immediate: None }));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn display() {
        let bytecode = [op::PUSH0, op::PUSH1, 0x69, op::PUSH2, 0x01, 0x02];
        let s = format_bytecode(&bytecode, DEF_SPEC);
        assert_eq!(s, "PUSH0 PUSH1 0x69 PUSH2 0x0102");
    }

    #[test]
    fn encode_decode_single_roundtrip() {
        for x in 0..=255u8 {
            if let Some(n) = decode_single(x) {
                assert_eq!(encode_single(n), Some(x), "roundtrip failed for x={x}, n={n}");
            }
        }
        for n in 0..=255u8 {
            if let Some(x) = encode_single(n) {
                assert_eq!(decode_single(x), Some(n), "roundtrip failed for n={n}, x={x}");
            }
        }
        // Out-of-range: values in [236,255] ∪ [0,16] have no valid encoding.
        assert_eq!(encode_single(0), None);
        assert_eq!(encode_single(16), None);
        assert_eq!(encode_single(236), None);
        // Invalid raw bytes.
        assert_eq!(decode_single(91), None);
        assert_eq!(decode_single(127), None);
    }

    #[test]
    fn encode_decode_pair_roundtrip() {
        // encode → decode is always exact.
        for n in 1..=30u8 {
            for m in 1..=30u8 {
                if let Some(x) = encode_pair(n, m) {
                    assert_eq!(
                        decode_pair(x),
                        Some((n, m)),
                        "roundtrip failed for (n,m)=({n},{m}), x={x}"
                    );
                }
            }
        }
        // decode → encode: some pairs have multiple encodings, so the raw byte may differ,
        // but decoding the result must produce the same pair.
        // Some decoded pairs may have values too large to re-encode.
        for x in 0..=255u8 {
            if let Some((n, m)) = decode_pair(x)
                && let Some(re) = encode_pair(n, m)
            {
                assert_eq!(
                    decode_pair(re),
                    Some((n, m)),
                    "decode roundtrip failed for x={x}, re={re}, (n,m)=({n},{m})"
                );
            }
        }
        // Zero indices are invalid.
        assert_eq!(encode_pair(0, 1), None);
        assert_eq!(encode_pair(1, 0), None);
        // Invalid raw bytes: [82, 127].
        assert_eq!(decode_pair(82), None);
        assert_eq!(decode_pair(127), None);
        // Edge: 80 and 81 are now valid.
        assert!(decode_pair(80).is_some());
        assert!(decode_pair(81).is_some());
    }
}
