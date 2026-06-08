use std::{net::IpAddr, str::FromStr, time::Duration};

use alloy_provider::{
    ProviderBuilder,
    ext::{AdminApi, NetApi},
};
use alloy_transport::TransportError;
use anyhow::{Context, Result, anyhow};
use base_common_network::Base;
use base_consensus_gossip::PeerStats;
use base_consensus_peers::{BootNode, NodeRecord};
use base_consensus_rpc::BaseP2PApiClient;
use jsonrpsee::http_client::HttpClientBuilder;
use serde::Serialize;
use url::Url;

/// Advertised discovery endpoint information for a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryInfo {
    /// UDP discovery port advertised by the node.
    pub udp_port: u16,
    /// Whether discovery v4 is enabled.
    pub v4_enabled: bool,
    /// Whether discovery v5 is enabled.
    pub v5_enabled: bool,
}

/// Parsed advertised endpoint for one p2p layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeEndpoint {
    /// Advertised public IP address.
    pub advertised_ip: IpAddr,
    /// Advertised TCP listening port.
    pub rlpx_tcp_port: u16,
    /// Advertised discovery configuration.
    pub discovery: DiscoveryInfo,
}

/// Combined EL + CL advertised endpoint report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfoReport {
    /// Execution-layer advertised endpoint.
    pub el: Option<NodeEndpoint>,
    /// Consensus-layer advertised endpoint.
    pub cl: NodeEndpoint,
}

/// Combined EL + CL peer-count summary.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerStatsReport {
    /// Connected EL peer count from `net_peerCount`.
    pub el_count: u32,
    /// Connected CL peer statistics from `opp2p_peerStats`.
    pub cl: PeerStats,
}

/// Humanized peer row used by `basectl p2p peers` pretty and JSON output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerSummary {
    /// Peer identifier.
    pub id: String,
    /// Best-effort remote address string.
    pub address: String,
    /// Connection direction label.
    pub direction: String,
}

/// Connected peers per layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerListReport {
    /// Connected EL peers.
    pub el: Option<Vec<PeerSummary>>,
    /// Connected CL peers.
    pub cl: Vec<PeerSummary>,
}

/// Fetches the advertised EL + CL endpoints used by the node.
pub async fn fetch_node_info(rpc: &Url, cl_rpc: &Url) -> Result<NodeInfoReport> {
    let cl_client = HttpClientBuilder::default()
        .request_timeout(Duration::from_secs(10))
        .build(cl_rpc.as_str())
        .with_context(|| format!("connecting to consensus node RPC at {cl_rpc}"))?;
    let el_provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .network::<Base>()
        .connect(rpc.as_str())
        .await
        .with_context(|| format!("connecting to EL RPC at {rpc}"))?;

    let (el, cl_info) = tokio::try_join!(
        async {
            match el_provider.node_info().await {
                Ok(info) => Ok(Some(parse_el_node_endpoint(
                    &info.enode,
                    &info.enr,
                    info.ip,
                    info.ports.discovery,
                    info.ports.listener,
                )?)),
                Err(err) if is_method_not_found(&err) => Ok(None),
                Err(err) => Err(err).with_context(|| format!("fetching admin_nodeInfo from {rpc}")),
            }
        },
        async {
            BaseP2PApiClient::opp2p_self(&cl_client)
                .await
                .with_context(|| format!("fetching opp2p_self from {cl_rpc}"))
        },
    )?;

    Ok(NodeInfoReport { el, cl: parse_cl_node_endpoint(&cl_info)? })
}

/// Fetches the connected EL + CL peer counts used by doctor and `p2p info`.
pub async fn fetch_peer_stats(rpc: &Url, cl_rpc: &Url) -> Result<PeerStatsReport> {
    let cl_client = HttpClientBuilder::default()
        .request_timeout(Duration::from_secs(10))
        .build(cl_rpc.as_str())
        .with_context(|| format!("connecting to consensus node RPC at {cl_rpc}"))?;
    let el_provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .network::<Base>()
        .connect(rpc.as_str())
        .await
        .with_context(|| format!("connecting to EL RPC at {rpc}"))?;

    let (el_count, cl) = tokio::try_join!(
        async {
            el_provider
                .net_peer_count()
                .await
                .with_context(|| format!("fetching net_peerCount from {rpc}"))
        },
        async {
            BaseP2PApiClient::opp2p_peer_stats(&cl_client)
                .await
                .with_context(|| format!("fetching opp2p_peerStats from {cl_rpc}"))
        },
    )?;

    let el_count = u32::try_from(el_count)
        .context("EL `net_peerCount` exceeded `u32::MAX`; unexpected RPC value")?;
    Ok(PeerStatsReport { el_count, cl })
}

