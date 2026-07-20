use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::{Arc, Mutex},
};

use alloy_consensus::{
    transaction::{Recovered, SignerRecoverable},
    BlockBody, Header, Receipt, SignableTransaction, Transaction as _, TxEip1559, TxEnvelope,
};
use alloy_chains::Chain;
use alloy_eips::{
    eip2718::{Decodable2718, Encodable2718},
    BlockNumHash,
};
use alloy_genesis::ChainConfig;
use alloy_primitives::{
    keccak256, Address, Bytes, Log, Signature as PrimitiveSignature, TxKind, B256, U256,
};

use async_trait::async_trait;
use k256::ecdsa::{signature::hazmat::PrehashSigner as _, SigningKey, VerifyingKey};
use alloy_rlp::Encodable as _;
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_evm::BaseEvmConfig;
use base_protocol::{BatchType, ChannelId, DERIVATION_VERSION_0, Frame, SingleBatch};
use base_common_consensus::{BaseBlock, BaseTxEnvelope, TxDeposit};
use base_common_genesis::{ChainGenesis, RollupConfig, SystemConfig};
use base_consensus_derive::{
    ChainProvider, DataAvailabilityProvider, DerivationPipeline, L2ChainProvider, Pipeline,
    PipelineBuilder, PipelineError, PipelineErrorKind, PolledAttributesQueueStage, ResetSignal,
    Signal, SignalReceiver, StatefulAttributesBuilder, StepResult,
};
use base_protocol::{
    AttributesWithParent, BatchValidationProvider, BlockInfo, L1BlockInfoTx, L2BlockInfo,
};
use reth_evm::{ConfigureEvm, Evm};
use revm::{
    Database, DatabaseCommit,
    context::result::{ExecutionResult, Output, ResultAndState},
    database::InMemoryDB,
    state::AccountInfo,
};

// ── L1 chain ────────────────────────────────────────────────────────────────

/// A minimal L1 block for the in-memory devnet.
///
/// Tracks only the fields the derivation pipeline reads via [`WasmL1Provider`]:
/// the header (number, timestamp, hash, parent_hash), plus receipts (empty for
/// deposit-free L1 blocks).  Batcher calldata is fed directly into [`InMemoryDap`]
/// rather than through real L1 transactions.
#[derive(Debug, Clone)]
pub struct L1Block {
    /// Consensus header.
    pub header: Header,
    /// Consensus receipts (empty for devnet blocks — no user deposits).
    pub receipts: Vec<Receipt>,
}

impl L1Block {
    /// Block number.
    pub const fn number(&self) -> u64 {
        self.header.number
    }

    /// Block timestamp.
    pub const fn timestamp(&self) -> u64 {
        self.header.timestamp
    }

    /// Compute the block hash by hashing the RLP-encoded header.
    pub fn hash(&self) -> B256 {
        self.header.hash_slow()
    }
}

fn block_info_from_l1(block: &L1Block) -> BlockInfo {
    BlockInfo {
        hash: block.hash(),
        number: block.number(),
        parent_hash: block.header.parent_hash,
        timestamp: block.timestamp(),
    }
}

// ── Shared L1 chain ──────────────────────────────────────────────────────────

type L1ChainInner = Arc<Mutex<Vec<L1Block>>>;

/// Append-only in-memory L1 chain shared between the devnet and [`WasmL1Provider`].
#[derive(Debug, Clone, Default)]
pub struct SharedL1 {
    inner: L1ChainInner,
}

impl SharedL1 {
    /// Create an empty chain.
    pub fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(Vec::new())) }
    }

    /// Append a newly-mined block.
    pub fn push(&self, block: L1Block) {
        self.inner.lock().expect("L1 chain lock poisoned").push(block);
    }

    fn with<R>(&self, f: impl FnOnce(&[L1Block]) -> R) -> R {
        let g = self.inner.lock().expect("L1 chain lock poisoned");
        f(&g)
    }
}

// ── WasmL1Provider ───────────────────────────────────────────────────────────

/// Error returned by [`WasmL1Provider`].
#[derive(Debug, thiserror::Error)]
pub enum L1ProviderError {
    /// Block not found by number.
    #[error("L1 block not found at number {0}")]
    BlockNotFound(u64),
    /// Block not found by hash.
    #[error("L1 block hash not found")]
    HashNotFound,
}

impl From<L1ProviderError> for PipelineErrorKind {
    fn from(e: L1ProviderError) -> Self {
        PipelineError::Provider(e.to_string()).temp()
    }
}

/// [`ChainProvider`] backed by the shared in-memory L1 chain.
#[derive(Debug, Clone)]
pub struct WasmL1Provider {
    chain: SharedL1,
}

impl WasmL1Provider {
    /// Create a new provider wrapping the given shared chain.
    pub fn new(chain: SharedL1) -> Self {
        Self { chain }
    }
}

#[async_trait]
impl ChainProvider for WasmL1Provider {
    type Error = L1ProviderError;

    async fn header_by_hash(&mut self, hash: B256) -> Result<Header, Self::Error> {
        self.chain.with(|blocks| {
            blocks
                .iter()
                .find(|b| b.hash() == hash)
                .map(|b| b.header.clone())
                .ok_or(L1ProviderError::HashNotFound)
        })
    }

    async fn block_info_by_number(&mut self, number: u64) -> Result<BlockInfo, Self::Error> {
        self.chain.with(|blocks| {
            blocks
                .get(number as usize)
                .map(block_info_from_l1)
                .ok_or(L1ProviderError::BlockNotFound(number))
        })
    }

    async fn receipts_by_hash(&mut self, hash: B256) -> Result<Vec<Receipt>, Self::Error> {
        self.chain.with(|blocks| {
            Ok(blocks
                .iter()
                .find(|b| b.hash() == hash)
                .map(|b| b.receipts.clone())
                .unwrap_or_default())
        })
    }

    async fn block_info_and_transactions_by_hash(
        &mut self,
        hash: B256,
    ) -> Result<(BlockInfo, Vec<TxEnvelope>), Self::Error> {
        self.chain.with(|blocks| {
            blocks
                .iter()
                .find(|b| b.hash() == hash)
                .map(|b| (block_info_from_l1(b), Vec::new()))
                .ok_or(L1ProviderError::HashNotFound)
        })
    }
}

// ── WasmL2Provider ───────────────────────────────────────────────────────────

/// Error returned by [`WasmL2Provider`].
#[derive(Debug, thiserror::Error)]
pub enum L2ProviderError {
    /// L2 block not found.
    #[error("L2 block not found at number {0}")]
    BlockNotFound(u64),
    /// System config not found.
    #[error("system config not found for L2 block {0}")]
    SystemConfigNotFound(u64),
}

impl From<L2ProviderError> for PipelineErrorKind {
    fn from(e: L2ProviderError) -> Self {
        PipelineError::Provider(e.to_string()).temp()
    }
}

/// [`L2ChainProvider`] and [`BatchValidationProvider`] backed by in-memory maps.
#[derive(Debug, Clone, Default)]
pub struct WasmL2Provider {
    blocks: Arc<Mutex<HashMap<u64, L2BlockInfo>>>,
    base_blocks: Arc<Mutex<HashMap<u64, BaseBlock>>>,
    system_configs: Arc<Mutex<HashMap<u64, SystemConfig>>>,
}

