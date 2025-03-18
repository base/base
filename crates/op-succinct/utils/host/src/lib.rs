pub mod block_range;
mod contract;
pub mod fetcher;
pub mod hosts;
mod proof;
pub mod stats;
pub use contract::*;
pub use proof::*;
pub mod metrics;

use clap::{Parser, ValueEnum};
use strum_macros::EnumString;

pub const RANGE_ELF_BUMP: &[u8] = include_bytes!("../../../elf/range-elf-bump");
pub const RANGE_ELF_EMBEDDED: &[u8] = include_bytes!("../../../elf/range-elf-embedded");
pub const AGGREGATION_ELF: &[u8] = include_bytes!("../../../elf/aggregation-elf");

// TODO: Update to Celestia Range ELF Embedded
pub const CELESTIA_RANGE_ELF_EMBEDDED: &[u8] = include_bytes!("../../../elf/range-elf-embedded");

// TODO: Update to EigenDA Range ELF Embedded
pub const EIGENDA_RANGE_ELF_EMBEDDED: &[u8] = include_bytes!("../../../elf/range-elf-embedded");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Parser, ValueEnum, EnumString)]
/// The configuration for the DA provider.
pub enum DAConfig {
    /// The default DA configuration.
    Default,
    /// The Celestia DA configuration.
    Celestia,
    /// The EigenDA DA configuration.
    EigenDA,
}
