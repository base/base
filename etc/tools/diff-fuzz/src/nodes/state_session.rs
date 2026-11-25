//! The chain of blocks the state campaign drives into real nodes: a tree of branches built by
//! the reference adapter, the operations that grow, reorganize, and restart it, and the checks
//! that read state back from every node against the generator's expected state.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use alloy_consensus::Header;
use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, U256, keccak256};
use alloy_trie::{
    EMPTY_ROOT_HASH, Nibbles, TrieAccount, proof::verify_proof, root::storage_root_unhashed,
};
use eyre::{WrapErr, bail, ensure, eyre};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    Adapter, Blocks, ChainGenerator, GeneratorOptions, PostAccount, Rng, STRESSOR, Stress,
    StressPlan, WorldState,
    nodes::{ChainFollower, Lag, NodeClient, NodeProcess, Reply, StateOp},
};

/// One block the nodes were sent.
#[derive(Debug, Clone)]
pub struct TreeBlock {
    /// Block number.
    pub number: u64,
    /// Block hash.
    pub hash: B256,
    /// Parent hash.
    pub parent: B256,
    /// State root.
    pub state_root: B256,
    /// Engine API method and parameters that submit the block (`None` for genesis).
    pub payload: Option<(String, Value)>,
    /// The generator after the block, parked; `None` once pruned.
    pub generator: Option<Box<ChainGenerator>>,
    /// Accounts and slots the block changed, with the parent slots of accounts it removed or
    /// wiped.
    pub touched: BTreeMap<Address, BTreeSet<B256>>,
}

/// Every block the nodes hold, as a tree rooted at genesis.
#[derive(Debug, Default)]
pub struct ChainTree {
    /// Blocks by hash.
    pub blocks: HashMap<B256, TreeBlock>,
    /// Hashes in the order they were added.
    pub order: Vec<B256>,
    /// Genesis hash.
    pub genesis: B256,
    /// The head every node was last moved to.
    pub head: B256,
    /// The highest finalized block sent.
    pub finalized: B256,
}

impl ChainTree {
    /// Blocks after the head that keep their generator snapshot.
    pub const KEEP_RECENT: u64 = 96;
    /// Older canonical blocks at multiples of this keep theirs.
    pub const CHECKPOINT: u64 = 32;

    /// The block `hash`, which must be in the tree.
    pub fn block(&self, hash: &B256) -> &TreeBlock {
        &self.blocks[hash]
    }

    /// The head block.
    pub fn head(&self) -> &TreeBlock {
        self.block(&self.head)
    }

    /// The ancestor `depth` blocks above `hash` (itself for zero), stopping at genesis.
    pub fn ancestor(&self, hash: B256, depth: u64) -> B256 {
        let mut at = hash;
        for _ in 0..depth {
            if at == self.genesis {
                break;
            }
            at = self.block(&at).parent;
        }
        at
    }

    /// Whether `ancestor` is `hash` or one of its ancestors.
    pub fn is_ancestor(&self, ancestor: B256, hash: B256) -> bool {
        let target = self.block(&ancestor).number;
        let mut at = hash;
        loop {
            let block = self.block(&at);
            if block.number <= target || at == self.genesis {
                return at == ancestor;
            }
            at = block.parent;
        }
    }

    /// The canonical block at `number` (on the head's chain).
    pub fn canonical(&self, number: u64) -> Option<B256> {
        let head = self.head();
        if number > head.number {
            return None;
        }
        Some(self.ancestor(self.head, head.number - number))
    }

    /// Blocks no other block builds on, most recent first.
    pub fn leaves(&self) -> Vec<B256> {
        let parents: BTreeSet<B256> = self.blocks.values().map(|b| b.parent).collect();
        self.order.iter().rev().filter(|h| !parents.contains(*h)).copied().collect()
    }

    /// Adds a block.
    pub fn insert(&mut self, block: TreeBlock) {
        self.order.push(block.hash);
        self.blocks.insert(block.hash, block);
    }

    /// Drops generator snapshots of old blocks (keeping canonical checkpoints) and every block
    /// on a branch that left the canonical chain at or below the finalized block.
    pub fn prune(&mut self) {
        let head = self.head().number;
        let finalized = self.block(&self.finalized).number;
        let canonical: BTreeSet<B256> = {
            let mut set = BTreeSet::new();
            let mut at = self.head;
            loop {
                set.insert(at);
                if at == self.genesis {
                    break;
                }
                at = self.block(&at).parent;
            }
            set
        };
        let dead: Vec<B256> = self
            .blocks
            .values()
            .filter(|b| !canonical.contains(&b.hash) && b.number <= finalized)
            .map(|b| b.hash)
            .collect();
        for hash in &dead {
            self.blocks.remove(hash);
        }
        // Descendants of dropped blocks are dropped too.
        loop {
            let orphans: Vec<B256> = self
                .blocks
                .values()
                .filter(|b| b.hash != self.genesis && !self.blocks.contains_key(&b.parent))
                .map(|b| b.hash)
                .collect();
            if orphans.is_empty() {
                break;
            }
            for hash in orphans {
                self.blocks.remove(&hash);
            }
        }
        self.order.retain(|h| self.blocks.contains_key(h));
        for block in self.blocks.values_mut() {
            let recent = block.number + Self::KEEP_RECENT > head;
            let checkpoint = canonical.contains(&block.hash)
                && block.number.is_multiple_of(Self::CHECKPOINT)
                && block.number + 16 * Self::CHECKPOINT > head;
            if !recent && !checkpoint && block.hash != self.genesis {
                block.generator = None;
            }
        }
    }
}

/// A difference between a node and the expected chain or state.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Divergence {
    /// What diverged: `payload`, `allRejected`, `badRoot`, `forkchoice`, `head`, `header`,
    /// `account`, `storage`, `proof`, `rpc`, `restart`, or `adapter`.
    pub kind: String,
    /// The node, or `all`.
    pub node: String,
    /// The field or method.
    pub detail: String,
    /// Block number, when one is involved.
    pub block: Option<u64>,
    /// Expected value.
    pub expected: Value,
    /// What the node returned.
    pub got: Value,
}

impl Divergence {
    /// A signature that groups divergences of one kind on one node and field.
    pub fn signature(&self) -> String {
        format!("{}:{}:{}", self.kind, self.node, self.detail)
    }

    /// Whether a second read showed the node agreeing again (see [`StateSession::check`]).
    pub fn transient(&self) -> bool {
        self.detail.ends_with(" (transient)")
    }

    fn new(
        kind: &str,
        node: &str,
        detail: &str,
        block: Option<u64>,
        expected: Value,
        got: Value,
    ) -> Self {
        Self {
            kind: kind.to_owned(),
            node: node.to_owned(),
            detail: detail.to_owned(),
            block,
            expected,
            got,
        }
    }
}

/// A block built on a parent but not yet in the tree.
#[derive(Debug)]
struct Built {
    block: TreeBlock,
    header: Header,
    txs: Vec<Bytes>,
}

