//! Rpc CLI Arguments
//!
//! Flags for configuring the RPC server.

use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

use base_consensus_rpc::RpcBuilder;

/// RPC CLI Arguments
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcArgs {
    /// Whether to disable the rpc server.
    pub rpc_disabled: bool,
    /// Prevent the RPC server from attempting to restart.
    pub no_restart: bool,
    /// RPC listening address.
    pub listen_addr: IpAddr,
    /// RPC listening port.
    pub listen_port: u16,
    /// Enable the admin API.
    pub enable_admin: bool,
    /// File path used to persist state changes made via the admin API so they persist across
    /// restarts. Disabled if not set.
    pub admin_persistence: Option<PathBuf>,
    /// Enables websocket rpc server to track block production
    pub ws_enabled: bool,
    /// Enables development RPC endpoints for engine state introspection
    pub dev_enabled: bool,
}

impl Default for RpcArgs {
    fn default() -> Self {
        Self {
            rpc_disabled: false,
            no_restart: false,
            listen_addr: "0.0.0.0".parse().unwrap(),
            listen_port: 9545,
            enable_admin: false,
            admin_persistence: None,
            ws_enabled: false,
            dev_enabled: false,
        }
    }
}

impl From<RpcArgs> for Option<RpcBuilder> {
    fn from(args: RpcArgs) -> Self {
        if args.rpc_disabled {
            return None;
        }
        Some(RpcBuilder {
            no_restart: args.no_restart,
            socket: SocketAddr::new(args.listen_addr, args.listen_port),
            enable_admin: args.enable_admin,
            admin_persistence: args.admin_persistence,
            ws_enabled: args.ws_enabled,
            dev_enabled: args.dev_enabled,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    #[test]
    fn test_default_rpc_args() {
        let args = RpcArgs::default();
        assert!(!args.rpc_disabled);
        assert!(!args.no_restart);
        assert_eq!(args.listen_addr, IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
        assert_eq!(args.listen_port, 9545);
        assert!(!args.enable_admin);
        assert!(args.admin_persistence.is_none());
        assert!(!args.ws_enabled);
        assert!(!args.dev_enabled);
    }
}
