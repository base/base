//! Reth engine persistence-default initialization utilities.

use reth_node_core::args::DefaultEngineValues;

/// Block counts at Denim's 200ms block interval: persistence starts once 30s of blocks are in
/// memory, and state/trie writes for the newest 11s of persisted blocks stay in memory so
/// repeated writes to the same keys reach disk once. The memory block buffer target keeps reth's
/// default, and buffer target + masking window must stay below the persistence threshold. The
/// backpressure threshold derives to twice the persistence threshold.
const PERSISTENCE_THRESHOLD: u64 = 150;
const NUM_STATE_MASKING_BLOCKS: u64 = 55;

/// Reth engine default initialization for Base execution layer binaries.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Engine;

impl Engine {
    /// Initializes Reth's global engine persistence defaults.
    ///
    /// Explicit `--engine.*` flags still override these values.
    ///
    /// ### Panics
    ///
    /// Panics if the engine defaults were already initialized.
    pub fn init_engine() {
        DefaultEngineValues::default()
            .with_persistence_threshold(PERSISTENCE_THRESHOLD)
            .with_num_state_masking_blocks(NUM_STATE_MASKING_BLOCKS)
            .try_init()
            .expect("failed to initialize engine defaults");
    }
}

/// Initializes Reth's global engine persistence defaults.
///
/// Use this in execution layer binaries (base-node-reth, base-builder) before parsing the CLI.
#[macro_export]
macro_rules! init_engine {
    () => {
        $crate::Engine::init_engine()
    };
}

#[cfg(test)]
mod tests {
    use reth_node_core::args::EngineArgs;

    use super::*;

    #[test]
    fn engine_defaults_scale_for_denim_blocks() {
        Engine::init_engine();

        let args = EngineArgs::default();
        assert_eq!(args.persistence_threshold, 150);
        assert_eq!(args.memory_block_buffer_target(), 5);
        assert_eq!(args.num_state_masking_blocks(), 55);
        assert_eq!(args.persistence_backpressure_threshold(), 300);
        args.validate().unwrap();
    }
}