/// Real nodes following one generated chain, with the processes that run them.
#[derive(Debug)]
pub struct StateSession {
    /// Chain seed.
    pub seed: u64,
    /// Generator options (with `state_stress`).
    pub options: GeneratorOptions,
    /// Reference adapter that builds every block.
    pub adapter: Adapter,
    /// Nodes under test.
    pub nodes: Vec<NodeClient>,
    /// Their processes, in the same order.
    pub procs: Vec<NodeProcess>,
    /// Blocks sent so far.
    pub tree: ChainTree,
    /// Safe and finalized distance below the head.
    pub lag: Lag,
    /// Deepest fork.
    pub max_depth: u64,
    /// Operations run since the nodes were last wiped.
    pub history: Vec<StateOp>,
    /// Engine API calls and restarts since the nodes were last wiped, for reproducers.
    pub trace: Vec<Value>,
    /// Counted events that are not divergences, such as blocks a node lost in a restart.
    pub observations: BTreeMap<String, u64>,
    /// Blocks built so far, over every wipe.
    pub blocks_built: u64,
    /// Every node's name in command-line order (restart masks index it).
    pub names: Vec<String>,
    /// Nodes and processes set aside by [`Self::focus`], with their positions.
    set_aside: Vec<(usize, NodeClient, NodeProcess)>,
    /// The fork point of the last reorg, a height worth reading.
    last_fork: Option<B256>,
    /// Head of the forkchoice update being sent, until every node has answered. A node that an
    /// injected fault crashed during it never answered, so recovery retries this head, as a
    /// consensus node retries an unanswered forkchoice.
    inflight_head: Option<B256>,
    rng: Rng,
}

impl StateSession {
    /// Starts every node on a wiped datadir from chain `seed`'s genesis file (written to
    /// `<root>/genesis.json`) and checks they agree on the genesis block.
    pub fn start(
        seed: u64,
        options: GeneratorOptions,
        adapter: Adapter,
        nodes: Vec<NodeClient>,
        procs: Vec<NodeProcess>,
        lag: Lag,
        max_depth: u64,
    ) -> eyre::Result<Self> {
        ensure!(nodes.len() == procs.len(), "one process per node");
        let root = procs.first().map(|p| p.root.clone()).ok_or_else(|| eyre!("no nodes"))?;
        // At block zero: base/base cannot reopen a datadir whose genesis is above it, and the
        // campaign restarts nodes.
        ChainFollower::write_genesis(seed, options.clone(), &root.join("genesis.json"), true)?;
        let mut session = Self {
            seed,
            options,
            adapter,
            nodes,
            procs,
            tree: ChainTree::default(),
            lag,
            max_depth,
            history: Vec::new(),
            trace: Vec::new(),
            observations: BTreeMap::new(),
            blocks_built: 0,
            names: Vec::new(),
            set_aside: Vec::new(),
            last_fork: None,
            inflight_head: None,
            rng: Rng::new(&[seed, 0x57a7_e5e5]),
        };
        session.names = session.nodes.iter().map(|n| n.name.clone()).collect();
        session.reset()?;
        Ok(session)
    }

    /// Stops every node but those named in `keep` and leaves them out until [`Self::unfocus`],
    /// so replays (minimization) skip slow nodes a divergence does not involve.
    pub fn focus(&mut self, keep: &[String]) {
        let mut index = self.nodes.len();
        while index > 0 {
            index -= 1;
            if !keep.contains(&self.nodes[index].name) {
                let node = self.nodes.remove(index);
                let mut proc = self.procs.remove(index);
                proc.stop(true);
                self.set_aside.push((index, node, proc));
            }
        }
    }

    /// Puts back the nodes [`Self::focus`] left out; they start with the next [`Self::reset`].
    pub fn unfocus(&mut self) {
        while let Some((index, node, proc)) = self.set_aside.pop() {
            self.nodes.insert(index, node);
            self.procs.insert(index, proc);
        }
    }

    /// Kills every node, wipes their datadirs, starts them again, and restarts the chain from
    /// genesis.
    pub fn reset(&mut self) -> eyre::Result<()> {
        for proc in &mut self.procs {
            proc.stop(true);
            proc.spawn(true)?;
        }
        let started: Vec<eyre::Result<Duration>> = thread::scope(|scope| {
            let handles: Vec<_> = self
                .procs
                .iter_mut()
                .zip(&self.nodes)
                .map(|(proc, node)| scope.spawn(move || proc.wait_ready(node)))
                .collect();
            handles.into_iter().map(|h| h.join().expect("start thread")).collect()
        });
        for result in started {
            result?;
        }
        let mut generator = ChainGenerator::new(self.seed, self.options.clone());
        // The genesis file puts the genesis block at zero (see `start`).
        let number = 0u64;
        let mut genesis: Option<(String, Value)> = None;
        for node in &self.nodes {
            let block =
                node.result("eth_getBlockByNumber", &json!([format!("{number:#x}"), false]))?;
            match &genesis {
                None => genesis = Some((node.name.clone(), block)),
                Some((name, first)) => ensure!(
                    first.get("hash") == block.get("hash"),
                    "genesis block differs: {name} {} vs {} {}",
                    first["hash"],
                    node.name,
                    block["hash"]
                ),
            }
        }
        let (_, block) = genesis.ok_or_else(|| eyre!("no nodes"))?;
        let rpc: alloy_rpc_types_eth::Header<Header> =
            serde_json::from_value(block).wrap_err("decoding the genesis header")?;
        generator.rebase(rpc.inner.clone())?;
        generator.park();
        let hash = rpc.hash;
        self.tree = ChainTree { genesis: hash, head: hash, finalized: hash, ..Default::default() };
        self.tree.insert(TreeBlock {
            number,
            hash,
            parent: B256::ZERO,
            state_root: rpc.inner.state_root,
            payload: None,
            generator: Some(Box::new(generator)),
            touched: BTreeMap::new(),
        });
        self.history.clear();
        self.inflight_head = None;
        self.trace.clear();
        self.last_fork = None;
        for node in &self.nodes {
            node.reset_coverage();
        }
        Ok(())
    }

    /// Blocks on the canonical chain above genesis.
    pub fn height(&self) -> u64 {
        self.tree.head().number - self.tree.block(&self.tree.genesis).number
    }

    /// Runs one operation. Divergences are returned; `Err` is a harness failure (the adapter
    /// could not build a block, a node could not be reached for a reason that is not the
    /// node's).
    ///
    /// When an injected fault crashed a node during the operation, what the operation saw on
    /// that node is dropped: the result is the node's recovery (see [`Self::recover_faults`]).
    pub fn run(&mut self, op: &StateOp) -> eyre::Result<Vec<Divergence>> {
        self.history.push(op.clone());
        let result = self.run_op(op);
        let result = self.recover_faults().map_or(result, Ok);
        result.map(|divergences| self.recover_disk_full(divergences))
    }

    /// Error text of a write that failed because the disk is full (ENOSPC).
    pub const DISK_FULL_ERROR: &str = "os error 28";
    /// How long a node may take to accept the head again after an injected full disk.
    pub const DISK_FULL_RECOVERY: Duration = Duration::from_secs(60);

    /// Handles the Engine API errors an injected full disk explains. A node whose current run
    /// printed tiger's full-disk line and that answered with the disk error ("os error 28")
    /// had its divergences in this operation dropped, as the disk error explains them; then the
    /// head's forkchoice is resent until every node takes it or [`Self::DISK_FULL_RECOVERY`]
    /// passes, and what still diverges after that is returned.
    pub fn recover_disk_full(&mut self, mut divergences: Vec<Divergence>) -> Vec<Divergence> {
        let explained: Vec<String> = self
            .procs
            .iter()
            .zip(&self.nodes)
            .filter(|(proc, node)| {
                proc.injected_disk_full().is_some()
                    && divergences.iter().any(|d| {
                        d.node == node.name && d.got.to_string().contains(Self::DISK_FULL_ERROR)
                    })
            })
            .map(|(_, node)| node.name.clone())
            .collect();
        if explained.is_empty() {
            return divergences;
        }
        for name in &explained {
            *self.observations.entry(format!("{name}: disk-full engine error")).or_default() += 1;
        }
        divergences.retain(|d| !explained.contains(&d.node));
        let started = Instant::now();
        loop {
            let resync = self.forkchoice_resync(self.tree.head, "disk full");
            if resync.is_empty() || started.elapsed() > Self::DISK_FULL_RECOVERY {
                divergences.extend(resync);
                return divergences;
            }
            thread::sleep(Duration::from_secs(1));
        }
    }

