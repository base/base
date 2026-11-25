//! Hermetic fixtures for archive tests: a signed OP chain across the Canyon fork, a node that
//! serves it, a store wrapper that injects write faults, and an in-process simulator.
//!
//! [`FixtureNode`] is hand-rolled rather than a `mockall` mock because it serves one
//! internally consistent chain (blocks, changesets and replayed state) across every
//! [`BlockSource`] and [`NodeState`] method. [`FaultyStore`] is hand-rolled because a lost
//! acknowledgement must apply the write to the real store and then fail, which an expectation
//! cannot express.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use alloy_consensus::{
    Eip658Value, Header, Receipt, SignableTransaction, TxEip1559, TxReceipt,
    constants::{EMPTY_ROOT_HASH, KECCAK_EMPTY},
    proofs::calculate_transaction_root,
};
use alloy_eips::eip4895::Withdrawals;
use alloy_primitives::{Address, B256, Bloom, Bytes as AlloyBytes, Log, Sealable, TxKind, U256, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use async_trait::async_trait;
use base_common_consensus::{BaseReceipt, BaseTxEnvelope, DepositReceipt, TxDeposit};
use base_common_genesis::BaseUpgrade;
use base_execution_chainspec::BaseChainSpec;
use base_execution_consensus::calculate_receipt_root_no_memo;
use bytes::Bytes;
use exoware_sdk::{RangeMode, StoreClient, prune_policy::PrunePolicy};

use crate::{
    AccountChange, AccountInfo, ArchiveConfig, ArchiveError, ArchiveKey, ArchiveStore, BlockBody,
    BlockData, BlockSource, BlockVerifier, ExowareArchive, ExowareStore, LocalAvailability,
    NodeState, PublisherOptions, StorageChange, StoreSession,
};

/// Timestamp of the fixture genesis block.
pub const GENESIS_TIMESTAMP: u64 = 1_700_000_000;
/// Seconds between fixture blocks.
pub const BLOCK_TIME: u64 = 2;
/// Blocks per segment used by most tests.
pub const TEST_N: u64 = 8;
/// Blocks at which the sparse account and sparse slot change.
pub const SPARSE_BLOCKS: [u64; 4] = [8, 12, 31, 32];
/// Slot of `contracts[1]` that changes only at [`SPARSE_BLOCKS`].
pub const SPARSE_SLOT: B256 = B256::with_last_byte(0x55);
/// Fixture contract bytecode.
pub const CONTRACT_CODE: [u8; 4] = [0x60, 0x00, 0x60, 0x00];
/// Per-test network budget.
pub const TEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Accounts and storage after a block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FixtureState {
    /// Existing accounts.
    pub accounts: BTreeMap<Address, AccountInfo>,
    /// Storage slots that were ever written.
    pub storage: BTreeMap<(Address, B256), U256>,
}

/// A signed OP chain with deposits, transfers, logs and storage writes. Block timestamps
/// straddle the Canyon activation, so blocks before it use the Regolith receipt-root rule
/// (deposit nonce stripped) and blocks after it the Canyon rule.
#[derive(Debug, Clone)]
pub struct ChainFixture {
    /// Blocks `0..len`.
    pub blocks: Vec<BlockData>,
    /// State after each block.
    pub states: Vec<FixtureState>,
    /// Chain spec with Canyon at block [`Self::canyon_block`].
    pub chain_spec: Arc<BaseChainSpec>,
    /// First block with Canyon active.
    pub canyon_block: u64,
    /// Transaction signers.
    pub senders: Vec<Address>,
    /// Deposit sender.
    pub depositor: Address,
    /// Recipient of every transfer.
    pub recipient: Address,
    /// Account that changes only at [`SPARSE_BLOCKS`].
    pub sparse: Address,
    /// Log-emitting contracts.
    pub contracts: [Address; 3],
    /// Event signatures used as topic 0.
    pub events: [B256; 2],
}

impl ChainFixture {
    /// Builds `len` blocks with Canyon activating at `canyon_block`.
    pub fn new(len: u64, canyon_block: u64) -> Self {
        let mut spec = BaseChainSpec::sepolia();
        assert!(spec.set_hardfork_activation_timestamp(BaseUpgrade::Regolith, 0), "regolith set");
        assert!(
            spec.set_hardfork_activation_timestamp(
                BaseUpgrade::Canyon,
                GENESIS_TIMESTAMP + BLOCK_TIME * canyon_block
            ),
            "canyon set"
        );
        let chain_spec = Arc::new(spec);
        let signers: Vec<PrivateKeySigner> = (1u8..=3)
            .map(|i| PrivateKeySigner::from_bytes(&B256::with_last_byte(i)).expect("valid key"))
            .collect();
        let senders: Vec<Address> = signers.iter().map(PrivateKeySigner::address).collect();
        let depositor = Address::with_last_byte(0xde);
        let recipient = Address::with_last_byte(0xee);
        let sparse = Address::with_last_byte(0x5a);
        let contracts = [Address::with_last_byte(0xc0), Address::with_last_byte(0xc1), Address::with_last_byte(0xc2)];
        let events = [keccak256("Transfer"), keccak256("Approval")];
        let code_hash = keccak256(CONTRACT_CODE);

        let mut state = FixtureState::default();
        for sender in &senders {
            state.accounts.insert(*sender, AccountInfo { nonce: 0, balance: U256::from(10u64.pow(18)), code_hash: KECCAK_EMPTY });
        }
        state.accounts.insert(depositor, AccountInfo { nonce: 0, balance: U256::ZERO, code_hash: KECCAK_EMPTY });
        for contract in contracts {
            state.accounts.insert(contract, AccountInfo { nonce: 1, balance: U256::ZERO, code_hash });
        }
        let genesis = Header {
            number: 0,
            timestamp: GENESIS_TIMESTAMP,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1),
            ..Default::default()
        };
        let mut blocks = vec![BlockData {
            header: genesis,
            body: BlockBody::default(),
            senders: vec![],
            receipts: vec![],
            account_changes: vec![],
            storage_changes: vec![],
        }];
        let mut states = vec![state.clone()];

        for number in 1..len {
            let before = state.clone();
            let timestamp = GENESIS_TIMESTAMP + BLOCK_TIME * number;
            let canyon = number >= canyon_block;
            let mut transactions = Vec::new();
            let mut tx_senders = Vec::new();
            let mut receipts = Vec::new();
            let mut gas = 0;

            let deposit = TxDeposit {
                source_hash: keccak256(number.to_be_bytes()),
                from: depositor,
                to: TxKind::Call(Address::with_last_byte(0x15)),
                mint: 0,
                value: U256::ZERO,
                gas_limit: 1_000_000,
                is_system_transaction: false,
                input: AlloyBytes::from(number.to_be_bytes().to_vec()),
            };
            transactions.push(BaseTxEnvelope::Deposit(deposit.seal_slow()));
            tx_senders.push(depositor);
            gas += 50_000;
            let depositor_account = state.accounts.get_mut(&depositor).expect("depositor");
            receipts.push(BaseReceipt::Deposit(DepositReceipt {
                inner: Receipt { status: Eip658Value::Eip658(true), cumulative_gas_used: gas, logs: vec![] },
                deposit_nonce: Some(depositor_account.nonce),
                deposit_receipt_version: canyon.then_some(1),
            }));
            depositor_account.nonce += 1;

            let mut transfers: Vec<(usize, Address, U256, Vec<Log>)> = Vec::new();
            for j in 1..=2u64 {
                let (address, topic0) = if number % 7 == 5 {
                    if j == 1 { (contracts[0], events[1]) } else { (contracts[1], events[0]) }
                } else {
                    (contracts[((number + j) % 3) as usize], events[((number * j) % 2) as usize])
                };
                let log = Log::new_unchecked(
                    address,
                    vec![topic0, B256::from(U256::from(number))],
                    AlloyBytes::from(number.to_be_bytes().to_vec()),
                );
                transfers.push((((number + j) % 3) as usize, recipient, U256::from(1_000 + number), vec![log]));
            }
            if SPARSE_BLOCKS.contains(&number) {
                transfers.push(((number % 3) as usize, sparse, U256::from(7), vec![]));
            }
            for (idx, to, value, logs) in transfers {
                let account = state.accounts.get_mut(&senders[idx]).expect("sender");
                let tx = TxEip1559 {
                    chain_id: chain_spec.chain.id(),
                    nonce: account.nonce,
                    gas_limit: 21_000,
                    max_fee_per_gas: 1,
                    max_priority_fee_per_gas: 0,
                    to: TxKind::Call(to),
                    value,
                    ..Default::default()
                };
                let signature = signers[idx].sign_hash_sync(&tx.signature_hash()).expect("signs");
                transactions.push(BaseTxEnvelope::Eip1559(tx.into_signed(signature)));
                tx_senders.push(senders[idx]);
                account.nonce += 1;
                account.balance -= value;
                state.accounts.entry(to).or_insert(AccountInfo { nonce: 0, balance: U256::ZERO, code_hash: KECCAK_EMPTY }).balance += value;
                gas += 21_000;
                receipts.push(BaseReceipt::Eip1559(Receipt {
                    status: Eip658Value::Eip658(true),
                    cumulative_gas_used: gas,
                    logs,
                }));
            }
            state.storage.insert((contracts[0], B256::from(U256::from(number % 3))), U256::from(number));
            if SPARSE_BLOCKS.contains(&number) {
                state.storage.insert((contracts[1], SPARSE_SLOT), U256::from(number));
            }

            let account_changes = state
                .accounts
                .iter()
                .filter(|(address, info)| before.accounts.get(*address) != Some(*info))
                .map(|(address, _)| AccountChange { address: *address, before: before.accounts.get(address).copied() })
                .collect();
            let storage_changes = state
                .storage
                .iter()
                .filter(|(key, value)| before.storage.get(*key) != Some(*value))
                .map(|((address, slot), _)| StorageChange {
                    address: *address,
                    slot: *slot,
                    before: before.storage.get(&(*address, *slot)).copied().unwrap_or_default(),
                })
                .collect();
            let body = BlockBody { transactions, withdrawals: canyon.then(Withdrawals::default) };
            let parent = blocks.last().expect("parent");
            let header = Header {
                parent_hash: parent.hash(),
                number,
                timestamp,
                gas_limit: 30_000_000,
                gas_used: gas,
                base_fee_per_gas: Some(1),
                state_root: keccak256(number.to_be_bytes()),
                transactions_root: calculate_transaction_root(&body.transactions),
                receipts_root: calculate_receipt_root_no_memo(&receipts, chain_spec.as_ref(), timestamp),
                logs_bloom: receipts.iter().fold(Bloom::ZERO, |bloom, r| bloom | r.bloom()),
                withdrawals_root: canyon.then_some(EMPTY_ROOT_HASH),
                ..Default::default()
            };
            blocks.push(BlockData {
                header,
                body,
                senders: tx_senders,
                receipts,
                account_changes,
                storage_changes,
            });
            states.push(state.clone());
        }
        Self { blocks, states, chain_spec, canyon_block, senders, depositor, recipient, sparse, contracts, events }
    }

    /// The standard fixture: 41 blocks (five full segments of [`TEST_N`] plus one block),
    /// Canyon at block 13, inside segment 1.
    pub fn standard() -> Self {
        Self::new(5 * TEST_N + 1, 13)
    }

    /// Archive config for this chain.
    pub fn config(&self, blocks_per_segment: u64, start_segment: u64, retain: Option<u64>) -> ArchiveConfig {
        ArchiveConfig::new(self.chain_spec.chain.id(), self.blocks[0].hash(), blocks_per_segment, start_segment, retain)
            .expect("valid config")
    }

    /// Verifier for this chain.
    pub fn verifier(&self) -> BlockVerifier {
        BlockVerifier::new(Arc::clone(&self.chain_spec))
    }

    /// The same query as an alloy [`alloy_rpc_types_eth::Filter`], so the oracle does not
    /// reuse the matcher under test.
    pub fn alloy_filter(filter: &crate::LogFilter) -> alloy_rpc_types_eth::Filter {
        let [t0, t1, t2, t3] = filter.topics.clone();
        alloy_rpc_types_eth::Filter::new()
            .from_block(filter.from)
            .to_block(filter.to)
            .address(filter.addresses.clone())
            .event_signature(t0)
            .topic1(t1)
            .topic2(t2)
            .topic3(t3)
    }

    /// Every log of blocks `[from, to]` matching `filter`, by brute force with alloy's matcher.
    pub fn expected_logs(&self, filter: &crate::LogFilter) -> Vec<(u64, u64, Log)> {
        let alloy = Self::alloy_filter(filter);
        let mut out = Vec::new();
        for block in &self.blocks[filter.from as usize..=filter.to as usize] {
            let mut log_index = 0;
            for receipt in &block.receipts {
                for log in &receipt.as_receipt().logs {
                    if alloy.matches(log) {
                        out.push((block.number(), log_index, log.clone()));
                    }
                    log_index += 1;
                }
            }
        }
        out
    }
}

