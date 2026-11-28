//! Execution-layer sync: when the consensus node names a block tiger does not have (a
//! forkchoice head or a payload's parent), tiger downloads the missing blocks from peers and
//! imports them, as base-reth's engine tree does. Gaps above [`BACKFILL_THRESHOLD`] are fetched
//! forward by number in batches (base-reth's backfill); the last stretch, and any reorg below the
//! local head, is fetched backward by hash from the target. Payloads the consensus node sent that
//! could not be imported yet (`EngineState::pending`) are imported from there, without a
//! download.

use std::{
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

use alloy_primitives::{B256, Bytes};
use base_tiger_net::{
    BlockBody, BlockId, BlockSource, ChainHead, Downloader, HeadersRequest, Network, SealedHeader,
};

#[cfg(feature = "tp2p")]
use crate::Fetcher;
use crate::{Node, engine::Imported};

/// Where sync downloads blocks from: devp2p, or with feature `tp2p` devp2p or tiger peers.
#[cfg(not(feature = "tp2p"))]
type Fetcher = Downloader;

/// Gaps longer than this are backfilled forward by number (base-reth's
/// `MIN_BLOCKS_FOR_PIPELINE_RUN`).
pub const BACKFILL_THRESHOLD: u64 = 32;
/// Headers per forward batch, each imported before the next is fetched.
const BATCH: u64 = 1024;
/// Most headers fetched backward from one target before giving up on linking it.
const MAX_BACKWARD: u64 = 4 * BATCH;
/// Longest wait for one block's execution witness from the upstream.
const WITNESS_TIMEOUT: Duration = Duration::from_secs(120);

/// The node's network and sync requests.
#[derive(Debug)]
pub struct Sync {
    /// The running devp2p network, unless the node syncs over tp2p only.
    pub network: Option<Network>,
    targets: Mutex<mpsc::Sender<B256>>,
}

impl Node {
    /// Starts the network and the sync thread.
    pub fn start_sync(
        self: &Arc<Self>,
        config: base_tiger_net::NetworkConfig,
    ) -> Result<(), String> {
        let source: Arc<dyn BlockSource> = Arc::clone(self) as Arc<dyn BlockSource>;
        let network = Network::start(config, source).map_err(|error| format!("starting the network: {error}"))?;
        eprintln!("tiger: node id {}", alloy_primitives::hex::encode(network.local_id()));
        let downloader = Downloader::new(network.clone(), base_tiger_net::DownloadConfig::default());
        #[cfg(feature = "tp2p")]
        let downloader = Fetcher::Devp2p(downloader);
        self.start_sync_with(Some(network), downloader)
    }

    /// Starts the sync thread downloading through `downloader`.
    pub fn start_sync_with(self: &Arc<Self>, network: Option<Network>, downloader: Fetcher) -> Result<(), String> {
        let (sender, receiver) = mpsc::channel();
        let sync = Sync { network, targets: Mutex::new(sender) };
        assert!(self.sync.set(sync).is_ok(), "sync starts once");
        let node = Arc::clone(self);
        thread::Builder::new()
            .name("sync".into())
            .spawn(move || node.sync_loop(&downloader, &receiver))
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Asks the sync thread to fetch `target` and its missing ancestors (no-op without a network).
    pub fn request_sync(&self, target: B256) {
        if let Some(sync) = self.sync.get() {
            let _ = sync.targets.lock().expect("sync lock").send(target);
        }
    }

    fn sync_loop(&self, downloader: &Fetcher, targets: &mpsc::Receiver<B256>) {
        while let Ok(mut target) = targets.recv() {
            // Only the newest request matters.
            while let Ok(newer) = targets.try_recv() {
                target = newer;
            }
            if self.chain.read().expect("chain lock").block(&target).is_some() {
                continue;
            }
            match self.sync_to(downloader, target) {
                Ok(imported) => eprintln!("tiger: synced {imported} blocks to {target}"),
                Err(error) => {
                    eprintln!("tiger: sync to {target}: {error}");
                    thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }

    /// Downloads and imports the blocks up to `target`. Returns how many were imported.
    fn sync_to(&self, downloader: &Fetcher, target: B256) -> Result<u64, String> {
        let error = |error: base_tiger_net::DownloadError| error.to_string();
        if let Some(imported) = self.import_pending(target)? {
            return Ok(imported);
        }
        eprintln!("tiger: syncing to {target}");
        let top = downloader.headers_back(target, u64::MAX, 1).map_err(error)?.remove(0);
        eprintln!("tiger: sync target is block {}", top.number());
        let mut imported = 0;
        // Forward backfill while the gap is long: by number, linked to the last block imported
        // (imported blocks become canonical only with the next forkchoice update).
        let head = self.chain.read().expect("chain lock").head();
        let (mut number, mut hash) = (head.number(), head.hash);
        while top.number() > number + BACKFILL_THRESHOLD {
            let count = BATCH.min(top.number() - number - 1);
            let headers = downloader
                .headers(HeadersRequest {
                    start: BlockId::Number(number + 1),
                    limit: count,
                    skip: 0,
                    reverse: false,
                })
                .map_err(error)?;
            if headers.first().is_none_or(|first| first.header.parent_hash != hash) {
                // Peers are on another branch below our head: fetch backward instead.
                break;
            }
            let linked = headers.windows(2).all(|pair| pair[1].header.parent_hash == pair[0].hash);
            if !linked {
                return Err("peer headers do not link".into());
            }
            eprintln!("tiger: backfilling blocks {}..={}", number + 1, number + headers.len() as u64);
            imported += self.import_downloaded(downloader, &headers)?;
            let last = headers.last().expect("checked non-empty");
            (number, hash) = (last.number(), last.hash);
        }
        // The last stretch (and reorgs): backward by hash from the target to a stored block.
        let mut headers: Vec<SealedHeader> = Vec::new();
        let mut from = target;
        let floor = self.chain.read().expect("chain lock").first();
        loop {
            let known = |hash: &B256| self.chain.read().expect("chain lock").block(hash).is_some();
            let top_number = headers.last().map_or(top.number(), |lowest| lowest.number() - 1);
            if top_number < floor {
                return Err(format!("{target} does not descend from the first stored block {floor}"));
            }
            let stop_at = top_number.saturating_sub(BATCH - 1).max(floor);
            let batch = downloader.headers_back(from, stop_at, BATCH).map_err(error)?;
            let mut done = false;
            for header in batch {
                if known(&header.hash) {
                    done = true;
                    break;
                }
                let parent = header.header.parent_hash;
                headers.push(header);
                if known(&parent) {
                    done = true;
                    break;
                }
            }
            if done {
                break;
            }
            if headers.len() as u64 >= MAX_BACKWARD {
                return Err(format!("no stored ancestor within {MAX_BACKWARD} blocks of {target}"));
            }
            from = headers.last().ok_or("peers returned no headers")?.header.parent_hash;
        }
        headers.reverse();
        imported += self.import_downloaded(downloader, &headers)?;
        Ok(imported)
    }

    /// Imports the buffered payloads from `target` down to a stored block, when they reach one.
    /// Returns how many were imported, or `None` when they do not link to a stored block.
    fn import_pending(&self, target: B256) -> Result<Option<u64>, String> {
        let mut blocks = Vec::new();
        {
            let engine = self.engine.lock().expect("engine lock");
            let chain = self.chain.read().expect("chain lock");
            let mut at = target;
            while chain.block(&at).is_none() {
                let Some((header, transactions)) = engine.pending.get(&at) else { return Ok(None) };
                blocks.push((header.clone(), at, transactions.clone()));
                at = header.parent_hash;
            }
        }
        blocks.reverse();
        for (header, hash, _) in &blocks {
            self.prewarm(header, *hash);
        }
        let mut imported = 0;
        for (header, hash, transactions) in blocks {
            imported += self.import_one(header, hash, &transactions)?;
            self.advance_canonical(hash)?;
            self.persistence_backpressure();
        }
        Ok(Some(imported))
    }

    /// Makes imported block `hash` canonical when it is an ancestor of the latest forkchoice
    /// head, so a long catch-up commits as it goes, as base-reth's backfill does, instead of
    /// holding every block in memory until the head is reached. On a stateless node this also
    /// keeps the stored tip at the next block's parent, where its proofs are fetched.
    fn advance_canonical(&self, hash: B256) -> Result<(), String> {
        let engine = self.engine.lock().expect("engine lock");
        let Some(head) = engine.forkchoice_head else { return Ok(()) };
        let mut chain = self.chain.write().expect("chain lock");
        let Some(block) = chain.block(&hash) else { return Ok(()) };
        // Walk back from the forkchoice head, through buffered payloads and stored blocks.
        let mut at = head;
        loop {
            let (number, parent) = match engine.pending.get(&at) {
                Some((header, _)) => (header.number, header.parent_hash),
                None => match chain.block(&at) {
                    Some(stored) => (stored.number(), stored.header.parent_hash),
                    // A gap: the head is a gossiped block far ahead of the blocks being
                    // backfilled. A block extending the canonical tip is committed (as base-reth's
                    // backfill does); a forkchoice to another branch later unwinds it.
                    None => {
                        if block.header.parent_hash == chain.head().hash {
                            chain.set_canonical(hash)?;
                        }
                        return Ok(());
                    }
                },
            };
            if number <= block.number() {
                if at == hash {
                    chain.set_canonical(hash)?;
                }
                return Ok(());
            }
            at = parent;
        }
    }

    /// Imports one block whose parent is stored, waiting (without holding the Engine API lock)
    /// for its execution witness when it needs one. Returns 1 when it was imported now.
    fn import_one(
        &self,
        header: alloy_consensus::Header,
        hash: B256,
        transactions: &[Bytes],
    ) -> Result<u64, String> {
        let number = header.number;
        loop {
            // Validation runs without the Engine API lock, so newPayload and forkchoiceUpdated
            // calls are answered (and buffered) while a long stretch imports.
            let checked = Self::check_known_invalid(
                &mut self.engine.lock().expect("engine lock"),
                &header,
                hash,
            );
            let outcome = match checked {
                Some(outcome) => outcome,
                None => match self
                    .validate_block(&header, hash, transactions)
                    .map_err(|error| error.message)?
                {
                    Ok(validated) => {
                        let mut engine = self.engine.lock().expect("engine lock");
                        self.store_validated(&mut engine, header.clone(), hash, validated)
                            .map_err(|error| error.message)?
                    }
                    Err(outcome) => outcome,
                },
            };
            if !matches!(outcome, Imported::AwaitingWitness) {
                self.engine.lock().expect("engine lock").pending.remove(&hash);
            }
            match outcome {
                Imported::Valid => return Ok(1),
                Imported::Known => return Ok(0),
                Imported::AwaitingWitness => {
                    let witnesses = self.witnesses.as_ref().expect("only a stateless node awaits witnesses");
                    if !witnesses.wait(hash, number, WITNESS_TIMEOUT) {
                        return Err(format!("no execution witness of block {number} {hash} from the upstream"));
                    }
                }
                Imported::MissingParent => return Err(format!("block {hash} has no parent")),
                Imported::Invalid { error, .. } => {
                    return Err(format!("block {number} {hash} is invalid: {error}"));
                }
            }
        }
    }

    /// Downloads the bodies of `headers` (ascending, linked) and imports the blocks in order.
    fn import_downloaded(
        &self,
        downloader: &Fetcher,
        headers: &[SealedHeader],
    ) -> Result<u64, String> {
        let bodies = downloader.bodies(headers, &mut |_| {}).map_err(|error| error.to_string())?;
        for header in headers {
            self.prewarm(&header.header, header.hash);
        }
        let mut imported = 0;
        for (header, body) in headers.iter().zip(bodies) {
            imported += self.import_one(header.header.clone(), header.hash, &body.transactions)?;
            self.advance_canonical(header.hash)?;
            self.persistence_backpressure();
        }
        Ok(imported)
    }
}

impl BlockSource for Node {
    fn chain_head(&self) -> ChainHead {
        let chain = self.chain.read().expect("chain lock");
        let head = chain.head();
        ChainHead {
            number: head.number(),
            hash: head.hash,
            timestamp: head.header.timestamp,
            earliest: chain.earliest(),
        }
    }

    fn header_by_hash(&self, hash: B256) -> Option<alloy_consensus::Header> {
        self.chain.read().expect("chain lock").header(&hash)
    }

    fn header_by_number(&self, number: u64) -> Option<alloy_consensus::Header> {
        self.chain.read().expect("chain lock").canonical_header(number)
    }

    fn body_by_hash(&self, hash: B256) -> Option<BlockBody> {
        use alloy_eips::eip2718::Encodable2718;
        let block = self.chain.read().expect("chain lock").block(&hash)?;
        Some(BlockBody {
            transactions: block.transactions.iter().map(|tx| Bytes::from(tx.encoded_2718())).collect(),
            ommers: Vec::new(),
            withdrawals: block.header.withdrawals_root.map(|_| Vec::new()),
        })
    }

    fn receipts_by_hash(&self, hash: B256) -> Option<Vec<Bytes>> {
        use alloy_eips::eip2718::Encodable2718;
        let block = self.chain.read().expect("chain lock").block(&hash)?;
        Some(block.receipts.iter().map(|receipt| Bytes::from(receipt.encoded_2718())).collect())
    }
}
