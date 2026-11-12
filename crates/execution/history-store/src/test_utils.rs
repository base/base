//! Hermetic chain fixtures for history store tests.
//!
//! [`ChainFixture`] produces a synthetic chain whose headers carry correct transaction roots,
//! receipt roots, blooms and parent hashes, with signed transactions, receipts with logs, and
//! account/storage changesets that match a replayed state. Tests use it to drive the writer,
//! verifier and reader without a running node.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex, PoisonError},
};

use alloy_consensus::{
    Eip658Value, Header, Receipt, SignableTransaction, TxEip1559, TxReceipt,
    proofs::calculate_transaction_root,
};
use alloy_primitives::{Address, B256, Bloom, Bytes, Log, TxKind, U256, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::{BaseReceipt, BaseTxEnvelope};
use base_execution_chainspec::BaseChainSpec;
use base_execution_consensus::calculate_receipt_root_no_memo;
use reth_db_api::models::{AccountBeforeTx, StorageBeforeTx};
use futures::{StreamExt, stream::BoxStream};
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, path::Path,
};
use reth_primitives_traits::Account;
use tokio::sync::oneshot;

use crate::{ArchiveBlock, BlockData, BlockRows, BodyIndices};

/// Chain id used by fixture transactions.
pub const FIXTURE_CHAIN_ID: u64 = 845_337;
/// Balance every fixture sender starts with.
pub const FIXTURE_START_BALANCE: u64 = 1_000_000_000_000;
/// Number of storage slots the fixture contract cycles through.
pub const FIXTURE_SLOTS: u64 = 3;

/// Account and storage state after a block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FixtureState {
    /// Accounts that exist.
    pub accounts: BTreeMap<Address, Account>,
    /// Non-zero storage slots.
    pub storage: BTreeMap<(Address, B256), U256>,
}

/// A synthetic, internally consistent chain.
#[derive(Debug, Clone)]
pub struct ChainFixture {
    /// Blocks `0..len`, block 0 being an empty genesis.
    pub blocks: Vec<BlockData>,
    /// State after each block (index = block number).
    pub states: Vec<FixtureState>,
    /// Chain spec used for receipt roots.
    pub chain_spec: Arc<BaseChainSpec>,
    /// Transaction senders.
    pub senders: Vec<Address>,
    /// Recipient of every transfer.
    pub recipient: Address,
    /// Contract whose storage changes every block.
    pub contract: Address,
}