impl WasmL2Provider {
    /// Create a provider pre-populated with the L2 genesis from `rollup_config`.
    pub fn from_genesis(rollup_config: &RollupConfig) -> Self {
        let provider = Self::default();
        let genesis_l2 = L2BlockInfo {
            block_info: BlockInfo {
                hash: rollup_config.genesis.l2.hash,
                number: rollup_config.genesis.l2.number,
                parent_hash: Default::default(),
                timestamp: rollup_config.genesis.l2_time,
            },
            l1_origin: BlockNumHash {
                hash: rollup_config.genesis.l1.hash,
                number: rollup_config.genesis.l1.number,
            },
            seq_num: 0,
        };
        let genesis_config = rollup_config
            .genesis
            .system_config
            .unwrap_or(SystemConfig { gas_limit: 30_000_000, ..Default::default() });
        provider.insert_block(genesis_l2);
        provider.insert_system_config(rollup_config.genesis.l2.number, genesis_config);
        provider
    }

    /// Insert an L2 block info entry.
    pub fn insert_block(&self, block: L2BlockInfo) {
        self.blocks
            .lock()
            .expect("L2 blocks lock poisoned")
            .insert(block.block_info.number, block);
    }

    /// Insert a full L2 block (needed by `block_by_number`).
    pub fn insert_base_block(&self, number: u64, block: BaseBlock) {
        self.base_blocks
            .lock()
            .expect("L2 base blocks lock poisoned")
            .insert(number, block);
    }

    /// Insert a system config for the given L2 block number.
    pub fn insert_system_config(&self, number: u64, config: SystemConfig) {
        self.system_configs
            .lock()
            .expect("L2 system configs lock poisoned")
            .insert(number, config);
    }
}

#[async_trait]
impl BatchValidationProvider for WasmL2Provider {
    type Error = L2ProviderError;

    async fn l2_block_info_by_number(
        &mut self,
        number: u64,
    ) -> Result<L2BlockInfo, L2ProviderError> {
        self.blocks
            .lock()
            .expect("L2 blocks lock poisoned")
            .get(&number)
            .copied()
            .ok_or(L2ProviderError::BlockNotFound(number))
    }

    async fn block_by_number(&mut self, number: u64) -> Result<BaseBlock, L2ProviderError> {
        self.base_blocks
            .lock()
            .expect("L2 base blocks lock poisoned")
            .get(&number)
            .cloned()
            .ok_or(L2ProviderError::BlockNotFound(number))
    }
}

#[async_trait]
impl L2ChainProvider for WasmL2Provider {
    type Error = L2ProviderError;

    async fn system_config_by_number(
        &mut self,
        number: u64,
        _rollup_config: Arc<RollupConfig>,
    ) -> Result<SystemConfig, L2ProviderError> {
        let system_configs =
            self.system_configs.lock().expect("L2 system configs lock poisoned");
        (0..=number)
            .rev()
            .find_map(|n| system_configs.get(&n).copied())
            .ok_or(L2ProviderError::SystemConfigNotFound(number))
    }
}

// ── InMemoryDap ──────────────────────────────────────────────────────────────

type DapQueueInner = Arc<Mutex<HashMap<B256, VecDeque<Bytes>>>>;

/// In-memory [`DataAvailabilityProvider`] that the batcher writes to and the pipeline reads from.
///
/// Keyed by L1 block hash. Each entry is a FIFO queue of calldata items, one per frame
/// (`DERIVATION_VERSION_0 ++ frame.encode()`).  The batcher pushes items after encoding
/// each channel; the pipeline pops them when deriving that L1 block.
///
/// The inner map is shared behind an [`Arc<Mutex>`] so the devnet can push data into the
/// DAP through [`DapQueue`] while the pipeline holds ownership of the [`InMemoryDap`].
#[derive(Debug, Clone, Default)]
pub struct InMemoryDap {
    queue: DapQueueInner,
}

/// A handle to the shared queue inside an [`InMemoryDap`].
///
/// The [`Devnet`] holds this handle and uses it to push frame calldata after
/// the batcher encodes each channel.
#[derive(Debug, Clone, Default)]
pub struct DapQueue {
    inner: DapQueueInner,
}

impl DapQueue {
    /// Push a calldata item for the given L1 block hash.
    pub fn push(&self, block_hash: B256, data: Bytes) {
        self.inner
            .lock()
            .expect("dap queue lock poisoned")
            .entry(block_hash)
            .or_default()
            .push_back(data);
    }
}

impl InMemoryDap {
    /// Create a linked `(InMemoryDap, DapQueue)` pair that share the same backing map.
    pub fn new_with_queue() -> (Self, DapQueue) {
        let inner = Arc::new(Mutex::new(HashMap::new()));
        (Self { queue: Arc::clone(&inner) }, DapQueue { inner })
    }
}

#[async_trait]
impl DataAvailabilityProvider for InMemoryDap {
    type Item = Bytes;

    async fn next(
        &mut self,
        block_ref: &BlockInfo,
        _batcher_addr: Address,
    ) -> base_consensus_derive::PipelineResult<Bytes> {
        let mut map = self.queue.lock().expect("dap queue lock poisoned");
        let deque = map.entry(block_ref.hash).or_default();
        deque.pop_front().ok_or(PipelineError::Eof.temp())
    }

    fn clear(&mut self) {
        // The backing map is keyed by L1 block hash; advancing to a new L1 block
        // naturally reads that block's data via `next(block_ref)`. There is no
        // global iterator cursor to reset, so this is intentionally a no-op.
    }
}

// ── Pipeline type alias ──────────────────────────────────────────────────────

type WasmAttrBuilder = StatefulAttributesBuilder<WasmL1Provider, WasmL2Provider>;

type WasmPipeline = DerivationPipeline<
    PolledAttributesQueueStage<InMemoryDap, WasmL1Provider, WasmL2Provider, WasmAttrBuilder>,
    WasmL2Provider,
>;

// ── L2 block production ──────────────────────────────────────────────────────

/// Produce one L2 block whose first transaction is the L1 block info deposit.
///
/// Uses Bedrock format (no hardforks active in the default [`RollupConfig`]).
fn produce_l2_block(
    rollup_config: &RollupConfig,
    l1_header: &Header,
    l2_parent: &BaseBlock,
    seq_num: u64,
    system_config: &SystemConfig,
    extra_txs: Vec<BaseTxEnvelope>,
) -> BaseBlock {
    let l2_block_time = l2_parent.header.timestamp + rollup_config.block_time;
    let l1_config = ChainConfig::default();
    let (_, deposit_sealed) = L1BlockInfoTx::try_new_with_deposit_tx(
        rollup_config,
        &l1_config,
        system_config,
        seq_num,
        l1_header,
        l2_block_time,
    )
    .expect("L1BlockInfoTx::try_new_with_deposit_tx failed");

    let deposit_tx = BaseTxEnvelope::Deposit(deposit_sealed);

    let number = l2_parent.header.number + 1;
    let parent_hash = l2_parent.header.hash_slow();

    let mut all_txs = vec![deposit_tx];
    all_txs.extend(extra_txs);

    let transactions_root = alloy_consensus::proofs::calculate_transaction_root(&all_txs);

    BaseBlock {
        header: Header {
            number,
            timestamp: l2_block_time,
            parent_hash,
            transactions_root,
            ..Default::default()
        },
        body: BlockBody { transactions: all_txs, ..Default::default() },
    }
}

/// Compute the [`L2BlockInfo`] for a freshly produced L2 block.
fn l2_block_info_from(block: &BaseBlock, l1_origin: BlockNumHash) -> L2BlockInfo {
    L2BlockInfo {
        block_info: BlockInfo {
            hash: block.header.hash_slow(),
            number: block.header.number,
            parent_hash: block.header.parent_hash,
            timestamp: block.header.timestamp,
        },
        l1_origin,
        seq_num: 0,
    }
}