    fn run_op(&mut self, op: &StateOp) -> eyre::Result<Vec<Divergence>> {
        match op {
            StateOp::Extend { blocks, stress, salt, fcu_each } => {
                self.extend(*blocks, stress, *salt, *fcu_each)
            }
            StateOp::Fork { depth, blocks, stress, salt, switch } => {
                self.fork(u64::from(*depth), *blocks, stress, *salt, *switch)
            }
            StateOp::Switch { tip } => self.switch(*tip),
            StateOp::Restart { kill, nodes } => Ok(self.restart(*kill, *nodes)),
            StateOp::Fault { writes, lose_unsynced, seed } => {
                let mode = if *lose_unsynced { "lose_unsynced" } else { "torn" };
                let fault = format!("seed={seed},crash_after_writes={writes},mode={mode}");
                for proc in &mut self.procs {
                    proc.fault = Some(fault.clone());
                }
                let divergences = self.restart(false, 0);
                for proc in &mut self.procs {
                    proc.fault = None;
                }
                Ok(divergences)
            }
            StateOp::DiskFull { writes, count, seed } => {
                let fault = format!(
                    "seed={seed},crash_after_writes={writes},mode=enospc,fail_writes={count}"
                );
                for proc in &mut self.procs {
                    proc.fault = Some(fault.clone());
                }
                let divergences = self.restart(false, 0);
                for proc in &mut self.procs {
                    proc.fault = None;
                }
                Ok(divergences)
            }
            StateOp::BadRoot { kind, salt } => self.bad_root(*kind, *salt),
            StateOp::Ancestor { depth } => Ok(self.ancestor(*depth)),
            StateOp::Check { heights, seed } => Ok(self.check(*heights, *seed)),
        }
    }

    fn extend(
        &mut self,
        blocks: u8,
        stress: &StressPlan,
        salt: u64,
        fcu_each: bool,
    ) -> eyre::Result<Vec<Divergence>> {
        let blocks = blocks.clamp(1, StateOp::MAX_BLOCKS);
        let mut parent = self.tree.head;
        for index in 0..blocks {
            let built = self.build(parent, stress, Self::salt(salt, index))?;
            let hash = built.block.hash;
            let divergences = self.submit(&built, true);
            self.tree.insert(built.block);
            if !divergences.is_empty() {
                return Ok(divergences);
            }
            parent = hash;
            if fcu_each || index + 1 == blocks {
                let divergences = self.forkchoice(hash);
                if !divergences.is_empty() {
                    return Ok(divergences);
                }
            }
        }
        Ok(Vec::new())
    }

    fn fork(
        &mut self,
        depth: u64,
        blocks: u8,
        stress: &StressPlan,
        salt: u64,
        switch: bool,
    ) -> eyre::Result<Vec<Divergence>> {
        let head = self.tree.head().number;
        let floor = self.tree.block(&self.tree.finalized).number;
        let mut depth = depth.clamp(1, self.max_depth).min(head.saturating_sub(floor));
        if depth == 0 {
            return Ok(Vec::new());
        }
        let mut point = self.tree.ancestor(self.tree.head, depth);
        while self.tree.block(&point).generator.is_none() && depth > 1 {
            depth -= 1;
            point = self.tree.ancestor(self.tree.head, depth);
        }
        self.last_fork = Some(point);
        let blocks = blocks.clamp(1, StateOp::MAX_BLOCKS);
        let mut parent = point;
        for index in 0..blocks {
            let mut salt = Self::salt(salt ^ 0xf02c_0000_0000_0000, index);
            let mut built = self.build(parent, stress, salt)?;
            // A block that may not depend on its salt (light stress) can come out equal to the
            // existing child; a forkchoice to that canonical ancestor is no reorg, and clients
            // may ignore it, so build another, or give up on the fork.
            let mut tries = 0;
            while index == 0
                && self.tree.blocks.contains_key(&built.block.hash)
                && self.tree.is_ancestor(built.block.hash, self.tree.head)
            {
                tries += 1;
                if tries > 8 {
                    return Ok(Vec::new());
                }
                salt = salt.wrapping_add(1);
                built = self.build(parent, stress, salt)?;
            }
            let hash = built.block.hash;
            let divergences = self.submit(&built, false);
            self.tree.insert(built.block);
            if !divergences.is_empty() {
                return Ok(divergences);
            }
            parent = hash;
        }
        if switch {
            return Ok(self.forkchoice(parent));
        }
        Ok(Vec::new())
    }

    fn switch(&mut self, tip: u8) -> eyre::Result<Vec<Divergence>> {
        let floor = self.tree.block(&self.tree.finalized).number;
        let candidates: Vec<B256> = self
            .tree
            .leaves()
            .into_iter()
            .filter(|leaf| *leaf != self.tree.head && self.tree.blocks[leaf].generator.is_some())
            .filter(|leaf| {
                // The fork point with the head must lie at or above the finalized block.
                let mut at = *leaf;
                while !self.tree.is_ancestor(at, self.tree.head) {
                    at = self.tree.block(&at).parent;
                }
                self.tree.block(&at).number >= floor
                    && self.tree.is_ancestor(self.tree.finalized, *leaf)
            })
            .collect();
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let target = candidates[usize::from(tip) % candidates.len()];
        let mut at = target;
        while !self.tree.is_ancestor(at, self.tree.head) {
            at = self.tree.block(&at).parent;
        }
        self.last_fork = Some(at);
        Ok(self.forkchoice(target))
    }

    /// [`StateOp::Ancestor`]: a forkchoice update to a canonical ancestor of the head.
    fn ancestor(&mut self, depth: u8) -> Vec<Divergence> {
        let head = self.tree.head;
        let floor = self.tree.block(&self.tree.finalized).number;
        let depth = u64::from(depth).min(self.tree.head().number.saturating_sub(floor));
        if depth == 0 {
            return Vec::new();
        }
        let target = self.tree.ancestor(head, depth);
        let state = self.forkchoice_state(target);
        let method = "engine_forkchoiceUpdatedV3";
        self.trace.push(json!({ "kind": "engine", "method": method, "params": state }));
        let number = self.tree.block(&target).number;
        let statuses = self.each_node(|node| Self::engine_status(node, method, &state));
        let mut divergences = Vec::new();
        let expected = json!({ "status": "VALID", "latestValidHash": target });
        for (node, status) in self.nodes.iter().zip(statuses) {
            let got = status.map_or_else(
                |e| json!(e),
                |s| {
                    json!({
                        "status": s["payloadStatus"]["status"],
                        "latestValidHash": s["payloadStatus"]["latestValidHash"],
                    })
                },
            );
            if got != expected {
                divergences.push(Divergence::new(
                    "forkchoice",
                    &node.name,
                    "ancestor",
                    Some(number),
                    expected.clone(),
                    got,
                ));
            }
        }
        let heads = self.each_node(|node| {
            node.result("eth_getBlockByNumber", &json!(["latest", false]))
                .map_or_else(|e| json!(format!("{e:#}")), |b| b["hash"].clone())
        });
        for (node, got) in self.nodes.iter().zip(heads) {
            if got != json!(head) {
                divergences.push(Divergence::new(
                    "head",
                    &node.name,
                    "latest after ancestor forkchoice",
                    Some(number),
                    json!(head),
                    got,
                ));
            }
        }
        divergences
    }

