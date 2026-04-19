//! Contains the builder for the [`RollupNode`].

use std::{path::PathBuf, sync::Arc, time::Duration};

use alloy_genesis::ChainConfig;
use alloy_provider::RootProvider;
use alloy_transport::TransportResult;
use base_common_network::Base;
use base_consensus_engine::BaseEngineClient;
use base_consensus_genesis::RollupConfig;
use base_consensus_providers::OnlineBeaconClient;
use base_consensus_rpc::RpcBuilder;
use url::Url;

use crate::{
    EngineConfig, NetworkConfig, RollupNode, SequencerConfig, actors::DerivationDelegateClient,
    service::node::L1Config,
};

/// Configuration for Derivation Delegate mode.
#[derive(Debug, Clone)]
pub struct DerivationDelegateConfig {
    /// The L2 consensus layer RPC URL to delegate derivation to.
    /// This CL must expose the `optimism_syncStatus` RPC endpoint.
    pub l2_cl_url: Url,
}

impl Default for DerivationDelegateConfig {
    fn default() -> Self {
        Self { l2_cl_url: Url::parse("http://localhost:9545").unwrap() }
    }
}

/// The [`L1ConfigBuilder`] is used to construct a [`L1Config`].
#[derive(Debug)]
pub struct L1ConfigBuilder {
    /// The L1 chain configuration.
    pub chain_config: ChainConfig,
    /// Whether to trust the L1 RPC.
    pub trust_rpc: bool,
    /// The L1 beacon API.
    pub beacon: Url,
    /// The L1 RPC URL.
    pub rpc_url: Url,
    /// The duration in seconds of an L1 slot. This can be used to hardcode a fixed slot
    /// duration if the l1-beacon's slot configuration is not available.
    pub slot_duration_override: Option<u64>,
    /// Number of L1 blocks to keep distance from the L1 head for the verifier.
    pub verifier_l1_confs: u64,
}

/// The [`RollupNodeBuilder`] is used to construct a [`RollupNode`] service.
#[derive(Debug)]
pub struct RollupNodeBuilder {
    /// The rollup configuration.
    pub config: RollupConfig,
    /// The L1 chain configuration.
    pub l1_config_builder: L1ConfigBuilder,
    /// Whether to trust the L2 RPC.
    pub l2_trust_rpc: bool,
    /// Engine builder configuration.
    pub engine_config: EngineConfig,
    /// The [`NetworkConfig`].
    pub p2p_config: NetworkConfig,
    /// An RPC Configuration.
    pub rpc_config: Option<RpcBuilder>,
    /// The [`SequencerConfig`].
    pub sequencer_config: Option<SequencerConfig>,
    /// Optional configuration for Derivation Delegate mode.
    /// When present, the node does not run derivation, instead trusting the configured delegate.
    pub derivation_delegate_config: Option<DerivationDelegateConfig>,
    /// Override for the finalized-block poll interval.
    ///
    /// When `None`, [`L1Config::default_finalized_poll_interval`] is used to select a
    /// chain-appropriate default derived from `config.l1_chain_id`.
    pub finalized_poll_interval: Option<Duration>,
    /// Optional path to the safe head database file.
    ///
    /// When set, enables persistent safe head tracking via redb and serves
    /// `optimism_safeHeadAtL1Block` RPC requests from the database.
    pub safedb_path: Option<PathBuf>,
}

impl RollupNodeBuilder {
    fn derivation_l2_provider_url(mut url: Url) -> Url {
        match url.scheme() {
            "ws" => {
                let _ = url.set_scheme("http");
            }
            "wss" => {
                let _ = url.set_scheme("https");
            }
            _ => {}
        }

        url
    }

    /// Creates a new [`RollupNodeBuilder`] with the given [`RollupConfig`].
    pub const fn new(
        config: RollupConfig,
        l1_config_builder: L1ConfigBuilder,
        l2_trust_rpc: bool,
        engine_config: EngineConfig,
        p2p_config: NetworkConfig,
        rpc_config: Option<RpcBuilder>,
    ) -> Self {
        Self {
            config,
            l1_config_builder,
            l2_trust_rpc,
            engine_config,
            p2p_config,
            rpc_config,
            sequencer_config: None,
            derivation_delegate_config: None,
            finalized_poll_interval: None,
            safedb_path: None,
        }
    }

    /// Sets the [`EngineConfig`] on the [`RollupNodeBuilder`].
    pub fn with_engine_config(self, engine_config: EngineConfig) -> Self {
        Self { engine_config, ..self }
    }

    /// Sets the [`RpcBuilder`] on the [`RollupNodeBuilder`].
    pub fn with_rpc_config(self, rpc_config: Option<RpcBuilder>) -> Self {
        Self { rpc_config, ..self }
    }

    /// Appends the [`SequencerConfig`] to the builder.
    pub fn with_sequencer_config(self, sequencer_config: SequencerConfig) -> Self {
        Self { sequencer_config: Some(sequencer_config), ..self }
    }

    /// Overrides the finalized-block poll interval.
    ///
    /// By default the interval is derived from `config.l1_chain_id` via
    /// [`L1Config::default_finalized_poll_interval`]. Use this method when you need a
    /// specific interval regardless of chain (e.g. in integration tests).
    pub fn with_finalized_poll_interval(self, interval: Duration) -> Self {
        Self { finalized_poll_interval: Some(interval), ..self }
    }

    /// Sets the Derivation Delegate configuration, trusting the configured delegate for safe head
    /// updates.
    pub fn with_derivation_delegate_config(
        self,
        derivation_delegate_config: Option<DerivationDelegateConfig>,
    ) -> Self {
        Self { derivation_delegate_config, ..self }
    }

