#![allow(missing_docs)]
//! The zkvm ELF binaries.

pub const AGGREGATION_ELF: &[u8] = include_bytes!("../../../elf/aggregation-elf");

pub const RANGE_ELF_EMBEDDED: &[u8] = include_bytes!("../../../elf/range-elf-embedded");
