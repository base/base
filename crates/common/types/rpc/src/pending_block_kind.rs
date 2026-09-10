use alloc::{format, string::String};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Config for the locally built pending block
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum PendingBlockKind {
    /// Return a pending block with header only, no transactions included
    Empty,
    /// Return null/no pending block
    None,
    /// Return a pending block with all transactions from the mempool (default behavior)
    #[default]
    Full,
}

impl core::str::FromStr for PendingBlockKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "empty" => Ok(Self::Empty),
            "none" => Ok(Self::None),
            "full" => Ok(Self::Full),
            _ => Err(format!(
                "Invalid pending block kind: {s}. Valid options are: empty, none, full"
            )),
        }
    }
}

impl PendingBlockKind {
    /// Returns true if the pending block kind is `None`
    pub const fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// Returns true if the pending block kind is `Empty`
    pub const fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }
}
