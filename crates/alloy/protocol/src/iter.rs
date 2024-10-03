//! An iterator over encoded frames.

use crate::frame::{Frame, FrameParseError, DERIVATION_VERSION_0};

/// An iterator over encoded frames.
#[derive(Debug, Clone)]
pub struct FrameIter<'a> {
    encoded: &'a [u8],
    cursor: usize,
}

impl<'a> FrameIter<'a> {
    /// Creates a new [FrameIter].
    pub const fn new(encoded: &'a [u8]) -> Self {
        Self { encoded, cursor: 1 }
    }
}

impl<'a> From<&'a [u8]> for FrameIter<'a> {
    fn from(encoded: &'a [u8]) -> Self {
        Self::new(encoded)
    }
}

impl<'a> Iterator for FrameIter<'a> {
    type Item = Result<Frame, FrameParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor >= self.encoded.len() {
            return None;
        }
        if self.encoded[0] != DERIVATION_VERSION_0 {
            return None;
        }

        let (new_start, frame) = Frame::parse_frame(self.encoded, self.cursor)
            .map_err(FrameParseError::FrameDecodingError)
            .ok()?;

        self.cursor += new_start;
        Some(Ok(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_iterate_none() {
        let bytes = Vec::new();
        let mut iter = FrameIter::new(bytes.as_slice());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_iterate_missing_version() {
        let bytes = vec![0x01, 0x02, 0x03, 0x04, 0x05];
        let mut iter = FrameIter::new(bytes.as_slice());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_iterate_many_frames() {
        let frame = Frame { id: [0xFF; 16], number: 0xEE, data: vec![0xDD; 50], is_last: true };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[DERIVATION_VERSION_0]);
        (0..5).for_each(|_| {
            bytes.extend_from_slice(&frame.encode());
        });

        // Create a frame iterator from the bytes.
        let mut iter = FrameIter::new(bytes.as_slice());

        // Check that the iterator returns the same frame 5 times.
        (0..5).for_each(|_| {
            let next = iter.next().unwrap().unwrap();
            assert_eq!(next, frame);
        });
    }
}