    fn restart(&mut self, kill: bool, mask: u8) -> Vec<Divergence> {
        // Mask bits name nodes by their position on the command line, also while minimization
        // runs on a subset.
        let selected: Vec<usize> = (0..self.nodes.len())
            .filter(|i| {
                let position = self.names.iter().position(|n| *n == self.nodes[*i].name);
                mask == 0 || position.is_some_and(|p| mask & (1 << p) != 0)
            })
            .collect();
        if selected.is_empty() {
            return Vec::new();
        }
        let names: Vec<String> = selected.iter().map(|i| self.nodes[*i].name.clone()).collect();
        self.trace.push(json!({ "kind": "restart", "nodes": names, "kill": kill }));
        let mut divergences = Vec::new();
        // Stop in parallel (a clean stop may take a while), launch from this thread (the nodes
        // die with the thread that launched them), and wait in parallel.
        let stopped: Vec<(usize, bool)> = thread::scope(|scope| {
            let handles: Vec<_> = self
                .procs
                .iter_mut()
                .enumerate()
                .filter(|(i, _)| selected.contains(i))
                .map(|(i, proc)| scope.spawn(move || (i, proc.stop(kill))))
                .collect();
            handles.into_iter().map(|h| h.join().expect("stop thread")).collect()
        });
        let launched: Vec<eyre::Result<()>> =
            selected.iter().map(|i| self.procs[*i].spawn(false)).collect();
        let outcomes: Vec<(usize, bool, eyre::Result<Duration>)> = thread::scope(|scope| {
            let handles: Vec<_> = self
                .procs
                .iter_mut()
                .zip(&self.nodes)
                .enumerate()
                .filter(|(i, _)| selected.contains(i))
                .zip(stopped)
                .zip(launched)
                .map(|(((i, (proc, node)), (_, clean)), launched)| {
                    scope.spawn(move || (i, clean, launched.and_then(|()| proc.wait_ready(node))))
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("restart thread")).collect()
        });
        for (index, clean, started) in outcomes {
            let name = self.nodes[index].name.clone();
            if !clean {
                *self.observations.entry(format!("{name}: clean stop timed out")).or_default() += 1;
            }
            if let Err(error) = started {
                let log = self.procs[index].root.join(format!("{name}.log"));
                let tail = fs::read_to_string(&log)
                    .map(|text| text.lines().rev().take(30).collect::<Vec<_>>().join("\n"))
                    .unwrap_or_default();
                divergences.push(Divergence::new(
                    "restart",
                    &name,
                    if kill { "start after kill" } else { "start after stop" },
                    Some(self.tree.head().number),
                    json!("answers JSON-RPC"),
                    json!({ "error": format!("{error:#}"), "logTail": tail }),
                ));
            }
        }
        if !divergences.is_empty() {
            return divergences;
        }
        let head = self.tree.head;
        self.forkchoice_resync(head, if kill { "kill" } else { "restart" })
    }

    fn bad_root(&mut self, kind: u8, salt: u64) -> eyre::Result<Vec<Divergence>> {
        let parent = self.tree.head;
        let stress = StressPlan::random(&mut Rng::new(&[salt]));
        let built = self.build(parent, &stress, salt)?;
        let right = built.header.state_root;
        let mut header = built.header.clone();
        header.state_root = match kind % 4 {
            0 => B256::from(U256::from(salt) | (U256::from(1) << 255)),
            1 => self.tree.block(&parent).state_root,
            2 => right ^ B256::from(U256::from(1) << (salt % 256)),
            _ => {
                let other = self.build(parent, &stress, salt.wrapping_add(1))?;
                let root = other.header.state_root;
                if root == right { right ^ B256::with_last_byte(1) } else { root }
            }
        };
        let hash = header.hash_slow();
        let (method, params) = ChainFollower::new_payload_params(&header, hash, &built.txs)?;
        self.trace.push(
            json!({ "kind": "engine", "method": method, "params": params, "expect": "INVALID" }),
        );
        let statuses = self.each_node(|node| Self::engine_status(node, method, &params));
        let mut divergences = Vec::new();
        for (node, status) in self.nodes.iter().zip(statuses) {
            match status {
                Ok(status) => match status["status"].as_str() {
                    Some("INVALID") => {}
                    Some("VALID") => divergences.push(Divergence::new(
                        "badRoot",
                        &node.name,
                        "wrong state root accepted",
                        Some(header.number),
                        json!({ "status": "INVALID", "stateRoot": right }),
                        json!({ "status": status, "stateRoot": header.state_root }),
                    )),
                    other => {
                        let key = format!(
                            "{}: wrong-root block answered {}",
                            node.name,
                            other.unwrap_or("?")
                        );
                        *self.observations.entry(key).or_default() += 1;
                    }
                },
                Err(error) => divergences.push(Divergence::new(
                    "badRoot",
                    &node.name,
                    "engine error",
                    Some(header.number),
                    json!({ "status": "INVALID" }),
                    json!(error),
                )),
            }
        }
        Ok(divergences)
    }

    /// The salt of block `index` of an operation.
    fn salt(salt: u64, index: u8) -> u64 {
        salt.wrapping_add(u64::from(index).wrapping_mul(0x9e37_79b9_7f4a_7c15))
    }

    /// Builds a block on `parent` with the reference adapter.
    fn build(&mut self, parent: B256, stress: &StressPlan, salt: u64) -> eyre::Result<Built> {
        let parent_block = self.tree.block(&parent);
        let mut generator = parent_block
            .generator
            .clone()
            .ok_or_else(|| eyre!("block {parent} has no generator snapshot"))?;
        generator.prepare(Some(*stress), salt);
        let mut response = self.adapter.call(generator.request()).map_err(|e| eyre!("{e}"))?;
        if let Some(base_fee) = response.computed_base_fee_per_gas
            && base_fee != U256::from(generator.request().header.base_fee_per_gas)
        {
            generator.set_base_fee(u64::try_from(base_fee).wrap_err("base fee above u64")?);
            response = self.adapter.call(generator.request()).map_err(|e| eyre!("{e}"))?;
        }
        ensure!(
            response.ok && response.valid,
            "reference adapter could not build: {:?} {:?}",
            response.error,
            response.invalid_reason
        );
        let request = generator.request().clone();
        let results = response.header.clone().ok_or_else(|| eyre!("no header in response"))?;
        let header = Blocks::assemble(parent, &request.header, &results)?;
        let rejected: BTreeSet<usize> = response.rejected.iter().map(|r| r.index).collect();
        let txs: Vec<Bytes> = request
            .transactions
            .iter()
            .enumerate()
            .filter(|(i, _)| !rejected.contains(i))
            .map(|(_, tx)| tx.clone())
            .collect();
        let (method, params) = ChainFollower::new_payload_params(&header, results.hash, &txs)?;
        let touched =
            Self::touched(&response.post, parent_block.generator.as_ref().map(|g| g.state()));
        generator.advance(&response).wrap_err("applying the reference post-state")?;
        generator.park();
        self.blocks_built += 1;
        Ok(Built {
            block: TreeBlock {
                number: header.number,
                hash: results.hash,
                parent,
                state_root: header.state_root,
                payload: Some((method.to_owned(), params)),
                generator: Some(generator),
                touched,
            },
            header,
            txs,
        })
    }

    fn touched(
        post: &BTreeMap<Address, Option<PostAccount>>,
        parent: Option<&WorldState>,
    ) -> BTreeMap<Address, BTreeSet<B256>> {
        let mut touched = BTreeMap::new();
        for (address, change) in post {
            let slots: &mut BTreeSet<B256> = touched.entry(*address).or_default();
            let gone = change.as_ref().is_none_or(|c| c.wiped);
            if let Some(change) = change {
                slots.extend(change.storage.keys().copied());
            }
            if gone && let Some(account) = parent.and_then(|p| p.account(address)) {
                slots.extend(account.storage.keys().take(16).copied());
            }
        }
        touched
    }

    /// Submits a built block to every node. Blocks on the head's chain must come back `VALID`;
    /// blocks of a side branch may also come back `ACCEPTED`.
    fn submit(&mut self, built: &Built, canonical: bool) -> Vec<Divergence> {
        let (method, params) = built.block.payload.clone().expect("built blocks carry a payload");
        self.trace.push(json!({ "kind": "engine", "method": method, "params": params }));
        let statuses = self.each_node(|node| Self::engine_status(node, &method, &params));
        let number = built.block.number;
        let mut divergences = Vec::new();
        let mut rejected = 0;
        for (node, status) in self.nodes.iter().zip(&statuses) {
            let answer = status.as_ref().ok().and_then(|s| s["status"].as_str());
            rejected += usize::from(answer == Some("INVALID"));
            let ok = answer == Some("VALID") || (answer == Some("ACCEPTED") && !canonical);
            if !ok {
                let got = status.as_ref().map_or_else(|e| json!(e), Clone::clone);
                divergences.push(Divergence::new(
                    "payload",
                    &node.name,
                    &format!("{method}: {}", Self::error_class(&got)),
                    Some(number),
                    json!({ "status": "VALID", "stateRoot": built.block.state_root }),
                    got,
                ));
            }
        }
        if rejected == self.nodes.len() {
            return vec![Divergence::new(
                "allRejected",
                "all",
                "reference block",
                Some(number),
                json!("VALID"),
                json!(
                    statuses
                        .into_iter()
                        .map(|s| s.unwrap_or_else(|e| json!(e)))
                        .collect::<Vec<_>>()
                ),
            )];
        }
        divergences
    }

    /// A payload status or error with hashes and numbers blanked, so one cause groups under one
    /// signature.
    fn error_class(status: &Value) -> String {
        let text = status
            .get("validationError")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| status.get("status").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_else(|| status.to_string());
        let mut out = String::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '0' && chars.peek() == Some(&'x') {
                chars.next();
                while chars.peek().is_some_and(char::is_ascii_hexdigit) {
                    chars.next();
                }
                out.push_str("0x…");
            } else if c.is_ascii_digit() {
                while chars.peek().is_some_and(char::is_ascii_digit) {
                    chars.next();
                }
                out.push('N');
            } else {
                out.push(c);
            }
        }
        out.chars().take(80).collect()
    }