/// Extract the true L1 origin from the L1BlockInfoTx embedded in derived payload attributes.
pub fn l1_origin_from_attrs(attrs: &AttributesWithParent) -> Option<BlockNumHash> {
    let txs = attrs.attributes.transactions.as_ref()?;
    let first = txs.first()?;
    let deposit = TxDeposit::decode_2718(&mut first.as_ref()).ok()?;
    let l1_info = L1BlockInfoTx::decode_calldata(&deposit.input).ok()?;
    Some(l1_info.id())
}

/// The devnet's pre-funded developer account private key (well-known, Anvil account 0).
pub const DEV_KEY: [u8; 32] = [
    0xac, 0x09, 0x74, 0xbe, 0xc3, 0x9a, 0x17, 0xe3, 0x6b, 0xa4, 0xa6, 0xb4, 0xd2, 0x38, 0xff, 0x94,
    0x4b, 0xac, 0xb4, 0x78, 0xcb, 0xed, 0x5e, 0xfc, 0xae, 0x78, 0x4d, 0x7b, 0xf4, 0xf2, 0xff, 0x80,
];

/// Derive the Ethereum [`Address`] for a secp256k1 [`VerifyingKey`].
pub fn address_from_verifying_key(key: &VerifyingKey) -> Address {
    let point = key.to_encoded_point(false);
    let hash = keccak256(&point.as_bytes()[1..]);
    Address::from_slice(&hash[12..])
}

// ── Devnet ───────────────────────────────────────────────────────────────────

/// WASM-compatible in-process devnet.
///
/// Drives all five components — L1 chain, batcher, sequencer (CL only),
/// validator (CL only) — in a single async state machine without any network
/// I/O, file system access, or RocksDB.
///
/// Use [`Devnet::new()`] to start, then drive with [`Devnet::run_epoch`].
pub struct Devnet {
    /// Shared rollup configuration (genesis, block_time, batcher_address, …).
    rollup_config: Arc<RollupConfig>,
    /// Chain specification used by [`BaseEvmConfig`].
    chain_spec: Arc<BaseChainSpec>,
    /// EVM configuration used to build per-block execution environments.
    evm_config: BaseEvmConfig,
    /// In-memory L1 chain. Shared with [`WasmL1Provider`].
    l1_chain: SharedL1,
    /// Last produced L2 block (sequencer unsafe head).
    l2_head: BaseBlock,
    /// Sequencer unsafe head pointer.
    seq_head: L2BlockInfo,
    /// Validator safe head pointer (last block the derivation pipeline confirmed).
    safe_head: L2BlockInfo,
    /// Current L1 origin used by the sequencer.
    l1_origin: BlockNumHash,
    /// Sequence number within the current L1 epoch (resets to 0 at each new L1 block).
    seq_num: u64,
    /// System config (kept in sync with genesis; no on-chain updates in the devnet).
    system_config: SystemConfig,
    /// Counter for generating unique channel IDs (one channel per L2 block).
    channel_seq: u64,
    /// Handle into the shared DAP queue: the batcher pushes frame calldata here.
    dap_queue: DapQueue,
    /// L2 chain provider shared with the derivation pipeline.
    l2_provider: WasmL2Provider,
    /// Derivation (validation) pipeline.
    pipeline: WasmPipeline,
    /// Queued user transactions to include in the next produced L2 block.
    pending_txs: Vec<BaseTxEnvelope>,
    /// Number of derived blocks whose independently-computed hash matched the sequenced block.
    pub verified_count: u64,
    /// Per-block debug lines from the last [`Devnet::derive_until_idle`] call.
    pub last_derive_debug: Vec<String>,
    /// Last observed account balances (mirrors committed EVM state for convenience).
    balances: HashMap<Address, U256>,
    /// Next nonce to use for local transaction signing helpers.
    nonces: HashMap<Address, u64>,
    /// Transaction hash → (block number, index within block), for RPC lookups.
    tx_locations: HashMap<B256, (u64, usize)>,
    /// Transaction hash → EVM execution details for receipts and RPC metadata.
    tx_execution: HashMap<B256, TxExecutionRecord>,
    /// Block number → total gas consumed by executed non-deposit transactions.
    block_gas_used: HashMap<u64, u64>,
    /// Flattened log stream with block/transaction indices for `eth_getLogs`.
    logs: Vec<IndexedLog>,
    /// Persistent in-memory EVM database.
    evm_db: InMemoryDB,
    /// Address of the devnet's pre-funded developer account.
    dev_address: Address,
}

#[derive(Debug, Clone)]
struct TxExecutionRecord {
    status: bool,
    gas_used: u64,
    cumulative_gas_used: u64,
    contract_address: Option<Address>,
    logs: Vec<Log>,
}

#[derive(Debug, Clone)]
struct IndexedLog {
    block_number: u64,
    block_hash: B256,
    tx_hash: B256,
    tx_index: usize,
    log_index: usize,
    log: Log,
}

impl fmt::Debug for Devnet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Devnet")
            .field("seq_head", &self.seq_head.block_info.number)
            .field("safe_head", &self.safe_head.block_info.number)
            .field("l1_tip", &self.l1_origin.number)
            .field("chain_spec", &self.chain_spec)
            .finish_non_exhaustive()
    }
}

fn block_to_calldata(block: &BaseBlock, channel_id: ChannelId) -> Bytes {
    let Some(BaseTxEnvelope::Deposit(deposit)) = block.body.transactions.first() else {
        panic!("L2 block has no deposit tx as first transaction");
    };
    let l1_info = L1BlockInfoTx::decode_calldata(&deposit.input)
        .expect("failed to decode L1BlockInfoTx from deposit tx");
    let epoch = l1_info.id();

    let transactions: Vec<Bytes> = block
        .body
        .transactions
        .iter()
        .filter(|tx| !matches!(tx, BaseTxEnvelope::Deposit(_)))
        .map(|tx| tx.encoded_2718().into())
        .collect();

    let batch = SingleBatch {
        parent_hash: block.header.parent_hash,
        epoch_num: epoch.number,
        epoch_hash: epoch.hash,
        timestamp: block.header.timestamp,
        transactions,
    };

    let mut raw = vec![BatchType::Single as u8];
    batch.encode(&mut raw);

    let mut rlp_buf = Vec::new();
    raw.as_slice().encode(&mut rlp_buf);

    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&rlp_buf, 9);

    let frame = Frame { id: channel_id, number: 0, data: compressed, is_last: true };

    let mut calldata = vec![DERIVATION_VERSION_0];
    calldata.extend_from_slice(&frame.encode());
    Bytes::from(calldata)
}

