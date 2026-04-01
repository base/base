//! Signal types for the `base-consensus-derive` pipeline.
//!
//! Signals are the primary method of communication in the downwards direction
//! of the pipeline. They allow the pipeline driver to perform actions such as
//! resetting all stages in the pipeline through message passing.

use base_protocol::{BlockInfo, L2BlockInfo};

/// A signal to send to the pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Reset the pipeline.
    Reset(ResetSignal),
    /// Hardfork Activation.
    Activation(ActivationSignal),
    /// Flush the currently active channel.
    FlushChannel,
    /// Provide a new L1 block to the L1 traversal stage.
    ProvideBlock(BlockInfo),
}

impl core::fmt::Display for Signal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Reset(_) => write!(f, "reset"),
            Self::Activation(_) => write!(f, "activation"),
            Self::FlushChannel => write!(f, "flush_channel"),
            Self::ProvideBlock(_) => write!(f, "provide_block"),
        }
    }
}

/// A pipeline reset signal.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ResetSignal {
    /// The L2 safe head to reset to.
    pub l2_safe_head: L2BlockInfo,
}

impl ResetSignal {
    /// Creates a new [`Signal::Reset`] from the [`ResetSignal`].
    pub const fn signal(self) -> Signal {
        Signal::Reset(self)
    }
}

/// A pipeline hardfork activation signal.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ActivationSignal {
    /// The L2 safe head to reset to.
    pub l2_safe_head: L2BlockInfo,
}

impl ActivationSignal {
    /// Creates a new [`Signal::Activation`] from the [`ActivationSignal`].
    pub const fn signal(self) -> Signal {
        Signal::Activation(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reset_signal() {
        let signal = ResetSignal::default();
        assert_eq!(signal.signal(), Signal::Reset(signal));
    }

    #[test]
    fn test_activation_signal() {
        let signal = ActivationSignal::default();
        assert_eq!(signal.signal(), Signal::Activation(signal));
    }
}