impl ChainFixture {
    /// Builds `len` blocks with `txs_per_block` transfers in each non-genesis block.
    pub fn new(len: u64, txs_per_block: u64) -> Self {
        let chain_spec = Arc::new(BaseChainSpec::devnet());
        let signers: Vec<PrivateKeySigner> = (1u8..=3)
            .map(|i| PrivateKeySigner::from_bytes(&B256::with_last_byte(i)).expect("valid key"))
            .collect();
        let senders: Vec<Address> = signers.iter().map(PrivateKeySigner::address).collect();
        let recipient = Address::with_last_byte(0xee);
        let contract = Address::with_last_byte(0xcc);

        let mut state = FixtureState::default();
        for sender in &senders {
            state.accounts.insert(
                *sender,
                Account {
                    nonce: 0,
                    balance: U256::from(FIXTURE_START_BALANCE),
                    ..Default::default()
                },
            );
        }
        state.accounts.insert(
            contract,
            Account {
                nonce: 1,
                balance: U256::ZERO,
                bytecode_hash: Some(keccak256([0x60, 0x00])),
                ..Default::default()
            },
        );

        let genesis = Self::seal(
            &chain_spec,
            Header {
                number: 0,
                timestamp: 1_700_000_000,
                gas_limit: 30_000_000,
                base_fee_per_gas: Some(1),
                ..Default::default()
            },
            BlockRows::default(),
        );
        let mut blocks = vec![genesis];
        let mut states = vec![state.clone()];
        let mut next_tx_num = 0;

        for number in 1..len {
            let before = state.clone();
            let mut transactions = Vec::new();
            let mut tx_senders = Vec::new();
            let mut receipts = Vec::new();
            let mut cumulative_gas = 0;
            for j in 0..txs_per_block {
                let idx = ((number + j) % senders.len() as u64) as usize;
                let sender = senders[idx];
                let value = U256::from(1_000 + number);
                let account = state.accounts.get_mut(&sender).expect("sender exists");
                let tx = TxEip1559 {
                    chain_id: FIXTURE_CHAIN_ID,
                    nonce: account.nonce,
                    gas_limit: 21_000,
                    max_fee_per_gas: 1,
                    max_priority_fee_per_gas: 0,
                    to: TxKind::Call(recipient),
                    value,
                    ..Default::default()
                };
                let signature =
                    signers[idx].sign_hash_sync(&tx.signature_hash()).expect("signs");
                transactions.push(BaseTxEnvelope::Eip1559(tx.into_signed(signature)));
                tx_senders.push(sender);
                account.nonce += 1;
                account.balance -= value;
                state.accounts.entry(recipient).or_default().balance += value;
                cumulative_gas += 21_000;
                receipts.push(BaseReceipt::Eip1559(Receipt {
                    status: Eip658Value::Eip658(true),
                    cumulative_gas_used: cumulative_gas,
                    logs: vec![Log::new_unchecked(
                        contract,
                        vec![B256::with_last_byte(idx as u8), B256::from(U256::from(number))],
                        Bytes::from(number.to_be_bytes().to_vec()),
                    )],
                }));
            }
            let slot = B256::from(U256::from(number % FIXTURE_SLOTS));
            state.storage.insert((contract, slot), U256::from(number));

            let account_changes = state
                .accounts
                .iter()
                .filter(|(address, account)| before.accounts.get(*address) != Some(*account))
                .map(|(address, _)| AccountBeforeTx {
                    address: *address,
                    info: before.accounts.get(address).copied(),
                })
                .collect();
            let storage_changes = vec![StorageBeforeTx {
                address: contract,
                key: slot,
                value: before.storage.get(&(contract, slot)).copied().unwrap_or_default(),
            }];

            let parent = blocks.last().expect("genesis exists");
            let header = Header {
                parent_hash: parent.hash,
                number,
                timestamp: parent.header.timestamp + 2,
                gas_limit: 30_000_000,
                gas_used: cumulative_gas,
                base_fee_per_gas: Some(1),
                state_root: keccak256(number.to_be_bytes()),
                ..Default::default()
            };
            let body_indices = BodyIndices { first_tx_num: next_tx_num, tx_count: txs_per_block };
            next_tx_num += txs_per_block;
            blocks.push(Self::seal(
                &chain_spec,
                header,
                BlockRows {
                    body_indices,
                    transactions,
                    senders: tx_senders,
                    receipts,
                    account_changes,
                    storage_changes,
                },
            ));
            states.push(state.clone());
        }

        Self { blocks, states, chain_spec, senders, recipient, contract }
    }

    /// Fills in the header roots and bloom from the rows and builds the block.
    pub fn seal(chain_spec: &BaseChainSpec, mut header: Header, rows: BlockRows) -> BlockData {
        header.transactions_root = calculate_transaction_root(&rows.transactions);
        header.receipts_root =
            calculate_receipt_root_no_memo(&rows.receipts, chain_spec, header.timestamp);
        header.logs_bloom =
            rows.receipts.iter().fold(Bloom::ZERO, |bloom, receipt| bloom | receipt.bloom());
        BlockData::new(header, rows)
    }

    /// Returns the wire form of blocks `from..=to`.
    pub fn wire(&self, from: u64, to: u64) -> Vec<ArchiveBlock> {
        self.blocks[from as usize..=to as usize].iter().map(ArchiveBlock::from_data).collect()
    }

    /// Returns the last block number.
    pub fn tip(&self) -> u64 {
        self.blocks.len() as u64 - 1
    }
}

