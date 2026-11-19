//! Read-only conductor cluster view served by the approval gateway, and the
//! TUI poller that uses it when basectl has no direct conductor RPC access.

use std::{str::FromStr, sync::Arc, time::Duration};

use alloy_primitives::B256;
use base_consensus_rpc::ServerSuffrage;
use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use serde::Deserialize;
use tokio::{sync::mpsc, task::JoinSet, time::Instant};
use url::Url;

use crate::{
    ConductorNodeStatus, ConductorPollUpdate, ConductorSource, GateClient, GateError, GateSettings,
    Lenient, MonitoringConfig,
};

/// Response of `GET /admin/conductor/cluster`.
///
/// Partial results are allowed: a node the gateway could not reach carries
/// `error` and `null` fields. Every per-node field may be `null`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ClusterResponse {
    /// Network the gateway serves.
    #[serde(default, deserialize_with = "Lenient::non_empty")]
    pub network: Option<String>,
    /// Raft server ID of the current leader.
    #[serde(default, deserialize_with = "Lenient::non_empty")]
    pub leader_id: Option<String>,
    /// Raft membership version.
    #[serde(default)]
    pub membership_version: Option<u64>,
    /// When the gateway assembled this view.
    #[serde(default, deserialize_with = "Lenient::time")]
    pub as_of: Option<DateTime<Utc>>,
    /// One entry per raft server.
    #[serde(default, deserialize_with = "Lenient::or_default")]
    pub servers: Vec<ClusterServer>,
}

/// One raft server in [`ClusterResponse`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ClusterServer {
    /// Raft server ID; the target ID for a transfer proposal.
    #[serde(default, deserialize_with = "Lenient::non_empty")]
    pub id: Option<String>,
    /// Raft address.
    #[serde(default, deserialize_with = "Lenient::non_empty")]
    pub addr: Option<String>,
    /// Whether the server is a raft voter.
    #[serde(default)]
    pub voter: Option<bool>,
    /// Whether the server is the raft leader.
    #[serde(default)]
    pub leader: Option<bool>,
    /// Conductor control-loop state.
    #[serde(default)]
    pub conductor: Option<ClusterConductorState>,
    /// Sequencer state.
    #[serde(default)]
    pub sequencer: Option<ClusterSequencerState>,
    /// L2 heads.
    #[serde(default)]
    pub heads: Option<ClusterHeads>,
    /// Connected consensus-layer peers. Absent from older gateways.
    #[serde(default)]
    pub cl_peers: Option<u64>,
    /// Why the gateway could not read this server, if it could not.
    #[serde(default, deserialize_with = "Lenient::non_empty")]
    pub error: Option<String>,
}

/// Conductor control-loop state of a [`ClusterServer`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct ClusterConductorState {
    /// Whether the conductor is actively sequencing.
    #[serde(default)]
    pub active: Option<bool>,
    /// Whether the control loop is paused.
    #[serde(default)]
    pub paused: Option<bool>,
    /// Whether the conductor is stopped.
    #[serde(default)]
    pub stopped: Option<bool>,
}

/// Sequencer state of a [`ClusterServer`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct ClusterSequencerState {
    /// Whether the conductor reports the sequencer healthy.
    #[serde(default)]
    pub healthy: Option<bool>,
    /// Whether the sequencer is producing blocks.
    #[serde(default)]
    pub active: Option<bool>,
}

/// L2 heads and L1 derivation progress of a [`ClusterServer`]. The hash and
/// L1 fields are absent from older gateways.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ClusterHeads {
    /// Unsafe L2 block number.
    #[serde(default, rename = "unsafe")]
    pub unsafe_l2: Option<u64>,
    /// Safe L2 block number.
    #[serde(default)]
    pub safe: Option<u64>,
    /// Finalized L2 block number.
    #[serde(default)]
    pub finalized: Option<u64>,
    /// Unsafe L2 block hash, as the gateway sent it. Only a valid 32-byte hex
    /// hash is shown (see [`ClusterResponse::statuses`]).
    #[serde(default, deserialize_with = "Lenient::non_empty")]
    pub unsafe_hash: Option<String>,
    /// L1 block the node has derived up to.
    #[serde(default)]
    pub current_l1: Option<u64>,
    /// L1 head the node sees.
    #[serde(default)]
    pub head_l1: Option<u64>,
}

impl ClusterServer {
    /// Display name: the raft server ID, else the address.
    pub fn name(&self) -> &str {
        self.id.as_deref().or(self.addr.as_deref()).unwrap_or("?")
    }
}

