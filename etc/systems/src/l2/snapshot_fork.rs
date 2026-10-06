//! Upstream L1 inputs for discovering the L1 block that derives a snapshot's unsafe tail.

use std::fmt;

use alloy_eips::BlockNumberOrTag;
use alloy_provider::{Provider, RootProvider};
use base_consensus_providers::{BeaconClient, OnlineBeaconClient};
use eyre::{OptionExt, Result, ensure, eyre};
use tokio::time::{Instant, timeout_at};
use url::Url;

use super::SnapshotInspection;

/// Upstream L1 execution and Beacon endpoints used for fork discovery.
///
/// Their URLs may embed credentials, including in query strings, so neither they nor provider
/// errors that could echo them appear in [`fmt::Debug`] output or errors.
#[derive(Clone)]
pub struct SnapshotForkSource {
    /// L1 execution JSON-RPC URL.
    pub execution: Url,
    /// L1 Beacon API URL.
    pub beacon: Url,
}

impl fmt::Debug for SnapshotForkSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotForkSource").finish_non_exhaustive()
    }
}

impl SnapshotForkSource {
    /// Environment variable holding the upstream L1 execution JSON-RPC URL.
    pub const EXECUTION_ENV: &'static str = "SNAPSHOT_UPSTREAM_EXECUTION";
    /// Environment variable holding the upstream L1 Beacon API URL.
    pub const BEACON_ENV: &'static str = "SNAPSHOT_UPSTREAM_BEACON";

    /// Reads both endpoints from [`Self::EXECUTION_ENV`] and [`Self::BEACON_ENV`] only.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            execution: Self::env_url(Self::EXECUTION_ENV)?,
            beacon: Self::env_url(Self::BEACON_ENV)?,
        })
    }

    /// Parses an endpoint environment variable without echoing its value on failure.
    pub fn env_url(name: &str) -> Result<Url> {
        let value =
            std::env::var(name).map_err(|_| eyre!("{name} must be set to an upstream L1 URL"))?;
        Url::parse(&value).map_err(|_| eyre!("{name} is not a valid URL"))
    }
}

/// Upstream L1 metadata that bounds fork discovery for one snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotForkMetadata {
    /// Upstream finalized L1 block number.
    pub finalized: u64,
    /// Last L1 block discovery may read: the earlier of [`Self::finalized`] and the latest
    /// snapshot block's L1 origin plus the sequencer window, after which its batch is invalid.
    pub l1_limit: u64,
    /// Beacon genesis time, in seconds since the Unix epoch.
    pub genesis_time: u64,
    /// Beacon slot duration, in seconds.
    pub slot_interval: u64,
}

