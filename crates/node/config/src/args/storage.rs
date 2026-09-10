//! Storage configuration arguments.

use clap::Args;

/// Storage uses the v2 layout for every new database.
#[derive(Debug, Default, Args, PartialEq, Eq, Clone, Copy)]
pub struct StorageArgs {}