/// A node serving a [`ChainFixture`].
#[derive(Debug, Clone)]
pub struct FixtureNode {
    /// Served chain.
    pub fixture: Arc<ChainFixture>,
    /// Reported finalized block.
    pub finalized: u64,
    /// Reported availability.
    pub availability: LocalAvailability,
    /// Lowest block whose canonical hash the node still knows.
    pub headers_from: u64,
    /// Whether the sender table is empty (senders must be recovered).
    pub senders_pruned: bool,
}

impl FixtureNode {
    /// A node with every block finalized and nothing pruned.
    pub fn new(fixture: Arc<ChainFixture>) -> Self {
        let finalized = fixture.blocks.len() as u64 - 1;
        Self { fixture, finalized, availability: LocalAvailability::default(), headers_from: 0, senders_pruned: false }
    }

    /// Block `number`, if served.
    pub fn block(&self, number: u64) -> Option<&BlockData> {
        self.fixture.blocks.get(number as usize)
    }
}

impl BlockSource for FixtureNode {
    fn finalized_block_number(&self) -> Result<Option<u64>, ArchiveError> {
        Ok(Some(self.finalized))
    }

    fn availability(&self) -> Result<LocalAvailability, ArchiveError> {
        Ok(self.availability)
    }

    fn header(&self, number: u64) -> Result<Option<Header>, ArchiveError> {
        Ok(self.block(number).map(|b| b.header.clone()))
    }

