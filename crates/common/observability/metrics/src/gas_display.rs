use alloc::{format, string::String};
use core::time::Duration;

/// Formats gas amounts and execution throughput for logs.
#[derive(Debug)]
pub struct GasDisplay;

impl GasDisplay {
    /// Represents one Kilogas, or `1_000` gas.
    pub const KILOGAS: u64 = 1_000;

    /// Represents one Megagas, or `1_000_000` gas.
    pub const MEGAGAS: u64 = Self::KILOGAS * 1_000;

    /// Represents one Gigagas, or `1_000_000_000` gas.
    pub const GIGAGAS: u64 = Self::MEGAGAS * 1_000;

    /// Represents one Teragas, or `1_000_000_000_000` gas.
    pub const TERAGAS: u64 = Self::GIGAGAS * 1_000;

    /// Returns a formatted gas throughput log, showing either:
    ///  * "Kgas/s", or 1,000 gas per second
    ///  * "Mgas/s", or 1,000,000 gas per second
    ///  * "Ggas/s", or 1,000,000,000 gas per second
    ///  * "Tgas/s", or 1,000,000,000,000 gas per second
    ///
    /// Depending on the magnitude of the gas throughput.
    pub fn throughput(gas: u64, execution_duration: Duration) -> String {
        let gas_per_second = gas as f64 / execution_duration.as_secs_f64();
        if gas_per_second < Self::MEGAGAS as f64 {
            format!("{:.2}Kgas/second", gas_per_second / Self::KILOGAS as f64)
        } else if gas_per_second < Self::GIGAGAS as f64 {
            format!("{:.2}Mgas/second", gas_per_second / Self::MEGAGAS as f64)
        } else if gas_per_second < Self::TERAGAS as f64 {
            format!("{:.2}Ggas/second", gas_per_second / Self::GIGAGAS as f64)
        } else {
            format!("{:.2}Tgas/second", gas_per_second / Self::TERAGAS as f64)
        }
    }

    /// Returns a formatted gas log, showing either:
    ///  * "Kgas", or 1,000 gas
    ///  * "Mgas", or 1,000,000 gas
    ///  * "Ggas", or 1,000,000,000 gas
    ///  * "Tgas", or 1,000,000,000,000 gas
    ///
    /// Depending on the magnitude of gas.
    pub fn amount(gas: u64) -> String {
        let gas = gas as f64;
        if gas < Self::MEGAGAS as f64 {
            format!("{:.2}Kgas", gas / Self::KILOGAS as f64)
        } else if gas < Self::GIGAGAS as f64 {
            format!("{:.2}Mgas", gas / Self::MEGAGAS as f64)
        } else if gas < Self::TERAGAS as f64 {
            format!("{:.2}Ggas", gas / Self::GIGAGAS as f64)
        } else {
            format!("{:.2}Tgas", gas / Self::TERAGAS as f64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gas_fmt() {
        let gas = 888;
        let gas_unit = GasDisplay::amount(gas);
        assert_eq!(gas_unit, "0.89Kgas");

        let gas = 100_000;
        let gas_unit = GasDisplay::amount(gas);
        assert_eq!(gas_unit, "100.00Kgas");

        let gas = 100_000_000;
        let gas_unit = GasDisplay::amount(gas);
        assert_eq!(gas_unit, "100.00Mgas");

        let gas = 100_000_000_000;
        let gas_unit = GasDisplay::amount(gas);
        assert_eq!(gas_unit, "100.00Ggas");

        let gas = 100_000_000_000_000;
        let gas_unit = GasDisplay::amount(gas);
        assert_eq!(gas_unit, "100.00Tgas");
    }

    #[test]
    fn test_gas_throughput_fmt() {
        let duration = Duration::from_secs(1);
        let gas = 100_000;
        let throughput = GasDisplay::throughput(gas, duration);
        assert_eq!(throughput, "100.00Kgas/second");

        let gas = 100_000_000;
        let throughput = GasDisplay::throughput(gas, duration);
        assert_eq!(throughput, "100.00Mgas/second");

        let gas = 100_000_000_000;
        let throughput = GasDisplay::throughput(gas, duration);
        assert_eq!(throughput, "100.00Ggas/second");
    }
}
