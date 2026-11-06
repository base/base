use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_consensus::{Header, Sealed, Transaction as _};
use alloy_primitives::{Address, B256};
use jsonrpsee::server::ServerHandle;
use lru::LruCache;
use parking_lot::{Mutex, RwLock};
use reth_chainspec::{ChainSpec, MAINNET, SEPOLIA};
use reth_ethereum_forks::{EthereumHardfork, ForkCondition};
use reth_ethereum_primitives::{BlockBody, Receipt};
use reth_network_p2p::download::DownloadClient;
use reth_tasks::Runtime;
use tokio::{
    sync::{Notify, mpsc},
    time::Instant,
};
use tracing::{debug, info, warn};
use url::Url;

use crate::{
    BeaconNetwork, BeaconPeers, BeaconPeersError, ForkSchedule, L1Peers, L1PeersError, L1Provider,
    L1Rpc, LightClient, LightClientError, LightClientUpdate, VerifiedChain,
};

/// Local endpoints of a running [`L1Node`].
#[derive(Debug)]
pub struct L1Endpoints {
    /// URL of the execution JSON-RPC subset, for `--l1-eth-rpc`.
    pub eth_rpc: Url,
    /// Keeps the JSON-RPC server running.
    pub server: ServerHandle,
    /// Typed in-process access for derivation and sequencing.
    pub provider: L1Provider,
}