impl Devnet {
    /// Create a new devnet and initialise the derivation pipeline.
    ///
    /// Genesis L1 and L2 block hashes are computed from the actual block headers so that the
    /// [`RollupConfig`] is consistent with the in-memory chain from the very first step.
    pub async fn new() -> Self {
        // Install WASM panic hook so panics show as `console.error` instead of
        // the opaque "unreachable" WASM trap.
        #[cfg(target_family = "wasm")]
        console_error_panic_hook::set_once();

        let system_config = SystemConfig { gas_limit: 30_000_000, ..Default::default() };

        let genesis_l1 = L1Block {
            header: Header { number: 0, timestamp: 0, ..Default::default() },
            receipts: vec![],
        };
        let genesis_l1_hash = genesis_l1.hash();

        let genesis_l2_block = BaseBlock {
            header: Header { number: 0, timestamp: 0, ..Default::default() },
            body: BlockBody::default(),
        };
        let genesis_l2_hash = genesis_l2_block.header.hash_slow();

        let rollup_config = Arc::new(RollupConfig {
            block_time: 2,
            l2_chain_id: Chain::from_id(901),
            genesis: ChainGenesis {
                l1: BlockNumHash { number: 0, hash: genesis_l1_hash },
                l2: BlockNumHash { number: 0, hash: genesis_l2_hash },
                l2_time: 0,
                system_config: Some(system_config),
            },
            ..Default::default()
        });

        let chain_spec = Arc::new(
            BaseChainSpecBuilder::default()
                .chain(Chain::from_id(901))
                .genesis(alloy_genesis::Genesis::default())
                .azul_activated()
                .build(),
        );
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));

        let l1_chain = SharedL1::new();
        l1_chain.push(genesis_l1);

        let l1_origin = BlockNumHash { number: 0, hash: genesis_l1_hash };

        let genesis_l2_info = L2BlockInfo {
            block_info: BlockInfo {
                hash: genesis_l2_hash,
                number: 0,
                parent_hash: B256::ZERO,
                timestamp: 0,
            },
            l1_origin,
            seq_num: 0,
        };

        let l2_provider = WasmL2Provider::from_genesis(&rollup_config);
        l2_provider.insert_base_block(0, genesis_l2_block.clone());
        let l1_provider = WasmL1Provider::new(l1_chain.clone());

        let (dap, dap_queue) = InMemoryDap::new_with_queue();

        let l1_origin_info = BlockInfo {
            hash: genesis_l1_hash,
            number: 0,
            parent_hash: B256::ZERO,
            timestamp: 0,
        };
        let attr_builder = StatefulAttributesBuilder::new(
            Arc::clone(&rollup_config),
            Arc::new(ChainConfig::default()),
            l2_provider.clone(),
            l1_provider.clone(),
        );
        let mut pipeline =
            PipelineBuilder::<WasmAttrBuilder, WasmL1Provider, WasmL2Provider, InMemoryDap>::new()
                .rollup_config(Arc::clone(&rollup_config))
                .origin(l1_origin_info)
                .dap_source(dap)
                .builder(attr_builder)
                .l2_chain_provider(l2_provider.clone())
                .chain_provider(l1_provider)
                .build_polled();

        // Reset the pipeline to the genesis safe head.
        pipeline
            .signal(Signal::Reset(ResetSignal { l2_safe_head: genesis_l2_info }))
            .await
            .expect("pipeline reset failed");

        let dev_key = SigningKey::from_bytes(&DEV_KEY.into()).expect("valid devnet key");
        let dev_address = address_from_verifying_key(dev_key.verifying_key());
        // 10,000 ETH, pre-funded so the built-in devnet key can send transfers immediately.
        let dev_balance = U256::from(10_000u64) * U256::from(10u64).pow(U256::from(18u64));
        let mut balances = HashMap::new();
        balances.insert(dev_address, dev_balance);
        let mut evm_db = InMemoryDB::default();
        evm_db.insert_account_info(
            dev_address,
            AccountInfo { balance: dev_balance, nonce: 0, ..Default::default() },
        );

        Self {
            rollup_config,
            chain_spec,
            evm_config,
            l1_chain,
            l2_head: genesis_l2_block,
            seq_head: genesis_l2_info,
            safe_head: genesis_l2_info,
            l1_origin,
            seq_num: 0,
            system_config,
            channel_seq: 0,
            dap_queue,
            l2_provider,
            pipeline,
            pending_txs: Vec::new(),
            verified_count: 0,
            last_derive_debug: Vec::new(),
            balances,
            nonces: HashMap::new(),
            tx_locations: HashMap::new(),
            tx_execution: HashMap::new(),
            block_gas_used: HashMap::new(),
            logs: Vec::new(),
            evm_db,
            dev_address,
        }
    }

    /// Mine `n` L1 blocks and return the new L1 tip block number.
    ///
    /// L1 block time equals [`RollupConfig::block_time`] (same as L2), giving a 1-to-1
    /// L1-epoch-to-L2-block ratio and eliminating null blocks in derivation.
    pub fn mine_l1_blocks(&mut self, n: u64) -> u64 {
        let mut tip = self.l1_chain.with(|b| b.last().expect("L1 chain empty").clone());
        for _ in 0..n {
            let new_block = L1Block {
                header: Header {
                    number: tip.number() + 1,
                    timestamp: tip.timestamp() + self.rollup_config.block_time,
                    parent_hash: tip.hash(),
                    ..Default::default()
                },
                receipts: vec![],
            };
            tip = new_block.clone();
            self.l1_chain.push(new_block);
        }
        self.l1_origin = BlockNumHash { number: tip.number(), hash: tip.hash() };
        self.seq_num = 0;
        tip.number()
    }

    /// Produce one L2 block on top of the current sequencer head.
    ///
    /// The new block contains only the L1 block info deposit transaction.
    /// The L1 origin is the current `l1_origin` pointer.
    pub fn produce_l2_block(&mut self) -> BaseBlock {
        let l1_tip = self.l1_chain.with(|b| b.last().expect("L1 chain empty").clone());
        let extra = std::mem::take(&mut self.pending_txs);
        let block = produce_l2_block(
            &self.rollup_config,
            &l1_tip.header,
            &self.l2_head,
            self.seq_num,
            &self.system_config,
            extra,
        );
        let block_info = l2_block_info_from(&block, self.l1_origin);

        self.seq_head = block_info;
        self.seq_num += 1;
        self.l2_provider.insert_block(block_info);
        self.l2_provider.insert_base_block(block.header.number, block.clone());
        self.l2_head = block.clone();
        self.apply_ledger_updates(&block);
        block
    }

    /// Execute all non-deposit transactions in a produced block via the real EVM.
    fn apply_ledger_updates(&mut self, block: &BaseBlock) {
        let mut cumulative_gas_used = 0u64;
        let block_hash = block.header.hash_slow();

        for (tx_index, tx) in block.body.transactions.iter().enumerate() {
            if matches!(tx, BaseTxEnvelope::Deposit(_)) {
                continue;
            }
            let tx_hash = tx.tx_hash();
            self.tx_locations.insert(tx_hash, (block.header.number, tx_index));

            let Ok(sender) = tx.recover_signer() else {
                self.tx_execution.insert(
                    tx_hash,
                    TxExecutionRecord {
                        status: false,
                        gas_used: 0,
                        cumulative_gas_used,
                        contract_address: None,
                        logs: Vec::new(),
                    },
                );
                continue;
            };

            let recovered = Recovered::new_unchecked(tx.clone(), sender);
            let execution_header = self.execution_header_for(&block.header);
            let evm_env = self
                .evm_config
                .evm_env(&execution_header)
                .expect("failed to build evm env for produced block");
            let mut evm = self.evm_config.evm_with_env(&mut self.evm_db, evm_env);
            let tx_result = evm.transact(&recovered);

            let (status, gas_used, contract_address, logs) = match tx_result {
                Ok(ResultAndState { state, result }) => {
                    let gas_used = result.tx_gas_used();
                    cumulative_gas_used = cumulative_gas_used.saturating_add(gas_used);
                    let contract_address = match result {
                        ExecutionResult::Success { output: Output::Create(_, address), .. } => {
                            address
                        }
                        _ => None,
                    };
                    let status = result.is_success();
                    let logs = result.logs().to_vec();
                    evm.db_mut().commit(state);
                    (status, gas_used, contract_address, logs)
                }
                Err(_) => (false, 0, None, Vec::new()),
            };

            self.tx_execution.insert(
                tx_hash,
                TxExecutionRecord {
                    status,
                    gas_used,
                    cumulative_gas_used,
                    contract_address,
                    logs: logs.clone(),
                },
            );

            let base_log_index = self.logs.len();
            for (relative_log_index, log) in logs.into_iter().enumerate() {
                self.logs.push(IndexedLog {
                    block_number: block.header.number,
                    block_hash,
                    tx_hash,
                    tx_index,
                    log_index: base_log_index + relative_log_index,
                    log,
                });
            }

            self.sync_account_caches(sender);
            if let Some(to) = tx.to() {
                self.sync_account_caches(to);
            }
            if let Some(addr) = contract_address {
                self.sync_account_caches(addr);
            }

            let next_nonce = tx.nonce() + 1;
            let entry = self.nonces.entry(sender).or_insert(0);
            *entry = (*entry).max(next_nonce);
        }

        self.block_gas_used.insert(block.header.number, cumulative_gas_used);
    }

    fn sync_account_caches(&mut self, address: Address) {
        if let Ok(Some(account)) = self.evm_db.basic(address) {
            self.balances.insert(address, account.balance);
            self.nonces
                .entry(address)
                .and_modify(|nonce| *nonce = (*nonce).max(account.nonce))
                .or_insert(account.nonce);
        }
    }

    fn execution_header_for(&self, header: &Header) -> Header {
        let mut execution_header = header.clone();
        execution_header.gas_limit = self.system_config.gas_limit;
        execution_header.base_fee_per_gas = Some(1_000_000_000);
        execution_header
    }

    /// Produce `n` L2 blocks in sequence.
    pub fn produce_l2_blocks(&mut self, n: u64) -> Vec<BaseBlock> {
        (0..n).map(|_| self.produce_l2_block()).collect()
    }

    /// Encode and push calldata for each block directly into the DAP at the current L1 tip hash.
    pub fn submit_l2_blocks(&mut self, blocks: Vec<BaseBlock>) {
        let l1_hash = self.l1_chain.with(|b| b.last().expect("L1 chain empty").hash());
        for block in blocks {
            let mut channel_id = ChannelId::default();
            channel_id[..8].copy_from_slice(&self.channel_seq.to_le_bytes());
            self.channel_seq += 1;
            self.dap_queue.push(l1_hash, block_to_calldata(&block, channel_id));
        }
    }

    /// Step the derivation pipeline until it is idle (no more data for the current L1 tip).
    ///
    /// Returns the number of L2 blocks derived (i.e., newly confirmed safe).
    pub async fn derive_until_idle(&mut self) -> usize {
        let mut count = 0;
        // Stop only when the pipeline can't advance the L1 origin (no more L1 blocks).
        // `StepFailed(Temporary)` means "not enough data yet" — we keep trying.
        // A run of consecutive temporary failures without any progress (PreparedAttributes
        // or AdvancedOrigin) means we are genuinely stuck; break then.
        let mut consecutive_fails = 0u32;
        loop {
            if consecutive_fails > 200 {
                break;
            }
            match self.pipeline.step(self.safe_head).await {
                StepResult::PreparedAttributes => {
                    consecutive_fails = 0;
                    if let Some(attrs) = self.pipeline.next() {
                        let parent = &attrs.parent;
                        let ts = attrs.attributes.payload_attributes.timestamp;
                        let block_number = parent.block_info.number + 1;

                        let txs_bytes = attrs.attributes.transactions.as_deref().unwrap_or(&[]);
                        let attrs_tx_count = txs_bytes.len();

                        let decode_results: Vec<Result<BaseTxEnvelope, _>> = txs_bytes
                            .iter()
                            .map(|b| BaseTxEnvelope::decode_2718(&mut b.as_ref()))
                            .collect();
                        let decode_failures = decode_results.iter().filter(|r| r.is_err()).count();
                        let txs: Vec<BaseTxEnvelope> =
                            decode_results.into_iter().filter_map(|r| r.ok()).collect();

                        let transactions_root =
                            alloy_consensus::proofs::calculate_transaction_root(&txs);
                        let derived_hash = Header {
                            number: block_number,
                            parent_hash: parent.block_info.hash,
                            timestamp: ts,
                            transactions_root,
                            ..Default::default()
                        }
                        .hash_slow();

                        let sequenced_hash = self
                            .l2_provider
                            .blocks
                            .lock()
                            .expect("L2 blocks lock poisoned")
                            .get(&block_number)
                            .map(|b| b.block_info.hash);

                        let matched = sequenced_hash.map(|sq| derived_hash == sq);
                        if matched == Some(true) {
                            self.verified_count += 1;
                        }

                        self.last_derive_debug.push(format!(
                            "block={block_number} attrs_txs={attrs_tx_count} \
                             decode_failures={decode_failures} \
                             derived={derived_hash:?} \
                             seq={sequenced_hash:?} \
                             matched={matched:?}",
                        ));

                        let l1_origin =
                            l1_origin_from_attrs(&attrs).unwrap_or(parent.l1_origin);
                        let l1_ts = self.l1_chain.with(|blocks| {
                            blocks.get(l1_origin.number as usize).map(|b| b.timestamp())
                        });
                        let seq_num = l1_ts
                            .map(|t| ts.saturating_sub(t) / self.rollup_config.block_time)
                            .unwrap_or(0);
                        let new_safe = L2BlockInfo {
                            block_info: BlockInfo {
                                hash: derived_hash,
                                number: block_number,
                                parent_hash: parent.block_info.hash,
                                timestamp: ts,
                            },
                            l1_origin,
                            seq_num,
                        };
                        self.safe_head = new_safe;
                        count += 1;
                    }
                }
                StepResult::AdvancedOrigin => {
                    consecutive_fails = 0;
                }
                StepResult::StepFailed(PipelineErrorKind::Temporary(_)) => {
                    consecutive_fails += 1;
                }
                StepResult::StepFailed(e) => {
                    panic!("derivation pipeline step failed: {e:?}");
                }
                // OriginAdvanceErr means no more L1 blocks are available — we are done.
                StepResult::OriginAdvanceErr(_) => break,
            }
        }
        count
    }

    /// Return per-block derivation debug info collected during the last epoch.
    pub fn get_derive_debug(&self) -> String {
        self.last_derive_debug.join("\n")
    }

    /// Mine, produce, and encode `min(l1_blocks, l2_blocks)` blocks interleaved, then derive.
    pub async fn run_epoch(&mut self, l1_blocks: u64, l2_blocks: u64) -> usize {
        let n = l1_blocks.min(l2_blocks);
        for _ in 0..n {
            self.mine_l1_blocks(1);
            let block = self.produce_l2_block();
            let l1_hash = self.l1_chain.with(|b| b.last().expect("L1 chain empty").hash());
            let mut channel_id = ChannelId::default();
            channel_id[..8].copy_from_slice(&self.channel_seq.to_le_bytes());
            self.channel_seq += 1;
            self.dap_queue.push(l1_hash, block_to_calldata(&block, channel_id));
        }
        self.derive_until_idle().await
    }

    /// Decode and enqueue a raw 2718-encoded transaction for inclusion in the next L2 block.
    pub fn queue_transaction(&mut self, tx_bytes: Vec<u8>) -> Result<(), String> {
        let tx = BaseTxEnvelope::decode_2718(&mut tx_bytes.as_slice())
            .map_err(|e| format!("invalid transaction: {e}"))?;
        self.pending_txs.push(tx);
        Ok(())
    }

    /// Create and sign a test EIP-1559 ETH transfer using the built-in devnet key (Anvil key 0).
    pub fn create_test_transfer(&mut self, to: Address, value_wei: u64) -> Vec<u8> {
        let nonce_entry = self.nonces.entry(self.dev_address).or_insert(0);
        if let Ok(Some(account)) = self.evm_db.basic(self.dev_address) {
            *nonce_entry = (*nonce_entry).max(account.nonce);
        }
        let nonce = *nonce_entry;
        *nonce_entry += 1;
        let tx = TxEip1559 {
            chain_id: self.rollup_config.l2_chain_id.id(),
            nonce,
            to: TxKind::Call(to),
            value: U256::from(value_wei),
            gas_limit: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000,
            ..Default::default()
        };
        self.sign_test_eip1559(tx)
    }

    /// Create and sign a test EIP-1559 contract creation transaction.
    pub fn create_test_contract_deploy(&mut self, init_code: Vec<u8>, value_wei: u64) -> Vec<u8> {
        let nonce_entry = self.nonces.entry(self.dev_address).or_insert(0);
        if let Ok(Some(account)) = self.evm_db.basic(self.dev_address) {
            *nonce_entry = (*nonce_entry).max(account.nonce);
        }
        let nonce = *nonce_entry;
        *nonce_entry += 1;
        let tx = TxEip1559 {
            chain_id: self.rollup_config.l2_chain_id.id(),
            nonce,
            to: TxKind::Create,
            value: U256::from(value_wei),
            gas_limit: 1_000_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000,
            input: Bytes::from(init_code),
            ..Default::default()
        };
        self.sign_test_eip1559(tx)
    }

    fn sign_test_eip1559(&self, tx: TxEip1559) -> Vec<u8> {
        let key = SigningKey::from_bytes(&DEV_KEY.into()).expect("valid devnet key");
        let sig_hash = tx.signature_hash();
        let (sig, rid) = key.sign_prehash(sig_hash.as_slice()).expect("signing failed");
        let sig_bytes: k256::ecdsa::Signature = sig;
        let rid: k256::ecdsa::RecoveryId = rid;
        let bytes = sig_bytes.to_bytes();
        let prim_sig = PrimitiveSignature::new(
            U256::from_be_slice(&bytes[..32]),
            U256::from_be_slice(&bytes[32..]),
            rid.is_y_odd(),
        );
        let signed = tx.into_signed(prim_sig);
        let env: BaseTxEnvelope = signed.into();
        let mut buf = Vec::new();
        env.encode_2718(&mut buf);
        buf
    }

    /// Return the L2 chain ID used by this devnet.
    pub fn devnet_chain_id(&self) -> u64 {
        self.rollup_config.l2_chain_id.id()
    }

    /// Return the address of the pre-funded developer account (Anvil key 0).
    pub const fn dev_account_address(&self) -> Address {
        self.dev_address
    }

    /// Return the sequencer unsafe head.
    pub fn sequencer_head(&self) -> L2BlockInfo {
        self.seq_head
    }

    /// Return the validator safe head.
    pub fn validator_safe(&self) -> L2BlockInfo {
        self.safe_head
    }

    /// Return the validator unsafe head (same as safe in CL-only mode).
    pub fn validator_unsafe(&self) -> L2BlockInfo {
        self.safe_head
    }

    /// Return the L1 tip block number.
    pub fn l1_tip_number(&self) -> u64 {
        self.l1_chain.with(|b| b.last().map(|b| b.number()).unwrap_or(0))
    }

    /// Mine, produce, and encode `n` blocks into the DAP without deriving; returns `n`.
    pub fn mine_and_encode(&mut self, n: u64) -> u64 {
        for _ in 0..n {
            self.mine_l1_blocks(1);
            let block = self.produce_l2_block();
            let l1_hash = self.l1_chain.with(|b| b.last().expect("L1 chain empty").hash());
            let mut channel_id = ChannelId::default();
            channel_id[..8].copy_from_slice(&self.channel_seq.to_le_bytes());
            self.channel_seq += 1;
            self.dap_queue.push(l1_hash, block_to_calldata(&block, channel_id));
        }
        n
    }

    /// Return how many derived blocks had their hash independently match a sequenced block.
    pub fn verified_block_count(&self) -> u64 {
        self.verified_count
    }

    fn resolve_block_number(&self, tag: Option<&serde_json::Value>) -> u64 {
        match tag.and_then(serde_json::Value::as_str) {
            Some("earliest") => 0,
            Some("safe") | Some("finalized") => self.safe_head.block_info.number,
            Some(s) if s.starts_with("0x") => {
                u64::from_str_radix(&s[2..], 16).unwrap_or(self.seq_head.block_info.number)
            }
            _ => self.seq_head.block_info.number,
        }
    }

    fn tx_json(&self, block_number: u64, tx_index: usize, tx: &BaseTxEnvelope) -> serde_json::Value {
        let block_hash = self
            .l2_provider
            .blocks
            .lock()
            .expect("L2 blocks lock poisoned")
            .get(&block_number)
            .map(|b| format!("{}", b.block_info.hash));
        let from = tx.recover_signer().map(|a| format!("{a}")).unwrap_or_default();
        let to = tx.to().map(|a| format!("{a}"));
        serde_json::json!({
            "hash": format!("{}", tx.tx_hash()),
            "nonce": format!("0x{:x}", tx.nonce()),
            "blockHash": block_hash,
            "blockNumber": format!("0x{block_number:x}"),
            "transactionIndex": format!("0x{tx_index:x}"),
            "from": from,
            "to": to,
            "value": format!("0x{:x}", tx.value()),
            "gas": format!("0x{:x}", tx.gas_limit()),
            "gasPrice": format!("0x{:x}", tx.max_fee_per_gas()),
            "input": format!("{}", tx.input()),
            "chainId": format!("0x{:x}", self.rollup_config.l2_chain_id.id()),
            "type": "0x2",
        })
    }

    fn block_json(&self, number: u64, full_tx: bool) -> Option<serde_json::Value> {
        let info = self.l2_provider.blocks.lock().expect("L2 blocks lock poisoned").get(&number).copied()?;
        let base_block =
            self.l2_provider.base_blocks.lock().expect("L2 base blocks lock poisoned").get(&number).cloned()?;
        let gas_used = self.block_gas_used.get(&number).copied().unwrap_or(0);
        let transactions: Vec<serde_json::Value> = if full_tx {
            base_block
                .body
                .transactions
                .iter()
                .enumerate()
                .map(|(i, tx)| self.tx_json(number, i, tx))
                .collect()
        } else {
            base_block
                .body
                .transactions
                .iter()
                .map(|tx| serde_json::Value::String(format!("{}", tx.tx_hash())))
                .collect()
        };
        Some(serde_json::json!({
            "number": format!("0x{number:x}"),
            "hash": format!("{}", info.block_info.hash),
            "parentHash": format!("{}", info.block_info.parent_hash),
            "timestamp": format!("0x{:x}", info.block_info.timestamp),
            "transactions": transactions,
            "gasLimit": format!("0x{:x}", self.system_config.gas_limit),
            "gasUsed": format!("0x{gas_used:x}"),
            "miner": "0x0000000000000000000000000000000000000000",
        }))
    }

    fn parse_quantity_u64(value: &serde_json::Value) -> Option<u64> {
        match value {
            serde_json::Value::String(s) if s.starts_with("0x") => {
                u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()
            }
            serde_json::Value::String(s) => s.parse::<u64>().ok(),
            serde_json::Value::Number(n) => n.as_u64(),
            _ => None,
        }
    }

    fn parse_quantity_u256(value: &serde_json::Value) -> Option<U256> {
        match value {
            serde_json::Value::String(s) if s.starts_with("0x") => {
                U256::from_str_radix(s.trim_start_matches("0x"), 16).ok()
            }
            serde_json::Value::String(s) => U256::from_str_radix(s, 10).ok(),
            serde_json::Value::Number(n) => Some(U256::from(n.as_u64()?)),
            _ => None,
        }
    }

    fn account_info(&mut self, address: Address) -> Option<AccountInfo> {
        self.evm_db.basic(address).ok().flatten()
    }

    fn account_balance(&mut self, address: Address) -> U256 {
        self.account_info(address).map(|a| a.balance).unwrap_or(U256::ZERO)
    }

    fn account_nonce(&mut self, address: Address) -> u64 {
        self.account_info(address).map(|a| a.nonce).unwrap_or(0)
    }

    fn parse_call_data(call_obj: &serde_json::Map<String, serde_json::Value>) -> Result<Vec<u8>, (i64, String)> {
        let maybe_data = call_obj
            .get("input")
            .or_else(|| call_obj.get("data"))
            .and_then(serde_json::Value::as_str);

        match maybe_data {
            Some(data) => alloy_primitives::hex::decode(data.trim_start_matches("0x"))
                .map_err(|e| (-32602, format!("invalid input/data hex: {e}"))),
            None => Ok(Vec::new()),
        }
    }

    fn topic_filter_matches(log: &Log, topics_filter: Option<&Vec<serde_json::Value>>) -> bool {
        let Some(filters) = topics_filter else {
            return true;
        };
        let topics = log.data.topics();
        for (index, filter) in filters.iter().enumerate() {
            match filter {
                serde_json::Value::Null => continue,
                serde_json::Value::String(topic) => {
                    let matches = topics
                        .get(index)
                        .map(|actual| actual.to_string().eq_ignore_ascii_case(topic))
                        .unwrap_or(false);
                    if !matches {
                        return false;
                    }
                }
                serde_json::Value::Array(options) => {
                    let Some(actual) = topics.get(index) else {
                        return false;
                    };
                    let any_match = options.iter().any(|candidate| {
                        candidate
                            .as_str()
                            .map(|s| actual.to_string().eq_ignore_ascii_case(s))
                            .unwrap_or(false)
                    });
                    if !any_match {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        true
    }

    fn logs_for_receipt(
        &self,
        tx_hash: B256,
        block_number: u64,
        block_hash: B256,
        tx_index: usize,
        logs: &[Log],
    ) -> Vec<serde_json::Value> {
        logs.iter()
            .enumerate()
            .map(|(i, log)| {
                serde_json::json!({
                    "address": format!("{}", log.address),
                    "topics": log.data.topics().iter().map(|topic| format!("{topic}")).collect::<Vec<_>>(),
                    "data": format!("{}", log.data.data),
                    "blockNumber": format!("0x{block_number:x}"),
                    "transactionHash": format!("{tx_hash}"),
                    "transactionIndex": format!("0x{tx_index:x}"),
                    "blockHash": format!("{block_hash}"),
                    "logIndex": format!("0x{i:x}"),
                    "removed": false,
                })
            })
            .collect()
    }

    /// Handle one JSON-RPC 2.0 request and return the serialized response.
    ///
    /// Implements a minimal Ethereum JSON-RPC surface (`eth_*`/`net_*`/`web3_*`) backed by the
    /// devnet's sequencer state and an in-memory EVM database.
    pub fn rpc_request(&mut self, request_json: &str) -> String {
        let req: serde_json::Value = match serde_json::from_str(request_json) {
            Ok(v) => v,
            Err(e) => {
                return serde_json::json!({
                    "jsonrpc": "2.0", "id": serde_json::Value::Null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")}
                })
                .to_string();
            }
        };
        let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let method = req.get("method").and_then(serde_json::Value::as_str).unwrap_or("");
        let empty = Vec::new();
        let params = req.get("params").and_then(serde_json::Value::as_array).unwrap_or(&empty);

        let result: Result<serde_json::Value, (i64, String)> = match method {
            "eth_chainId" => Ok(serde_json::json!(format!(
                "0x{:x}",
                self.rollup_config.l2_chain_id.id()
            ))),
            "net_version" => Ok(serde_json::json!(self.rollup_config.l2_chain_id.id().to_string())),
            "web3_clientVersion" => Ok(serde_json::json!("base-wasm-devnet/0.1")),
            "eth_syncing" => Ok(serde_json::json!(false)),
            "eth_accounts" => Ok(serde_json::json!([format!("{}", self.dev_address)])),
            "eth_blockNumber" => {
                Ok(serde_json::json!(format!("0x{:x}", self.seq_head.block_info.number)))
            }
            "eth_gasPrice" | "eth_maxPriorityFeePerGas" | "eth_maxFeePerGas" => {
                Ok(serde_json::json!("0x3b9aca00"))
            }
            "eth_estimateGas" => Ok(serde_json::json!("0x5208")),
            "eth_getBalance" => {
                let addr = params
                    .first()
                    .and_then(serde_json::Value::as_str)
                    .and_then(|s| s.parse::<Address>().ok());
                match addr {
                    Some(a) => {
                        let bal = self.account_balance(a);
                        Ok(serde_json::json!(format!("0x{bal:x}")))
                    }
                    None => Err((-32602, "invalid address".to_string())),
                }
            }
            "eth_getTransactionCount" => {
                let addr = params
                    .first()
                    .and_then(serde_json::Value::as_str)
                    .and_then(|s| s.parse::<Address>().ok());
                match addr {
                    Some(a) => {
                        let nonce = self.account_nonce(a);
                        Ok(serde_json::json!(format!("0x{nonce:x}")))
                    }
                    None => Err((-32602, "invalid address".to_string())),
                }
            }
            "eth_getBlockByNumber" => {
                let number = self.resolve_block_number(params.first());
                let full_tx = params.get(1).and_then(serde_json::Value::as_bool).unwrap_or(false);
                Ok(self.block_json(number, full_tx).unwrap_or(serde_json::Value::Null))
            }
            "eth_getBlockByHash" => {
                let hash = params.first().and_then(serde_json::Value::as_str).and_then(|s| s.parse::<B256>().ok());
                let full_tx = params.get(1).and_then(serde_json::Value::as_bool).unwrap_or(false);
                let number = hash.and_then(|h| {
                    self.l2_provider
                        .blocks
                        .lock()
                        .expect("L2 blocks lock poisoned")
                        .values()
                        .find(|b| b.block_info.hash == h)
                        .map(|b| b.block_info.number)
                });
                Ok(number
                    .and_then(|n| self.block_json(n, full_tx))
                    .unwrap_or(serde_json::Value::Null))
            }
            "eth_getTransactionByHash" => {
                let hash = params.first().and_then(serde_json::Value::as_str).and_then(|s| s.parse::<B256>().ok());
                let located = hash.and_then(|h| self.tx_locations.get(&h).copied());
                let tx_json = located.and_then(|(block_number, tx_index)| {
                    let base_block = self
                        .l2_provider
                        .base_blocks
                        .lock()
                        .expect("L2 base blocks lock poisoned")
                        .get(&block_number)
                        .cloned()?;
                    let tx = base_block.body.transactions.get(tx_index)?.clone();
                    Some(self.tx_json(block_number, tx_index, &tx))
                });
                Ok(tx_json.unwrap_or(serde_json::Value::Null))
            }
            "eth_getTransactionReceipt" => {
                let hash = params.first().and_then(serde_json::Value::as_str).and_then(|s| s.parse::<B256>().ok());
                let located = hash.and_then(|h| self.tx_locations.get(&h).copied());
                let receipt = located.and_then(|(block_number, tx_index)| {
                    let tx_hash = hash.expect("hash present when located is Some");
                    let tx_exec = self.tx_execution.get(&tx_hash)?;
                    let block_hash = self
                        .l2_provider
                        .blocks
                        .lock()
                        .expect("L2 blocks lock poisoned")
                        .get(&block_number)
                        .map(|b| b.block_info.hash)
                        .unwrap_or_default();
                    Some(serde_json::json!({
                        "transactionHash": format!("{tx_hash}"),
                        "blockNumber": format!("0x{block_number:x}"),
                        "blockHash": format!("{block_hash}"),
                        "transactionIndex": format!("0x{tx_index:x}"),
                        "status": if tx_exec.status { "0x1" } else { "0x0" },
                        "gasUsed": format!("0x{:x}", tx_exec.gas_used),
                        "cumulativeGasUsed": format!("0x{:x}", tx_exec.cumulative_gas_used),
                        "contractAddress": tx_exec.contract_address.map(|address| format!("{address}")),
                        "logs": self.logs_for_receipt(tx_hash, block_number, block_hash, tx_index, &tx_exec.logs),
                        "logsBloom": format!("0x{}", "0".repeat(512)),
                    }))
                });
                Ok(receipt.unwrap_or(serde_json::Value::Null))
            }
            "eth_call" => {
                (|| {
                    let call_obj = params
                        .first()
                        .and_then(serde_json::Value::as_object)
                        .ok_or_else(|| (-32602, "invalid call object".to_string()))?;
                    let _block_number = self.resolve_block_number(params.get(1));
                    let to = match call_obj.get("to") {
                        Some(serde_json::Value::String(raw_to)) => Some(
                            raw_to
                                .parse::<Address>()
                                .map_err(|_| (-32602, "invalid to address".to_string()))?,
                        ),
                        Some(serde_json::Value::Null) | None => None,
                        Some(_) => return Err((-32602, "invalid to address".to_string())),
                    };
                    let from = call_obj
                        .get("from")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|s| s.parse::<Address>().ok())
                        .unwrap_or(self.dev_address);
                    let value = call_obj
                        .get("value")
                        .and_then(Self::parse_quantity_u256)
                        .unwrap_or(U256::ZERO);
                    let gas_limit = call_obj
                        .get("gas")
                        .and_then(Self::parse_quantity_u64)
                        .unwrap_or(30_000_000)
                        .min(16_777_216);
                    let input = Self::parse_call_data(call_obj)?;
                    let nonce = self.account_nonce(from);

                    let call_tx = TxEip1559 {
                        chain_id: self.rollup_config.l2_chain_id.id(),
                        nonce,
                        to: to.map_or(TxKind::Create, TxKind::Call),
                        value,
                        gas_limit,
                        max_fee_per_gas: 1_000_000_000,
                        max_priority_fee_per_gas: 1_000_000,
                        input: Bytes::from(input),
                        ..Default::default()
                    };

                    let envelope = self.sign_test_eip1559(call_tx);
                    let tx = BaseTxEnvelope::decode_2718(&mut envelope.as_slice())
                        .map_err(|e| (-32602, format!("failed to decode call tx: {e}")))?;
                    let recovered = Recovered::new_unchecked(tx, from);

                    let evm_env = self
                        .evm_config
                        .evm_env(&self.execution_header_for(&self.l2_head.header))
                        .map_err(|e| (-32000, format!("failed to build call env: {e}")))?;
                    let mut evm = self.evm_config.evm_with_env(&mut self.evm_db, evm_env);
                    let ResultAndState { result, .. } = evm
                        .transact(&recovered)
                        .map_err(|e| (-32000, format!("eth_call execution failed: {e}")))?;

                    if matches!(result, ExecutionResult::Halt { .. }) {
                        Err((-32000, "eth_call halted".to_string()))
                    } else {
                        let output = result.output().cloned().unwrap_or_default();
                        Ok(serde_json::json!(format!("{output}")))
                    }
                })()
            }
            "eth_getCode" => {
                (|| {
                    let addr = params
                        .first()
                        .and_then(serde_json::Value::as_str)
                        .and_then(|s| s.parse::<Address>().ok())
                        .ok_or_else(|| (-32602, "invalid address".to_string()))?;
                    let code = self
                        .account_info(addr)
                        .and_then(|account| account.code)
                        .map(|bytecode| bytecode.bytes().to_vec())
                        .unwrap_or_default();
                    Ok(serde_json::json!(format!("0x{}", alloy_primitives::hex::encode(code))))
                })()
            }
            "eth_getStorageAt" => {
                (|| {
                    let addr = params
                        .first()
                        .and_then(serde_json::Value::as_str)
                        .and_then(|s| s.parse::<Address>().ok())
                        .ok_or_else(|| (-32602, "invalid address".to_string()))?;
                    let slot = params
                        .get(1)
                        .and_then(Self::parse_quantity_u256)
                        .ok_or_else(|| (-32602, "invalid slot".to_string()))?;
                    let value = self
                        .evm_db
                        .storage(addr, slot)
                        .map_err(|e| (-32000, format!("failed to read storage: {e}")))?;
                    Ok(serde_json::json!(format!("0x{value:064x}")))
                })()
            }
            "eth_getLogs" => {
                let filter = params.first().and_then(serde_json::Value::as_object);
                let from_block = filter
                    .and_then(|f| f.get("fromBlock"))
                    .map(|v| self.resolve_block_number(Some(v)))
                    .unwrap_or(0);
                let to_block = filter
                    .and_then(|f| f.get("toBlock"))
                    .map(|v| self.resolve_block_number(Some(v)))
                    .unwrap_or(self.resolve_block_number(None));
                let address_filter = filter.and_then(|f| f.get("address"));
                let topics_filter = filter
                    .and_then(|f| f.get("topics"))
                    .and_then(serde_json::Value::as_array);

                let logs = self
                    .logs
                    .iter()
                    .filter(|entry| entry.block_number >= from_block && entry.block_number <= to_block)
                    .filter(|entry| {
                        let Some(address_filter) = address_filter else {
                            return true;
                        };
                        match address_filter {
                            serde_json::Value::String(addr) => {
                                entry.log.address.to_string().eq_ignore_ascii_case(addr)
                            }
                            serde_json::Value::Array(addrs) => addrs.iter().any(|candidate| {
                                candidate
                                    .as_str()
                                    .map(|addr| entry.log.address.to_string().eq_ignore_ascii_case(addr))
                                    .unwrap_or(false)
                            }),
                            _ => false,
                        }
                    })
                    .filter(|entry| Self::topic_filter_matches(&entry.log, topics_filter))
                    .map(|entry| {
                        serde_json::json!({
                            "address": format!("{}", entry.log.address),
                            "topics": entry.log.data.topics().iter().map(|topic| format!("{topic}")).collect::<Vec<_>>(),
                            "data": format!("{}", entry.log.data.data),
                            "blockNumber": format!("0x{:x}", entry.block_number),
                            "transactionHash": format!("{}", entry.tx_hash),
                            "transactionIndex": format!("0x{:x}", entry.tx_index),
                            "blockHash": format!("{}", entry.block_hash),
                            "logIndex": format!("0x{:x}", entry.log_index),
                            "removed": false,
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(serde_json::json!(logs))
            }
            "eth_sendRawTransaction" => {
                let raw = params.first().and_then(serde_json::Value::as_str);
                match raw.and_then(|s| alloy_primitives::hex::decode(s.trim_start_matches("0x")).ok()) {
                    Some(bytes) => match BaseTxEnvelope::decode_2718(&mut bytes.as_slice()) {
                        Ok(tx) => {
                            let hash = format!("{}", tx.tx_hash());
                            self.pending_txs.push(tx);
                            Ok(serde_json::json!(hash))
                        }
                        Err(e) => Err((-32602, format!("invalid transaction: {e}"))),
                    },
                    None => Err((-32602, "invalid raw transaction hex".to_string())),
                }
            }
            other => Err((-32601, format!("method not found: {other}"))),
        };

        match result {
            Ok(value) => {
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": value}).to_string()
            }
            Err((code, message)) => serde_json::json!({
                "jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}
            })
            .to_string(),
        }
    }
}