/// A mock node that serves `fixture` and reports `finalized` as its finalized block.
#[cfg(test)]
pub fn mock_node(fixture: &ChainFixture, finalized: u64) -> crate::MockChainSource {
    let mut source = crate::MockChainSource::new();
    source.expect_name().return_const("fixture".to_string());
    source.expect_status().returning(move || {
        Ok(crate::ArchiveStatus {
            head: finalized,
            safe: finalized,
            finalized,
            earliest_local: 0,
            max_range_blocks: crate::DEFAULT_MAX_RANGE_BLOCKS,
            max_range_bytes: crate::DEFAULT_MAX_RANGE_BYTES,
        })
    });
    let fixture = fixture.clone();
    source.expect_get_block_range().returning(move |from, to| Ok(fixture.wire(from, to)));
    source
}

/// A node that serves a [`ChainFixture`] with every block finalized.
///
/// Hand-rolled rather than a `mockall` mock because integration tests (outside this crate's
/// `cfg(test)`) need it too.
#[derive(Debug, Clone)]
pub struct FixtureSource {
    /// The served chain.
    pub fixture: ChainFixture,
}

#[async_trait::async_trait]
impl crate::ChainSource for FixtureSource {
    fn name(&self) -> String {
        "fixture".to_string()
    }

    async fn status(&self) -> Result<crate::ArchiveStatus, crate::HistoryStoreError> {
        let tip = self.fixture.tip();
        Ok(crate::ArchiveStatus {
            head: tip,
            safe: tip,
            finalized: tip,
            earliest_local: 0,
            max_range_blocks: crate::DEFAULT_MAX_RANGE_BLOCKS,
            max_range_bytes: crate::DEFAULT_MAX_RANGE_BYTES,
        })
    }

    async fn get_block_range(
        &self,
        from: u64,
        to: u64,
    ) -> Result<Vec<ArchiveBlock>, crate::HistoryStoreError> {
        Ok(self.fixture.wire(from, to))
    }
}

/// Opens a writer over `store` (prefix `chain`, `N = blocks_per_segment`) backed by one
/// node serving the whole fixture as finalized.
pub async fn fixture_writer(
    store: std::sync::Arc<dyn object_store::ObjectStore>,
    fixture: &ChainFixture,
    blocks_per_segment: u64,
) -> crate::ArchiveWriter {
    crate::ArchiveWriter::open(
        store,
        crate::ArchiveLayout::new("chain"),
        crate::ChainIdentity {
            chain_id: FIXTURE_CHAIN_ID,
            genesis_hash: fixture.blocks[0].hash,
            blocks_per_segment,
        },
        crate::SourcePool::new(vec![Arc::new(FixtureSource { fixture: fixture.clone() })]),
        crate::ChunkVerifier::new(fixture.chain_spec.clone()),
        crate::WriterConfig::default(),
        test_index_options(),
    )
    .await
    .expect("writer opens")
}

/// Index options with a short manifest poll interval for tests.
pub fn test_index_options() -> crate::IndexOptions {
    crate::IndexOptions {
        gc_enabled: true,
        manifest_poll_interval: std::time::Duration::from_millis(20),
    }
}

/// Object-store operation a [`GatedStore`] gate matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOp {
    /// `put_opts`.
    Put,
    /// `get_opts`.
    Get,
    /// One location of `delete_stream`.
    Delete,
    /// `list` and `list_with_offset`.
    List,
    /// Any of the above.
    Any,
}

/// A gate armed on a [`GatedStore`] but not yet hit.
#[derive(Debug)]
pub struct ArmedGate {
    /// Operation the gate matches.
    pub op: GateOp,
    /// Path prefix the gate matches.
    pub prefix: String,
    /// Matching operations to let through before pausing.
    pub skip: usize,
    /// Signals the paused path to the test.
    pub reached: oneshot::Sender<Path>,
    /// Resolves when the test releases the operation.
    pub release: oneshot::Receiver<()>,
}

/// Test side of one gate: wait until an operation is paused there, then release it.
#[derive(Debug)]
pub struct Gate {
    /// Receives the paused path.
    pub reached: Option<oneshot::Receiver<Path>>,
    /// Releases the paused operation.
    pub release: Option<oneshot::Sender<()>>,
}

