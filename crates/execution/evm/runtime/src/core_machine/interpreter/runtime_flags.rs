#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::hardfork::SpecId;

/// Runtime flags that control interpreter execution behavior.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct RuntimeFlags {
    /// Whether the current execution context is static (read-only).
    pub is_static: bool,
    /// The current EVM specification ID.
    pub spec_id: SpecId,
}
