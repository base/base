//! Optional serialization bounds for sealed block data.

/// Helper trait that requires de-/serialize implementation since `serde` feature is enabled.
#[cfg(feature = "serde")]
pub trait MaybeSerde: serde::Serialize + for<'de> serde::Deserialize<'de> {}
/// Noop. Helper trait that would require de-/serialize implementation if `serde` feature were
/// enabled.
#[cfg(not(feature = "serde"))]
pub trait MaybeSerde {}

#[cfg(feature = "serde")]
impl<T> MaybeSerde for T where T: serde::Serialize + for<'de> serde::Deserialize<'de> {}
#[cfg(not(feature = "serde"))]
impl<T> MaybeSerde for T {}
