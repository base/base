//! Precompile Error Types
//!
//! Defines errors specific to the privacy precompiles that are not covered
//! by revm's `PrecompileError`.

use core::fmt;

/// Error that can occur when parsing precompile input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrecompileInputError {
    /// Input is too short to contain the expected data
    InputTooShort {
        expected: usize,
        actual: usize,
    },
    /// Unknown function selector
    UnknownSelector([u8; 4]),
    /// Invalid slot type value
    InvalidSlotType(u8),
    /// Invalid ownership type value
    InvalidOwnershipType(u8),
    /// Invalid permission value
    InvalidPermission(u8),
    /// Zero address provided where non-zero required
    ZeroAddress,
    /// Invalid array length
    InvalidArrayLength,
    /// Context not available (TLS not set)
    NoContext,
    /// Caller is not authorized for this operation
    Unauthorized,
    /// Contract is already registered
    AlreadyRegistered,
    /// Contract is not registered
    NotRegistered,
    /// Caller is not the admin
    NotAdmin,
    /// Caller is not the slot owner
    NotOwner,
    /// Invalid slot configuration
    InvalidSlotConfig(String),
}

impl fmt::Display for PrecompileInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooShort { expected, actual } => {
                write!(f, "input too short: expected {} bytes, got {}", expected, actual)
            }
            Self::UnknownSelector(selector) => {
                write!(f, "unknown function selector: 0x{}", hex::encode(selector))
            }
            Self::InvalidSlotType(v) => write!(f, "invalid slot type: {}", v),
            Self::InvalidOwnershipType(v) => write!(f, "invalid ownership type: {}", v),
            Self::InvalidPermission(v) => write!(f, "invalid permission value: {}", v),
            Self::ZeroAddress => write!(f, "zero address not allowed"),
            Self::InvalidArrayLength => write!(f, "invalid array length"),
            Self::NoContext => write!(f, "precompile context not available"),
            Self::Unauthorized => write!(f, "caller not authorized"),
            Self::AlreadyRegistered => write!(f, "contract already registered"),
            Self::NotRegistered => write!(f, "contract not registered"),
            Self::NotAdmin => write!(f, "caller is not the admin"),
            Self::NotOwner => write!(f, "caller is not the slot owner"),
            Self::InvalidSlotConfig(msg) => write!(f, "invalid slot config: {}", msg),
        }
    }
}

impl std::error::Error for PrecompileInputError {}

/// Encode an error message for revert data.
///
/// Uses the standard Solidity error encoding:
/// `Error(string)` selector (0x08c379a0) + ABI-encoded string
pub fn encode_revert_message(msg: &str) -> Vec<u8> {
    // Error(string) selector
    let selector: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];

    let msg_bytes = msg.as_bytes();
    let msg_len = msg_bytes.len();

    // Offset to string data (32 bytes), string length (32 bytes), string data (padded)
    let padded_len = (msg_len + 31) / 32 * 32;
    let total_len = 4 + 32 + 32 + padded_len;

    let mut result = Vec::with_capacity(total_len);
    result.extend_from_slice(&selector);

    // Offset to string data (always 32 for a single string)
    let mut offset = [0u8; 32];
    offset[31] = 32;
    result.extend_from_slice(&offset);

    // String length
    let mut len_bytes = [0u8; 32];
    len_bytes[24..32].copy_from_slice(&(msg_len as u64).to_be_bytes());
    result.extend_from_slice(&len_bytes);

    // String data (padded to 32 bytes)
    result.extend_from_slice(msg_bytes);
    result.resize(total_len, 0);

    result
}

/// Encode a simple boolean return value.
pub fn encode_bool(value: bool) -> Vec<u8> {
    let mut result = [0u8; 32];
    if value {
        result[31] = 1;
    }
    result.to_vec()
}

/// Encode a simple success return (true).
pub fn encode_success() -> Vec<u8> {
    encode_bool(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_bool_true() {
        let encoded = encode_bool(true);
        assert_eq!(encoded.len(), 32);
        assert_eq!(encoded[31], 1);
        assert!(encoded[..31].iter().all(|&b| b == 0));
    }

    #[test]
    fn test_encode_bool_false() {
        let encoded = encode_bool(false);
        assert_eq!(encoded.len(), 32);
        assert!(encoded.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_encode_revert_message() {
        let msg = "test error";
        let encoded = encode_revert_message(msg);

        // Check selector
        assert_eq!(&encoded[0..4], &[0x08, 0xc3, 0x79, 0xa0]);

        // Check offset (32)
        assert_eq!(encoded[35], 32);

        // Check length (10)
        assert_eq!(encoded[67], 10);

        // Check message content
        assert_eq!(&encoded[68..78], b"test error");
    }

    #[test]
    fn test_error_display() {
        let err = PrecompileInputError::InputTooShort { expected: 100, actual: 50 };
        assert!(format!("{}", err).contains("100"));
        assert!(format!("{}", err).contains("50"));

        let err = PrecompileInputError::UnknownSelector([0xde, 0xad, 0xbe, 0xef]);
        assert!(format!("{}", err).contains("deadbeef"));

        let err = PrecompileInputError::InvalidSlotType(99);
        assert!(format!("{}", err).contains("99"));
    }
}