impl Gate {
    /// Waits until a matching operation is paused at the gate and returns its path.
    pub async fn reached(&mut self) -> Path {
        self.reached.take().expect("gate awaited once").await.expect("gated store dropped")
    }

    /// Lets the paused operation continue. Dropping the gate also releases it.
    pub fn release(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

/// An object store that pauses chosen operations until the test releases them, so tests can
/// reproduce an interleaving of two actors deterministically, without sleeps.
///
/// Hand-rolled rather than a `mockall` mock because it wraps a real store and must hold an
/// operation in flight while the test drives another actor.
#[derive(Debug, Clone)]
pub struct GatedStore {
    /// Store the operations run against.
    pub inner: Arc<dyn ObjectStore>,
    /// Gates not yet hit, in arming order.
    pub gates: Arc<Mutex<Vec<ArmedGate>>>,
}

impl GatedStore {
    /// Wraps `inner` with no gates armed.
    pub fn new(inner: Arc<dyn ObjectStore>) -> Self {
        Self { inner, gates: Arc::default() }
    }

    /// Arms a gate that pauses the first `op` on a path starting with `prefix`.
    pub fn arm(&self, op: GateOp, prefix: &str) -> Gate {
        self.arm_after(op, prefix, 0)
    }

    /// Arms a gate that lets `skip` matching operations through and pauses the next one.
    pub fn arm_after(&self, op: GateOp, prefix: &str, skip: usize) -> Gate {
        let (reached_tx, reached_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        self.gates.lock().unwrap_or_else(PoisonError::into_inner).push(ArmedGate {
            op,
            prefix: prefix.to_string(),
            skip,
            reached: reached_tx,
            release: release_rx,
        });
        Gate { reached: Some(reached_rx), release: Some(release_tx) }
    }

    /// Pauses at the first armed gate matching `op` on `path`, if any.
    pub async fn pass(gates: &Mutex<Vec<ArmedGate>>, op: GateOp, path: &Path) {
        let hit = {
            let mut gates = gates.lock().unwrap_or_else(PoisonError::into_inner);
            let position = gates.iter_mut().position(|gate| {
                let matches = (gate.op == op || gate.op == GateOp::Any)
                    && path.as_ref().starts_with(&gate.prefix);
                if matches && gate.skip > 0 {
                    gate.skip -= 1;
                    return false;
                }
                matches
            });
            position.map(|position| gates.remove(position))
        };
        if let Some(gate) = hit {
            let _ = gate.reached.send(path.clone());
            let _ = gate.release.await;
        }
    }
}

impl fmt::Display for GatedStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GatedStore({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for GatedStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        Self::pass(&self.gates, GateOp::Put, location).await;
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> object_store::Result<GetResult> {
        Self::pass(&self.gates, GateOp::Get, location).await;
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        let gates = Arc::clone(&self.gates);
        let locations = locations
            .then(move |location| {
                let gates = Arc::clone(&gates);
                async move {
                    if let Ok(path) = &location {
                        Self::pass(&gates, GateOp::Delete, path).await;
                    }
                    location
                }
            })
            .boxed();
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        let gates = Arc::clone(&self.gates);
        let inner = Arc::clone(&self.inner);
        let prefix = prefix.cloned();
        // The inner listing starts only after the gate, so a paused listing sees later writes.
        futures::stream::once(async move {
            Self::pass(&gates, GateOp::List, &prefix.clone().unwrap_or_default()).await;
            inner.list(prefix.as_ref())
        })
        .flatten()
        .boxed()
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        let gates = Arc::clone(&self.gates);
        let inner = Arc::clone(&self.inner);
        let prefix = prefix.cloned();
        let offset = offset.clone();
        futures::stream::once(async move {
            Self::pass(&gates, GateOp::List, &prefix.clone().unwrap_or_default()).await;
            inner.list_with_offset(prefix.as_ref(), &offset)
        })
        .flatten()
        .boxed()
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        let path = prefix.cloned().unwrap_or_default();
        Self::pass(&self.gates, GateOp::List, &path).await;
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}