    /// Sends one Engine API call and returns the payload status (`newPayload`) or the forkchoice
    /// response, retrying `SYNCING` for a while.
    fn engine_status(node: &NodeClient, method: &str, params: &Value) -> Result<Value, String> {
        let mut last = Value::Null;
        for _ in 0..20 {
            let result = node.engine(method, params).map_err(|e| format!("{e:#}"))?;
            let status = if method.starts_with("engine_forkchoice") {
                result["payloadStatus"]["status"].clone()
            } else {
                result["status"].clone()
            };
            if status != json!("SYNCING") {
                return Ok(result);
            }
            last = result;
            thread::sleep(Duration::from_millis(100));
        }
        Ok(last)
    }

    /// Runs `f` on every node at once.
    fn each_node<T: Send>(&self, f: impl Fn(&NodeClient) -> T + Sync) -> Vec<T> {
        thread::scope(|scope| {
            let f = &f;
            let handles: Vec<_> =
                self.nodes.iter().map(|node| scope.spawn(move || f(node))).collect();
            handles.into_iter().map(|h| h.join().expect("node thread")).collect()
        })
    }

    /// The forkchoice state for head `head`: safe and finalized trail it by the lag, and the
    /// finalized block never moves back.
    pub fn forkchoice_state(&mut self, head: B256) -> Value {
        let number = self.tree.block(&head).number;
        let genesis = self.tree.block(&self.tree.genesis).number;
        let at = |lag: u64| number.saturating_sub(lag).max(genesis);
        let mut finalized = self.tree.ancestor(head, number - at(self.lag.finalized));
        if self.tree.block(&finalized).number < self.tree.block(&self.tree.finalized).number {
            finalized = self.tree.finalized;
        }
        let mut safe = self.tree.ancestor(head, number - at(self.lag.safe));
        if self.tree.block(&safe).number < self.tree.block(&finalized).number {
            safe = finalized;
        }
        self.tree.finalized = finalized;
        json!([{ "headBlockHash": head, "safeBlockHash": safe, "finalizedBlockHash": finalized }, null])
    }

    /// Moves every node's head to `head`.
    fn forkchoice(&mut self, head: B256) -> Vec<Divergence> {
        self.forkchoice_resync(head, "")
    }

    /// Moves every node's head to `head`. A node that answers `SYNCING` is missing blocks (it
    /// lost them in a restart, or never kept a side branch); they are sent again and counted
    /// under `reason` when it is set.
    fn forkchoice_resync(&mut self, head: B256, reason: &str) -> Vec<Divergence> {
        let state = self.forkchoice_state(head);
        let method = "engine_forkchoiceUpdatedV3";
        self.trace.push(json!({ "kind": "engine", "method": method, "params": state }));
        let number = self.tree.block(&head).number;
        self.inflight_head = Some(head);
        let statuses = self.each_node(|node| Self::engine_status(node, method, &state));
        if self.procs.iter_mut().all(|proc| proc.injected_crash().is_none()) {
            self.inflight_head = None;
        }
        let mut divergences = Vec::new();
        for (index, mut status) in statuses.into_iter().enumerate() {
            let syncing =
                status.as_ref().is_ok_and(|s| s["payloadStatus"]["status"] == json!("SYNCING"));
            if syncing {
                match self.resync(index, head) {
                    Ok(resent) => {
                        if !reason.is_empty() && resent > 0 {
                            let key =
                                format!("{}: blocks resent after {reason}", self.nodes[index].name);
                            *self.observations.entry(key).or_default() += resent;
                        }
                        let node = &self.nodes[index];
                        self.trace.push(json!({ "kind": "engine", "node": node.name, "method": method, "params": state }));
                        status = Self::engine_status(node, method, &state);
                    }
                    Err(divergence) => {
                        divergences.push(*divergence);
                        continue;
                    }
                }
            }
            let valid =
                status.as_ref().is_ok_and(|s| s["payloadStatus"]["status"] == json!("VALID"));
            if !valid {
                let got = status.as_ref().map_or_else(|e| json!(e), Clone::clone);
                divergences.push(Divergence::new(
                    "forkchoice",
                    &self.nodes[index].name,
                    method,
                    Some(number),
                    json!({ "status": "VALID", "head": head }),
                    got,
                ));
            }
        }
        if !divergences.is_empty() {
            return divergences;
        }
        self.tree.head = head;
        let latest = |node: &NodeClient| {
            node.result("eth_getBlockByNumber", &json!(["latest", false]))
                .map_or_else(|e| json!(format!("{e:#}")), |b| b["hash"].clone())
        };
        let heads = self.each_node(latest);
        for (node, got) in self.nodes.iter().zip(heads) {
            if got != json!(head) {
                // As in `check`: a head that is right a while later was stale for a moment.
                thread::sleep(Self::RECHECK_DELAY);
                let detail =
                    if latest(node) == json!(head) { "latest (transient)" } else { "latest" };
                divergences.push(Divergence::new(
                    "head",
                    &node.name,
                    detail,
                    Some(number),
                    json!(head),
                    got,
                ));
            }
        }
        self.tree.prune();
        divergences
    }

