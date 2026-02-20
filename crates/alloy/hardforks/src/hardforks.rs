use alloy_hardforks::{EthereumHardforks, ForkCondition};

use crate::OpHardfork;

/// Extends [`EthereumHardforks`] with optimism helper methods.
#[auto_impl::auto_impl(&, Arc)]
pub trait OpHardforks: EthereumHardforks {
    /// Retrieves [`ForkCondition`] by an [`OpHardfork`]. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    fn op_fork_activation(&self, fork: OpHardfork) -> ForkCondition;

    /// Convenience method to check if [`OpHardfork::Bedrock`] is active at a given block
    /// number.
    fn is_bedrock_active_at_block(&self, block_number: u64) -> bool {
        self.op_fork_activation(OpHardfork::Bedrock).active_at_block(block_number)
    }

    /// Returns `true` if [`Regolith`](OpHardfork::Regolith) is active at given block
    /// timestamp.
    fn is_regolith_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.op_fork_activation(OpHardfork::Regolith).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Canyon`](OpHardfork::Canyon) is active at given block timestamp.
    fn is_canyon_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.op_fork_activation(OpHardfork::Canyon).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Ecotone`](OpHardfork::Ecotone) is active at given block timestamp.
    fn is_ecotone_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.op_fork_activation(OpHardfork::Ecotone).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Fjord`](OpHardfork::Fjord) is active at given block timestamp.
    fn is_fjord_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.op_fork_activation(OpHardfork::Fjord).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Granite`](OpHardfork::Granite) is active at given block timestamp.
    fn is_granite_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.op_fork_activation(OpHardfork::Granite).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Holocene`](OpHardfork::Holocene) is active at given block
    /// timestamp.
    fn is_holocene_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.op_fork_activation(OpHardfork::Holocene).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Isthmus`](OpHardfork::Isthmus) is active at given block
    /// timestamp.
    fn is_isthmus_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.op_fork_activation(OpHardfork::Isthmus).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Jovian`](OpHardfork::Jovian) is active at given block
    /// timestamp.
    fn is_jovian_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.op_fork_activation(OpHardfork::Jovian).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Interop`](OpHardfork::Interop) is active at given block
    /// timestamp.
    fn is_interop_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.op_fork_activation(OpHardfork::Interop).active_at_timestamp(timestamp)
    }
}