    fn body(&self, number: u64) -> Result<Option<BlockBody>, ArchiveError> {
        Ok(self.block(number).map(|b| b.body.clone()))
    }

    fn receipts(&self, number: u64) -> Result<Option<Vec<BaseReceipt>>, ArchiveError> {
        Ok(self.block(number).map(|b| b.receipts.clone()))
    }

    fn senders(&self, number: u64) -> Result<Option<Vec<Address>>, ArchiveError> {
        if self.senders_pruned {
            return Ok(None);
        }
        Ok(self.block(number).map(|b| b.senders.clone()))
    }

    fn account_changes(&self, number: u64) -> Result<Vec<AccountChange>, ArchiveError> {
        Ok(self.block(number).map(|b| b.account_changes.clone()).unwrap_or_default())
    }

    fn storage_changes(&self, number: u64) -> Result<Vec<StorageChange>, ArchiveError> {
        Ok(self.block(number).map(|b| b.storage_changes.clone()).unwrap_or_default())
    }
}

#[async_trait]
impl NodeState for FixtureNode {
    async fn canonical_hash(&self, number: u64) -> Result<Option<B256>, ArchiveError> {
        if number < self.headers_from {
            return Ok(None);
        }
        Ok(self.block(number).map(BlockData::hash))
    }