    /// Sends node `index` the blocks of `target`'s chain it does not have, oldest first.
    /// Returns how many it was sent.
    fn resync(&mut self, index: usize, target: B256) -> Result<u64, Box<Divergence>> {
        let node = &self.nodes[index];
        let mut missing = Vec::new();
        let mut at = target;
        while at != self.tree.genesis {
            let known =
                node.result("eth_getBlockByHash", &json!([at, false])).is_ok_and(|b| !b.is_null());
            if known {
                break;
            }
            missing.push(at);
            at = self.tree.block(&at).parent;
        }
        let count = missing.len() as u64;
        for hash in missing.into_iter().rev() {
            let block = self.tree.block(&hash);
            let (method, params) = block.payload.clone().expect("only genesis has no payload");
            self.trace.push(
                json!({ "kind": "engine", "node": node.name, "method": method, "params": params }),
            );
            let status = Self::engine_status(node, &method, &params);
            let ok = status
                .as_ref()
                .is_ok_and(|s| matches!(s["status"].as_str(), Some("VALID" | "ACCEPTED")));
            if !ok {
                // Name the outcome in the signature so a rejection and a transport error after
                // a restart count as different findings.
                let outcome = status
                    .as_ref()
                    .map_or("error", |s| s["status"].as_str().unwrap_or("unknown"))
                    .to_owned();
                return Err(Box::new(Divergence::new(
                    "payload",
                    &node.name,
                    &format!("resend after restart: {outcome}"),
                    Some(block.number),
                    json!("VALID"),
                    status.unwrap_or_else(|e| json!(e)),
                )));
            }
        }
        Ok(count)
    }

    /// Reads state back from every node at `heights` canonical heights (the head first, then
    /// its parent, the last fork point, and older blocks) and compares it with the expected
    /// state: the header, and for the accounts and slots the block touched plus random present
    /// and absent ones, `eth_getProof` (values and proofs verified against the expected roots),
    /// `eth_getStorageAt`, `eth_getBalance`, `eth_getTransactionCount`, and `eth_getCode`.
    pub fn check(&mut self, heights: u8, seed: u64) -> Vec<Divergence> {
        let mut rng = Rng::new(&[seed, self.rng.next_u64()]);
        let head = self.tree.head;
        let mut targets = vec![head];
        let mut candidates = vec![self.tree.head().parent];
        if let Some(point) = self.last_fork.filter(|p| self.tree.blocks.contains_key(p)) {
            candidates.push(point);
        }
        let with_state: Vec<B256> = (0..=self.height())
            .filter_map(|back| {
                let hash = self.tree.ancestor(head, back);
                self.tree.block(&hash).generator.is_some().then_some(hash)
            })
            .collect();
        for _ in 0..4 {
            candidates.push(with_state[rng.index(with_state.len())]);
        }
        for hash in candidates {
            if targets.len() >= usize::from(heights.max(1)) {
                break;
            }
            let canonical =
                self.tree.blocks.contains_key(&hash) && self.tree.is_ancestor(hash, head);
            if canonical && self.tree.block(&hash).generator.is_some() && !targets.contains(&hash) {
                targets.push(hash);
            }
        }
        let mut divergences = Vec::new();
        for hash in targets {
            let keys = rng.clone();
            let (mut found, observed) = self.check_block(hash, &mut rng);
            if !found.is_empty() {
                // Read the same keys again a while later: a divergence that is gone was a node
                // serving stale state for a moment (such as an index trailing a reorg).
                thread::sleep(Self::RECHECK_DELAY);
                let (again, _) = self.check_block(hash, &mut keys.clone());
                let persistent: BTreeSet<String> =
                    again.iter().map(Divergence::signature).collect();
                for divergence in &mut found {
                    if !persistent.contains(&divergence.signature()) {
                        divergence.detail.push_str(" (transient)");
                    }
                }
            }
            divergences.extend(found);
            for key in observed {
                *self.observations.entry(key).or_default() += 1;
            }
        }
        divergences
    }

