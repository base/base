//! The sequencer endpoints of the batcher, and which of them is the leader.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use alloy_provider::{Provider, RootProvider};
use base_common_network::Base;
use base_consensus_rpc::AdminApiClient;
use base_runtime::Runtime;
use futures::future;
use jsonrpsee::{
    core::ClientError,
    http_client::{HttpClient, HttpClientBuilder},
};
use tracing::{info, warn};
use url::Url;

/// One sequencer endpoint, which serves both the rollup node methods and the L2 blocks.
#[derive(derive_more::Debug)]
pub struct Sequencer {
    /// The endpoint origin, for logs and errors. It never carries a path or credentials.
    pub origin: String,
    /// Client of the rollup node methods, `optimism_*` and `admin_sequencerActive`.
    #[debug(skip)]
    pub rollup_node_client: HttpClient,
    /// Provider of the L2 blocks.
    #[debug(skip)]
    pub l2_provider: RootProvider<Base>,
}

impl Sequencer {
    /// Creates the clients of the sequencer endpoint at `url`.
    pub async fn new(url: &Url) -> eyre::Result<Self> {
        let origin = url.origin().ascii_serialization();

        let rollup_node_client = HttpClientBuilder::default()
            .build(url.as_str())
            .map_err(|e| eyre::eyre!("failed to build the rollup node client for {origin}: {e}"))?;
        let l2_provider = RootProvider::connect(url.as_str())
            .await
            .map_err(|e| eyre::eyre!("failed to build the L2 provider for {origin}: {e}"))?;

        Ok(Self { origin, rollup_node_client, l2_provider })
    }
}

/// The sequencer endpoints of the batcher, one of which is the leader that builds the blocks.
///
/// A single sequencer is the leader. Among several, the leader is the first whose
/// `admin_sequencerActive` answers `true`, which [`refresh_leader`](Self::refresh_leader) looks
/// for.
///
/// As a [`Provider`], it sends each request to the current leader.
#[derive(Debug)]
pub struct Sequencers {
    sequencers: Vec<Sequencer>,
    /// Index of the leader in `sequencers`.
    leader_index: AtomicUsize,
}

impl Sequencers {
    /// How long a sequencer has to answer `admin_sequencerActive` in
    /// [`refresh_leader`](Self::refresh_leader).
    pub const SEQUENCER_ACTIVE_RPC_TIMEOUT: Duration = Duration::from_secs(2);

    /// Creates the sequencers at `urls`. The first one is the leader until
    /// [`refresh_leader`](Self::refresh_leader) finds another.
    ///
    /// # Errors
    ///
    /// Returns an error when `urls` is empty or one of them is not an HTTP URL.
    pub async fn new(urls: &[Url]) -> eyre::Result<Self> {
        if urls.is_empty() {
            eyre::bail!("at least one sequencer URL is required");
        }
        let sequencers = future::try_join_all(urls.iter().map(Sequencer::new)).await?;
        Ok(Self { sequencers, leader_index: AtomicUsize::new(0) })
    }

    /// The current leader.
    pub fn leader(&self) -> &Sequencer {
        &self.sequencers[self.leader_index.load(Ordering::Relaxed)]
    }

    /// Asks every sequencer whether it is active and makes the first active one the leader.
    ///
    /// A sequencer that answers `false`, answers an error or does not answer within
    /// [`SEQUENCER_ACTIVE_RPC_TIMEOUT`](Self::SEQUENCER_ACTIVE_RPC_TIMEOUT) is not active.
    ///
    /// # Errors
    ///
    /// Returns an error naming the answer of every sequencer when none is active. The leader
    /// is then unchanged.
    pub async fn refresh_leader(&self) -> eyre::Result<()> {
        let answers = future::join_all(self.sequencers.iter().map(|sequencer| {
            tokio::time::timeout(
                Self::SEQUENCER_ACTIVE_RPC_TIMEOUT,
                sequencer.rollup_node_client.admin_sequencer_active(),
            )
        }))
        .await;
        let is_active: Vec<bool> =
            answers.iter().map(|answer| matches!(answer, Ok(Ok(true)))).collect();

        let active_count = is_active.iter().filter(|active| **active).count();
        if active_count > 1 {
            warn!(active_count, "several sequencers are active");
        }

        let Some(leader) = is_active.iter().position(|active| *active) else {
            let reasons: Vec<String> = self
                .sequencers
                .iter()
                .zip(&answers)
                .map(|(sequencer, answer)| {
                    let reason = match answer {
                        Ok(Ok(_)) => "not active".to_string(),
                        Ok(Err(ClientError::Call(error))) => error.message().to_string(),
                        Ok(Err(error)) => error.to_string(),
                        Err(_) => {
                            format!("no answer within {:?}", Self::SEQUENCER_ACTIVE_RPC_TIMEOUT)
                        }
                    };
                    format!("{}: {reason}", sequencer.origin)
                })
                .collect();
            eyre::bail!("no sequencer is active: {}", reasons.join(", "));
        };
        if leader != self.leader_index.swap(leader, Ordering::Relaxed) {
            info!(leader = %self.sequencers[leader].origin, "sequencer leader changed");
        }
        Ok(())
    }