    async fn account_at(&self, block: u64, address: Address) -> Result<Option<AccountInfo>, ArchiveError> {
        Ok(self.fixture.states[block as usize].accounts.get(&address).copied())
    }

    async fn storage_at(&self, block: u64, address: Address, slot: B256) -> Result<U256, ArchiveError> {
        Ok(self.fixture.states[block as usize].storage.get(&(address, slot)).copied().unwrap_or_default())
    }

    async fn bytecode(&self, code_hash: B256) -> Result<Option<AlloyBytes>, ArchiveError> {
        Ok((code_hash == keccak256(CONTRACT_CODE)).then(|| AlloyBytes::from_static(&CONTRACT_CODE)))
    }
}

/// When a [`PutFault`] fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutMatch {
    /// Every put from the `n`-th on (0-based), while the fault has firings left.
    AtOrAfter(usize),
    /// Any put that writes a segment marker.
    Marker,
}

/// What a [`PutFault`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    /// Fail without writing.
    Fail,
    /// Write, then report a failure (lost acknowledgement).
    LostAck,
}

/// One scripted put fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PutFault {
    /// When it fires.
    pub when: PutMatch,
    /// What it does.
    pub kind: FaultKind,
    /// How many more times it fires.
    pub times: usize,
}

/// A store that injects scripted put faults and read failures in front of a real store, and
/// can hold its first put at a barrier shared with another store.
#[derive(Debug)]
pub struct FaultyStore<S> {
    /// Real store.
    pub inner: Arc<S>,
    /// Scripted faults.
    pub faults: Mutex<Vec<PutFault>>,
    /// Puts seen, including faulted ones.
    pub puts: Mutex<usize>,
    /// Remaining reads (through any session) that fail.
    pub failing_reads: Arc<AtomicUsize>,
    /// Barrier the first put waits on.
    pub first_put_barrier: Option<Arc<tokio::sync::Barrier>>,
    /// Whether the first put has passed the barrier.
    pub barrier_passed: AtomicBool,
}

