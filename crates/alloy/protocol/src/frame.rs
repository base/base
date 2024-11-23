//! Frame Types

use crate::ChannelId;
use alloc::vec::Vec;

/// The version of the derivation pipeline.
pub const DERIVATION_VERSION_0: u8 = 0;

/// Count the tagging info as 200 in terms of buffer size.
pub const FRAME_OVERHEAD: usize = 200;

/// Frames cannot be larger than 1MB.
///
/// Data transactions that carry frames are generally not larger than 128 KB due to L1 network
/// conditions, but we leave space to grow larger anyway (gas limit allows for more data).
pub const MAX_FRAME_LEN: usize = 1_000_000;

/// A frame decoding error.
#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameDecodingError {
    /// The frame data is too large.
    #[error("Frame data too large: {0} bytes")]
    DataTooLarge(usize),
    /// The frame data is too short.
    #[error("Frame data too short: {0} bytes")]
    DataTooShort(usize),
    /// Error decoding the frame id.
    #[error("Invalid frame id")]
    InvalidId,
    /// Error decoding the frame number.
    #[error("Invalid frame number")]
    InvalidNumber,
    /// Error decoding the frame data length.
    #[error("Invalid frame data length")]
    InvalidDataLength,
}

/// Frame parsing error.
#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameParseError {
    /// Error parsing the frame data.
    #[error("Frame decoding error: {0}")]
    FrameDecodingError(FrameDecodingError),
    /// No frames to parse.
    #[error("No frames to parse")]
    NoFrames,
    /// Unsupported derivation version.
    #[error("Unsupported derivation version")]
    UnsupportedVersion,
    /// Frame data length mismatch.
    #[error("Frame data length mismatch")]
    DataLengthMismatch,
    /// No frames decoded.
    #[error("No frames decoded")]
    NoFramesDecoded,
}

/// A channel frame is a segment of a channel's data.
///
/// *Encoding*
/// frame = `channel_id ++ frame_number ++ frame_data_length ++ frame_data ++ is_last`
/// * channel_id        = bytes16
/// * frame_number      = uint16
/// * frame_data_length = uint32
/// * frame_data        = bytes
/// * is_last           = bool
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Frame {
    /// The unique idetifier for the frame.
    pub id: ChannelId,
    /// The number of the frame.
    pub number: u16,
    /// The data within the frame.
    pub data: Vec<u8>,
    /// Whether or not the frame is the last in the sequence.
    pub is_last: bool,
}

impl Frame {
    /// Creates a new [Frame].
    pub const fn new(id: ChannelId, number: u16, data: Vec<u8>, is_last: bool) -> Self {
        Self { id, number, data, is_last }
    }

