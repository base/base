//! Backend implementations for the proof host.

mod offline;
pub use offline::OfflineHostBackend;

mod online;
pub use online::OnlineHostBackend;