impl SnapshotForkMetadata {
    /// Reads and validates `source`'s metadata for `inspection`, giving up at `deadline`.
    ///
    /// Takes a deadline rather than a duration so a whole discovery shares one budget. Fails when
    /// the execution chain ID is not the rollup's L1 chain ID, when upstream finality is missing or
    /// behind the latest snapshot block's L1 origin, or when Beacon metadata is unreadable or has
    /// a zero slot duration. Errors never contain upstream URLs.
    pub async fn read(
        source: &SnapshotForkSource,
        inspection: &SnapshotInspection,
        deadline: Instant,
    ) -> Result<Self> {
        let mut stage = "reading the upstream L1 chain ID";
        let read = async {
            let config = &inspection.rollup_config;
            let latest_origin = inspection.latest.block_info.l1_origin.number;
            let l1: RootProvider = RootProvider::new_http(source.execution.clone());
            let chain_id = l1
                .get_chain_id()
                .await
                .map_err(|_| eyre!("failed to read the upstream L1 chain ID"))?;
            ensure!(
                chain_id == config.l1_chain_id,
                "upstream L1 chain ID {chain_id} does not match rollup L1 chain ID {}",
                config.l1_chain_id
            );

            stage = "reading the upstream finalized L1 block";
            let finalized = l1
                .get_block_by_number(BlockNumberOrTag::Finalized)
                .await
                .map_err(|_| eyre!("failed to read the upstream finalized L1 block"))?
                .ok_or_eyre("upstream L1 has no finalized block")?
                .header
                .number;
            ensure!(
                finalized >= latest_origin,
                "upstream finalized L1 block {finalized} is behind latest snapshot L1 origin \
                 {latest_origin}"
            );

            let beacon = OnlineBeaconClient::new_http(source.beacon.to_string());
            stage = "reading upstream Beacon genesis";
            let genesis_time = beacon
                .genesis_time()
                .await
                .map_err(|_| eyre!("failed to read upstream Beacon genesis time"))?
                .data
                .genesis_time;
            stage = "reading upstream Beacon slot duration";
            let slot_interval = beacon
                .slot_interval()
                .await
                .map_err(|_| eyre!("failed to read upstream Beacon slot duration"))?
                .data
                .seconds_per_slot;
            ensure!(slot_interval > 0, "upstream Beacon slot duration must be positive");

            Ok(Self {
                finalized,
                l1_limit: latest_origin.saturating_add(config.seq_window_size).min(finalized),
                genesis_time,
                slot_interval,
            })
        };
        timeout_at(deadline, read).await.map_err(|_| eyre!("timed out {stage}"))?
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use axum::{
        Router,
        extract::{RawQuery, State},
        http::StatusCode,
        routing::{get, post},
    };
    use serde_json::{Value, json};
    use tokio::{
        net::TcpListener,
        time::{Instant, sleep},
    };

    use super::{SnapshotForkMetadata, SnapshotForkSource};
    use crate::{SnapshotInspection, test_utils::SnapshotRpcFixture};

    /// Query-string credential every upstream request must carry.
    const CREDENTIAL: &str = "key=secret";
    const L1_CHAIN_ID: u64 = 1;
    /// Latest L1 origin of [`SnapshotRpcFixture`]'s default chain.
    const LATEST_ORIGIN: u64 = 11;
    const SEQ_WINDOW_SIZE: u64 = 10;
    const GENESIS_TIME: u64 = 1_606_824_023;
    const DEADLINE: Duration = Duration::from_secs(10);

    /// Upstream L1 execution JSON-RPC and Beacon API, served from one HTTP server.
    #[derive(Clone)]
    struct Upstream {
        chain_id: u64,
        finalized: Option<u64>,
        genesis: (StatusCode, String),
        spec: (StatusCode, String),
        stall_spec: bool,
    }

    impl Default for Upstream {
        fn default() -> Self {
            Self {
                chain_id: L1_CHAIN_ID,
                finalized: Some(100),
                genesis: (
                    StatusCode::OK,
                    json!({"data": {"genesis_time": GENESIS_TIME.to_string()}}).to_string(),
                ),
                spec: Self::spec("12"),
                stall_spec: false,
            }
        }
    }

    impl Upstream {
        /// A Beacon spec response with decimal `SECONDS_PER_SLOT`, as Beacon nodes serve it.
        fn spec(seconds_per_slot: &str) -> (StatusCode, String) {
            (StatusCode::OK, json!({"data": {"SECONDS_PER_SLOT": seconds_per_slot}}).to_string())
        }

        fn authorized(
            query: Option<String>,
            response: (StatusCode, String),
        ) -> (StatusCode, String) {
            if query.as_deref() == Some(CREDENTIAL) {
                response
            } else {
                (StatusCode::UNAUTHORIZED, String::new())
            }
        }

        async fn rpc(
            State(upstream): State<Self>,
            RawQuery(query): RawQuery,
            body: String,
        ) -> (StatusCode, String) {
            let request: Value = serde_json::from_str(&body).unwrap();
            let result = match request["method"].as_str() {
                Some("eth_chainId") => json!(format!("{:#x}", upstream.chain_id)),
                Some("eth_getBlockByNumber") if request["params"][0] == "finalized" => {
                    upstream.finalized.map_or(Value::Null, |number| {
                        let mut block = alloy_rpc_types_eth::Block::<
                            alloy_rpc_types_eth::Transaction,
                        >::default();
                        block.header.inner.number = number;
                        serde_json::to_value(block).unwrap()
                    })
                }
                method => panic!("unexpected upstream request {method:?}"),
            };
            let response = json!({"jsonrpc": "2.0", "id": request["id"], "result": result});
            Self::authorized(query, (StatusCode::OK, response.to_string()))
        }

        /// Reads metadata for the default snapshot chain, whose rollup config accepts batches
        /// through L1 block `LATEST_ORIGIN + SEQ_WINDOW_SIZE`.
        async fn read(self, deadline: Duration) -> eyre::Result<SnapshotForkMetadata> {
            let fixture = SnapshotRpcFixture::default();
            let mut rollup_config = fixture.rollup_config();
            rollup_config.l1_chain_id = L1_CHAIN_ID;
            rollup_config.seq_window_size = SEQ_WINDOW_SIZE;
            let (l2_url, l2) = fixture.serve(SnapshotRpcFixture::CHAIN_ID).await;
            let inspection = SnapshotInspection::read(
                l2_url,
                Arc::new(rollup_config),
                SnapshotRpcFixture::CHAIN_ID,
            )
            .await
            .unwrap();
            l2.stop().unwrap();

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let source = SnapshotForkSource {
                execution: format!("{base}/rpc?{CREDENTIAL}").parse().unwrap(),
                beacon: format!("{base}/beacon/?{CREDENTIAL}").parse().unwrap(),
            };
            let app = Router::new()
                .route("/rpc", post(Self::rpc))
                .route(
                    "/beacon/eth/v1/beacon/genesis",
                    get(|State(upstream): State<Self>, RawQuery(query)| async move {
                        Self::authorized(query, upstream.genesis)
                    }),
                )
                .route(
                    "/beacon/eth/v1/config/spec",
                    get(|State(upstream): State<Self>, RawQuery(query)| async move {
                        if upstream.stall_spec {
                            sleep(Duration::from_secs(3600)).await;
                        }
                        Self::authorized(query, upstream.spec)
                    }),
                )
                .with_state(self);
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

            let result =
                SnapshotForkMetadata::read(&source, &inspection, Instant::now() + deadline).await;
            server.abort();
            if let Err(error) = &result {
                let report = format!("{error:?}");
                assert!(!report.contains("127.0.0.1") && !report.contains("secret"), "{report}");
            }
            result
        }
    }

    #[tokio::test]
    async fn reads_metadata_and_bounds_l1_range() {
        for (finalized, l1_limit) in
            [(100, LATEST_ORIGIN + SEQ_WINDOW_SIZE), (15, 15), (LATEST_ORIGIN, LATEST_ORIGIN)]
        {
            let upstream = Upstream { finalized: Some(finalized), ..Default::default() };
            let metadata = upstream.read(DEADLINE).await.unwrap();
            assert_eq!(
                metadata,
                SnapshotForkMetadata {
                    finalized,
                    l1_limit,
                    genesis_time: GENESIS_TIME,
                    slot_interval: 12
                }
            );
        }
    }

    #[tokio::test]
    async fn rejects_invalid_upstream_metadata() {
        let default = Upstream::default;
        for (upstream, message) in [
            (Upstream { chain_id: 11_155_111, ..default() }, "does not match rollup L1 chain ID 1"),
            (Upstream { finalized: None, ..default() }, "upstream L1 has no finalized block"),
            (
                Upstream { finalized: Some(LATEST_ORIGIN - 1), ..default() },
                "upstream finalized L1 block 10 is behind latest snapshot L1 origin 11",
            ),
            (
                Upstream { genesis: (StatusCode::OK, "not json".to_string()), ..default() },
                "failed to read upstream Beacon genesis time",
            ),
            (
                Upstream { spec: (StatusCode::INTERNAL_SERVER_ERROR, String::new()), ..default() },
                "failed to read upstream Beacon slot duration",
            ),
            (Upstream { spec: Upstream::spec("0"), ..default() }, "slot duration must be positive"),
        ] {
            let error = upstream.read(DEADLINE).await.unwrap_err();
            assert!(error.to_string().contains(message), "{error:?}");
        }
    }

    #[tokio::test]
    async fn times_out_at_deadline_on_stalled_upstream() {
        let upstream = Upstream { stall_spec: true, ..Default::default() };
        let error = upstream.read(Duration::from_millis(500)).await.unwrap_err();
        assert_eq!(error.to_string(), "timed out reading upstream Beacon slot duration");
    }

    #[test]
    fn hides_endpoints_from_debug_and_env_errors() {
        let source = SnapshotForkSource {
            execution: "https://user:secret@l1.example/key".parse().unwrap(),
            beacon: "https://beacon.example/?token=secret".parse().unwrap(),
        };
        assert_eq!(format!("{source:?}"), "SnapshotForkSource { .. }");
        let error = SnapshotForkSource::env_url("SNAPSHOT_FORK_TEST_UNSET_URL").unwrap_err();
        assert_eq!(
            error.to_string(),
            "SNAPSHOT_FORK_TEST_UNSET_URL must be set to an upstream L1 URL"
        );
    }
}