/// Fetches connected EL + CL peer lists for `basectl p2p peers`.
pub async fn fetch_connected_peers(rpc: &Url, cl_rpc: &Url) -> Result<PeerListReport> {
    let cl_client = HttpClientBuilder::default()
        .request_timeout(Duration::from_secs(10))
        .build(cl_rpc.as_str())
        .with_context(|| format!("connecting to consensus node RPC at {cl_rpc}"))?;
    let el_provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .network::<Base>()
        .connect(rpc.as_str())
        .await
        .with_context(|| format!("connecting to EL RPC at {rpc}"))?;

    let (el, cl_peers) = tokio::try_join!(
        async {
            match el_provider.peers().await {
                Ok(peers) => {
                    let mut peers = peers
                        .into_iter()
                        .map(|peer| PeerSummary {
                            id: peer.id,
                            address: peer.network.remote_address.to_string(),
                            direction: if peer.network.inbound {
                                "Inbound".to_string()
                            } else {
                                "Outbound".to_string()
                            },
                        })
                        .collect::<Vec<_>>();
                    peers.sort_by(|a, b| a.id.cmp(&b.id));
                    Ok(Some(peers))
                }
                Err(err) if is_method_not_found(&err) => Ok(None),
                Err(err) => Err(err).with_context(|| format!("fetching admin_peers from {rpc}")),
            }
        },
        async {
            BaseP2PApiClient::opp2p_peers(&cl_client, true)
                .await
                .with_context(|| format!("fetching opp2p_peers(true) from {cl_rpc}"))
        },
    )?;

    let mut cl = cl_peers
        .peers
        .into_iter()
        .map(|(id, peer)| PeerSummary {
            id,
            address: peer.addresses.join(", "),
            direction: peer.direction.to_string(),
        })
        .collect::<Vec<_>>();
    cl.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(PeerListReport { el, cl })
}

const fn is_method_not_found(err: &TransportError) -> bool {
    matches!(err, TransportError::ErrorResp(payload) if payload.code == -32601)
}

fn parse_el_node_endpoint(
    enode: &str,
    enr: &str,
    fallback_ip: IpAddr,
    fallback_discovery_port: u16,
    fallback_listener_port: u16,
) -> Result<NodeEndpoint> {
    let record =
        NodeRecord::from_str(enode).with_context(|| format!("parsing EL enode `{enode}`"))?;
    let parsed_enr = parse_enr_fields(enr).with_context(|| format!("parsing EL ENR `{enr}`")).ok();

    let advertised_ip = parsed_enr.and_then(|fields| fields.ip).unwrap_or(fallback_ip);
    let rlpx_tcp_port = parsed_enr
        .and_then(|fields| fields.tcp_port)
        .filter(|port| *port != 0)
        .unwrap_or(if record.tcp_port != 0 { record.tcp_port } else { fallback_listener_port });
    let v4_enabled = record.udp_port != 0;
    let v5_udp_port = parsed_enr.and_then(|fields| fields.udp_port).unwrap_or(0);
    let v5_enabled = v5_udp_port != 0;
    let udp_port = if v5_enabled {
        v5_udp_port
    } else if record.udp_port != 0 {
        record.udp_port
    } else {
        fallback_discovery_port
    };

    Ok(NodeEndpoint {
        advertised_ip,
        rlpx_tcp_port,
        discovery: DiscoveryInfo { udp_port, v4_enabled, v5_enabled },
    })
}

fn parse_cl_node_endpoint(peer: &base_consensus_gossip::PeerInfo) -> Result<NodeEndpoint> {
    let enr = peer.enr.as_deref().ok_or_else(|| {
        anyhow!("`opp2p_self` did not return an ENR; cannot determine advertised CL endpoint")
    })?;
    let fields = parse_enr_fields(enr).with_context(|| format!("parsing CL ENR `{enr}`"))?;
    let advertised_ip =
        fields.ip.ok_or_else(|| anyhow!("CL ENR did not contain an advertised IP address"))?;
    let rlpx_tcp_port =
        fields.tcp_port.ok_or_else(|| anyhow!("CL ENR did not contain an advertised TCP port"))?;
    let udp_port =
        fields.udp_port.ok_or_else(|| anyhow!("CL ENR did not contain an advertised UDP port"))?;

    Ok(NodeEndpoint {
        advertised_ip,
        rlpx_tcp_port,
        discovery: DiscoveryInfo { udp_port, v4_enabled: false, v5_enabled: udp_port != 0 },
    })
}

