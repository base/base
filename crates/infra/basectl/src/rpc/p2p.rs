//! P2P RPC fetch helpers — EL admin / net and CL opp2p endpoints.

use std::{net::IpAddr, str::FromStr, time::Duration};

use alloy_provider::{
    Provider, ProviderBuilder,
    ext::{AdminApi, NetApi},
};
use alloy_rpc_client::RpcClient;
use alloy_transport::TransportError;
use alloy_transport_http::Http;
use anyhow::{Context, Result, anyhow};
use base_common_network::Base;
use base_consensus_gossip::PeerStats;
use base_consensus_peers::{BootNode, NodeRecord};
use base_consensus_rpc::BaseP2PApiClient;
use jsonrpsee::{
    core::client::Error as JsonRpcClientError,
    http_client::{HttpClient, HttpClientBuilder},
};
use serde::Serialize;
use tracing::{debug, warn};
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
    pub tcp_port: u16,
    /// Advertised discovery configuration.
    pub discovery: DiscoveryInfo,
}

/// Combined EL + CL advertised endpoint report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfoReport {
    /// Execution-layer advertised endpoint. `None` when the EL RPC does not
    /// expose `admin_nodeInfo` or its enode/ENR could not be parsed.
    pub el: Option<NodeEndpoint>,
    /// Consensus-layer advertised endpoint. `None` when the `opp2p_self` ENR
    /// was missing or could not be parsed.
    pub cl: Option<NodeEndpoint>,
}

/// Combined EL + CL peer-count summary.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerStatsReport {
    /// Connected EL peer count from `net_peerCount`, narrowed from its native
    /// `u64` to `u32` so the humanized output reports a uniform peer-count type
    /// across both layers (CL's `PeerStats::connected` is `u32`). The raw path
    /// [`RawPeerCounts::el`] preserves the native `u64`.
    pub el_count: Option<u32>,
    /// Connected CL peer statistics from `opp2p_peerStats`.
    pub cl: Option<PeerStats>,
}

/// Execution-layer p2p endpoint and peer-count summary.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ElInfoReport {
    /// Execution-layer advertised endpoint. `None` when the EL RPC does not
    /// expose `admin_nodeInfo` or its enode/ENR could not be parsed.
    pub endpoint: Option<NodeEndpoint>,
    /// Connected EL peer count from `net_peerCount`.
    pub peer_count: Option<u32>,
    /// Reason the EL peer count is absent, when applicable.
    pub peer_count_error: Option<&'static str>,
}

/// Consensus-layer p2p endpoint and peer-count summary.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClInfoReport {
    /// Consensus-layer advertised endpoint. `None` when the `opp2p_self` ENR
    /// was missing or could not be parsed.
    pub endpoint: Option<NodeEndpoint>,
    /// Connected CL peer statistics from `opp2p_peerStats`.
    pub peer_stats: Option<PeerStats>,
    /// Reason the CL peer statistics are absent, when applicable.
    pub peer_stats_error: Option<&'static str>,
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
    /// Connected CL peers, or `None` when `opp2p_peers` is not exposed.
    pub cl: Option<Vec<PeerSummary>>,
}

/// Raw `p2p info` payload — passthrough RPC wire shapes for `--json --raw`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RawInfoReport {
    /// Raw `admin_nodeInfo` result, or `None` when the EL RPC does not expose it.
    pub el: Option<serde_json::Value>,
    /// Reason the EL `admin_nodeInfo` result is absent, when applicable.
    pub el_error: Option<String>,
    /// Raw `opp2p_self` result, or `None` when not exposed.
    pub cl: Option<serde_json::Value>,
    /// Reason the CL `opp2p_self` result is absent, when applicable.
    pub cl_error: Option<String>,
    /// Connected peer counts per layer.
    pub peer_counts: RawPeerCounts,
}