/// Errors from one light-client sync step.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// Fetching light-client data failed.
    #[error(transparent)]
    Source(#[from] BeaconPeersError),
    /// Light-client data failed verification.
    #[error(transparent)]
    LightClient(#[from] LightClientError),
    /// Peers did not serve the verified head.
    #[error(transparent)]
    Peers(#[from] L1PeersError),
}

/// A stateless L1 node: heads come from the beacon light client, block data from devp2p peers.
#[derive(Debug)]
pub struct L1Node {
    /// devp2p fetcher.
    pub peers: L1Peers,
    /// Canonical headers back from the light-client optimistic head.
    pub chain: RwLock<VerifiedChain>,
    /// Execution block hash of the light-client finalized head.
    pub finalized: RwLock<Option<B256>>,
    /// Verified bodies by block hash.
    pub bodies: Mutex<LruCache<B256, Arc<BlockBody>>>,
    /// Verified receipts by block hash.
    pub receipts: Mutex<LruCache<B256, Arc<Vec<Receipt>>>>,
    /// Notified whenever the canonical tip moves.
    pub advanced: Notify,
}

impl L1Node {
    /// Headers kept in the canonical index, about two weeks of L1.
    pub const MAX_HEADERS: usize = 100_000;
    /// Bodies and receipts kept in memory.
    pub const CACHE_SIZE: NonZeroUsize = NonZeroUsize::new(1024).unwrap();
    /// Headers requested per batch.
    pub const BATCH: u64 = 512;
    /// Most recent blocks whose data is fetched before consensus asks for it.
    pub const PREFETCH_DEPTH: usize = 32;

    /// Creates a node with an empty index.
    pub fn new(peers: L1Peers) -> Self {
        Self {
            peers,
            chain: RwLock::new(VerifiedChain::new(Self::MAX_HEADERS)),
            finalized: RwLock::new(None),
            bodies: Mutex::new(LruCache::new(Self::CACHE_SIZE)),
            receipts: Mutex::new(LruCache::new(Self::CACHE_SIZE)),
            advanced: Notify::new(),
        }
    }

    /// Returns the verified header with `hash`, fetching it from peers if it is not canonical.
    pub async fn header_by_hash(&self, hash: B256) -> Result<Sealed<Header>, L1PeersError> {
        if let Some(header) = self.chain.read().by_hash(hash).cloned() {
            return Ok(header);
        }
        let mut headers = self.peers.headers_back(hash, 1).await?;
        headers.truncate(1);
        Ok(headers.remove(0))
    }

    /// Returns the canonical header with `hash`, backfilling below the index from peers.
    ///
    /// Returns `None` if `hash` is not on the canonical chain.
    pub async fn canonical_header(
        &self,
        hash: B256,
    ) -> Result<Option<Sealed<Header>>, L1PeersError> {
        if let Some(header) = self.chain.read().by_hash(hash).cloned() {
            return Ok(Some(header));
        }
        if self.chain.read().tip().is_none() {
            return Err(L1PeersError::Unavailable { kind: "headers", hash });
        }
        let Ok(header) = self.header_by_hash(hash).await else {
            return if self.peers.fetch.num_connected_peers() > 0 {
                Ok(None)
            } else {
                Err(L1PeersError::Unavailable { kind: "headers", hash })
            };
        };
        Ok(self.header_by_number(header.number).await?.filter(|canonical| canonical.hash() == hash))
    }

    /// Returns the canonical header at `number`, backfilling below the index from peers.
    ///
    /// Returns `None` above the verified head.
    pub async fn header_by_number(
        &self,
        number: u64,
    ) -> Result<Option<Sealed<Header>>, L1PeersError> {
        let (mut parent, mut next) = {
            let chain = self.chain.read();
            if let Some(header) = chain.by_number.get(&number) {
                return Ok(Some(header.clone()));
            }
            match chain.lowest() {
                Some(lowest) if number < lowest.number => (lowest.parent_hash, lowest.number),
                _ => return Ok(None),
            }
        };
        while next > number {
            let headers = self.peers.headers_back(parent, (next - number).min(Self::BATCH)).await?;
            self.chain.write().prepend(&headers);
            if let Some(header) = headers.iter().find(|header| header.number == number) {
                return Ok(Some(header.clone()));
            }
            let [.., oldest] = &headers[..] else { break };
            (parent, next) = (oldest.parent_hash, oldest.number);
        }
        Ok(None)
    }

    /// Returns the verified body of `header`.
    pub async fn body(&self, header: &Sealed<Header>) -> Result<Arc<BlockBody>, L1PeersError> {
        if let Some(body) = self.bodies.lock().get(&header.hash()) {
            return Ok(Arc::clone(body));
        }
        let body = Arc::new(self.peers.body(header).await?);
        self.bodies.lock().put(header.hash(), Arc::clone(&body));
        Ok(body)
    }

    /// Returns the verified receipts of `header`.
    pub async fn receipts(
        &self,
        header: &Sealed<Header>,
    ) -> Result<Arc<Vec<Receipt>>, L1PeersError> {
        if let Some(receipts) = self.receipts.lock().get(&header.hash()) {
            return Ok(Arc::clone(receipts));
        }
        let receipts = Arc::new(self.peers.receipts(header).await?);
        self.receipts.lock().put(header.hash(), Arc::clone(&receipts));
        Ok(receipts)
    }

    /// Makes the block `head` the canonical tip, walking back to where it joins the index.
    pub async fn advance(&self, head: B256) -> Result<(), L1PeersError> {
        let tip = self.chain.read().tip().map(|tip| (tip.hash(), tip.number));
        if tip.is_some_and(|(hash, _)| hash == head) {
            return Ok(());
        }
        let mut headers = match tip {
            Some((_, tip)) => {
                let header = self.header_by_hash(head).await?;
                if self.chain.write().extend(std::slice::from_ref(&header)) {
                    self.peers.update_head(&header);
                    self.advanced.notify_one();
                    return Ok(());
                }
                let count = (header.number.saturating_sub(tip) + 1).clamp(1, Self::BATCH);
                let mut headers = vec![header];
                headers.extend(self.peers.headers_back(headers[0].parent_hash, count).await?);
                headers
            }
            None => self.peers.headers_back(head, Self::BATCH).await?,
        };
        let number = headers[0].number;
        if !self.chain.write().extend(&headers) {
            if let Some(oldest) = headers.last() {
                headers.extend(self.peers.headers_back(oldest.parent_hash, Self::BATCH).await?);
            }
            let mut chain = self.chain.write();
            if !chain.extend(&headers) {
                warn!(target: "l1", head = %head, number, "verified head does not join the index, resetting");
                chain.reset(&headers);
            }
        }
        self.peers.update_head(&headers[0]);
        self.advanced.notify_one();
        Ok(())
    }

    /// Fetches the body, receipts and batch-inbox blobs of each new canonical block before
    /// consensus asks, so its reads are answered from memory.
    ///
    /// Bodies and receipts are kept whole: deposit source hashes index every log in the block.
    ///
    /// ponytail: only the last [`Self::PREFETCH_DEPTH`] blocks, so reads further back, as after
    /// a restart, still go to peers. Persist the sequencing window to cover those.
    pub async fn prefetch(self: Arc<Self>, beacon: BeaconPeers, batch_inbox: Address) {
        let schedule = &beacon.network.schedule;
        loop {
            self.advanced.notified().await;
            let recent: Vec<_> = self
                .chain
                .read()
                .by_number
                .values()
                .rev()
                .take(Self::PREFETCH_DEPTH)
                .cloned()
                .collect();
            for header in recent {
                let had_body = self.bodies.lock().contains(&header.hash());
                let body = match self.body(&header).await {
                    Ok(body) => body,
                    Err(error) => {
                        debug!(target: "l1", error = %error, number = header.number, "failed to prefetch L1 body");
                        continue;
                    }
                };
                let mut hashes = Vec::new();
                for tx in &body.transactions {
                    if tx.to() == Some(batch_inbox) {
                        hashes.extend_from_slice(tx.blob_versioned_hashes().unwrap_or_default());
                    }
                }
                let had_blobs = hashes.iter().all(|hash| beacon.blob_cache.lock().contains(hash));
                let slot = header.timestamp.saturating_sub(schedule.genesis_time)
                    / schedule.seconds_per_slot;
                if let Err(error) = beacon.blobs(slot, &hashes).await {
                    debug!(target: "l1", error = %error, slot, "failed to prefetch batch blobs");
                    continue;
                }
                let had_receipts = self.receipts.lock().contains(&header.hash());
                if let Err(error) = self.receipts(&header).await {
                    debug!(target: "l1", error = %error, number = header.number, "failed to prefetch L1 receipts");
                    continue;
                }
                if !(had_body && had_blobs && had_receipts) {
                    debug!(target: "l1", number = header.number, blobs = hashes.len(), "prefetched L1 block");
                }
            }
        }
    }

    /// Follows the light client forever, saving each new finalized header to `store`.
    ///
    /// Updates from `gossip` are applied as they arrive. `source` is polled every slot for the
    /// latest updates while gossip has been quiet for two slots, and for sync committee updates
    /// whenever the next committee is unknown.
    pub async fn sync(
        self: Arc<Self>,
        source: BeaconPeers,
        mut client: LightClient,
        slot: Duration,
        mut gossip: mpsc::Receiver<LightClientUpdate>,
        store: PathBuf,
    ) {
        if let Err(error) = self.follow(&client, &store).await {
            self.warn_sync_failed(&error);
        }
        let quiet = slot * 2;
        let mut gossiped: Option<Instant> = None;
        loop {
            while let Ok(update) = gossip.try_recv() {
                self.apply_gossip(&mut client, &update, &mut gossiped, &store).await;
            }
            let poll = gossiped.is_none_or(|at| at.elapsed() > quiet);
            if (poll || client.next_committee.is_none())
                && let Err(error) = self.sync_step(&source, &mut client, poll, &store).await
            {
                self.warn_sync_failed(&error);
            }
            let tick = tokio::time::sleep(slot);
            tokio::pin!(tick);
            loop {
                tokio::select! {
                    () = &mut tick => break,
                    Some(update) = gossip.recv() => {
                        self.apply_gossip(&mut client, &update, &mut gossiped, &store).await;
                    }
                }
            }
        }
    }

    async fn apply_gossip(
        &self,
        client: &mut LightClient,
        update: &LightClientUpdate,
        gossiped: &mut Option<Instant>,
        store: &Path,
    ) {
        match client.apply(update) {
            Ok(advanced) => {
                *gossiped = Some(Instant::now());
                if advanced && let Err(error) = self.follow(client, store).await {
                    self.warn_sync_failed(&error);
                }
            }
            Err(error) => {
                if matches!(error, LightClientError::UnknownSyncCommittee(_)) {
                    *gossiped = None;
                }
                debug!(target: "l1", error = %error, "rejected gossiped light-client update");
            }
        }
    }

    fn warn_sync_failed(&self, error: &SyncError) {
        warn!(
            target: "l1",
            error = %error,
            peers = self.peers.fetch.num_connected_peers(),
            "embedded L1 sync step failed"
        );
    }

    /// Fetches sync committee updates if the client needs them and, when `poll`, the latest
    /// finality and optimistic updates.
    async fn sync_step(
        &self,
        source: &BeaconPeers,
        client: &mut LightClient,
        poll: bool,
        store: &Path,
    ) -> Result<(), SyncError> {
        let had_next = client.next_committee.is_some();
        // Few Gloas peers serve finality updates, so the optimistic head keeps moving without one.
        let finality = if poll {
            source
                .finality_update()
                .await
                .inspect_err(|error| {
                    warn!(target: "l1", error = %error, "no L1 finality update, following optimistic head");
                })
                .ok()
        } else {
            None
        };
        let optimistic = if poll { Some(source.optimistic_update().await) } else { None };
        let max_known = client.finalized_period() + u64::from(had_next);
        let ahead =
            |update: &LightClientUpdate| LightClient::period(update.signature_slot) > max_known;
        if !had_next
            || finality.as_ref().is_some_and(ahead)
            || optimistic.as_ref().is_some_and(|res| res.as_ref().is_ok_and(ahead))
        {
            match source.updates(client.finalized_period(), 128).await {
                Ok(updates) => {
                    for update in updates {
                        client.apply(&update)?;
                    }
                }
                Err(error) => {
                    warn!(target: "l1", error = %error, "failed to fetch L1 sync committee updates");
                }
            }
        }
        if let Some(finality) = finality
            && let Err(error) = client.apply(&finality)
        {
            warn!(target: "l1", error = %error, "rejected finality update");
        }
        if let Some(Ok(optimistic)) = &optimistic
            && let Err(error) = client.apply(optimistic)
        {
            warn!(target: "l1", error = %error, "rejected optimistic update");
        }
        if !had_next
            && client.next_committee.is_some()
            && let Err(error) = client.save(store)
        {
            warn!(target: "l1", error = %error, "failed to save L1 light-client state");
        }
        self.follow(client, store).await?;
        if let Some(Err(error)) = optimistic {
            return Err(error.into());
        }
        Ok(())
    }

    /// Saves a new finalized header to `store` and makes the optimistic head canonical.
    async fn follow(&self, client: &LightClient, store: &Path) -> Result<(), SyncError> {
        let finalized = client.finalized.block_hash();
        if self.finalized.write().replace(finalized) != Some(finalized) {
            info!(target: "l1", hash = %finalized, slot = client.finalized.beacon.slot, "L1 finalized head");
            if let Err(error) = client.save(store) {
                warn!(target: "l1", error = %error, "failed to save L1 light-client state");
            }
        }
        self.advance(client.optimistic.block_hash()).await?;
        Ok(())
    }

    /// Starts the L1 node for the L1 chain `l1_chain_id`, keeping the light-client state,
    /// beacon node key and custody columns in `datadir`. Blobs sent to `batch_inbox` are fetched
    /// as soon as their block is verified.
    ///
    /// The JSON-RPC server stops when the returned handle is dropped.
    pub async fn start(
        l1_chain_id: u64,
        batch_inbox: Address,
        checkpoint: Option<B256>,
        port: u16,
        datadir: PathBuf,
        runtime: Runtime,
    ) -> eyre::Result<L1Endpoints> {
        let network = BeaconNetwork::for_chain(l1_chain_id)
            .ok_or_else(|| eyre::eyre!("embedded L1 does not support L1 chain {l1_chain_id}"))?;
        let chain_spec = Self::chain_spec(l1_chain_id)?;
        std::fs::create_dir_all(&datadir)?;
        let store = datadir.join("light-client.json");
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let l1_peers = L1Peers::spawn(chain_spec, port, B256::ZERO, now, runtime).await?;

        let schedule = network.schedule.clone();
        let slot = Duration::from_secs(schedule.seconds_per_slot);
        let default_checkpoint = network.checkpoint;
        let (peers, gossip) = BeaconPeers::spawn(network, datadir).await?;
        let client = match Self::restore(checkpoint, &schedule, &store) {
            Some(client) => client,
            None => {
                Self::bootstrap(&peers, schedule, checkpoint.unwrap_or(default_checkpoint)).await?
            }
        };
        let node = Arc::new(Self::new(l1_peers));
        tokio::spawn(Arc::clone(&node).sync(peers.clone(), client, slot, gossip, store));
        let (eth_rpc, server) = L1Rpc { node: Arc::clone(&node) }.start().await?;
        info!(target: "l1", url = %eth_rpc, l1_chain_id, "embedded L1 started");
        tokio::spawn(Arc::clone(&node).prefetch(peers.clone(), batch_inbox));
        tokio::spawn(peers.custody.clone().sync(peers.clone(), Arc::clone(&node), slot));
        let provider = L1Provider { node, beacon: peers };
        Ok(L1Endpoints { eth_rpc, server, provider })
    }

    /// Restores the light client saved in `store`, unless a checkpoint was given explicitly.
    ///
    /// ponytail: saved state wins over the built-in checkpoint, however old it is.
    fn restore(
        checkpoint: Option<B256>,
        schedule: &ForkSchedule,
        store: &Path,
    ) -> Option<LightClient> {
        if checkpoint.is_some() {
            return None;
        }
        match LightClient::load(schedule.clone(), store) {
            Ok(client) => {
                info!(target: "l1", slot = client.finalized.beacon.slot, "restored L1 light client");
                Some(client)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                warn!(target: "l1", error = %error, "ignoring saved L1 light-client state");
                None
            }
        }
    }

    /// Bootstraps a light client from `checkpoint`, retrying every slot until a peer serves it.
    async fn bootstrap(
        peers: &BeaconPeers,
        schedule: ForkSchedule,
        checkpoint: B256,
    ) -> eyre::Result<LightClient> {
        let bootstrap = loop {
            match peers.bootstrap(checkpoint).await {
                Ok(bootstrap) => break bootstrap,
                Err(error) => {
                    warn!(target: "l1", error = %error, checkpoint = %checkpoint, "L1 light-client bootstrap failed, retrying");
                    tokio::time::sleep(Duration::from_secs(schedule.seconds_per_slot)).await;
                }
            }
        };
        Ok(LightClient::bootstrap(schedule, checkpoint, &bootstrap)?)
    }

    /// Sepolia's Amsterdam activation, which the pinned reth chain spec predates.
    pub const SEPOLIA_AMSTERDAM_TIMESTAMP: u64 = 1_791_294_816;

    /// Returns the built-in chain spec for `l1_chain_id`.
    ///
    /// ponytail: public L1s only; devnets need their chain spec and bootnodes plumbed in. Drop
    /// the Sepolia Amsterdam patch once reth ships it, since a fork missing here gives peers a
    /// fork ID they reject.
    pub fn chain_spec(l1_chain_id: u64) -> eyre::Result<Arc<ChainSpec>> {
        match l1_chain_id {
            1 => Ok(MAINNET.clone()),
            11_155_111 => {
                let mut spec = (**SEPOLIA).clone();
                spec.hardforks.insert(
                    EthereumHardfork::Amsterdam,
                    ForkCondition::Timestamp(Self::SEPOLIA_AMSTERDAM_TIMESTAMP),
                );
                Ok(Arc::new(spec))
            }
            id => eyre::bail!("embedded L1 does not support L1 chain {id}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::fixed_bytes;
    use reth_ethereum_forks::ForkHash;

    use super::*;

    #[test]
    fn sepolia_fork_id_matches_live_peers() {
        let spec = L1Node::chain_spec(11_155_111).unwrap();
        assert_eq!(spec.latest_fork_id().hash, ForkHash(fixed_bytes!("0x6c1d9423").0));
    }
}