impl<S> FaultyStore<S> {
    /// Wraps `inner` with `faults`.
    pub fn new(inner: Arc<S>, faults: Vec<PutFault>) -> Self {
        Self {
            inner,
            faults: Mutex::new(faults),
            puts: Mutex::new(0),
            failing_reads: Arc::new(AtomicUsize::new(0)),
            first_put_barrier: None,
            barrier_passed: AtomicBool::new(false),
        }
    }

    /// Holds the first put until every party of `barrier` arrived.
    pub fn with_first_put_barrier(mut self, barrier: Arc<tokio::sync::Barrier>) -> Self {
        self.first_put_barrier = Some(barrier);
        self
    }

    /// Makes the next `reads` reads fail.
    pub fn fail_reads(&self, reads: usize) {
        self.failing_reads.store(reads, Ordering::SeqCst);
    }

    /// Puts seen so far.
    pub fn put_count(&self) -> usize {
        *self.puts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Session of a [`FaultyStore`]: fails reads while the store has failing reads left.
#[derive(Debug)]
pub struct FaultySession<Sess> {
    /// Real session.
    pub inner: Sess,
    /// Shared failing-read budget.
    pub failing_reads: Arc<AtomicUsize>,
}

impl<Sess> FaultySession<Sess> {
    /// Consumes one failing read, if any is left.
    pub fn fail(&self, op: &'static str) -> Result<(), ArchiveError> {
        let failed = self
            .failing_reads
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1))
            .is_ok();
        if failed { Err(ArchiveError::store(op, "injected read failure")) } else { Ok(()) }
    }
}

#[async_trait]
impl<Sess: StoreSession> StoreSession for FaultySession<Sess> {
    async fn get(&self, key: &Bytes) -> Result<Option<Bytes>, ArchiveError> {
        self.fail("get")?;
        self.inner.get(key).await
    }

    async fn get_many(&self, keys: &[Bytes]) -> Result<Vec<Option<Bytes>>, ArchiveError> {
        self.fail("get_many")?;
        self.inner.get_many(keys).await
    }

    async fn range(
        &self,
        start: &Bytes,
        end: &Bytes,
        limit: usize,
        mode: RangeMode,
    ) -> Result<Vec<(Bytes, Bytes)>, ArchiveError> {
        self.fail("range")?;
        self.inner.range(start, end, limit, mode).await
    }
}

#[async_trait]
impl<S: ArchiveStore> ArchiveStore for FaultyStore<S> {
    type Session = FaultySession<S::Session>;

    fn session(&self) -> Self::Session {
        FaultySession { inner: self.inner.session(), failing_reads: Arc::clone(&self.failing_reads) }
    }