/// Raw per-layer connected peer counts.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RawPeerCounts {
    /// EL peer count from `net_peerCount`, in its native `u64` wire type. The
    /// humanized path [`PeerStatsReport::el_count`] narrows this to `u32`.
    pub el: u64,
    /// Raw `opp2p_peerStats` result, or `None` when not exposed.
    pub cl: Option<serde_json::Value>,
    /// Reason the CL `opp2p_peerStats` result is absent, when applicable.
    pub cl_error: Option<String>,
}

/// Raw `p2p peers` payload — passthrough RPC wire shapes for `--json --raw`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RawPeersReport {
    /// Raw `admin_peers` result, or `None` when the EL RPC does not expose it.
    pub el: Option<serde_json::Value>,
    /// Reason the EL `admin_peers` result is absent, when applicable.
    pub el_error: Option<String>,
    /// Raw `opp2p_peers(true)` result, or `None` when not exposed.
    pub cl: Option<serde_json::Value>,
    /// Reason the CL `opp2p_peers` result is absent, when applicable.
    pub cl_error: Option<String>,
}

/// Fetches the advertised endpoints and connected peer-count summary for
/// `basectl p2p info`.
///
/// All four RPC calls (EL `admin_nodeInfo` + `net_peerCount`, CL `opp2p_self` +
/// `opp2p_peerStats`) run concurrently over a single connection per layer.
pub async fn fetch_info(rpc: &Url, cl_rpc: &Url) -> Result<(NodeInfoReport, PeerStatsReport)> {
    let (el, cl) = tokio::try_join!(fetch_el_info(rpc), fetch_cl_info(cl_rpc))?;
    let node_info = NodeInfoReport { el: el.endpoint, cl: cl.endpoint };
    let peer_stats = PeerStatsReport { el_count: el.peer_count, cl: cl.peer_stats };
    Ok((node_info, peer_stats))
}

/// Adds an execution-layer peer through `admin_addPeer`.
pub async fn add_peer(rpc: &Url, enode: &str) -> Result<bool> {
    let el_provider = connect_el(rpc).await?;
    match el_provider.add_peer(enode).await {
        Ok(accepted) => Ok(accepted),
        Err(err) if is_method_not_found(&err) => {
            Err(err).with_context(|| format!("`admin_addPeer` not exposed by {rpc}"))
        }
        Err(err) => Err(err).with_context(|| format!("calling admin_addPeer on {rpc}")),
    }
}

/// Removes an execution-layer peer through `admin_removePeer`.
pub async fn remove_peer(rpc: &Url, enode: &str) -> Result<bool> {
    let el_provider = connect_el(rpc).await?;
    match el_provider.remove_peer(enode).await {
        Ok(accepted) => Ok(accepted),
        Err(err) if is_method_not_found(&err) => {
            Err(err).with_context(|| format!("`admin_removePeer` not exposed by {rpc}"))
        }
        Err(err) => Err(err).with_context(|| format!("calling admin_removePeer on {rpc}")),
    }
}

/// Connects a consensus-layer peer through `opp2p_connectPeer`.
pub async fn connect_peer(cl_rpc: &Url, multiaddr: &str) -> Result<()> {
    let cl_client = connect_cl(cl_rpc)?;
    match BaseP2PApiClient::opp2p_connect_peer(&cl_client, multiaddr.to_string()).await {
        Ok(()) => Ok(()),
        Err(err) if is_jsonrpc_method_not_found(&err) => {
            Err(err).with_context(|| format!("`opp2p_connectPeer` not exposed by {cl_rpc}"))
        }
        Err(err) => Err(err).with_context(|| format!("calling opp2p_connectPeer on {cl_rpc}")),
    }
}

/// Disconnects a consensus-layer peer through `opp2p_disconnectPeer`.
pub async fn disconnect_peer(cl_rpc: &Url, peer_id: &str) -> Result<()> {
    let cl_client = connect_cl(cl_rpc)?;
    match BaseP2PApiClient::opp2p_disconnect_peer(&cl_client, peer_id.to_string()).await {
        Ok(()) => Ok(()),
        Err(err) if is_jsonrpc_method_not_found(&err) => {
            Err(err).with_context(|| format!("`opp2p_disconnectPeer` not exposed by {cl_rpc}"))
        }
        Err(err) => Err(err).with_context(|| format!("calling opp2p_disconnectPeer on {cl_rpc}")),
    }
}

