//! Metering RPC settings.

use crate::MeteredOpcodes;

/// Configuration for metering RPC.
#[derive(Debug)]
pub struct MeteringConfig {
    /// Whether metering is enabled.
    pub enabled: bool,

    /// Opcodes and precompiles to track for gas metering.
    pub metered_opcodes: MeteredOpcodes,
}

impl MeteringConfig {
    /// Creates a configuration with metering disabled.
    pub fn disabled() -> Self {
        Self { enabled: false, ..Self::enabled() }
    }

    /// Creates a configuration with metering enabled.
    pub fn enabled() -> Self {
        Self { enabled: true, metered_opcodes: MeteredOpcodes::default() }
    }

    /// Sets the opcodes and precompiles to track for gas metering.
    pub fn with_metered_opcodes(mut self, opcodes: MeteredOpcodes) -> Self {
        self.metered_opcodes = opcodes;
        self
    }
}
