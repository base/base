//! Base receipt loading.

pub use reth_rpc_eth_api::{BaseReceiptBuilder, BaseReceiptConverter, ReceiptFieldsBuilder};
use reth_rpc_eth_api::{RpcNodeCore, helpers::LoadReceipt};

use crate::BaseEthApi;

impl<N> LoadReceipt for BaseEthApi<N> where N: RpcNodeCore {}