/// Bans a consensus-layer peer through `opp2p_blockPeer`.
pub async fn ban_peer(cl_rpc: &Url, peer_id: &str) -> Result<()> {
    let cl_client = connect_cl(cl_rpc)?;
    match BaseP2PApiClient::opp2p_block_peer(&cl_client, peer_id.to_string()).await {
        Ok(()) => Ok(()),
        Err(err) if is_jsonrpc_method_not_found(&err) => {
            Err(err).with_context(|| format!("`opp2p_blockPeer` not exposed by {cl_rpc}"))
        }
        Err(err) => Err(err).with_context(|| format!("calling opp2p_blockPeer on {cl_rpc}")),
    }
}

/// Unbans a consensus-layer peer through `opp2p_unblockPeer`.
pub async fn unban_peer(cl_rpc: &Url, peer_id: &str) -> Result<()> {
    let cl_client = connect_cl(cl_rpc)?;
    match BaseP2PApiClient::opp2p_unblock_peer(&cl_client, peer_id.to_string()).await {
        Ok(()) => Ok(()),
        Err(err) if is_jsonrpc_method_not_found(&err) => {
            Err(err).with_context(|| format!("`opp2p_unblockPeer` not exposed by {cl_rpc}"))
        }
        Err(err) => Err(err).with_context(|| format!("calling opp2p_unblockPeer on {cl_rpc}")),
    }
}

/// Lists consensus-layer peers banned through `opp2p_blockPeer`.
pub async fn list_banned_peers(cl_rpc: &Url) -> Result<Vec<String>> {
    let cl_client = connect_cl(cl_rpc)?;
    match BaseP2PApiClient::opp2p_list_blocked_peers(&cl_client).await {
        Ok(peers) => Ok(peers),
        Err(err) if is_jsonrpc_method_not_found(&err) => {
            Err(err).with_context(|| format!("`opp2p_listBlockedPeers` not exposed by {cl_rpc}"))
        }
        Err(err) => Err(err).with_context(|| format!("calling opp2p_listBlockedPeers on {cl_rpc}")),
    }
}

/// Fetches execution-layer advertised endpoint and peer-count summary.
pub async fn fetch_el_info(rpc: &Url) -> Result<ElInfoReport> {
    let el_provider = connect_el(rpc).await?;

    let (endpoint, peer_count) = tokio::join!(
        async {
            match el_provider.node_info().await {
                Ok(info) => match parse_el_node_endpoint(
                    &info.enode,
                    &info.enr,
                    info.ip,
                    info.ports.discovery,
                    info.ports.listener,
                ) {
                    Ok(endpoint) => Ok::<Option<NodeEndpoint>, anyhow::Error>(Some(endpoint)),
                    Err(err) => {
                        warn!(error = %err, "failed to parse EL node endpoint; reporting EL as unavailable");
                        Ok::<Option<NodeEndpoint>, anyhow::Error>(None)
                    }
                },
                Err(err) if is_method_not_found(&err) => {
                    Ok::<Option<NodeEndpoint>, anyhow::Error>(None)
                }
                Err(err) => {
                    warn!(error = %err, "failed to fetch EL node endpoint; reporting EL endpoint as unavailable");
                    Ok::<Option<NodeEndpoint>, anyhow::Error>(None)
                }
            }
        },
        async {
            match el_provider.net_peer_count().await {
                Ok(peer_count) => u32::try_from(peer_count).map_or(
                    (None, Some("EL `net_peerCount` exceeded `u32::MAX`; unexpected RPC value")),
                    |peer_count| (Some(peer_count), None),
                ),
                Err(err) => {
                    warn!(error = %err, "failed to fetch EL peer count; reporting EL peer count as unavailable");
                    (None, Some("EL `net_peerCount` unavailable from this RPC"))
                }
            }
        },
    );
    Ok(ElInfoReport {
        endpoint: endpoint?,
        peer_count: peer_count.0,
        peer_count_error: peer_count.1,
    })
}