    /// Reads state at block `hash`; returns the divergences and the observations to count.
    fn check_block(&self, hash: B256, rng: &mut Rng) -> (Vec<Divergence>, Vec<String>) {
        let block = self.tree.block(&hash);
        let Some(generator) = &block.generator else {
            return (Vec::new(), Vec::new());
        };
        let state = generator.state();
        let keys = Self::keys(block, state, rng);
        let number = format!("{:#x}", block.number);
        let mut batch = vec![
            json!({ "jsonrpc": "2.0", "id": 0, "method": "eth_getBlockByNumber", "params": [number, false] }),
        ];
        for (address, slots) in &keys {
            let id = batch.len();
            batch.push(json!({ "jsonrpc": "2.0", "id": id, "method": "eth_getProof", "params": [address, slots, number] }));
            for method in ["eth_getBalance", "eth_getTransactionCount", "eth_getCode"] {
                let id = batch.len();
                batch.push(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": [address, number] }));
            }
            for slot in slots {
                let id = batch.len();
                batch.push(json!({ "jsonrpc": "2.0", "id": id, "method": "eth_getStorageAt", "params": [address, slot, number] }));
            }
        }
        // Nodes cap batch sizes (needlefish at 100 requests).
        let bodies: Vec<String> = batch
            .chunks(Self::BATCH_MAX)
            .map(|chunk| Value::Array(chunk.to_vec()).to_string())
            .collect();
        let outcomes = self.each_node(|node| {
            // Proofs history and other indexes may trail the head briefly: a batch with an error
            // in it is read again for a while before the error counts.
            let mut attempt = 0;
            loop {
                let mut results = Some(HashMap::new());
                let mut last = Reply::Transport(String::new());
                for body in &bodies {
                    let reply = node.send(body);
                    match (Self::batch_results(&reply), results.as_mut()) {
                        (Some(part), Some(all)) => all.extend(part),
                        _ => results = None,
                    }
                    last = reply;
                }
                let failing = results.as_ref().is_none_or(|r| r.values().any(Result::is_err));
                if !failing || attempt == Self::READ_RETRIES {
                    return (last, results, attempt);
                }
                attempt += 1;
                thread::sleep(Duration::from_millis(200));
            }
        });
        let mut divergences = Vec::new();
        let mut retried = Vec::new();
        for (node, (reply, results, retries)) in self.nodes.iter().zip(outcomes) {
            if retries > 0 {
                retried.push(format!("{}: state reads retried", node.name));
            }
            let Some(results) = results else {
                divergences.push(Divergence::new(
                    "rpc",
                    &node.name,
                    "batch",
                    Some(block.number),
                    json!("array"),
                    json!(format!("{reply:?}")),
                ));
                continue;
            };
            let mut found = Vec::new();
            Self::compare(&node.name, block, state, &keys, &batch, &results, &mut found);
            divergences.extend(found);
        }
        (divergences, retried)
    }

    /// Most requests in one batch.
    const BATCH_MAX: usize = 90;

    /// How long after a divergent read the same keys are read again.
    const RECHECK_DELAY: Duration = Duration::from_secs(5);

    /// Times a batch of state reads with an error in it is sent again.
    const READ_RETRIES: u32 = 15;

    /// A batch reply's results by request id, `None` when it is not a batch reply.
    fn batch_results(reply: &Reply) -> Option<HashMap<u64, Result<Value, Value>>> {
        let Reply::Body(Value::Array(items)) = reply else {
            return None;
        };
        let mut results = HashMap::new();
        for item in items {
            let Some(id) = item.get("id").and_then(Value::as_u64) else { continue };
            let result = item.get("error").map_or_else(
                || Ok(item.get("result").cloned().unwrap_or(Value::Null)),
                |error| Err(error.clone()),
            );
            results.insert(id, result);
        }
        Some(results)
    }

    /// Accounts and slots to read at `block`.
    fn keys(block: &TreeBlock, state: &WorldState, rng: &mut Rng) -> Vec<(Address, Vec<B256>)> {
        let mut addresses: Vec<Address> = vec![STRESSOR];
        let touched: Vec<&Address> = block.touched.keys().collect();
        for _ in 0..6.min(touched.len()) {
            let address = *touched[rng.index(touched.len())];
            if !addresses.contains(&address) {
                addresses.push(address);
            }
        }
        // Removed accounts first: they must read as absent.
        for address in block.touched.keys().filter(|a| state.account(a).is_none()).take(2) {
            if !addresses.contains(address) {
                addresses.push(*address);
            }
        }
        let accounts: Vec<&Address> = state.accounts.keys().collect();
        for _ in 0..2 {
            addresses.push(*accounts[rng.index(accounts.len())]);
        }
        addresses.push(Stress::empty_account(rng.below(Stress::EMPTY_ACCOUNTS)));
        addresses.push(rng.address());
        let mut seen = BTreeSet::new();
        addresses.retain(|address| seen.insert(*address));
        addresses
            .into_iter()
            .map(|address| {
                let mut slots: Vec<B256> = Vec::new();
                if let Some(touched) = block.touched.get(&address) {
                    let touched: Vec<&B256> = touched.iter().collect();
                    for _ in 0..6.min(touched.len()) {
                        slots.push(*touched[rng.index(touched.len())]);
                    }
                }
                if let Some(account) = state.account(&address) {
                    let present: Vec<&B256> = account.storage.keys().collect();
                    for _ in 0..2.min(present.len()) {
                        slots.push(*present[rng.index(present.len())]);
                    }
                }
                slots.push(Stress::probe_slot(rng));
                slots.push(Stress::probe_slot(rng));
                slots.push(B256::from(U256::from(rng.below(1024))));
                slots.sort();
                slots.dedup();
                (address, slots)
            })
            .collect()
    }

    fn compare(
        node: &str,
        block: &TreeBlock,
        state: &WorldState,
        keys: &[(Address, Vec<B256>)],
        batch: &[Value],
        results: &HashMap<u64, Result<Value, Value>>,
        out: &mut Vec<Divergence>,
    ) {
        let number = Some(block.number);
        let mut push = |kind: &str, detail: &str, expected: Value, got: Value| {
            out.push(Divergence::new(kind, node, detail, number, expected, got));
        };
        let get = |id: usize| -> Result<&Value, Value> {
            match results.get(&(id as u64)) {
                Some(Ok(value)) => Ok(value),
                Some(Err(error)) => Err(error.clone()),
                None => Err(json!("no reply")),
            }
        };
        match get(0) {
            Ok(header) => {
                if header.get("hash") != Some(&json!(block.hash))
                    || header.get("stateRoot") != Some(&json!(block.state_root))
                {
                    push(
                        "header",
                        "eth_getBlockByNumber",
                        json!({ "hash": block.hash, "stateRoot": block.state_root }),
                        json!({ "hash": header.get("hash"), "stateRoot": header.get("stateRoot") }),
                    );
                }
            }
            Err(error) => push("rpc", "eth_getBlockByNumber", json!("block"), error),
        }
        for (id, request) in batch.iter().enumerate().skip(1) {
            let method = request["method"].as_str().unwrap_or_default();
            let address: Address =
                serde_json::from_value(request["params"][0].clone()).unwrap_or_default();
            let account = state.account(&address);
            let reply = match get(id) {
                Ok(reply) => reply,
                Err(error) => {
                    push(
                        "rpc",
                        method,
                        json!("result"),
                        json!({ "address": address, "error": error }),
                    );
                    continue;
                }
            };
            match method {
                "eth_getBalance" => {
                    let expected = account.map_or(U256::ZERO, |a| a.balance);
                    if Self::quantity(reply) != Some(expected) {
                        push(
                            "account",
                            "balance",
                            json!({ "address": address, "value": expected }),
                            reply.clone(),
                        );
                    }
                }
                "eth_getTransactionCount" => {
                    let expected = U256::from(account.map_or(0, |a| a.nonce));
                    if Self::quantity(reply) != Some(expected) {
                        push(
                            "account",
                            "nonce",
                            json!({ "address": address, "value": expected }),
                            reply.clone(),
                        );
                    }
                }
                "eth_getCode" => {
                    let expected = account.map_or(KECCAK256_EMPTY, |a| a.code_hash);
                    let code: Option<Bytes> = serde_json::from_value(reply.clone()).ok();
                    let got = code.map(keccak256);
                    if got != Some(expected) {
                        push(
                            "account",
                            "code",
                            json!({ "address": address, "codeHash": expected }),
                            json!({ "codeHash": got }),
                        );
                    }
                }
                "eth_getStorageAt" => {
                    let slot: B256 =
                        serde_json::from_value(request["params"][1].clone()).unwrap_or_default();
                    let expected = state.storage(&address, slot);
                    if Self::quantity(reply) != Some(expected) {
                        push(
                            "storage",
                            "eth_getStorageAt",
                            json!({ "address": address, "slot": slot, "value": expected }),
                            reply.clone(),
                        );
                    }
                }
                "eth_getProof" => {
                    let slots = keys
                        .iter()
                        .find(|(a, _)| *a == address)
                        .map(|(_, s)| s.as_slice())
                        .unwrap_or_default();
                    if let Err(failure) =
                        Self::check_proof(address, account, slots, block.state_root, reply)
                    {
                        let (detail, expected, got) = *failure;
                        push("proof", &detail, expected, got);
                    }
                }
                _ => {}
            }
        }
    }

    /// Checks one `eth_getProof` reply against the expected account and the block's root.
    fn check_proof(
        address: Address,
        account: Option<&crate::Account>,
        slots: &[B256],
        root: B256,
        reply: &Value,
    ) -> Result<(), Box<(String, Value, Value)>> {
        let fail = |detail: &str, expected: Value| {
            Err(Box::new((
                detail.to_owned(),
                json!({ "address": address, "expected": expected }),
                reply.clone(),
            )))
        };
        let storage_root = account.map_or(EMPTY_ROOT_HASH, |a| {
            storage_root_unhashed(a.storage.iter().map(|(k, v)| (*k, *v)))
        });
        let (balance, nonce, code_hash) =
            account.map_or((U256::ZERO, 0, KECCAK256_EMPTY), |a| (a.balance, a.nonce, a.code_hash));
        if Self::quantity(&reply["balance"]) != Some(balance) {
            return fail("balance", json!(balance));
        }
        if Self::quantity(&reply["nonce"]) != Some(U256::from(nonce)) {
            return fail("nonce", json!(nonce));
        }
        let hash = |key: &str| serde_json::from_value::<B256>(reply[key].clone()).ok();
        // An absent account may report a zero code hash and storage hash.
        let absent = account.is_none();
        let got_code = hash("codeHash");
        if got_code != Some(code_hash) && !(absent && got_code == Some(B256::ZERO)) {
            return fail("codeHash", json!(code_hash));
        }
        let got_storage = hash("storageHash");
        if got_storage != Some(storage_root) && !(absent && got_storage == Some(B256::ZERO)) {
            return fail("storageHash", json!(storage_root));
        }
        let nodes: Vec<Bytes> =
            serde_json::from_value(reply["accountProof"].clone()).unwrap_or_default();
        let expected = account.map(|a| {
            alloy_rlp::encode(TrieAccount::new(a.nonce, a.balance, storage_root, a.code_hash))
        });
        if let Err(error) =
            verify_proof(root, Nibbles::unpack(keccak256(address)), expected, &nodes)
        {
            return fail("accountProof", json!(format!("verifies against {root}: {error}")));
        }
        let proofs = reply["storageProof"].as_array().cloned().unwrap_or_default();
        if proofs.len() != slots.len() {
            return fail("storageProof count", json!(slots.len()));
        }
        for (slot, proof) in slots.iter().zip(&proofs) {
            let value = account.and_then(|a| a.storage.get(slot).copied()).unwrap_or_default();
            if Self::quantity(&proof["value"]) != Some(value) {
                return fail("storageProof value", json!({ "slot": slot, "value": value }));
            }
            let nodes: Vec<Bytes> =
                serde_json::from_value(proof["proof"].clone()).unwrap_or_default();
            let expected = (!value.is_zero()).then(|| alloy_rlp::encode(value));
            if let Err(error) =
                verify_proof(storage_root, Nibbles::unpack(keccak256(slot)), expected, &nodes)
            {
                return fail(
                    "storageProof",
                    json!({ "slot": slot, "error": format!("verifies against {storage_root}: {error}") }),
                );
            }
        }
        Ok(())
    }

    /// A hex quantity or a 32-byte hex word as a number.
    fn quantity(value: &Value) -> Option<U256> {
        let text = value.as_str()?;
        let digits = text.strip_prefix("0x")?;
        if digits.is_empty() || digits.len() > 64 {
            return None;
        }
        U256::from_str_radix(digits, 16).ok()
    }

    /// Writes a reproducer: the genesis file, the operations since the last wipe, the Engine API
    /// trace (with restarts), and the divergences.
    pub fn save_reproducer(&self, dir: &PathBuf, divergences: &[Divergence]) -> eyre::Result<()> {
        fs::create_dir_all(dir)?;
        let root = &self.procs[0].root;
        fs::copy(root.join("genesis.json"), dir.join("genesis.json"))?;
        fs::write(dir.join("ops.json"), serde_json::to_vec_pretty(&self.history)?)?;
        let trace: String = self.trace.iter().map(|t| format!("{t}\n")).collect();
        fs::write(dir.join("trace.jsonl"), trace)?;
        fs::write(dir.join("divergences.json"), serde_json::to_vec_pretty(divergences)?)?;
        Ok(())
    }

    /// [`Self::check`], recovering a node an injected fault crashed during the reads (a node
    /// that persists in the background can crash at any time): what it answered while going
    /// down is dropped, and the reads run again after its recovery.
    pub fn check_recovering(&mut self, heights: u8, seed: u64) -> Vec<Divergence> {
        let divergences = self.check(heights, seed);
        match self.recover_faults() {
            Some(recovered) if recovered.is_empty() => self.check(heights, seed),
            Some(recovered) => recovered,
            None => divergences,
        }
    }

    /// Replays `ops` from genesis on wiped nodes, with the campaign's checks after every
    /// operation, and returns the divergences of the first operation that has lasting ones, or
    /// else every transient one.
    pub fn replay(&mut self, ops: &[StateOp]) -> eyre::Result<Vec<Divergence>> {
        self.reset()?;
        let mut transient = Vec::new();
        for op in ops {
            let mut divergences = self.run(op)?;
            if divergences.is_empty() && !matches!(op, StateOp::Check { .. }) {
                let seed = self.rng.next_u64();
                divergences = self.check_recovering(1, seed);
            }
            if divergences.iter().any(|d| !d.transient()) {
                return Ok(divergences);
            }
            transient.extend(divergences);
        }
        let seed = self.rng.next_u64();
        let divergences = self.check_recovering(3, seed);
        if divergences.iter().any(|d| !d.transient()) {
            return Ok(divergences);
        }
        transient.extend(divergences);
        Ok(transient)
    }

    /// Restarts every node an injected fault crashed (see [`NodeProcess::injected_crash`]),
    /// with fault injection off for that run, and moves it back to the head, resending the
    /// blocks it lost. Returns `None` when no node crashed that way, otherwise the recovery's
    /// divergences: a node that cannot reopen its datadir, rejects the head, or reads back
    /// different state is a finding.
    pub fn recover_faults(&mut self) -> Option<Vec<Divergence>> {
        let crashed: Vec<(usize, String)> = self
            .procs
            .iter_mut()
            .enumerate()
            .filter_map(|(i, proc)| proc.injected_crash().map(|line| (i, line)))
            .collect();
        if crashed.is_empty() {
            return None;
        }
        let mut divergences = Vec::new();
        for (index, line) in crashed {
            let name = self.nodes[index].name.clone();
            // Group by the phase the crash hit: `commit <n> <step> ...` without the numbers.
            let phase = line
                .split("phase=\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .map(|phase| {
                    phase
                        .split(' ')
                        .filter(|w| w.parse::<u64>().is_err())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            let mode = if line.contains("LoseUnsynced") { "lose_unsynced" } else { "torn" };
            *self.observations.entry(format!("{name}: fault crash {mode} {phase}")).or_default() +=
                1;
            self.trace.push(json!({ "kind": "fault", "node": name, "line": line }));
            let proc = &mut self.procs[index];
            proc.stop(true);
            let started = proc.spawn(false).and_then(|()| proc.wait_ready(&self.nodes[index]));
            if let Err(error) = started {
                let tail = fs::read_to_string(proc.log_path())
                    .map(|text| text.lines().rev().take(30).collect::<Vec<_>>().join("\n"))
                    .unwrap_or_default();
                divergences.push(Divergence::new(
                    "restart",
                    &name,
                    "start after injected fault",
                    Some(self.tree.head().number),
                    json!("answers JSON-RPC"),
                    json!({ "error": format!("{error:#}"), "fault": line, "logTail": tail }),
                ));
            }
        }
        if divergences.is_empty() {
            let head = self.inflight_head.take().unwrap_or(self.tree.head);
            divergences = self.forkchoice_resync(head, "fault");
        }
        Some(divergences)
    }

    /// Fails unless every node process is still running.
    pub fn alive(&mut self) -> eyre::Result<()> {
        for proc in &mut self.procs {
            if !proc.alive() {
                bail!("{} exited", proc.name);
            }
        }
        Ok(())
    }
}
