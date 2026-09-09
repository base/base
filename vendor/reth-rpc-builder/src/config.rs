use std::net::SocketAddr;

use jsonrpsee::server::ServerConfigBuilder;
use reth_node_core::args::RpcServerArgs;
use reth_rpc_eth_types::{EthConfig, EthStateCacheConfig, GasPriceOracleConfig};
use reth_rpc_layer::JwtSecret;
use reth_rpc_server_types::RpcModuleSelection;
use tracing::warn;

use crate::{RpcModuleConfig, RpcServerConfig, TransportRpcModuleConfig};

/// A trait that provides a configured RPC server.
///
/// This provides all basic config values for the RPC server and is implemented by the
/// [`RpcServerArgs`] type.
pub trait RethRpcServerConfig {
    /// The configured ethereum RPC settings.
    fn eth_config(&self) -> EthConfig;

    /// Returns state cache configuration.
    fn state_cache_config(&self) -> EthStateCacheConfig;

    /// Returns the max request size in bytes.
    fn rpc_max_request_size_bytes(&self) -> u32;

    /// Returns the max response size in bytes.
    fn rpc_max_response_size_bytes(&self) -> u32;

    /// Extracts the gas price oracle config from the args.
    fn gas_price_oracle_config(&self) -> GasPriceOracleConfig;

    /// Creates the [`TransportRpcModuleConfig`] from cli args.
    ///
    /// This sets all the api modules, and configures additional settings like gas price oracle
    /// settings in the [`TransportRpcModuleConfig`].
    fn transport_rpc_module_config(&self) -> TransportRpcModuleConfig;

    /// Returns the default server config for http/ws
    fn http_ws_server_builder(&self) -> ServerConfigBuilder;

    /// Creates the [`RpcServerConfig`] from cli args.
    fn rpc_server_config(&self) -> RpcServerConfig;

    /// Returns whether built-in RPC request metrics are enabled.
    fn rpc_metrics_enabled(&self) -> bool;

    /// Returns the configured jwt secret key for the regular rpc servers, if any.
    ///
    /// Note: this is not used for the auth server (engine API).
    fn rpc_secret_key(&self) -> Option<JwtSecret>;
}

impl RethRpcServerConfig for RpcServerArgs {
    fn eth_config(&self) -> EthConfig {
        EthConfig::default()
            .max_tracing_requests(self.rpc_max_tracing_requests)
            .max_blocking_io_requests(self.rpc_max_blocking_io_requests)
            .max_trace_filter_blocks(self.rpc_max_trace_filter_blocks)
            .max_blocks_per_filter(self.rpc_max_blocks_per_filter.unwrap_or_max())
            .max_logs_per_response(self.rpc_max_logs_per_response.unwrap_or_max() as usize)
            .eth_proof_window(self.rpc_eth_proof_window)
            .rpc_gas_cap(self.rpc_gas_cap)
            .rpc_max_simulate_blocks(self.rpc_max_simulate_blocks)
            .compute_state_root_for_eth_simulate(self.rpc_compute_state_root_for_eth_simulate)
            .state_cache(self.state_cache_config())
            .gpo_config(self.gas_price_oracle_config())
            .proof_permits(self.rpc_proof_permits)
            .pending_block_kind(self.rpc_pending_block)
            .raw_tx_forwarder(self.rpc_forwarder.clone())
            .rpc_evm_memory_limit(self.rpc_evm_memory_limit)
            .force_blob_sidecar_upcasting(self.rpc_force_blob_sidecar_upcasting)
    }

    fn state_cache_config(&self) -> EthStateCacheConfig {
        EthStateCacheConfig {
            max_blocks: self.rpc_state_cache.max_blocks,
            max_receipts: self.rpc_state_cache.max_receipts,
            max_headers: self.rpc_state_cache.max_headers,
            max_bals: self.rpc_state_cache.max_bals,
            max_concurrent_db_requests: self.rpc_state_cache.max_concurrent_db_requests,
            max_cached_tx_hashes: self.rpc_state_cache.max_cached_tx_hashes,
        }
    }

    fn rpc_max_request_size_bytes(&self) -> u32 {
        self.rpc_max_request_size.get().saturating_mul(1024 * 1024)
    }

    fn rpc_max_response_size_bytes(&self) -> u32 {
        self.rpc_max_response_size.get().saturating_mul(1024 * 1024)
    }

    fn gas_price_oracle_config(&self) -> GasPriceOracleConfig {
        self.gas_price_oracle.gas_price_oracle_config()
    }

    fn transport_rpc_module_config(&self) -> TransportRpcModuleConfig {
        let mut config = TransportRpcModuleConfig::default()
            .with_config(RpcModuleConfig::new(self.eth_config()));

        if self.http {
            config = config.with_http(
                self.http_api
                    .clone()
                    .unwrap_or_else(|| RpcModuleSelection::standard_modules().into()),
            );
        }

        if self.ws {
            config = config.with_ws(
                self.ws_api
                    .clone()
                    .unwrap_or_else(|| RpcModuleSelection::standard_modules().into()),
            );
        }

        config
    }