/// Fetches consensus-layer advertised endpoint and peer-count summary.
pub async fn fetch_cl_info(cl_rpc: &Url) -> Result<ClInfoReport> {
    let cl_client = connect_cl(cl_rpc)?;

    let (cl_info, peer_stats) = tokio::join!(
        async {
            match BaseP2PApiClient::opp2p_self(&cl_client).await {
                Ok(info) => Ok(Some(info)),
                Err(err) if is_jsonrpc_method_not_found(&err) => Ok(None),
                Err(err) => Err(err).with_context(|| format!("fetching opp2p_self from {cl_rpc}")),
            }
        },
        async {
            match BaseP2PApiClient::opp2p_peer_stats(&cl_client).await {
                Ok(stats) => Ok((Some(stats), None)),
                Err(err) if is_jsonrpc_method_not_found(&err) => {
                    Ok((None, Some("CL `opp2p_peerStats` not exposed by this RPC")))
                }
                Err(err) => {
                    Err(err).with_context(|| format!("fetching opp2p_peerStats from {cl_rpc}"))
                }
            }
        },
    );
    let cl_info = cl_info?;
    let (peer_stats, peer_stats_error) = peer_stats?;

    let endpoint = cl_info.map_or_else(
        || {
            warn!(rpc = %cl_rpc, "CL opp2p_self not exposed by this RPC; reporting CL endpoint as unavailable");
            None
        },
        |info| match parse_cl_node_endpoint(&info) {
            Ok(endpoint) => Some(endpoint),
            Err(err) => {
                warn!(error = %err, "failed to parse CL node endpoint from opp2p_self; reporting CL as unavailable");
                None
            }
        },
    );
    Ok(ClInfoReport { endpoint, peer_stats, peer_stats_error })
}

/// Fetches connected EL + CL peer lists for `basectl p2p peers`.
pub async fn fetch_connected_peers(rpc: &Url, cl_rpc: &Url) -> Result<PeerListReport> {
    let (cl_client, el_provider) = connect_layers(rpc, cl_rpc).await?;

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
            match BaseP2PApiClient::opp2p_peers(&cl_client, true).await {
                Ok(peers) => Ok(Some(peers)),
                Err(err) if is_jsonrpc_method_not_found(&err) => Ok(None),
                Err(err) => {
                    Err(err).with_context(|| format!("fetching opp2p_peers(true) from {cl_rpc}"))
                }
            }
        },
    )?;

    let cl = cl_peers.map(|peers| {
        let mut cl = peers
            .peers
            .into_iter()
            .map(|(id, peer)| PeerSummary {
                id,
                address: peer.addresses.join(", "),
                direction: peer.direction.to_string(),
            })
            .collect::<Vec<_>>();
        cl.sort_by(|a, b| a.id.cmp(&b.id));
        cl
    });
    Ok(PeerListReport { el, cl })
}

