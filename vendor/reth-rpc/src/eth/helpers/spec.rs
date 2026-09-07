use alloy_primitives::U256;
use reth_rpc_eth_api::{RpcNodeCore, helpers::EthApiSpec};

use crate::EthApi;

impl<N> EthApiSpec for EthApi<N>
where
    N: RpcNodeCore,
{
    fn starting_block(&self) -> U256 {
        self.inner.starting_block()
    }
}
