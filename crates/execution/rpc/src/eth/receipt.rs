//! Base receipt loading.

pub use reth_rpc_eth_api::{BaseReceiptBuilder, BaseReceiptConverter, ReceiptFieldsBuilder};
use reth_rpc_eth_api::{RpcConvert, RpcNodeCore, helpers::LoadReceipt};

use crate::{BaseEthApi, BaseEthApiError};

impl<N, Rpc> LoadReceipt for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Error = BaseEthApiError>,
{
}
