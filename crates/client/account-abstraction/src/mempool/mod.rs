//! UserOperation Mempool
//!
//! This module implements an ERC-7562 compliant mempool for UserOperations.
//! It provides:
//! - Per-entrypoint UserOperation storage
//! - Reputation tracking for entities (sender, paymaster, factory, aggregator)
//! - Mempool-specific validation rules (STO-040, STO-041, AUTH-040)
//! - Block watching for automatic removal of included UserOps
//! - Query operations for bundle building
//! - p2p propagation support (optional, for sequencer nodes)
//!
//! # Architecture
//!
//! The mempool is organized as follows:
//! - `UserOpPool`: Top-level container holding per-entrypoint pools
//! - `EntryPointPool`: Stores UserOps for a single entrypoint
//! - `ReputationManager`: Tracks entity reputation (opsSeen, opsIncluded)
//! - `BlockWatcher`: Monitors chain for included UserOps
//!
//! # Modes
//!
//! The mempool supports two modes (mutually exclusive):
//! - **Tips Mode** (default): UserOps are forwarded to TIPS service
//! - **Mempool Mode**: UserOps are stored locally (for sequencer nodes)
//!
//! When mempool mode is enabled, p2p can optionally be configured to share
//! UserOps with other sequencer nodes in a private network.

mod block_watcher;
mod config;
mod error;
pub mod p2p;
mod pool;
mod provider;
mod reputation;
mod rules;

pub use block_watcher::{BlockWatcher, ParsedUserOpEvent};
pub use config::MempoolConfig;
pub use error::{MempoolError, MempoolResult};
pub use p2p::{GossipConfig, GossipError, UserOpGossip, UserOpGossipHandle, UserOpGossipMessage};
pub use pool::{EntryPointPool, PooledUserOp, UserOpPool, UserOpStatus};
pub use provider::{SharedUserOpMempoolProvider, UserOpMempoolProvider};
pub use reputation::{EntityReputation, ReputationManager, ReputationStatus};
pub use rules::MempoolRuleChecker;
