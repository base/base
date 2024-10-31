use std::fmt;

use alloy_primitives::U256;
use anyhow::Result;
use op_succinct_host_utils::fetcher::FeeData;

pub struct AggregateFeeData {
    pub start: u64,
    pub end: u64,
    pub num_transactions: u64,
    pub total_l1_fee: U256,
    pub total_tx_fees: u128,
}

impl fmt::Display for AggregateFeeData {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let eth = self.total_l1_fee / U256::from(10).pow(U256::from(18));
        let gwei = (self.total_l1_fee / U256::from(10).pow(U256::from(9)))
            % U256::from(10).pow(U256::from(9));
        write!(
            f,
            "Start: {}, End: {}, Aggregate: {} transactions, {}.{:09} ETH L1 fee",
            self.start, self.end, self.num_transactions, eth, gwei
        )
    }
}

pub fn aggregate_fee_data(fee_data: Vec<FeeData>) -> Result<AggregateFeeData> {
    let mut aggregate_fee_data = AggregateFeeData {
        start: fee_data[0].block_number,
        end: fee_data[fee_data.len() - 1].block_number,
        num_transactions: 0,
        total_l1_fee: U256::ZERO,
        total_tx_fees: 0,
    };

    for data in fee_data {
        aggregate_fee_data.num_transactions += 1;
        aggregate_fee_data.total_l1_fee += data.l1_gas_cost;
        aggregate_fee_data.total_tx_fees += data.tx_fee;
    }

    Ok(aggregate_fee_data)
}
