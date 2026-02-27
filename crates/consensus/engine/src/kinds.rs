//! Contains the different kinds of execution engine clients that can be used.

use derive_more::{Display, FromStr};

/// Identifies the type of execution layer client for behavior customization.
///
/// Different execution clients may have slight variations in API behavior
/// or supported features. This enum allows the engine to adapt its behavior
/// accordingly, though as of v0.1.0, behavior is equivalent across all types.
///
/// # Examples
///
/// ```rust
/// use base_consensus_engine::EngineKind;
/// use std::str::FromStr;
///
/// // Parse from string
/// let kind = EngineKind::from_str("geth").unwrap();
/// assert_eq!(kind, EngineKind::Geth);
///
/// // Display as string
/// assert_eq!(EngineKind::Reth.to_string(), "reth");
/// ```
#[derive(Debug, Display, FromStr, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    /// Geth execution client.
    #[display("geth")]
    Geth,
    /// Reth execution client.
    #[display("reth")]
    Reth,
    /// Erigon execution client.
    #[display("erigon")]
    Erigon,
}

impl EngineKind {
    /// Contains all valid engine client kinds.
    pub const KINDS: [Self; 3] = [Self::Geth, Self::Reth, Self::Erigon];
}
