//! The stablecoin-specific `IB20Stablecoin` wire surface at Cobalt. The stablecoin extension did not
//! change at Cobalt, so this aliases the frozen [`v1`](super::v1) surface rather than redefining it;
//! `IB20StablecoinV1` and `IB20StablecoinV2` are the same type until the surface actually moves.

pub use super::v1::IB20Stablecoin;