    /// Looks for the leader every `poll_interval`, starting after the first interval, until
    /// `runtime` is cancelled.
    pub async fn track_leader<R: Runtime>(self: Arc<Self>, runtime: R, poll_interval: Duration) {
        loop {
            tokio::select! {
                biased;
                () = runtime.cancelled() => return,
                () = runtime.sleep(poll_interval) => {}
            }

            if let Err(error) = self.refresh_leader().await {
                warn!(
                    error = %error,
                    leader = %self.leader().origin,
                    "failed to find the sequencer leader, keeping the current one"
                );
            }
        }
    }
}

impl Provider<Base> for Sequencers {
    fn root(&self) -> &RootProvider<Base> {
        &self.leader().l2_provider
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use base_runtime::{Cancellation, TokioRuntime};

    use super::*;
    use crate::test_utils::{Activity, FakeSequencer};

    /// The leader is the active sequencer, which the reads go to. A sequencer that is stopped,
    /// one that is not the leader behind its conductor and one that is unreachable are passed
    /// over, even when they come first.
    #[tokio::test]
    async fn refresh_leader_makes_the_active_sequencer_the_leader() {
        let stopped = FakeSequencer::start(Activity::Stopped, 1).await;
        let not_leader = FakeSequencer::start(Activity::NotLeader, 2).await;
        let unreachable = "http://127.0.0.1:1".parse().unwrap();
        let active = FakeSequencer::start(Activity::Active, 3).await;
        let urls = [stopped.url.clone(), not_leader.url.clone(), unreachable, active.url.clone()];
        let sequencers = Sequencers::new(&urls).await.unwrap();

        sequencers.refresh_leader().await.unwrap();

        assert_eq!(sequencers.get_chain_id().await.unwrap(), 3);
    }

    /// A sequencer that accepts the connection but never answers is passed over once its answer
    /// times out, and holds the search back no longer than that.
    #[tokio::test]
    async fn refresh_leader_passes_over_a_sequencer_that_never_answers() {
        let silent = TcpListener::bind("127.0.0.1:0").unwrap();
        let silent_url = format!("http://{}", silent.local_addr().unwrap()).parse().unwrap();
        let active = FakeSequencer::start(Activity::Active, 1).await;
        let sequencers = Sequencers::new(&[silent_url, active.url.clone()]).await.unwrap();

        tokio::time::timeout(
            Sequencers::SEQUENCER_ACTIVE_RPC_TIMEOUT * 2,
            sequencers.refresh_leader(),
        )
        .await
        .expect("the search must not outlast the answer timeout")
        .unwrap();

        assert_eq!(sequencers.get_chain_id().await.unwrap(), 1);
    }

    /// Without an active sequencer the search fails, naming what each sequencer answered, and
    /// the leader stays the last one found.
    #[tokio::test]
    async fn refresh_leader_fails_and_keeps_the_leader_when_no_sequencer_is_active() {
        let first = FakeSequencer::start(Activity::Stopped, 1).await;
        let second = FakeSequencer::start(Activity::Active, 2).await;
        let sequencers = Sequencers::new(&[first.url.clone(), second.url.clone()]).await.unwrap();
        sequencers.refresh_leader().await.unwrap();

        second.set_activity(Activity::NotLeader);
        let error = sequencers.refresh_leader().await.unwrap_err();

        assert_eq!(
            error.to_string(),
            format!(
                "no sequencer is active: {}: not active, {}: not the leader",
                first.url.origin().ascii_serialization(),
                second.url.origin().ascii_serialization()
            )
        );
        assert_eq!(sequencers.get_chain_id().await.unwrap(), 2);
    }

    /// A read through the provider goes to the leader, and to the new leader once
    /// `track_leader` finds it. The tracker stops when the runtime is cancelled.
    #[tokio::test]
    async fn reads_go_to_the_current_leader() {
        let first = FakeSequencer::start(Activity::Active, 1).await;
        let second = FakeSequencer::start(Activity::NotLeader, 2).await;
        let sequencers =
            Arc::new(Sequencers::new(&[first.url.clone(), second.url.clone()]).await.unwrap());
        let runtime = TokioRuntime::new();
        let tracking = tokio::spawn(
            Arc::clone(&sequencers).track_leader(runtime.clone(), Duration::from_millis(1)),
        );
        assert_eq!(sequencers.get_chain_id().await.unwrap(), 1);

        first.set_activity(Activity::NotLeader);
        second.set_activity(Activity::Active);
        tokio::time::timeout(Duration::from_secs(5), async {
            while sequencers.get_chain_id().await.unwrap() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the reads must move to the second sequencer");

        runtime.cancel();
        tokio::time::timeout(Duration::from_secs(5), tracking)
            .await
            .expect("the tracker must stop on cancellation")
            .unwrap();
    }
}