    /// Encode the frame into a byte vector.
    pub fn encode(&self) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(16 + 2 + 4 + self.data.len() + 1);
        encoded.extend_from_slice(&self.id);
        encoded.extend_from_slice(&self.number.to_be_bytes());
        encoded.extend_from_slice(&(self.data.len() as u32).to_be_bytes());
        encoded.extend_from_slice(&self.data);
        encoded.push(self.is_last as u8);
        encoded
    }

    /// Decode a frame from a byte vector.
    pub fn decode(encoded: &[u8]) -> Result<(usize, Self), FrameDecodingError> {
        const BASE_FRAME_LEN: usize = 16 + 2 + 4 + 1;

        if encoded.len() < BASE_FRAME_LEN {
            return Err(FrameDecodingError::DataTooShort(encoded.len()));
        }

        let id = encoded[..16].try_into().map_err(|_| FrameDecodingError::InvalidId)?;
        let number = u16::from_be_bytes(
            encoded[16..18].try_into().map_err(|_| FrameDecodingError::InvalidNumber)?,
        );
        let data_len = u32::from_be_bytes(
            encoded[18..22].try_into().map_err(|_| FrameDecodingError::InvalidDataLength)?,
        ) as usize;

        if data_len > MAX_FRAME_LEN || data_len >= encoded.len() - (BASE_FRAME_LEN - 1) {
            return Err(FrameDecodingError::DataTooLarge(data_len));
        }

        let data = encoded[22..22 + data_len].to_vec();
        let is_last = encoded[22 + data_len] == 1;
        Ok((BASE_FRAME_LEN + data_len, Self { id, number, data, is_last }))
    }

    /// Parses a single frame from the given data at the given starting position,
    /// returning the frame and the number of bytes consumed.
    pub fn parse_frame(data: &[u8], start: usize) -> Result<(usize, Self), FrameDecodingError> {
        let (frame_len, frame) = Self::decode(&data[start..])?;
        Ok((frame_len, frame))
    }

    /// Parse the on chain serialization of frame(s) in an L1 transaction. Currently
    /// only version 0 of the serialization format is supported. All frames must be parsed
    /// without error and there must not be any left over data and there must be at least one
    /// frame.
    ///
    /// Frames are stored in L1 transactions with the following format:
    /// * `data = DerivationVersion0 ++ Frame(s)` Where there is one or more frames concatenated
    ///   together.
    pub fn parse_frames(encoded: &[u8]) -> Result<Vec<Self>, FrameParseError> {
        if encoded.is_empty() {
            return Err(FrameParseError::NoFrames);
        }
        if encoded[0] != DERIVATION_VERSION_0 {
            return Err(FrameParseError::UnsupportedVersion);
        }

        let data = &encoded[1..];
        let mut frames = Vec::new();
        let mut offset = 0;
        while offset < data.len() {
            let (frame_length, frame) =
                Self::decode(&data[offset..]).map_err(FrameParseError::FrameDecodingError)?;
            frames.push(frame);
            offset += frame_length;
        }

        if offset != data.len() {
            return Err(FrameParseError::DataLengthMismatch);
        }
        if frames.is_empty() {
            return Err(FrameParseError::NoFramesDecoded);
        }

        Ok(frames)
    }

    /// Calculates the size of the frame + overhead for storing the frame. The sum of the frame size
    /// of each frame in a channel determines the channel's size. The sum of the channel sizes
    /// is used for pruning & compared against the max channel bank size.
    pub fn size(&self) -> usize {
        self.data.len() + FRAME_OVERHEAD
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use alloc::vec;

    #[test]
    fn test_encode_frame_roundtrip() {
        let frame = Frame { id: [0xFF; 16], number: 0xEE, data: vec![0xDD; 50], is_last: true };

        let (_, frame_decoded) = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(frame, frame_decoded);
    }

    #[test]
    fn test_data_too_short() {
        let frame = Frame { id: [0xFF; 16], number: 0xEE, data: vec![0xDD; 22], is_last: true };
        let err = Frame::decode(&frame.encode()[..22]).unwrap_err();
        assert_eq!(err, FrameDecodingError::DataTooShort(22));
    }

    #[test]
    fn test_decode_exceeds_max_data_len() {
        let frame = Frame {
            id: [0xFF; 16],
            number: 0xEE,
            data: vec![0xDD; MAX_FRAME_LEN + 1],
            is_last: true,
        };
        let err = Frame::decode(&frame.encode()).unwrap_err();
        assert_eq!(err, FrameDecodingError::DataTooLarge(MAX_FRAME_LEN + 1));
    }

    #[test]
    fn test_decode_malicious_data_len() {
        let frame = Frame { id: [0xFF; 16], number: 0xEE, data: vec![0xDD; 50], is_last: true };
        let mut encoded = frame.encode();
        let data_len = (encoded.len() - 22) as u32;
        encoded[18..22].copy_from_slice(&data_len.to_be_bytes());

        let err = Frame::decode(&encoded).unwrap_err();
        assert_eq!(err, FrameDecodingError::DataTooLarge(encoded.len() - 22_usize));

        let valid_data_len = (encoded.len() - 23) as u32;
        encoded[18..22].copy_from_slice(&valid_data_len.to_be_bytes());
        let (_, frame_decoded) = Frame::decode(&encoded).unwrap();
        assert_eq!(frame, frame_decoded);
    }

    #[test]
    fn test_decode_many() {
        let frame = Frame { id: [0xFF; 16], number: 0xEE, data: vec![0xDD; 50], is_last: true };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[DERIVATION_VERSION_0]);
        (0..5).for_each(|_| {
            bytes.extend_from_slice(&frame.encode());
        });

        let frames = Frame::parse_frames(bytes.as_slice()).unwrap();
        assert_eq!(frames.len(), 5);
        (0..5).for_each(|i| {
            assert_eq!(frames[i], frame);
        });
    }
}