impl ClusterResponse {
    /// Per-node rows for the conductor view, in the gateway's order. Fields
    /// the gateway does not serve (the safe hash, EL) are `None`, as is an
    /// unsafe hash that isn't a 32-byte hex hash, so gateway text never
    /// reaches the hash cell.
    pub fn statuses(&self) -> Vec<ConductorNodeStatus> {
        self.servers
            .iter()
            .map(|server| {
                let conductor = server.conductor.unwrap_or_default();
                let sequencer = server.sequencer.unwrap_or_default();
                let heads = server.heads.clone().unwrap_or_default();
                ConductorNodeStatus {
                    name: server.name().to_string(),
                    // Without an ID the node cannot be a transfer target.
                    is_leader: server.id.as_ref().and(server.leader),
                    conductor_active: conductor.active,
                    conductor_paused: conductor.paused,
                    conductor_stopped: conductor.stopped,
                    sequencer_healthy: sequencer.healthy,
                    sequencer_active: sequencer.active,
                    unsafe_l2_block: heads.unsafe_l2,
                    unsafe_l2_hash: heads
                        .unsafe_hash
                        .as_deref()
                        .and_then(|h| B256::from_str(h).ok()),
                    safe_l2_block: heads.safe,
                    safe_l2_hash: None,
                    finalized_l2_block: heads.finalized,
                    current_l1_block: heads.current_l1,
                    head_l1_block: heads.head_l1,
                    cl_peer_count: server.cl_peers.map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
                    el_block: None,
                    el_syncing: None,
                    el_peer_count: None,
                    suffrage: server.voter.map(|voter| {
                        if voter { ServerSuffrage::Voter } else { ServerSuffrage::Nonvoter }
                    }),
                    discovered: false,
                }
            })
            .collect()
    }

    /// The gateway's error for the node named `name`, if any.
    pub fn error_for(&self, name: &str) -> Option<&str> {
        self.servers.iter().find(|server| server.name() == name)?.error.as_deref()
    }
}

/// Where the conductor view gets its data.
#[derive(Debug, Clone)]
pub enum ConductorFeed {
    /// Direct conductor, CL and EL RPCs (a `conductors:` list or a bootstrap
    /// conductor RPC).
    Direct(ConductorSource),
    /// The approval gateway's cluster view, used when no conductor RPC is
    /// configured but a gateway is.
    Gateway(GateSettings),
}

impl ConductorFeed {
    /// Poll interval for direct conductor RPCs.
    pub const DIRECT_POLL_INTERVAL: Duration = Duration::from_millis(200);
    /// Poll interval for the gateway's cluster view (the gateway caches it
    /// for 1 s, so polling faster only adds load).
    pub const GATEWAY_POLL_INTERVAL: Duration = Duration::from_secs(2);

    /// Direct RPC config wins; otherwise the gateway, when one is set.
    pub fn resolve(
        config: &MonitoringConfig,
        conductor_rpc: Option<Url>,
    ) -> Result<Option<Self>, GateError> {
        if let Some(source) = config.conductor_source(conductor_rpc) {
            return Ok(Some(Self::Direct(source)));
        }
        Ok(GateSettings::resolve(config, None, None)?.map(Self::Gateway))
    }

    /// How often the view's data is refreshed.
    pub const fn poll_interval(&self) -> Duration {
        match self {
            Self::Direct(_) => Self::DIRECT_POLL_INTERVAL,
            Self::Gateway(_) => Self::GATEWAY_POLL_INTERVAL,
        }
    }
}

/// What [`GateClusterPoller`] needs from the gateway.
#[cfg_attr(test, mockall::automock)]
pub trait ClusterSource: Send + Sync {
    /// Fetches the cluster view once, without logging in.
    fn fetch(&self) -> BoxFuture<'static, Result<ClusterResponse, GateError>>;

    /// Logs in after a 401, or reads an explicit token source again;
    /// resolves once a new token is in use or that failed.
    fn login(&self, token_url: Option<String>) -> BoxFuture<'static, Result<(), GateError>>;

    /// Picks up a token cached elsewhere; returns whether it changed.
    fn reload_token(&self) -> bool;

    /// Whether the token comes from an explicit source, which [`Self::login`]
    /// reads again instead of logging in.
    fn explicit_source(&self) -> bool;
}