#[derive(Debug, Clone, Copy)]
struct EnrFields {
    ip: Option<IpAddr>,
    tcp_port: Option<u16>,
    udp_port: Option<u16>,
}

fn parse_enr_fields(raw: &str) -> Result<EnrFields> {
    if raw.trim().is_empty() {
        return Err(anyhow!("empty ENR"));
    }
    let bootnode = BootNode::parse_bootnode(raw)?;
    let BootNode::Enr(enr) = bootnode else {
        return Err(anyhow!("expected `enr:` record, got enode"));
    };

    Ok(EnrFields {
        ip: enr.ip4().map(IpAddr::V4).or_else(|| enr.ip6().map(IpAddr::V6)),
        tcp_port: enr.tcp4().or_else(|| enr.tcp6()),
        udp_port: enr.udp4().or_else(|| enr.udp6()),
    })
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use base_consensus_gossip::{
        Connectedness, Direction, GossipScores, PeerInfo, PeerScores, ReqRespScores,
    };

    use super::{NodeEndpoint, parse_cl_node_endpoint, parse_el_node_endpoint};

    #[test]
    fn parses_el_endpoint_from_enode_and_enr() {
        let endpoint = parse_el_node_endpoint(
            "enode://d7dfaea49c7ef37701e668652bcf1bc63d3abb2ae97593374a949e175e4ff128730a2f35199f3462a56298b981dfc395a5abebd2d6f0284ffe5bdc3d8e258b86@127.0.0.1:30304?discport=30301",
            "enr:-Jy4QIvS0dKBLjTTV_RojS8hjriwWsJNHRVyOh4Pk4aUXc5SZjKRVIOeYc7BqzEmbCjLdIY4Ln7x5ZPf-2SsBAc2_zqGAYSwY1zog2V0aMfGhNegsXuAgmlkgnY0gmlwhBiT_DiJc2VjcDI1NmsxoQLX366knH7zdwHmaGUrzxvGPTq7Kul1kzdKlJ4XXk_xKIRzbmFwwIN0Y3CCdmA",
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            30301,
            30304,
        )
        .unwrap();

        assert_eq!(
            endpoint,
            NodeEndpoint {
                advertised_ip: IpAddr::V4(Ipv4Addr::new(24, 147, 252, 56)),
                rlpx_tcp_port: 30304,
                discovery: super::DiscoveryInfo {
                    udp_port: 30301,
                    v4_enabled: true,
                    v5_enabled: false,
                },
            }
        );
    }

    #[test]
    fn parses_cl_endpoint_from_opp2p_self_enr() {
        let peer = PeerInfo {
            peer_id: "peer-id".to_string(),
            node_id: "node-id".to_string(),
            user_agent: "agent".to_string(),
            protocol_version: "1.0.0".to_string(),
            enr: Some("enr:-J64QBbwPjPLZ6IOOToOLsSjtFUjjzN66qmBZdUexpO32Klrc458Q24kbty2PdRaLacHM5z-cZQr8mjeQu3pik6jPSOGAYYFIqBfgmlkgnY0gmlwhDaRWFWHb3BzdGFja4SzlAUAiXNlY3AyNTZrMaECmeSnJh7zjKrDSPoNMGXoopeDF4hhpj5I0OsQUUt4u8uDdGNwgiQGg3VkcIIkBg".to_string()),
            addresses: vec!["/ip4/127.0.0.1/tcp/8999/p2p/peer-id".to_string()],
            protocols: None,
            connectedness: Connectedness::Connected,
            direction: Direction::Outbound,
            protected: false,
            chain_id: 8453,
            latency: 0,
            gossip_blocks: true,
            peer_scores: PeerScores {
                gossip: GossipScores::default(),
                req_resp: ReqRespScores::default(),
            },
        };

        let endpoint = parse_cl_node_endpoint(&peer).unwrap();

        assert!(matches!(endpoint.advertised_ip, IpAddr::V4(_)));
        assert!(endpoint.rlpx_tcp_port > 0);
        assert!(endpoint.discovery.udp_port > 0);
        assert!(!endpoint.discovery.v4_enabled);
        assert!(endpoint.discovery.v5_enabled);
    }
}
