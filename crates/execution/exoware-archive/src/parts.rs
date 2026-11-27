//! Continuation parts of block-family values.
//!
//! A store value holds at most [`VALUE_PART_LEN`] bytes. A block-family value shorter than the
//! part length is stored unchanged in its base row. A longer one, or one of exactly the part
//! length, stores its first part-length bytes in the base row and the rest in continuation rows
//! [`ArchiveKey::block_part`] `1, 2, …`. Every part but the last is exactly the part length and
//! the last is shorter, empty when the value length is a multiple of the part length, so "a part
//! shorter than the part length ends the value" holds without exception.

use std::future::Future;

use bytes::Bytes;

use crate::{ArchiveError, ArchiveKey, Family, VALUE_PART_LEN};

/// Splits block-family values into parts and reassembles them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueParts {
    /// Bytes per part; [`VALUE_PART_LEN`] in the format, smaller in tests.
    pub part_len: usize,
}

impl Default for ValueParts {
    fn default() -> Self {
        Self::new(VALUE_PART_LEN)
    }
}

impl ValueParts {
    /// Parts of `part_len` bytes (at least one).
    pub const fn new(part_len: usize) -> Self {
        assert!(part_len > 0, "part length must be positive");
        Self { part_len }
    }

    /// Number of continuation parts after the base row for a value of `len` bytes.
    pub const fn continuations(&self, len: usize) -> usize {
        len / self.part_len
    }

    /// Largest value length that fits in the base row plus `u16::MAX` continuation parts.
    pub const fn max_len(&self) -> usize {
        self.part_len.saturating_mul(u16::MAX as usize + 1).saturating_sub(1)
    }

    /// Splits `value` into its base part followed by its continuation parts `1, 2, …`. Fails
    /// with [`ArchiveError::TooManyParts`] when more than `u16::MAX` continuations are needed.
    pub fn split<'a>(
        &self,
        value: &'a [u8],
    ) -> Result<impl Iterator<Item = &'a [u8]> + 'a, ArchiveError> {
        let len = value.len();
        if len > self.max_len() {
            return Err(ArchiveError::TooManyParts { len, max: self.max_len() });
        }
        let part_len = self.part_len;
        Ok((0..=self.continuations(len))
            .map(move |i| &value[(i * part_len).min(len)..((i + 1) * part_len).min(len)]))
    }

    /// Splits block `number`'s `family` value into its rows: the base key and, when it
    /// continues, the continuation keys `1, 2, …`. A value shorter than the part length is
    /// returned as its single base row without copying.
    pub fn rows(
        &self,
        family: Family,
        number: u64,
        value: Vec<u8>,
    ) -> Result<Vec<(Bytes, Vec<u8>)>, ArchiveError> {
        if value.len() < self.part_len {
            return Ok(vec![(ArchiveKey::block(family, number), value)]);
        }
        Ok(self
            .split(&value)?
            .enumerate()
            .map(|(i, part)| {
                let key = if i == 0 {
                    ArchiveKey::block(family, number)
                } else {
                    // `split` yields at most `u16::MAX` continuations.
                    ArchiveKey::block_part(family, number, i as u16)
                };
                (key, part.to_vec())
            })
            .collect())
    }

    /// Starts reassembling block `number`'s `family` value from its base row. A base value
    /// longer than the part length is corrupt.
    pub fn assemble(
        &self,
        family: Family,
        number: u64,
        base: Bytes,
    ) -> Result<PartAssembly, ArchiveError> {
        let mut assembly =
            PartAssembly { parts: *self, family, number, value: Vec::new(), base, next: Some(1) };
        assembly.check(0, assembly.base.len())?;
        if assembly.base.len() < self.part_len {
            assembly.next = None;
        } else {
            assembly.value.extend_from_slice(&assembly.base);
        }
        Ok(assembly)
    }

    /// Reassembles block `number`'s `family` value from its base row, reading continuation
    /// parts with `read_part` only while the last part read is exactly the part length. A
    /// base value shorter than the part length is returned as is, without any read.
    pub async fn read<F, Fut>(
        &self,
        family: Family,
        number: u64,
        base: Bytes,
        read_part: F,
    ) -> Result<Bytes, ArchiveError>
    where
        F: FnMut(u16) -> Fut,
        Fut: Future<Output = Result<Option<Bytes>, ArchiveError>>,
    {
        self.assemble(family, number, base)?.read_rest(read_part).await
    }
}

/// A block-family value being reassembled part by part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartAssembly {
    /// Part length.
    pub parts: ValueParts,
    /// Family of the value.
    pub family: Family,
    /// Block number.
    pub number: u64,
    /// Concatenated parts, when the value continues past its base row.
    pub value: Vec<u8>,
    /// Base row value.
    pub base: Bytes,
    /// Index of the next part to read; `None` once a part shorter than the part length ended
    /// the value.
    pub next: Option<u16>,
}

