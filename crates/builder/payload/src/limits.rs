/// Flashblock resource limit calculator.

/// Computed resource limits for a single flashblock batch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlashblockBatchLimits {
    /// Gas budget for this flashblock.
    pub gas_per_batch: u64,
    /// DA bytes budget for this flashblock.
    pub da_per_batch: Option<u64>,
    /// DA footprint budget for this flashblock.
    pub da_footprint_per_batch: Option<u64>,
    /// Execution time budget in microseconds.
    pub execution_time_per_batch_us: Option<u128>,
    /// State root time budget in microseconds.
    pub state_root_time_per_batch_us: Option<u128>,
}

/// Flashblock resource limit calculator.
///
/// Computes per-flashblock resource budgets (gas, DA, execution time)
/// by distributing block-level limits evenly across flashblocks.
#[derive(Debug)]
pub struct FlashblockLimits;

impl FlashblockLimits {
    /// Calculate the initial resource limits for a flashblock batch.
    ///
    /// Distributes block-level gas, DA, and time budgets evenly across
    /// the target number of flashblocks.
    pub fn for_batch(
        block_gas_limit: u64,
        max_da_block_size: Option<u64>,
        da_footprint_scalar: Option<u16>,
        flashblock_execution_time_budget_us: Option<u128>,
        block_state_root_time_budget_us: Option<u128>,
        flashblocks_per_block: u64,
    ) -> FlashblockBatchLimits {
        let gas_per_batch = block_gas_limit / flashblocks_per_block;
        let da_per_batch = max_da_block_size.map(|da_limit| da_limit / flashblocks_per_block);
        let da_footprint_per_batch =
            da_footprint_scalar.map(|_| block_gas_limit / flashblocks_per_block);
        let execution_time_per_batch_us = flashblock_execution_time_budget_us;
        let state_root_time_per_batch_us =
            block_state_root_time_budget_us.map(|budget| budget / flashblocks_per_block as u128);

        FlashblockBatchLimits {
            gas_per_batch,
            da_per_batch,
            da_footprint_per_batch,
            execution_time_per_batch_us,
            state_root_time_per_batch_us,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_gas_distribution() {
        let limits = FlashblockLimits::for_batch(
            30_000_000, // 30M gas
            None, None, None, None, 10, // 10 flashblocks
        );
        assert_eq!(limits.gas_per_batch, 3_000_000);
        assert_eq!(limits.da_per_batch, None);
        assert_eq!(limits.da_footprint_per_batch, None);
        assert_eq!(limits.execution_time_per_batch_us, None);
        assert_eq!(limits.state_root_time_per_batch_us, None);
    }

    #[test]
    fn da_limits_distributed() {
        let limits = FlashblockLimits::for_batch(
            30_000_000,
            Some(100_000), // 100KB DA limit
            Some(1),       // non-None scalar enables footprint
            None,
            None,
            5,
        );
        assert_eq!(limits.gas_per_batch, 6_000_000);
        assert_eq!(limits.da_per_batch, Some(20_000));
        // da_footprint uses gas_limit / flashblocks_per_block
        assert_eq!(limits.da_footprint_per_batch, Some(6_000_000));
    }

    #[test]
    fn da_limits_disabled() {
        let limits = FlashblockLimits::for_batch(
            30_000_000, None, // no DA limit
            None, // no scalar
            None, None, 5,
        );
        assert_eq!(limits.da_per_batch, None);
        assert_eq!(limits.da_footprint_per_batch, None);
    }

    #[test]
    fn execution_time_budgets() {
        let limits = FlashblockLimits::for_batch(
            30_000_000,
            None,
            None,
            Some(1_000_000),  // 1s execution budget
            Some(10_000_000), // 10s state root budget
            4,
        );
        assert_eq!(limits.gas_per_batch, 7_500_000);
        // execution time is NOT divided - it's per-flashblock already
        assert_eq!(limits.execution_time_per_batch_us, Some(1_000_000));
        // state root time IS divided across flashblocks
        assert_eq!(limits.state_root_time_per_batch_us, Some(2_500_000));
    }

    #[test]
    fn single_flashblock_gets_all_resources() {
        let limits = FlashblockLimits::for_batch(
            30_000_000,
            Some(100_000),
            Some(1),
            Some(500_000),
            Some(2_000_000),
            1,
        );
        assert_eq!(limits.gas_per_batch, 30_000_000);
        assert_eq!(limits.da_per_batch, Some(100_000));
        assert_eq!(limits.da_footprint_per_batch, Some(30_000_000));
        assert_eq!(limits.execution_time_per_batch_us, Some(500_000));
        assert_eq!(limits.state_root_time_per_batch_us, Some(2_000_000));
    }

    #[test]
    fn uneven_division_truncates() {
        let limits = FlashblockLimits::for_batch(
            100, // not evenly divisible by 3
            Some(100),
            None,
            None,
            Some(100),
            3,
        );
        // Integer division: 100 / 3 = 33
        assert_eq!(limits.gas_per_batch, 33);
        assert_eq!(limits.da_per_batch, Some(33));
        assert_eq!(limits.state_root_time_per_batch_us, Some(33));
    }
}
