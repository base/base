//! Contains RPC handler implementations specific to streams subscriptions.

use reth_rpc_eth_api::{RpcNodeCore, helpers::EthSubscriptions};

use crate::EthApi;

impl<N> EthSubscriptions for EthApi<N> where N: RpcNodeCore {}