/// Fetches raw `admin_nodeInfo` / `opp2p_self` plus peer counts for `--raw`.
pub async fn fetch_raw_info(rpc: &Url, cl_rpc: &Url) -> Result<RawInfoReport> {
    let (cl_client, el_provider) = connect_layers(rpc, cl_rpc).await?;

    let ((el_value, el_error), cl_info, el_count, cl_stats) = tokio::try_join!(
        async {
            match el_provider.node_info().await {
                Ok(info) => Ok((
                    Some(
                        serde_json::to_value(&info)
                            .context("serializing admin_nodeInfo to JSON value")?,
                    ),
                    None,
                )),
                Err(err) if is_method_not_found(&err) => {
                    Ok((None, Some("EL `admin_nodeInfo` not exposed by this RPC".to_string())))
                }
                Err(err) => Err(err).with_context(|| format!("fetching admin_nodeInfo from {rpc}")),
            }
        },
        async {
            match BaseP2PApiClient::opp2p_self(&cl_client).await {
                Ok(info) => Ok(Some(info)),
                Err(err) if is_jsonrpc_method_not_found(&err) => Ok(None),
                Err(err) => Err(err).with_context(|| format!("fetching opp2p_self from {cl_rpc}")),
            }
        },
        async {
            el_provider
                .net_peer_count()
                .await
                .with_context(|| format!("fetching net_peerCount from {rpc}"))
        },
        async {
            match BaseP2PApiClient::opp2p_peer_stats(&cl_client).await {
                Ok(stats) => Ok(Some(stats)),
                Err(err) if is_jsonrpc_method_not_found(&err) => Ok(None),
                Err(err) => {
                    Err(err).with_context(|| format!("fetching opp2p_peerStats from {cl_rpc}"))
                }
            }
        },
    )?;

    let (cl_value, cl_error) = match cl_info {
        Some(v) => {
            (Some(serde_json::to_value(&v).context("serializing opp2p_self to JSON value")?), None)
        }
        None => (None, Some("CL `opp2p_self` not exposed by this RPC".to_string())),
    };
    let (cl_stats_value, cl_stats_error) = match cl_stats {
        Some(v) => (
            Some(serde_json::to_value(v).context("serializing opp2p_peerStats to JSON value")?),
            None,
        ),
        None => (None, Some("CL `opp2p_peerStats` not exposed by this RPC".to_string())),
    };
    Ok(RawInfoReport {
        el: el_value,
        el_error,
        cl: cl_value,
        cl_error,
        peer_counts: RawPeerCounts { el: el_count, cl: cl_stats_value, cl_error: cl_stats_error },
    })
}

/// Fetches raw `admin_peers` / `opp2p_peers(true)` listings for `--raw`.
pub async fn fetch_raw_peers(rpc: &Url, cl_rpc: &Url) -> Result<RawPeersReport> {
    let (cl_client, el_provider) = connect_layers(rpc, cl_rpc).await?;

    let ((el_value, el_error), cl_peers) = tokio::try_join!(
        async {
            match el_provider.peers().await {
                Ok(peers) => Ok((
                    Some(
                        serde_json::to_value(&peers)
                            .context("serializing admin_peers to JSON value")?,
                    ),
                    None,
                )),
                Err(err) if is_method_not_found(&err) => {
                    Ok((None, Some("EL `admin_peers` not exposed by this RPC".to_string())))
                }
                Err(err) => Err(err).with_context(|| format!("fetching admin_peers from {rpc}")),
            }
        },
        async {
            match BaseP2PApiClient::opp2p_peers(&cl_client, true).await {
                Ok(peers) => Ok(Some(peers)),
                Err(err) if is_jsonrpc_method_not_found(&err) => Ok(None),
                Err(err) => {
                    Err(err).with_context(|| format!("fetching opp2p_peers(true) from {cl_rpc}"))
                }
            }
        },
    )?;

    let (cl_value, cl_error) = match cl_peers {
        Some(v) => {
            (Some(serde_json::to_value(&v).context("serializing opp2p_peers to JSON value")?), None)
        }
        None => (None, Some("CL `opp2p_peers` not exposed by this RPC".to_string())),
    };
    Ok(RawPeersReport { el: el_value, el_error, cl: cl_value, cl_error })
}

/// Connects to the EL provider and CL RPC client used by every p2p fetch.
async fn connect_layers(rpc: &Url, cl_rpc: &Url) -> Result<(HttpClient, impl Provider<Base>)> {
    let cl_client = connect_cl(cl_rpc)?;
    let el_provider = connect_el(rpc).await?;
    Ok((cl_client, el_provider))
}

fn connect_cl(cl_rpc: &Url) -> Result<HttpClient> {
    HttpClientBuilder::default()
        .request_timeout(Duration::from_secs(10))
        .build(cl_rpc.as_str())
        .with_context(|| format!("connecting to consensus node RPC at {cl_rpc}"))
}

