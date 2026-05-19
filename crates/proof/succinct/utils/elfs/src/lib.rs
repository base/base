#![doc = include_str!("../README.md")]
/// Aggregation program ELF binary.
pub const AGGREGATION_ELF: &[u8] = include_bytes!(env!("AGGREGATION_ELF_PATH"));

/// Range program ELF binary.
pub const RANGE_ELF_EMBEDDED: &[u8] = include_bytes!(env!("RANGE_ELF_EMBEDDED_PATH"));