impl ClusterSource for GateClient {
    fn fetch(&self) -> BoxFuture<'static, Result<ClusterResponse, GateError>> {
        let client = self.clone();
        Box::pin(async move { client.cluster().await })
    }

    fn login(&self, token_url: Option<String>) -> BoxFuture<'static, Result<(), GateError>> {
        let client = self.clone();
        Box::pin(async move { client.relogin(token_url).await.map(drop) })
    }

    fn reload_token(&self) -> bool {
        self.reload_cached_token()
    }

    fn explicit_source(&self) -> bool {
        self.has_explicit_source()
    }
}

/// Polls the gateway's cluster view for the TUI.
#[derive(Debug, Clone, Copy)]
pub struct GateClusterPoller;

impl GateClusterPoller {
    /// After a login attempt (or a read of an explicit token source), wait
    /// this long before another one may start. Until then a 401 pauses
    /// polling until a token is available.
    pub const LOGIN_RETRY_AFTER: Duration = Duration::from_secs(300);
    /// Shown while a browser login started by the poller is pending.
    pub const LOGIN_PENDING: &str =
        "Approval gateway login: finish signing in in the browser; polling resumes after";
    /// Shown while the poller reads an explicit token source again after a 401.
    pub const TOKEN_SOURCE_PENDING: &str =
        "Approval gateway rejected the token; reading the configured token source again";

    /// Notice for a token problem while the token comes from an explicit
    /// source: the poller reads the source again once the retry window ends.
    fn explicit_source_notice(error: &GateError) -> String {
        format!(
            "{error}; the configured token source is read again within {} minutes",
            Self::LOGIN_RETRY_AFTER.as_secs() / 60
        )
    }

    /// Builds the TUI client and polls until `tx` closes. A client that can't
    /// be built is reported once as a notice.
    pub async fn start(settings: GateSettings, tx: mpsc::Sender<ConductorPollUpdate>) {
        match settings.client_for_tui_login().await {
            Ok(client) => Self::run(Arc::new(client), tx).await,
            Err(error) => {
                let _ = tx.send(ConductorPollUpdate::GateNotice(error.tui_message())).await;
            }
        }
    }

    /// Polls every [`ConductorFeed::GATEWAY_POLL_INTERVAL`] until `tx` closes.
    ///
    /// A 401 starts one login and pauses polling. Polling resumes when the
    /// login succeeds or a cached token appears. A failed login is not
    /// retried for [`Self::LOGIN_RETRY_AFTER`], so the browser never opens on
    /// every poll. With an explicit token source the "login" reads that
    /// source again under the same bound. A source that keeps returning the
    /// rejected token is not retried with it within the window; when the
    /// window ends, the poller sends one request with the current token
    /// before reading the source again, so that token costs one request per
    /// window.
    pub async fn run(source: Arc<dyn ClusterSource>, tx: mpsc::Sender<ConductorPollUpdate>) {
        let mut interval = tokio::time::interval(ConductorFeed::GATEWAY_POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Dropping the set aborts a pending login, so it ends with the poller.
        let mut login: JoinSet<Result<(), GateError>> = JoinSet::new();
        let mut last_login: Option<Instant> = None;
        let mut awaiting_token = false;
        let explicit = source.explicit_source();
        let token_notice = |error: &GateError| {
            if explicit { Self::explicit_source_notice(error) } else { error.tui_message() }
        };

        loop {
            interval.tick().await;

            if !login.is_empty() {
                match login.try_join_next() {
                    Some(Ok(Ok(()))) => awaiting_token = false,
                    Some(Ok(Err(error))) => {
                        if tx
                            .send(ConductorPollUpdate::GateNotice(token_notice(&error)))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Some(Err(error)) => {
                        let notice = format!("approval gateway login failed: {error}");
                        if tx.send(ConductorPollUpdate::GateNotice(notice)).await.is_err() {
                            break;
                        }
                    }
                    None if source.reload_token() => {
                        login = JoinSet::new();
                        awaiting_token = false;
                    }
                    None => continue,
                }
            }

            if awaiting_token {
                let retry_due = last_login.is_none_or(|at| at.elapsed() >= Self::LOGIN_RETRY_AFTER);
                if !(source.reload_token() || retry_due) {
                    continue;
                }
                awaiting_token = false;
            }

            let update = match source.fetch().await {
                Ok(cluster) => ConductorPollUpdate::GateSnapshot(Box::new(cluster)),
                Err(GateError::Unauthenticated { message, token_url }) => {
                    awaiting_token = true;
                    let may_login = login.is_empty()
                        && last_login.is_none_or(|at| at.elapsed() >= Self::LOGIN_RETRY_AFTER);
                    if may_login {
                        last_login = Some(Instant::now());
                        login.spawn(source.login(token_url));
                        let pending =
                            if explicit { Self::TOKEN_SOURCE_PENDING } else { Self::LOGIN_PENDING };
                        ConductorPollUpdate::GateNotice(pending.to_string())
                    } else {
                        let error = GateError::Unauthenticated { message, token_url };
                        ConductorPollUpdate::GateNotice(token_notice(&error))
                    }
                }
                Err(error) => ConductorPollUpdate::GateNotice(error.to_string()),
            };
            if tx.send(update).await.is_err() {
                break;
            }
        }
    }
}

/// Gateway cluster-view fixtures shared by tests.
#[cfg(test)]
pub mod test_utils {
    /// The gateway contract's example response, verbatim.
    pub const CLUSTER_EXAMPLE: &str = r#"{
  "network": "base-mainnet",
  "leader_id": "sequencer-0",
  "membership_version": 7,
  "as_of": "2026-11-10T12:00:00Z",
  "servers": [
    {
      "id": "sequencer-0",
      "addr": "op-conductor-0:5050",
      "voter": true,
      "leader": true,
      "conductor": {"active": true, "paused": false, "stopped": false},
      "sequencer": {"healthy": true, "active": true},
      "heads": {"unsafe": 1204310, "safe": 1204250, "finalized": 1203900},
      "error": null
    }
  ]
}"#;

    /// A partial result: the second node failed, so it has `error` set and
    /// `null` fields (including nested ones), and `as_of` is omitted.
    pub const CLUSTER_PARTIAL: &str = r#"{
  "network": "base-mainnet",
  "leader_id": "sequencer-0",
  "membership_version": 7,
  "servers": [
    {
      "id": "sequencer-0",
      "addr": "op-conductor-0:5050",
      "voter": true,
      "leader": true,
      "conductor": {"active": true, "paused": false, "stopped": false},
      "sequencer": {"healthy": true, "active": true},
      "heads": {"unsafe": 1204310, "safe": 1204250, "finalized": 1203900},
      "error": null
    },
    {
      "id": "sequencer-1",
      "addr": null,
      "voter": null,
      "leader": null,
      "conductor": null,
      "sequencer": {"healthy": null, "active": null},
      "heads": null,
      "error": "conductor rpc: context deadline exceeded"
    }
  ]
}"#;

