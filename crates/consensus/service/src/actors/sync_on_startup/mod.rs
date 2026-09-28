//! Sync-on-startup: follow the canonical chain like a validator, then switch once to the
//! configured sequencer mode.

mod config;
pub use config::{SyncOnStartupConfig, SyncProgress};

mod engine_handler;
pub use engine_handler::{StartupSyncEngineRequestHandler, StartupSyncHandoffRequest};
