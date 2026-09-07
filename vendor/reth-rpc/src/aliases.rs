use reth_rpc_convert::RpcConvert;
use reth_rpc_eth_types::EthApiError;

/// Boxed RPC converter.
pub type DynRpcConverter<Error = EthApiError> = Box<dyn RpcConvert<Error = Error>>;