    /// A newer gateway's response with the CL fields: `sequencer-0` has them,
    /// `sequencer-1` has them null or absent.
    pub const CLUSTER_WITH_CL: &str = r#"{
  "network": "base-mainnet",
  "leader_id": "sequencer-0",
  "membership_version": 7,
  "servers": [
    {
      "id": "sequencer-0",
      "addr": "op-conductor-0:5050",
      "voter": true,
      "leader": true,
      "conductor": {"active": true, "paused": false, "stopped": false},
      "sequencer": {"healthy": true, "active": true},
      "heads": {"unsafe": 1204310, "safe": 1204250, "finalized": 1203900,
                "unsafe_hash": "0xabcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
                "current_l1": 21000000, "head_l1": 21000004},
      "cl_peers": 12,
      "error": null
    },
    {
      "id": "sequencer-1",
      "addr": "op-conductor-1:5050",
      "voter": true,
      "leader": false,
      "conductor": {"active": false, "paused": false, "stopped": false},
      "sequencer": {"healthy": true, "active": false},
      "heads": {"unsafe": 1204310, "safe": 1204250, "finalized": 1203900,
                "unsafe_hash": null, "current_l1": 21000000, "head_l1": null},
      "cl_peers": null,
      "error": null
    }
  ]
}"#;

    /// Two healthy nodes, `sequencer-0` leading.
    pub const CLUSTER_HEALTHY: &str = r#"{
  "network": "base-mainnet",
  "leader_id": "sequencer-0",
  "membership_version": 7,
  "as_of": "2026-11-10T12:00:00Z",
  "servers": [
    {
      "id": "sequencer-0",
      "addr": "op-conductor-0:5050",
      "voter": true,
      "leader": true,
      "conductor": {"active": true, "paused": false, "stopped": false},
      "sequencer": {"healthy": true, "active": true},
      "heads": {"unsafe": 1204310, "safe": 1204250, "finalized": 1203900},
      "error": null
    },
    {
      "id": "sequencer-1",
      "addr": "op-conductor-1:5050",
      "voter": true,
      "leader": false,
      "conductor": {"active": false, "paused": false, "stopped": false},
      "sequencer": {"healthy": true, "active": false},
      "heads": {"unsafe": 1204310, "safe": 1204250, "finalized": 1203900},
      "error": null
    }
  ]
}"#;
}

