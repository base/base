use std::{sync::Arc, time::Duration};

use alloy_consensus::{
    EMPTY_OMMER_ROOT_HASH, EMPTY_ROOT_HASH, Header, Sealed,
    proofs::{calculate_transaction_root, calculate_withdrawals_root},
};
use alloy_primitives::{B256, Sealable};
use reth_chainspec::{ChainSpec, EthChainSpec};
use reth_eth_wire_types::{EthNetworkPrimitives, EthVersion, GetReceipts, GetReceipts70};
use reth_ethereum_forks::{ForkCondition, Head};
use reth_ethereum_primitives::{BlockBody, Receipt};
use reth_network::{FetchClient, NetworkConfig, NetworkHandle, NetworkManager, PeerRequest, Peers};
use reth_network_api::PeerId;
use reth_network_p2p::{
    bodies::client::BodiesClient,
    download::DownloadClient,
    headers::client::{HeadersClient, HeadersRequest},
};
use reth_storage_api::noop::NoopProvider;
use reth_tasks::Runtime;
use tokio::sync::oneshot;
use tracing::debug;

/// Errors returned by [`L1Peers`].
#[derive(Debug, thiserror::Error)]
pub enum L1PeersError {
    /// No peer returned data that verified against the request.
    #[error("no peer returned valid {kind} for block {hash}")]
    Unavailable {
        /// What was requested.
        kind: &'static str,
        /// The requested block hash.
        hash: B256,
    },
}

/// Fetches L1 headers, bodies and receipts from devp2p peers and verifies each response against
/// the block hash or header it was requested for.
#[derive(Debug, Clone)]
pub struct L1Peers {
    /// Handle to the devp2p network.
    pub network: NetworkHandle<EthNetworkPrimitives>,
    /// Header and body fetcher with built-in peer selection.
    pub fetch: FetchClient<EthNetworkPrimitives>,
    /// L1 chain spec, used for the status handshake.
    pub chain_spec: Arc<ChainSpec>,
}

impl L1Peers {
    /// Attempts per request before giving up.
    pub const ATTEMPTS: usize = 8;
    /// Timeout of a single request, including waiting for a peer.
    pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

    /// Starts a devp2p node for `chain_spec` listening on `port`, advertising block `hash` at
    /// `timestamp` as its head until [`Self::update_head`] is called.
    ///
    /// ponytail: uses a fresh node key on every start, so peers see a new node ID each restart.
    pub async fn spawn(
        chain_spec: Arc<ChainSpec>,
        port: u16,
        hash: B256,
        timestamp: u64,
        runtime: Runtime,
    ) -> eyre::Result<Self> {
        // The fork ID only depends on which forks are active, so a head past every block-based
        // fork at the right timestamp matches peers before the real block number is known.
        let number = chain_spec
            .hardforks
            .forks_iter()
            .filter_map(|(_, condition)| match condition {
                ForkCondition::Block(block)
                | ForkCondition::TTD { fork_block: Some(block), .. } => Some(block),
                _ => None,
            })
            .max()
            .unwrap_or_default();
        let head = Head {
            number,
            hash,
            timestamp,
            total_difficulty: chain_spec.final_paris_total_difficulty().unwrap_or_default(),
            ..Default::default()
        };
        let config =
            NetworkConfig::<(), EthNetworkPrimitives>::builder_with_rng_secret_key(runtime)
                .listener_port(port)
                .discovery_port(port)
                .disable_tx_gossip(true)
                .set_head(head)
                .boot_nodes(chain_spec.bootnodes().unwrap_or_default())
                .build_with_noop_provider(Arc::clone(&chain_spec));
        let (network, manager, _, request_handler) = NetworkManager::builder(config)
            .await?
            .request_handler(NoopProvider::eth(Arc::clone(&chain_spec)))
            .split_with_handle();
        let fetch = manager.fetch_client();
        tokio::spawn(manager);
        tokio::spawn(request_handler);
        Ok(Self { network, fetch, chain_spec })
    }

    /// Advertises `header` as the local head so peers compute a matching fork ID.
    pub fn update_head(&self, header: &Sealed<Header>) {
        self.network.update_status(Head {
            number: header.number,
            hash: header.hash(),
            difficulty: header.difficulty,
            total_difficulty: self.chain_spec.final_paris_total_difficulty().unwrap_or_default(),
            timestamp: header.timestamp,
        });
    }

