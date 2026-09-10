use std::net::SocketAddr;

use base_execution_rpc_handlers::{EthConfig, EthStateCacheConfig};
use jsonrpsee::server::ServerConfigBuilder;
use reth_node_core::args::RpcServerArgs;

use crate::{RpcModuleConfig, RpcServerConfig, TransportRpcModuleConfig};

/// Resolved settings for the Base RPC server and its handlers.
#[derive(Debug)]
pub struct RpcConfig {
    /// Ethereum API limits, caches, and execution settings.
    pub eth: EthConfig,
    /// API modules enabled on each transport.
    pub modules: TransportRpcModuleConfig,
    /// Transport listeners, authentication, and request limits.
    pub server: RpcServerConfig,
}

impl RpcConfig {
    /// Resolves CLI arguments once for RPC startup.
    pub fn new(args: &RpcServerArgs) -> Self {
        let cache = EthStateCacheConfig {
            max_blocks: args.rpc_state_cache.max_blocks,
            max_receipts: args.rpc_state_cache.max_receipts,
            max_headers: args.rpc_state_cache.max_headers,
            max_bals: args.rpc_state_cache.max_bals,
            max_concurrent_db_requests: args.rpc_state_cache.max_concurrent_db_requests,
            max_cached_tx_hashes: args.rpc_state_cache.max_cached_tx_hashes,
        };
        let gpo = args.gas_price_oracle.gas_price_oracle_config();
        let max_request = args.rpc_max_request_size.get().saturating_mul(1024 * 1024);
        let max_response = args.rpc_max_response_size.get().saturating_mul(1024 * 1024);
        let eth = EthConfig::default()
            .max_tracing_requests(args.rpc_max_tracing_requests)
            .max_blocking_io_requests(args.rpc_max_blocking_io_requests)
            .max_trace_filter_blocks(args.rpc_max_trace_filter_blocks)
            .max_blocks_per_filter(args.rpc_max_blocks_per_filter.unwrap_or_max())
            .max_logs_per_response(args.rpc_max_logs_per_response.unwrap_or_max() as usize)
            .eth_proof_window(args.rpc_eth_proof_window)
            .rpc_gas_cap(args.rpc_gas_cap)
            .rpc_max_simulate_blocks(args.rpc_max_simulate_blocks)
            .compute_state_root_for_eth_simulate(args.rpc_compute_state_root_for_eth_simulate)
            .state_cache(cache)
            .gpo_config(gpo)
            .proof_permits(args.rpc_proof_permits)
            .pending_block_kind(args.rpc_pending_block)
            .raw_tx_forwarder(args.rpc_forwarder.clone())
            .rpc_evm_memory_limit(args.rpc_evm_memory_limit)
            .force_blob_sidecar_upcasting(args.rpc_force_blob_sidecar_upcasting);
        let server_builder = ServerConfigBuilder::new()
            .max_connections(args.rpc_max_connections.get())
            .max_request_body_size(max_request)
            .max_response_body_size(max_response)
            .max_subscriptions_per_connection(args.rpc_max_subscriptions_per_connection.get());
        let mut modules =
            TransportRpcModuleConfig::default().with_config(RpcModuleConfig::new(eth.clone()));
        if args.http {
            modules = modules.with_http();
        }
        if args.ws {
            modules = modules.with_ws();
        }
        let server = {
            let mut config = RpcServerConfig::default()
                .with_jwt_secret(args.rpc_jwtsecret)
                .with_rpc_metrics_enabled(!args.rpc_disable_metrics);

            if args.http {
                let socket_address = SocketAddr::new(args.http_addr, args.http_port);
                config = config
                    .with_http_address(socket_address)
                    .with_http(server_builder.clone())
                    .with_http_cors(args.http_corsdomain.clone())
                    .with_http_disable_compression(args.http_disable_compression);
            }

            if args.ws {
                let socket_address = SocketAddr::new(args.ws_addr, args.ws_port);
                // Ensure WS CORS is applied regardless of HTTP being enabled
                config = config
                    .with_ws_address(socket_address)
                    .with_ws(server_builder.clone())
                    .with_ws_cors(args.ws_allowed_origins.clone());
            }

            config
        };
        Self { eth, modules, server }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use base_common_types_rpc as constants;
    use base_execution_rpc_handlers::RPC_DEFAULT_GAS_CAP;
    use clap::{Args, Parser};
    use reth_node_core::args::RpcServerArgs;

    use crate::RpcConfig;

    /// A helper type to parse Args more easily
    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    #[test]
    fn test_rpc_gas_cap() {
        let args = CommandParser::<RpcServerArgs>::parse_from(["reth"]).args;
        let config = RpcConfig::new(&args).eth;
        assert_eq!(config.rpc_gas_cap, u64::from(RPC_DEFAULT_GAS_CAP));

        let args =
            CommandParser::<RpcServerArgs>::parse_from(["reth", "--rpc.gascap", "1000"]).args;
        let config = RpcConfig::new(&args).eth;
        assert_eq!(config.rpc_gas_cap, 1000);

        let args = CommandParser::<RpcServerArgs>::try_parse_from(["reth", "--rpc.gascap", "0"]);
        assert!(args.is_err());
    }

    #[test]
    fn test_rpc_server_config() {
        let args = CommandParser::<RpcServerArgs>::parse_from([
            "reth",
            "--http",
            "--ws",
            "--ws.addr",
            "127.0.0.1",
            "--ws.port",
            "8888",
        ])
        .args;
        let config = RpcConfig::new(&args).server;
        assert_eq!(
            config.http_address().unwrap(),
            SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::LOCALHOST,
                constants::DEFAULT_HTTP_RPC_PORT
            ))
        );
        assert_eq!(
            config.ws_address().unwrap(),
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8888))
        );

        assert!(config.rpc_metrics_enabled());
    }

    #[test]
    fn test_rpc_server_config_disable_metrics() {
        let args =
            CommandParser::<RpcServerArgs>::parse_from(["reth", "--rpc.disable-metrics"]).args;
        let config = RpcConfig::new(&args).server;
        assert!(!config.rpc_metrics_enabled());
    }

    #[test]
    fn test_zero_filter_limits() {
        let args = CommandParser::<RpcServerArgs>::parse_from([
            "reth",
            "--rpc-max-blocks-per-filter",
            "0",
            "--rpc-max-logs-per-response",
            "0",
        ])
        .args;

        let config = RpcConfig::new(&args).eth.filter_config();
        assert_eq!(config.max_blocks_per_filter, Some(u64::MAX));
        assert_eq!(config.max_logs_per_response, Some(usize::MAX));
    }

    #[test]
    fn test_custom_filter_limits() {
        let args = CommandParser::<RpcServerArgs>::parse_from([
            "reth",
            "--rpc-max-blocks-per-filter",
            "100",
            "--rpc-max-logs-per-response",
            "200",
        ])
        .args;

        let config = RpcConfig::new(&args).eth.filter_config();
        assert_eq!(config.max_blocks_per_filter, Some(100));
        assert_eq!(config.max_logs_per_response, Some(200));
    }
}