#[cfg(test)]
mod tests {
    use std::{
        str::FromStr,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use alloy_primitives::B256;
    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode},
        routing::get,
    };
    use base_consensus_rpc::ServerSuffrage;
    use chrono::{TimeZone, Utc};
    use rstest::rstest;
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, sync::mpsc, time::Instant};
    use url::Url;

    use super::{
        ClusterResponse, ClusterSource, ConductorFeed, GateClusterPoller, MockClusterSource,
        test_utils::{CLUSTER_EXAMPLE, CLUSTER_HEALTHY, CLUSTER_PARTIAL, CLUSTER_WITH_CL},
    };
    use crate::{
        ConductorNodeConfig, ConductorPollUpdate, ConductorSource, GateError, GateSettings,
        MonitoringConfig, run_conductor_poller,
    };

    fn unauthenticated() -> GateError {
        GateError::Unauthenticated {
            message: "unauthenticated".to_string(),
            token_url: Some("https://login.example/token".to_string()),
        }
    }

    fn snapshot() -> ClusterResponse {
        serde_json::from_str(CLUSTER_EXAMPLE).unwrap()
    }

    #[test]
    fn parses_the_contract_example() {
        let cluster = snapshot();

        assert_eq!(cluster.network.as_deref(), Some("base-mainnet"));
        assert_eq!(cluster.leader_id.as_deref(), Some("sequencer-0"));
        assert_eq!(cluster.membership_version, Some(7));
        assert_eq!(cluster.as_of, Some(Utc.with_ymd_and_hms(2026, 11, 10, 12, 0, 0).unwrap()));
        let server = &cluster.servers[0];
        assert_eq!(server.addr.as_deref(), Some("op-conductor-0:5050"));
        assert_eq!(server.error, None);

        let status = &cluster.statuses()[0];
        assert_eq!(status.name, "sequencer-0");
        assert_eq!(status.is_leader, Some(true));
        assert_eq!(
            (status.conductor_active, status.conductor_paused, status.conductor_stopped),
            (Some(true), Some(false), Some(false))
        );
        assert_eq!((status.sequencer_healthy, status.sequencer_active), (Some(true), Some(true)));
        assert_eq!(
            (status.unsafe_l2_block, status.safe_l2_block, status.finalized_l2_block),
            (Some(1_204_310), Some(1_204_250), Some(1_203_900))
        );
        assert_eq!(status.suffrage, Some(ServerSuffrage::Voter));
        assert_eq!(status.cl_peer_count, None);
        assert_eq!(status.el_block, None);
    }

    #[test]
    fn parses_the_cl_fields_when_present_absent_or_null() {
        let cluster: ClusterResponse = serde_json::from_str(CLUSTER_WITH_CL).unwrap();
        let heads = cluster.servers[0].heads.clone().unwrap();
        assert_eq!(
            heads.unsafe_hash.as_deref(),
            Some("0xabcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
        );
        assert_eq!((heads.current_l1, heads.head_l1), (Some(21_000_000), Some(21_000_004)));
        assert_eq!(cluster.servers[0].cl_peers, Some(12));
        let status = &cluster.statuses()[0];
        assert_eq!(
            status.unsafe_l2_hash,
            Some(B256::from_str(heads.unsafe_hash.as_deref().unwrap()).unwrap())
        );
        assert_eq!(
            (status.current_l1_block, status.head_l1_block),
            (Some(21_000_000), Some(21_000_004))
        );
        assert_eq!(status.cl_peer_count, Some(12));

        // Null.
        let heads = cluster.servers[1].heads.clone().unwrap();
        assert_eq!((heads.unsafe_hash, heads.head_l1), (None, None));
        assert_eq!(cluster.servers[1].cl_peers, None);

        // Absent (an older gateway).
        let older: ClusterResponse = serde_json::from_str(CLUSTER_HEALTHY).unwrap();
        let heads = older.servers[0].heads.clone().unwrap();
        assert_eq!((heads.unsafe_hash, heads.current_l1, heads.head_l1), (None, None, None));
        assert_eq!(older.servers[0].cl_peers, None);

        // A hash that isn't 32-byte hex never reaches the hash cell.
        let odd = CLUSTER_WITH_CL.replace("0xabcdef0123456789", "\\u001b[2J0123456789");
        let odd: ClusterResponse = serde_json::from_str(&odd).unwrap();
        assert_eq!(odd.statuses()[0].unsafe_l2_hash, None);
    }

    #[test]
    fn parses_a_partial_result_with_nulls() {
        let cluster: ClusterResponse = serde_json::from_str(CLUSTER_PARTIAL).unwrap();

        assert_eq!(cluster.as_of, None);
        assert_eq!(cluster.servers.len(), 2);
        let failed = &cluster.servers[1];
        assert_eq!(failed.addr, None);
        assert_eq!(failed.conductor, None);
        assert_eq!(failed.heads, None);
        assert_eq!(
            cluster.error_for("sequencer-1"),
            Some("conductor rpc: context deadline exceeded")
        );
        assert_eq!(cluster.error_for("sequencer-0"), None);

        let status = &cluster.statuses()[1];
        assert_eq!(status.name, "sequencer-1");
        assert_eq!(status.is_leader, None);
        assert_eq!(status.conductor_paused, None);
        assert_eq!(status.sequencer_healthy, None);
        assert_eq!(status.unsafe_l2_block, None);
        assert_eq!(status.suffrage, None);
    }

    #[rstest]
    #[case::null_servers(r#"{"servers": null, "as_of": "0001-01-01T00:00:00Z"}"#)]
    #[case::empty_object("{}")]
    fn tolerates_null_lists_and_zero_times(#[case] body: &str) {
        let cluster: ClusterResponse = serde_json::from_str(body).unwrap();

        assert_eq!(cluster, ClusterResponse::default());
    }

    fn static_node() -> ConductorNodeConfig {
        ConductorNodeConfig {
            name: "op-conductor-0".to_string(),
            conductor_rpc: Url::parse("http://127.0.0.1:6545").unwrap(),
            cl_rpc: Url::parse("http://127.0.0.1:7545").unwrap(),
            server_id: "sequencer-0".to_string(),
            raft_addr: "op-conductor-0:5050".to_string(),
            el_rpc: None,
            docker_conductor: None,
            docker_el: None,
            docker_cl: None,
        }
    }

    #[test]
    fn feed_uses_the_gateway_only_without_conductor_rpc() {
        let mut config = MonitoringConfig::mainnet();
        config.gate_url = Some(Url::parse("https://gate.example").unwrap());

        let feed = ConductorFeed::resolve(&config, None).unwrap().unwrap();
        assert!(matches!(feed, ConductorFeed::Gateway(_)), "{feed:?}");
        assert_eq!(feed.poll_interval(), std::time::Duration::from_secs(2));

        let bootstrap = Url::parse("http://127.0.0.1:6545").unwrap();
        let feed = ConductorFeed::resolve(&config, Some(bootstrap)).unwrap().unwrap();
        assert!(matches!(feed, ConductorFeed::Direct(ConductorSource::Discover { .. })));
        assert_eq!(feed.poll_interval(), std::time::Duration::from_millis(200));

        config.conductors = Some(vec![static_node()]);
        let feed = ConductorFeed::resolve(&config, None).unwrap().unwrap();
        assert!(matches!(feed, ConductorFeed::Direct(ConductorSource::Static(_))));
        assert_eq!(feed.poll_interval(), std::time::Duration::from_millis(200));

        config.conductors = None;
        config.gate_url = None;
        assert!(ConductorFeed::resolve(&config, None).unwrap().is_none());
    }

    /// Receives `count` updates and returns the paused-clock gaps between them.
    async fn gaps(rx: &mut mpsc::Receiver<ConductorPollUpdate>, count: usize) -> Vec<u128> {
        let mut last = None;
        let mut gaps = Vec::new();
        for _ in 0..count {
            rx.recv().await.expect("poller is running");
            let now = Instant::now();
            if let Some(previous) = last.replace(now) {
                gaps.push((now - previous).as_millis());
            }
        }
        gaps
    }

    #[tokio::test(start_paused = true)]
    async fn gateway_poller_polls_every_two_seconds() {
        let fetches = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&fetches);
        let mut source = MockClusterSource::new();
        source.expect_fetch().returning(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(snapshot()) })
        });
        source.expect_login().never();
        source.expect_explicit_source().return_const(false);
        let (tx, mut rx) = mpsc::channel(8);
        let poller = tokio::spawn(GateClusterPoller::run(Arc::new(source), tx));

        assert_eq!(gaps(&mut rx, 4).await, vec![2000, 2000, 2000]);
        assert_eq!(fetches.load(Ordering::SeqCst), 4);
        poller.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn direct_poller_polls_every_200_ms() {
        let (tx, mut rx) = mpsc::channel(8);
        let poller = tokio::spawn(run_conductor_poller(ConductorSource::Static(Vec::new()), tx));

        assert_eq!(gaps(&mut rx, 4).await, vec![200, 200, 200]);
        poller.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn unauthorized_triggers_one_login_then_backs_off() {
        let fetches = Arc::new(AtomicUsize::new(0));
        let logins = Arc::new(AtomicUsize::new(0));
        let (fetch_count, login_count) = (Arc::clone(&fetches), Arc::clone(&logins));
        let mut source = MockClusterSource::new();
        source.expect_fetch().returning(move || {
            fetch_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(unauthenticated()) })
        });
        source.expect_login().returning(move |token_url| {
            assert_eq!(token_url.as_deref(), Some("https://login.example/token"));
            login_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(GateError::LoginRequired { url: None }) })
        });
        source.expect_reload_token().return_const(false);
        source.expect_explicit_source().return_const(false);
        let (tx, mut rx) = mpsc::channel(64);
        let poller = tokio::spawn(GateClusterPoller::run(Arc::new(source), tx));

        let first = rx.recv().await.unwrap();
        assert!(
            matches!(&first, ConductorPollUpdate::GateNotice(n) if n == GateClusterPoller::LOGIN_PENDING),
            "{first:?}"
        );
        // A minute of ticks: the failed login is reported, nothing is retried.
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        assert_eq!(logins.load(Ordering::SeqCst), 1);
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        let second = rx.recv().await.unwrap();
        assert!(
            matches!(&second, ConductorPollUpdate::GateNotice(n) if n.contains("login needed")),
            "{second:?}"
        );
        assert!(rx.try_recv().is_err());

        // Once the retry window passes, one more poll and one more login.
        tokio::time::sleep(GateClusterPoller::LOGIN_RETRY_AFTER).await;
        assert_eq!(logins.load(Ordering::SeqCst), 2);
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
        poller.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn polling_resumes_after_login_succeeds() {
        let fetches = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&fetches);
        let mut source = MockClusterSource::new();
        source.expect_fetch().returning(move || {
            let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
            Box::pin(async move { if first { Err(unauthenticated()) } else { Ok(snapshot()) } })
        });
        source.expect_login().times(1).returning(|_| Box::pin(async { Ok(()) }));
        source.expect_reload_token().return_const(false);
        source.expect_explicit_source().return_const(false);
        let (tx, mut rx) = mpsc::channel(8);
        let poller = tokio::spawn(GateClusterPoller::run(Arc::new(source), tx));

        assert!(matches!(rx.recv().await.unwrap(), ConductorPollUpdate::GateNotice(_)));
        let next = rx.recv().await.unwrap();
        assert!(matches!(next, ConductorPollUpdate::GateSnapshot(_)), "{next:?}");
        poller.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn polling_resumes_when_a_cached_token_appears() {
        let fetches = Arc::new(AtomicUsize::new(0));
        let reloads = Arc::new(AtomicUsize::new(0));
        let (fetch_count, reload_count) = (Arc::clone(&fetches), Arc::clone(&reloads));
        let mut source = MockClusterSource::new();
        source.expect_fetch().returning(move || {
            let first = fetch_count.fetch_add(1, Ordering::SeqCst) == 0;
            Box::pin(async move { if first { Err(unauthenticated()) } else { Ok(snapshot()) } })
        });
        source
            .expect_login()
            .times(1)
            .returning(|_| Box::pin(async { Err(GateError::LoginRequired { url: None }) }));
        // A token shows up in the cache on the fifth check.
        source
            .expect_reload_token()
            .returning(move || reload_count.fetch_add(1, Ordering::SeqCst) >= 4);
        source.expect_explicit_source().return_const(false);
        let (tx, mut rx) = mpsc::channel(8);
        let poller = tokio::spawn(GateClusterPoller::run(Arc::new(source), tx));

        let mut updates = Vec::new();
        while !matches!(updates.last(), Some(ConductorPollUpdate::GateSnapshot(_))) {
            updates.push(rx.recv().await.unwrap());
        }
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
        assert!(updates.len() <= 3, "{updates:?}");
        poller.abort();
    }

    async fn serve(router: Router) -> Url {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Url::parse(&format!("http://{addr}/")).unwrap()
    }

    fn settings(url: Url, token: &str) -> GateSettings {
        GateSettings {
            url,
            token_file: None,
            token_command: None,
            timeout: GateSettings::DEFAULT_TIMEOUT,
            env_token: Some(token.to_string()),
            cache_root: None,
            token_command_timeout: GateSettings::TOKEN_COMMAND_TIMEOUT,
        }
    }

    #[tokio::test]
    async fn client_fetches_the_cluster_with_the_bearer_token() {
        let router = Router::new().route(
            "/admin/conductor/cluster",
            get(|headers: HeaderMap| async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok());
                assert_eq!(auth, Some("Bearer tok-1"));
                Json(serde_json::from_str::<Value>(CLUSTER_PARTIAL).unwrap())
            }),
        );
        let client = settings(serve(router).await, "tok-1").client_for_tui_login().await.unwrap();

        let cluster = ClusterSource::fetch(&client).await.unwrap();

        assert_eq!(cluster.servers.len(), 2);
        assert_eq!(cluster.leader_id.as_deref(), Some("sequencer-0"));
    }

    #[tokio::test]
    async fn client_reports_401_with_token_url_without_logging_in() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let router = Router::new().route(
            "/admin/conductor/cluster",
            get(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async {
                    (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({"error": "unauthenticated", "token_url": "https://login.example/token"})),
                    )
                }
            }),
        );
        let client = settings(serve(router).await, "stale").client_for_tui_login().await.unwrap();

        let error = ClusterSource::fetch(&client).await.unwrap_err();

        assert_eq!(error, unauthenticated());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // An explicit source is read again instead of logging in; the same
        // rejected token is an error, so the poller backs off.
        assert!(ClusterSource::explicit_source(&client));
        assert!(matches!(ClusterSource::login(&client, None).await, Err(GateError::Token(_))));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// Polls a gateway that accepts only `tok-2`, with a token command that
    /// counts its runs in `dir/runs` and prints `output` (`$n` is the run).
    /// Returns the updates, the Authorization headers seen, and the runs.
    async fn poll_with_command(
        output: &str,
        dir: &std::path::Path,
    ) -> (mpsc::Receiver<ConductorPollUpdate>, Arc<Mutex<Vec<String>>>, std::path::PathBuf) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let router = Router::new().route(
            "/admin/conductor/cluster",
            get(move |headers: HeaderMap| {
                let auth = headers["authorization"].to_str().unwrap().to_string();
                log.lock().unwrap().push(auth.clone());
                async move {
                    if auth == "Bearer tok-2" {
                        (StatusCode::OK, CLUSTER_EXAMPLE.to_string())
                    } else {
                        (StatusCode::UNAUTHORIZED, json!({"error": "unauthenticated"}).to_string())
                    }
                }
            }),
        );
        let runs = dir.join("runs");
        let file = runs.display();
        let settings = GateSettings {
            env_token: None,
            token_command: Some(format!(
                "n=$(cat '{file}' 2>/dev/null || echo 0); n=$((n + 1)); echo $n > '{file}'; echo {output}"
            )),
            ..settings(serve(router).await, "unused")
        };
        let client = settings.client_for_tui_login().await.unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(GateClusterPoller::run(Arc::new(client), tx));
        let first = rx.recv().await.unwrap();
        assert!(
            matches!(&first, ConductorPollUpdate::GateNotice(n) if n == GateClusterPoller::TOKEN_SOURCE_PENDING),
            "{first:?}"
        );
        (rx, seen, runs)
    }

    #[tokio::test]
    async fn poller_recovers_when_the_token_command_returns_a_new_token() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rx, seen, runs) = poll_with_command("tok-$n", dir.path()).await;

        let next = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap();
        assert!(matches!(next, ConductorPollUpdate::GateSnapshot(_)), "{next:?}");
        assert_eq!(*seen.lock().unwrap(), vec!["Bearer tok-1", "Bearer tok-2"]);
        assert_eq!(std::fs::read_to_string(runs).unwrap().trim(), "2");
    }

    #[tokio::test]
    async fn unchanged_rejected_token_is_not_sent_again_within_the_retry_window() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rx, seen, runs) = poll_with_command("tok-stale", dir.path()).await;

        let next = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap();
        assert!(
            matches!(&next, ConductorPollUpdate::GateNotice(n)
                if n.contains("returned the token the gateway rejected") && n.contains("read again within 5 minutes")),
            "{next:?}"
        );
        // Another poll tick passes: the source was read again once, and the
        // rejected token was not sent again.
        tokio::time::sleep(ConductorFeed::GATEWAY_POLL_INTERVAL + Duration::from_millis(500)).await;
        assert!(rx.try_recv().is_err());
        assert_eq!(std::fs::read_to_string(runs).unwrap().trim(), "2");
        assert_eq!(*seen.lock().unwrap(), vec!["Bearer tok-stale"]);
    }
}
