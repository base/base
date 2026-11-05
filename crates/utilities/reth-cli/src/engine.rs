//! Reth engine persistence-default initialization utilities.

use reth_node_core::args::DefaultEngineValues;

/// Reth's defaults are block counts sized for 2s blocks; these keep the same time margins at
/// Denim's 200ms block interval. The memory block buffer target and state masking window keep
/// reth's defaults, which must together stay below the persistence threshold.
const PERSISTENCE_THRESHOLD: u64 = 70;
const PERSISTENCE_BACKPRESSURE_THRESHOLD: u64 = 160;

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
            .with_persistence_backpressure_threshold(PERSISTENCE_BACKPRESSURE_THRESHOLD)
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
        assert_eq!(args.persistence_threshold, 70);
        assert_eq!(args.persistence_backpressure_threshold(), 160);
        args.validate().unwrap();
    }
}
