//! Block abstraction.
//!
//! This module provides the core block types and transformations:
//!
//! ```rust
//! # use base_common_types_chain::{SealedBlock, RecoveredBlock};
//! # fn example(block: base_common_types_chain::BaseBlock) -> Result<(), Box<dyn std::error::Error>> {
//! // Basic block flow
//!
//! // Seal (compute hash)
//! let sealed: SealedBlock = block.seal();
//!
//! // Recover senders
//! let recovered: RecoveredBlock = sealed.try_recover()?;
//!
//! // Access components
//! let senders = recovered.senders();
//! let hash = recovered.hash();
//! # Ok(())
//! # }
//! ```

mod sealed;
pub use sealed::{SealedBlock, SealedBlockWith};

mod sealed_or_recovered;
pub use sealed_or_recovered::SealedOrRecoveredBlock;

mod recovered;
pub use recovered::{IndexedTx, RecoveredBlock};

mod body;
pub use body::BlockBody;
mod error;
pub use error::{BlockRecoveryError, SealedBlockRecoveryError};
mod header;
pub use header::BlockHeader;
