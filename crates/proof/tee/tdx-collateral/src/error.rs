//! Error type for Intel TDX collateral hydration.

use std::error::Error;

/// TDX collateral hydration error.
pub type TdxCollateralError = Box<dyn Error + Send + Sync>;

/// Convenience result alias for TDX collateral hydration.
pub type Result<T> = std::result::Result<T, TdxCollateralError>;