impl PartAssembly {
    /// Index of the next continuation part, or `None` when the value is complete.
    pub const fn next_part(&self) -> Option<u16> {
        self.next
    }

    /// Continuation key of the next part, or `None` when the value is complete.
    pub fn next_key(&self) -> Option<Bytes> {
        self.next.map(|part| ArchiveKey::block_part(self.family, self.number, part))
    }

    /// Appends continuation part `part`. Fails when the value is already complete (extra
    /// data), when `part` is not the next part (an earlier one is missing), or when the part is
    /// longer than the part length.
    pub fn push(&mut self, part: u16, value: &[u8]) -> Result<(), ArchiveError> {
        let Some(next) = self.next else {
            return Err(self.corrupt(format!("has extra data in part {part} after its last part")));
        };
        if part != next {
            return Err(self.missing());
        }
        self.check(part, value.len())?;
        self.value.extend_from_slice(value);
        if value.len() < self.parts.part_len {
            self.next = None;
        } else if part == u16::MAX {
            return Err(self.corrupt("continues past the last possible part".to_string()));
        } else {
            self.next = Some(part + 1);
        }
        Ok(())
    }

    /// The error for a missing next part.
    pub fn missing(&self) -> ArchiveError {
        let part = self.next.unwrap_or_default();
        self.corrupt(format!("is missing continuation part {part}"))
    }

    /// The complete value; fails while a part is still missing.
    pub fn finish(self) -> Result<Bytes, ArchiveError> {
        if self.next.is_some() {
            return Err(self.missing());
        }
        Ok(if self.value.is_empty() { self.base } else { Bytes::from(self.value) })
    }

    /// Reads the remaining parts with `read_part`, then returns the complete value.
    pub async fn read_rest<F, Fut>(mut self, mut read_part: F) -> Result<Bytes, ArchiveError>
    where
        F: FnMut(u16) -> Fut,
        Fut: Future<Output = Result<Option<Bytes>, ArchiveError>>,
    {
        while let Some(part) = self.next {
            let value = read_part(part).await?.ok_or_else(|| self.missing())?;
            self.push(part, &value)?;
        }
        self.finish()
    }

    /// Fails when part `part` of `len` bytes is longer than the part length.
    fn check(&self, part: u16, len: usize) -> Result<(), ArchiveError> {
        if len > self.parts.part_len {
            return Err(self.corrupt(format!(
                "part {part} has {len} bytes, above the {}-byte part length",
                self.parts.part_len
            )));
        }
        Ok(())
    }