    /// Enables persistent safe head tracking by setting the path to the redb database file.
    pub fn with_safedb_path(self, path: PathBuf) -> Self {
        Self { safedb_path: Some(path), ..self }
    }

    /// Assembles the [`RollupNode`] service.
    ///
    /// Returns an error if the internal L2 provider transport cannot be constructed. WebSocket
    /// URLs are normalized to HTTP(S) so the derivation pipeline's request/response L2 provider
    /// remains lazy during startup. `file://` URLs still connect eagerly because IPC is an
    /// explicit opt-in transport.
    pub async fn build(self) -> TransportResult<RollupNode> {
        let mut l1_beacon = OnlineBeaconClient::new_http(self.l1_config_builder.beacon.to_string());
        if let Some(l1_slot_duration) = self.l1_config_builder.slot_duration_override {
            l1_beacon = l1_beacon.with_l1_slot_duration_override(l1_slot_duration);
        }

        let finalized_poll_interval = self
            .finalized_poll_interval
            .unwrap_or_else(|| L1Config::default_finalized_poll_interval(self.config.l1_chain_id));

        let l1_config = L1Config {
            chain_config: Arc::new(self.l1_config_builder.chain_config),
            trust_rpc: self.l1_config_builder.trust_rpc,
            beacon_client: l1_beacon,
            engine_provider: RootProvider::new_http(self.l1_config_builder.rpc_url.clone()),
            finalized_poll_interval,
            verifier_l1_confs: self.l1_config_builder.verifier_l1_confs,
        };

        let l2_provider_url = Self::derivation_l2_provider_url(self.engine_config.l2_url.clone());
        let l2_provider = BaseEngineClient::<RootProvider, RootProvider<Base>>::rpc_client::<Base>(
            l2_provider_url,
            self.engine_config.l2_jwt_secret,
        )
        .await?;

        let rollup_config = Arc::new(self.config);

        let p2p_config = self.p2p_config;
        let sequencer_config = self.sequencer_config.unwrap_or_default();

        let derivation_delegate_provider = self.derivation_delegate_config.as_ref().map(|config| {
            DerivationDelegateClient::new(config.l2_cl_url.clone()).expect(
                "Failed to create Derivation Delegate provider despite config being present",
            )
        });

        Ok(RollupNode {
            config: rollup_config,
            l1_config,
            l2_provider,
            l2_trust_rpc: self.l2_trust_rpc,
            engine_config: self.engine_config,
            rpc_builder: self.rpc_config,
            p2p_config,
            sequencer_config,
            derivation_delegate_provider,
            safedb_path: self.safedb_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr},
        sync::Arc,
    };

    use alloy_primitives::Address;
    use alloy_rpc_types_engine::JwtSecret;
    use base_consensus_disc::LocalNode;
    use discv5::enr::k256::ecdsa::SigningKey;
    use libp2p::Multiaddr;

    use super::*;
    use crate::NodeMode;

    fn test_builder(l2_url: Url) -> RollupNodeBuilder {
        let rollup_config = RollupConfig::default();
        let l1_config_builder = L1ConfigBuilder {
            chain_config: ChainConfig::default(),
            trust_rpc: true,
            beacon: Url::parse("http://127.0.0.1:5052").unwrap(),
            rpc_url: Url::parse("http://127.0.0.1:8545").unwrap(),
            slot_duration_override: None,
            verifier_l1_confs: 0,
        };
        let engine_config = EngineConfig {
            config: Arc::new(rollup_config.clone()),
            l2_url,
            l2_jwt_secret: JwtSecret::random(),
            l1_url: Url::parse("http://127.0.0.1:8545").unwrap(),
            mode: NodeMode::Validator,
        };
        let discovery_listen = LocalNode::new(
            SigningKey::from_bytes((&[7_u8; 32]).into()).unwrap(),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            0,
        );
        let p2p_config = NetworkConfig::new(
            rollup_config.clone(),
            discovery_listen,
            "/ip4/127.0.0.1/tcp/0".parse::<Multiaddr>().unwrap(),
            Address::ZERO,
        );

        RollupNodeBuilder::new(
            rollup_config,
            l1_config_builder,
            true,
            engine_config,
            p2p_config,
            None,
        )
    }

    #[test]
    fn derivation_l2_provider_url_normalizes_websocket_schemes() {
        let ws_url = RollupNodeBuilder::derivation_l2_provider_url(
            Url::parse("ws://127.0.0.1:8551/path?query=value").unwrap(),
        );
        assert_eq!(ws_url.as_str(), "http://127.0.0.1:8551/path?query=value");

        let wss_url = RollupNodeBuilder::derivation_l2_provider_url(
            Url::parse("wss://127.0.0.1:8551/path?query=value").unwrap(),
        );
        assert_eq!(wss_url.as_str(), "https://127.0.0.1:8551/path?query=value");

        let file_url = Url::parse("file:///tmp/base-engine.ipc").unwrap();
        let normalized_file_url = RollupNodeBuilder::derivation_l2_provider_url(file_url.clone());
        assert_eq!(normalized_file_url, file_url);
    }

    #[tokio::test]
    async fn build_keeps_ws_startup_lazy_for_derivation_provider() {
        let rollup_node =
            test_builder(Url::parse("ws://127.0.0.1:8551").unwrap()).build().await.unwrap();

        assert_eq!(rollup_node.engine_config.l2_url.scheme(), "ws");
    }
}