    /// Fetches `limit` headers walking back from `start`, verified to hash-link to `start`.
    pub async fn headers_back(
        &self,
        start: B256,
        limit: u64,
    ) -> Result<Vec<Sealed<Header>>, L1PeersError> {
        for _ in 0..Self::ATTEMPTS {
            let request = self.fetch.get_headers(HeadersRequest::falling(start.into(), limit));
            let response = match tokio::time::timeout(Self::REQUEST_TIMEOUT, request).await {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    debug!(target: "l1", error = %error, start = %start, "header request failed");
                    continue;
                }
                Err(_) => continue,
            };
            let (peer, headers) = response.split();
            if headers.is_empty() {
                debug!(target: "l1", peer = %peer, start = %start, "peer returned no headers");
                continue;
            }
            let sealed: Vec<_> = headers.into_iter().map(Header::seal_slow).collect();
            let mut expected = start;
            let linked = sealed.iter().all(|header| {
                let ok = header.hash() == expected;
                expected = header.parent_hash;
                ok
            });
            if linked {
                return Ok(sealed);
            }
            debug!(
                target: "l1",
                peer = %peer,
                start = %start,
                first = %sealed[0].hash(),
                first_number = sealed[0].number,
                "peer returned unlinked headers"
            );
            self.fetch.report_bad_message(peer);
        }
        Err(L1PeersError::Unavailable { kind: "headers", hash: start })
    }

    /// Fetches the body of `header`, verified against its transaction and withdrawal roots.
    pub async fn body(&self, header: &Sealed<Header>) -> Result<BlockBody, L1PeersError> {
        if header.transactions_root == EMPTY_ROOT_HASH
            && header.withdrawals_root.is_none_or(|root| root == EMPTY_ROOT_HASH)
        {
            return Ok(BlockBody {
                withdrawals: header.withdrawals_root.map(|_| Default::default()),
                ..Default::default()
            });
        }
        for _ in 0..Self::ATTEMPTS {
            let request = self.fetch.get_block_bodies(vec![header.hash()]);
            let Ok(Ok(response)) = tokio::time::timeout(Self::REQUEST_TIMEOUT, request).await
            else {
                continue;
            };
            let (peer, bodies) = response.split();
            if let Some(body) = bodies.into_iter().next()
                && Self::body_matches(header, &body)
            {
                return Ok(body);
            }
            debug!(target: "l1", peer = %peer, block = %header.hash(), "peer returned invalid body");
            self.fetch.report_bad_message(peer);
        }
        Err(L1PeersError::Unavailable { kind: "body", hash: header.hash() })
    }

    /// Fetches the receipts of `header`, verified against its receipts root.
    pub async fn receipts(&self, header: &Sealed<Header>) -> Result<Vec<Receipt>, L1PeersError> {
        if header.receipts_root == EMPTY_ROOT_HASH {
            return Ok(Vec::new());
        }
        let peers = self.network.get_all_peers().await.unwrap_or_default();
        for peer in peers.iter().cycle().take(Self::ATTEMPTS.min(peers.len() * 2)) {
            let receipts = tokio::time::timeout(
                Self::REQUEST_TIMEOUT,
                self.receipts_from(peer.remote_id, peer.eth_version, header.hash()),
            )
            .await;
            if let Ok(Some(receipts)) = receipts
                && Receipt::calculate_receipt_root_no_memo(&receipts) == header.receipts_root
            {
                return Ok(receipts);
            }
            debug!(target: "l1", peer = %peer.remote_id, block = %header.hash(), "peer returned invalid receipts");
            self.fetch.report_bad_message(peer.remote_id);
        }
        Err(L1PeersError::Unavailable { kind: "receipts", hash: header.hash() })
    }

    async fn receipts_from(
        &self,
        peer: PeerId,
        version: EthVersion,
        hash: B256,
    ) -> Option<Vec<Receipt>> {
        if version >= EthVersion::Eth70 {
            let mut all = Vec::new();
            loop {
                let (response, rx) = oneshot::channel();
                let request = GetReceipts70 {
                    first_block_receipt_index: all.len() as u64,
                    block_hashes: vec![hash],
                };
                self.network.send_request(peer, PeerRequest::GetReceipts70 { request, response });
                let page = rx.await.ok()?.ok()?;
                let batch = page.receipts.into_iter().next()?;
                if page.last_block_incomplete && batch.is_empty() {
                    return None;
                }
                all.extend(batch);
                if !page.last_block_incomplete {
                    return Some(all);
                }
            }
        } else if version >= EthVersion::Eth69 {
            let (response, rx) = oneshot::channel();
            let request = GetReceipts(vec![hash]);
            self.network.send_request(peer, PeerRequest::GetReceipts69 { request, response });
            rx.await.ok()?.ok()?.0.into_iter().next()
        } else {
            let (response, rx) = oneshot::channel();
            let request = GetReceipts(vec![hash]);
            self.network.send_request(peer, PeerRequest::GetReceipts { request, response });
            let receipts = rx.await.ok()?.ok()?.0.into_iter().next()?;
            Some(receipts.into_iter().map(|receipt| receipt.receipt).collect())
        }
    }

    fn body_matches(header: &Header, body: &BlockBody) -> bool {
        body.ommers.is_empty()
            && header.ommers_hash == EMPTY_OMMER_ROOT_HASH
            && calculate_transaction_root(&body.transactions) == header.transactions_root
            && body.withdrawals.as_ref().map(|w| calculate_withdrawals_root(w))
                == header.withdrawals_root
    }
}