    /// An [`ArchiveError::Unavailable`] for this value with `reason`.
    fn corrupt(&self, reason: String) -> ArchiveError {
        ArchiveError::unavailable(
            self.family,
            format!("value of block {} {reason} (corrupt store)", self.number),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::BTreeMap};

    use super::*;

    const PARTS: ValueParts = ValueParts::new(4);

    /// Value `0, 1, …, len − 1`.
    fn value(len: usize) -> Vec<u8> {
        (0..len as u8).collect()
    }

    /// Reads block 7's body from `rows` through [`ValueParts::read`], recording each part read.
    async fn read(rows: &BTreeMap<Bytes, Vec<u8>>) -> (Result<Bytes, ArchiveError>, Vec<u16>) {
        let reads = RefCell::new(Vec::new());
        let base = Bytes::from(rows[&ArchiveKey::block(Family::Body, 7)].clone());
        let result = PARTS
            .read(Family::Body, 7, base, |part| {
                reads.borrow_mut().push(part);
                let found = rows
                    .get(&ArchiveKey::block_part(Family::Body, 7, part))
                    .map(|v| Bytes::from(v.clone()));
                async move { Ok(found) }
            })
            .await;
        (result, reads.into_inner())
    }

    /// Exact parts and keys for lengths around the part length, and the round trip.
    #[tokio::test]
    async fn split_and_reassemble_at_small_part_length() {
        let cases: [(usize, &[&[u8]]); 6] = [
            (0, &[&[]]),
            (3, &[&[0, 1, 2]]),
            (4, &[&[0, 1, 2, 3], &[]]),
            (5, &[&[0, 1, 2, 3], &[4]]),
            (8, &[&[0, 1, 2, 3], &[4, 5, 6, 7], &[]]),
            (9, &[&[0, 1, 2, 3], &[4, 5, 6, 7], &[8]]),
        ];
        for (len, expected) in cases {
            let whole = value(len);
            let parts: Vec<&[u8]> = PARTS.split(&whole).unwrap().collect();
            assert_eq!(parts, expected, "len {len}: exact parts");
            assert_eq!(PARTS.continuations(len), expected.len() - 1, "len {len}: continuations");
            let rows = PARTS.rows(Family::Body, 7, whole.clone()).unwrap();
            let mut keys = vec![ArchiveKey::block(Family::Body, 7)];
            keys.extend(
                (1..expected.len() as u16).map(|p| ArchiveKey::block_part(Family::Body, 7, p)),
            );
            assert_eq!(
                rows.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
                keys,
                "len {len}: base key, then parts from 1"
            );
            let stored: BTreeMap<Bytes, Vec<u8>> = rows.into_iter().collect();
            let (result, reads) = read(&stored).await;
            assert_eq!(result.unwrap().as_ref(), whole.as_slice(), "len {len}: reassembled");
            assert_eq!(
                reads,
                (1..expected.len() as u16).collect::<Vec<_>>(),
                "len {len}: reads exactly the continuation parts"
            );
        }
    }

    /// A missing part, an over-long part and data after the last part are corruption.
    #[tokio::test]
    async fn reassembly_rejects_corrupt_parts() {
        let rows: BTreeMap<Bytes, Vec<u8>> =
            PARTS.rows(Family::Body, 7, value(9)).unwrap().into_iter().collect();

        let mut missing = rows.clone();
        missing.remove(&ArchiveKey::block_part(Family::Body, 7, 2));
        let (result, reads) = read(&missing).await;
        assert_eq!(
            result,
            Err(ArchiveError::unavailable(
                Family::Body,
                "value of block 7 is missing continuation part 2 (corrupt store)"
            )),
            "missing part 2"
        );
        assert_eq!(reads, vec![1, 2], "stops at the missing part");

        let mut long = rows.clone();
        long.insert(ArchiveKey::block_part(Family::Body, 7, 1), value(5));
        let (result, _) = read(&long).await;
        assert_eq!(
            result,
            Err(ArchiveError::unavailable(
                Family::Body,
                "value of block 7 part 1 has 5 bytes, above the 4-byte part length (corrupt store)"
            )),
            "over-long part"
        );

        let base_long = PARTS.assemble(Family::Body, 7, Bytes::from(value(5)));
        assert_eq!(
            base_long,
            Err(ArchiveError::unavailable(
                Family::Body,
                "value of block 7 part 0 has 5 bytes, above the 4-byte part length (corrupt store)"
            )),
            "over-long base"
        );

        let mut assembly = PARTS.assemble(Family::Body, 7, Bytes::from(value(4))).unwrap();
        assert_eq!(
            assembly.next_key(),
            Some(ArchiveKey::block_part(Family::Body, 7, 1)),
            "next key"
        );
        assembly.push(1, &[9]).unwrap();
        assert_eq!(assembly.next_part(), None, "a short part ends the value");
        assert_eq!(
            assembly.push(2, &[9]),
            Err(ArchiveError::unavailable(
                Family::Body,
                "value of block 7 has extra data in part 2 after its last part (corrupt store)"
            )),
            "extra data"
        );

        let mut skipped = PARTS.assemble(Family::Body, 7, Bytes::from(value(4))).unwrap();
        assert!(
            matches!(skipped.push(2, &[9]), Err(ArchiveError::Unavailable { family: Family::Body, reason }) if reason.contains("missing continuation part 1")),
            "a skipped part is missing"
        );
        assert!(
            matches!(skipped.finish(), Err(ArchiveError::Unavailable { family: Family::Body, .. })),
            "an incomplete value does not finish"
        );
    }

    /// A short base value is returned as is with no part read; more than `u16::MAX`
    /// continuations are refused.
    #[tokio::test]
    async fn short_values_and_part_limit() {
        let base = Bytes::from(value(3));
        let (result, reads) =
            read(&[(ArchiveKey::block(Family::Body, 7), value(3))].into_iter().collect()).await;
        assert_eq!(result.unwrap(), base, "short value unchanged");
        assert!(reads.is_empty(), "no part read for a short value");

        let parts = ValueParts::new(1);
        assert_eq!(parts.max_len(), u16::MAX as usize, "base plus u16::MAX one-byte parts");
        assert_eq!(
            parts.split(&vec![0; u16::MAX as usize]).unwrap().count(),
            u16::MAX as usize + 1,
            "at the limit"
        );
        assert!(
            matches!(
                parts.split(&vec![0; u16::MAX as usize + 1]),
                Err(ArchiveError::TooManyParts { len, max }) if len == u16::MAX as usize + 1 && max == u16::MAX as usize
            ),
            "one byte over the limit"
        );
        assert_eq!(
            VALUE_PART_LEN,
            exoware_sdk::limits::MAX_VALUE_LEN,
            "parts fill one store value"
        );
    }
}