async fn connect_el(rpc: &Url) -> Result<impl Provider<Base>> {
    let http_client = alloy_transport_http::reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .with_context(|| format!("building EL HTTP client for {rpc}"))?;
    let transport = Http::with_client(http_client, rpc.clone());
    let el_provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .network::<Base>()
        .connect_client(RpcClient::new(transport, false));
    Ok(el_provider)
}

const fn is_method_not_found(err: &TransportError) -> bool {
    matches!(err, TransportError::ErrorResp(payload) if payload.code == -32601)
}

fn is_jsonrpc_method_not_found(err: &JsonRpcClientError) -> bool {
    matches!(err, JsonRpcClientError::Call(payload) if payload.code() == -32601)
}

/// Parses the EL advertised endpoint from `admin_nodeInfo`'s enode + ENR,
/// preferring ENR-advertised values and falling back to the enode record and
/// the supplied `admin_nodeInfo` ports.
///
/// Discovery flags are inferred heuristically and are best-effort: `v4_enabled`
/// is taken from the enode carrying a non-zero discovery (UDP) port, and
/// `v5_enabled` from the ENR advertising a UDP port. This matches standard
/// Reth/Geth output, but the inference is imperfect — e.g. a discv5-only node
/// whose enode still includes a non-zero `discport` will report
/// `v4_enabled = true`.
fn parse_el_node_endpoint(
    enode: &str,
    enr: &str,
    fallback_ip: IpAddr,
    fallback_discovery_port: u16,
    fallback_listener_port: u16,
) -> Result<NodeEndpoint> {
    let record =
        NodeRecord::from_str(enode).with_context(|| format!("parsing EL enode `{enode}`"))?;
    let parsed_enr = parse_enr_fields(enr)
        .inspect_err(|err| {
            debug!(error = %err, enr = %enr, "failed to parse EL ENR; falling back to enode/admin_nodeInfo values")
        })
        .ok();

    let advertised_ip = parsed_enr.and_then(|fields| fields.ip).unwrap_or(fallback_ip);
    let tcp_port = parsed_enr
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
        tcp_port,
        discovery: DiscoveryInfo { udp_port, v4_enabled, v5_enabled },
    })
}

fn parse_cl_node_endpoint(peer: &base_consensus_gossip::PeerInfo) -> Result<NodeEndpoint> {
    let enr = peer.enr.as_deref().ok_or_else(|| {
        anyhow!("`opp2p_self` did not return an ENR; cannot determine advertised CL endpoint")
    })?;
    let fields = parse_enr_fields(enr).with_context(|| format!("parsing CL ENR `{enr}`"))?;

    let mut missing = Vec::new();
    if fields.ip.is_none() {
        missing.push("IP address");
    }
    if fields.tcp_port.is_none() {
        missing.push("TCP port");
    }
    if fields.udp_port.is_none() {
        missing.push("UDP port");
    }

    match (fields.ip, fields.tcp_port, fields.udp_port) {
        (Some(advertised_ip), Some(tcp_port), Some(udp_port)) => Ok(NodeEndpoint {
            advertised_ip,
            tcp_port,
            discovery: DiscoveryInfo { udp_port, v4_enabled: false, v5_enabled: udp_port != 0 },
        }),
        _ => Err(anyhow!("CL ENR is missing required field(s): {}", missing.join(", "))),
    }
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
    use jsonrpsee::{core::client::Error as JsonRpcClientError, types::ErrorObjectOwned};

    use super::{
        NodeEndpoint, is_jsonrpc_method_not_found, parse_cl_node_endpoint, parse_el_node_endpoint,
    };

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
                tcp_port: 30304,
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
        assert!(endpoint.tcp_port > 0);
        assert!(endpoint.discovery.udp_port > 0);
        assert!(!endpoint.discovery.v4_enabled);
        assert!(endpoint.discovery.v5_enabled);
    }

    #[test]
    fn detects_jsonrpc_method_not_found_for_cl_p2p_methods() {
        let err = JsonRpcClientError::Call(ErrorObjectOwned::owned(
            -32601,
            "method not found",
            None::<()>,
        ));

        assert!(is_jsonrpc_method_not_found(&err));
    }
}
