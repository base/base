//! Host backend implementations for offline and online proof generation.

mod offline;
pub use offline::OfflineHostBackend;

mod online;
pub use online::OnlineHostBackend;