    fn http_ws_server_builder(&self) -> ServerConfigBuilder {
        ServerConfigBuilder::new()
            .max_connections(self.rpc_max_connections.get())
            .max_request_body_size(self.rpc_max_request_size_bytes())
            .max_response_body_size(self.rpc_max_response_size_bytes())
            .max_subscriptions_per_connection(self.rpc_max_subscriptions_per_connection.get())
    }

    fn rpc_server_config(&self) -> RpcServerConfig {
        let mut config = RpcServerConfig::default()
            .with_jwt_secret(self.rpc_secret_key())
            .with_rpc_metrics_enabled(self.rpc_metrics_enabled());

        if self.http_api.is_some() && !self.http {
            warn!(
                target: "reth::cli",
                "The --http.api flag is set but --http is not enabled. HTTP RPC API will not be exposed."
            );
        }

        if self.ws_api.is_some() && !self.ws {
            warn!(
                target: "reth::cli",
                "The --ws.api flag is set but --ws is not enabled. WS RPC API will not be exposed."
            );
        }

        if self.http {
            let socket_address = SocketAddr::new(self.http_addr, self.http_port);
            config = config
                .with_http_address(socket_address)
                .with_http(self.http_ws_server_builder())
                .with_http_cors(self.http_corsdomain.clone())
                .with_http_disable_compression(self.http_disable_compression);
        }

        if self.ws {
            let socket_address = SocketAddr::new(self.ws_addr, self.ws_port);
            // Ensure WS CORS is applied regardless of HTTP being enabled
            config = config
                .with_ws_address(socket_address)
                .with_ws(self.http_ws_server_builder())
                .with_ws_cors(self.ws_allowed_origins.clone());
        }

        config
    }

    fn rpc_metrics_enabled(&self) -> bool {
        !self.rpc_disable_metrics
    }

    fn rpc_secret_key(&self) -> Option<JwtSecret> {
        self.rpc_jwtsecret
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use clap::{Args, Parser};
    use reth_node_core::args::RpcServerArgs;
    use reth_rpc_eth_types::RPC_DEFAULT_GAS_CAP;
    use reth_rpc_server_types::{RethRpcModule, RpcModuleSelection, constants};

    use crate::config::RethRpcServerConfig;

    /// A helper type to parse Args more easily
    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    #[test]
    fn test_rpc_gas_cap() {
        let args = CommandParser::<RpcServerArgs>::parse_from(["reth"]).args;
        let config = args.eth_config();
        assert_eq!(config.rpc_gas_cap, u64::from(RPC_DEFAULT_GAS_CAP));

        let args =
            CommandParser::<RpcServerArgs>::parse_from(["reth", "--rpc.gascap", "1000"]).args;
        let config = args.eth_config();
        assert_eq!(config.rpc_gas_cap, 1000);

        let args = CommandParser::<RpcServerArgs>::try_parse_from(["reth", "--rpc.gascap", "0"]);
        assert!(args.is_err());
    }

    #[test]
    fn test_transport_rpc_module_config() {
        let args = CommandParser::<RpcServerArgs>::parse_from([
            "reth",
            "--http.api",
            "eth,admin,debug",
            "--http",
            "--ws",
        ])
        .args;
        let config = args.transport_rpc_module_config();
        let expected = [RethRpcModule::Eth, RethRpcModule::Admin, RethRpcModule::Debug];
        assert_eq!(config.http().cloned().unwrap().into_selection(), expected.into());
        assert_eq!(
            config.ws().cloned().unwrap().into_selection(),
            RpcModuleSelection::standard_modules()
        );
    }

    #[test]
    fn test_transport_rpc_module_trim_config() {
        let args = CommandParser::<RpcServerArgs>::parse_from([
            "reth",
            "--http.api",
            " eth, admin, debug",
            "--http",
            "--ws",
        ])
        .args;
        let config = args.transport_rpc_module_config();
        let expected = [RethRpcModule::Eth, RethRpcModule::Admin, RethRpcModule::Debug];
        assert_eq!(config.http().cloned().unwrap().into_selection(), expected.into());
        assert_eq!(
            config.ws().cloned().unwrap().into_selection(),
            RpcModuleSelection::standard_modules()
        );
    }

    #[test]
    fn test_unique_rpc_modules() {
        let args = CommandParser::<RpcServerArgs>::parse_from([
            "reth",
            "--http.api",
            " eth, admin, debug, eth,admin",
            "--http",
            "--ws",
        ])
        .args;
        let config = args.transport_rpc_module_config();
        let expected = [RethRpcModule::Eth, RethRpcModule::Admin, RethRpcModule::Debug];
        assert_eq!(config.http().cloned().unwrap().into_selection(), expected.into());
        assert_eq!(
            config.ws().cloned().unwrap().into_selection(),
            RpcModuleSelection::standard_modules()
        );
    }

    #[test]
    fn test_rpc_server_config() {
        let args = CommandParser::<RpcServerArgs>::parse_from([
            "reth",
            "--http.api",
            "eth,admin,debug",
            "--http",
            "--ws",
            "--ws.addr",
            "127.0.0.1",
            "--ws.port",
            "8888",
        ])
        .args;
        let config = args.rpc_server_config();
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
        let config = args.rpc_server_config();
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

        let config = args.eth_config().filter_config();
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

        let config = args.eth_config().filter_config();
        assert_eq!(config.max_blocks_per_filter, Some(100));
        assert_eq!(config.max_logs_per_response, Some(200));
    }
}