    async fn put(&self, rows: &[(Bytes, Bytes)]) -> Result<u64, ArchiveError> {
        if let Some(barrier) = &self.first_put_barrier
            && !self.barrier_passed.swap(true, Ordering::SeqCst)
        {
            barrier.wait().await;
        }
        let index = {
            let mut puts = self.puts.lock().unwrap_or_else(PoisonError::into_inner);
            *puts += 1;
            *puts - 1
        };
        let has_marker = rows.iter().any(|(k, _)| ArchiveKey::family(k) == Some(crate::Family::Marker));
        let fired = {
            let mut faults = self.faults.lock().unwrap_or_else(PoisonError::into_inner);
            faults
                .iter_mut()
                .find(|f| {
                    f.times > 0
                        && match f.when {
                            PutMatch::AtOrAfter(n) => index >= n,
                            PutMatch::Marker => has_marker,
                        }
                })
                .map(|f| {
                    f.times -= 1;
                    f.kind
                })
        };
        match fired {
            None => self.inner.put(rows).await,
            Some(FaultKind::Fail) => Err(ArchiveError::store("put", "injected failure")),
            Some(FaultKind::LostAck) => {
                self.inner.put(rows).await?;
                Err(ArchiveError::store("put", "injected lost acknowledgement"))
            }
        }
    }

    async fn prune(&self, policies: &[PrunePolicy]) -> Result<(), ArchiveError> {
        self.inner.prune(policies).await
    }

    fn key_prefix_len(&self) -> usize {
        self.inner.key_prefix_len()
    }
}

/// An in-process simulator and helpers to open namespaces on it.
#[derive(Debug)]
pub struct TestSimulator {
    /// Server task.
    pub task: tokio::task::JoinHandle<()>,
    /// Base URL.
    pub url: String,
}

impl TestSimulator {
    /// Starts a simulator on an ephemeral port with a temporary RocksDB.
    pub async fn start() -> Self {
        let (task, url) = exoware_simulator::open_temp().await.expect("simulator starts");
        Self { task, url }
    }

    /// Opens `namespace`.
    pub fn store(&self, namespace: &str) -> Arc<ExowareStore> {
        Arc::new(ExowareStore::new(StoreClient::new(&self.url), namespace, Duration::from_secs(10)).expect("store"))
    }

    /// Opens `namespace` and provisions `config` in it.
    pub async fn archive(&self, namespace: &str, config: &ArchiveConfig) -> ExowareArchive<ExowareStore> {
        let archive = ExowareArchive::new(self.store(namespace));
        archive.provision(config).await.expect("provision");
        archive
    }
}

impl Drop for TestSimulator {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Every logical row in a store namespace, in key order.
pub async fn dump<S: ArchiveStore>(store: &S) -> Vec<(Bytes, Bytes)> {
    store
        .session()
        .range(&Bytes::new(), &Bytes::new(), 1_000_000, RangeMode::Forward)
        .await
        .expect("dump")
}

/// Publisher options with fast retries and small batches.
pub fn fast_options() -> PublisherOptions {
    PublisherOptions {
        initial_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(5),
        idle_interval: Duration::from_millis(5),
        reconcile_interval: Duration::from_millis(5),
        ..PublisherOptions::default()
    }
}

/// Opens a publisher for `node` with [`fast_options`].
pub async fn publisher<S: ArchiveStore>(
    archive: ExowareArchive<S>,
    node: FixtureNode,
    config: ArchiveConfig,
) -> crate::SegmentPublisher<S, FixtureNode> {
    let verifier = node.fixture.verifier();
    crate::SegmentPublisher::open(archive, Arc::new(node), verifier, config, fast_options())
        .await
        .expect("publisher opens")
}

/// Opens a reader for `node`.
pub async fn reader<S: ArchiveStore>(
    archive: ExowareArchive<S>,
    node: FixtureNode,
) -> crate::ArchiveReader<S, FixtureNode> {
    let fixture = Arc::clone(&node.fixture);
    crate::ArchiveReader::open(
        archive,
        fixture.verifier(),
        Arc::new(node),
        fixture.chain_spec.chain.id(),
        fixture.blocks[0].hash(),
        crate::ReaderLimits::default(),
    )
    .await
    .expect("reader opens")
}

/// Publishes segments `config.start_segment..=last` of `fixture` into `archive`.
pub async fn publish_through<S: ArchiveStore>(
    archive: &ExowareArchive<S>,
    fixture: &Arc<ChainFixture>,
    config: ArchiveConfig,
    last: u64,
) {
    let publisher = publisher(archive.clone(), FixtureNode::new(Arc::clone(fixture)), config).await;
    for segment in config.start_segment..=last {
        publisher.publish_segment(segment).await.expect("segment publishes");
    }
}
