//! Flashblocks test harness module.
//!
//! Provides test utilities for flashblocks including:
//! - [`FlashblocksHarness`] - High-level test harness wrapping [`TestHarness`]
//! - [`FlashblocksParts`] - Components for interacting with flashblocks worker tasks
//! - [`FlashblocksTestExtension`] - Node extension for wiring up flashblocks in tests
//! - [`FlashblocksLocalNode`] - Local node wrapper with flashblocks helpers
//! - [`FlashblockBuilder`] - Test helper for building flashblocks
//! - [`FlashblocksBuilderTestHarness`] - Test harness builder for building flashblocks

use std::{
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_consensus::{Receipt, Transaction};
use alloy_eips::{BlockHashOrNumber, Encodable2718};
use alloy_primitives::{Address, B256, BlockNumber, Bytes, U256, hex::FromHex, map::HashMap};
use alloy_rpc_types_engine::PayloadId;
use base_client_node::{
    BaseBuilder, BaseNodeExtension,
    test_utils::{
        Account, L1_BLOCK_INFO_DEPOSIT_TX, L1_BLOCK_INFO_DEPOSIT_TX_HASH, LocalNode,
        LocalNodeProvider, NODE_STARTUP_DELAY_MS, TestHarness, build_test_genesis,
        init_silenced_tracing,
    },
};
use base_flashblocks::{
    EthApiExt, EthApiOverrideServer, EthPubSub, EthPubSubApiServer, FlashblocksAPI,
    FlashblocksReceiver, FlashblocksState, PendingBlocksAPI,
};
use base_flashtypes::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
};
use derive_more::Deref;
use eyre::Result;
use op_alloy_consensus::OpDepositReceipt;
use reth_chain_state::CanonStateSubscriptions;
use reth_chainspec::EthChainSpec;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_primitives::{OpBlock, OpReceipt, OpTransactionSigned};
use reth_primitives_traits::{Account as RethAccount, Block as BlockT, RecoveredBlock};
use reth_provider::{AccountReader, BlockNumReader, BlockReader, ChainSpecProvider};
use reth_transaction_pool::test_utils::TransactionBuilder;
use tokio::{
    sync::{mpsc, oneshot},
    time::sleep,
};
use tokio_stream::{StreamExt, wrappers::BroadcastStream};

// The amount of time to wait (in milliseconds) after sending a new flashblock or canonical block
// so it can be processed by the state processor
const SLEEP_TIME: u64 = 10;

/// Components that allow tests to interact with the Flashblocks worker tasks.
#[derive(Clone)]
pub struct FlashblocksParts {
    sender: mpsc::Sender<(Flashblock, oneshot::Sender<()>)>,
    state: Arc<FlashblocksState>,
}

impl fmt::Debug for FlashblocksParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlashblocksParts").finish_non_exhaustive()
    }
}

impl FlashblocksParts {
    /// Clone the shared [`FlashblocksState`] handle.
    pub fn state(&self) -> Arc<FlashblocksState> {
        Arc::clone(&self.state)
    }

    /// Send a flashblock to the background processor and wait until it is handled.
    pub async fn send(&self, flashblock: Flashblock) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.sender.send((flashblock, tx)).await.map_err(|err| eyre::eyre!(err))?;
        rx.await.map_err(|err| eyre::eyre!(err))?;
        Ok(())
    }
}

/// Test extension for flashblocks functionality.
///
/// This extension wires up the flashblocks canonical subscription and RPC modules for testing,
/// with optional control over canonical block processing.
#[derive(Clone, Debug)]
pub struct FlashblocksTestExtension {
    inner: Arc<FlashblocksTestExtensionInner>,
}

struct FlashblocksTestExtensionInner {
    sender: mpsc::Sender<(Flashblock, oneshot::Sender<()>)>,
    #[allow(clippy::type_complexity)]
    receiver: Arc<Mutex<Option<mpsc::Receiver<(Flashblock, oneshot::Sender<()>)>>>>,
    state: Arc<FlashblocksState>,
    process_canonical: bool,
}

impl fmt::Debug for FlashblocksTestExtensionInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlashblocksTestExtensionInner")
            .field("process_canonical", &self.process_canonical)
            .finish_non_exhaustive()
    }
}

impl FlashblocksTestExtension {
    /// Create a new flashblocks test extension.
    ///
    /// If `process_canonical` is true, canonical blocks are automatically processed.
    /// Set to false for tests that need manual control over canonical block timing.
    pub fn new(process_canonical: bool) -> Self {
        let (sender, receiver) = mpsc::channel::<(Flashblock, oneshot::Sender<()>)>(100);
        let inner = FlashblocksTestExtensionInner {
            sender,
            receiver: Arc::new(Mutex::new(Some(receiver))),
            state: Arc::new(FlashblocksState::new(5)),
            process_canonical,
        };
        Self { inner: Arc::new(inner) }
    }

    /// Get the flashblocks parts after the node has been launched.
    pub fn parts(&self) -> Result<FlashblocksParts> {
        Ok(FlashblocksParts {
            sender: self.inner.sender.clone(),
            state: Arc::clone(&self.inner.state),
        })
    }
}

impl BaseNodeExtension for FlashblocksTestExtension {
    fn apply(self: Box<Self>, builder: BaseBuilder) -> BaseBuilder {
        let state = Arc::clone(&self.inner.state);
        let receiver = Arc::clone(&self.inner.receiver);
        let process_canonical = self.inner.process_canonical;

        let state_for_start = Arc::clone(&state);
        let state_for_rpc = state;

        // Start state processor and subscriptions after node is started
        let builder = builder.add_node_started_hook(move |ctx| {
            let provider = ctx.provider().clone();

            // Start the state processor with the provider
            state_for_start.start(provider.clone());

            // Spawn a task to forward canonical state notifications to the in-memory state
            let provider_for_notify = provider;
            let mut canon_notify_stream =
                BroadcastStream::new(ctx.provider().subscribe_to_canonical_state());
            tokio::spawn(async move {
                while let Some(Ok(notification)) = canon_notify_stream.next().await {
                    provider_for_notify
                        .canonical_in_memory_state()
                        .notify_canon_state(notification);
                }
            });

            // If process_canonical is enabled, spawn a task to process canonical blocks
            if process_canonical {
                let state_for_canonical = state_for_start;
                let mut canonical_stream =
                    BroadcastStream::new(ctx.provider().subscribe_to_canonical_state());
                tokio::spawn(async move {
                    while let Some(Ok(notification)) = canonical_stream.next().await {
                        let committed = notification.committed();
                        for block in committed.blocks_iter() {
                            state_for_canonical.on_canonical_block_received(block.clone());
                        }
                    }
                });
            }

            Ok(())
        });

        builder.add_rpc_module(move |ctx| {
            let fb = state_for_rpc;

            let api_ext = EthApiExt::new(
                ctx.registry.eth_api().clone(),
                ctx.registry.eth_handlers().filter.clone(),
                Arc::clone(&fb),
            );
            ctx.modules.replace_configured(api_ext.into_rpc())?;

            // Register eth_subscribe subscription endpoint for flashblocks
            // Uses replace_configured since eth_subscribe already exists from reth's standard module
            // Pass eth_api to enable proxying standard subscription types to reth's implementation
            let eth_pubsub = EthPubSub::new(ctx.registry.eth_api().clone(), Arc::clone(&fb));
            ctx.modules.replace_configured(eth_pubsub.into_rpc())?;

            let fb_for_task = fb;
            let mut receiver = receiver
                .lock()
                .expect("flashblock receiver mutex poisoned")
                .take()
                .expect("flashblock receiver should only be initialized once");
            tokio::spawn(async move {
                while let Some((payload, tx)) = receiver.recv().await {
                    fb_for_task.on_flashblock_received(payload);
                    let _ = tx.send(());
                }
            });

            Ok(())
        })
    }
}

/// Local node wrapper that exposes helpers specific to Flashblocks tests.
pub struct FlashblocksLocalNode {
    node: LocalNode,
    parts: FlashblocksParts,
}

impl fmt::Debug for FlashblocksLocalNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlashblocksLocalNode")
            .field("node", &self.node)
            .field("parts", &self.parts)
            .finish()
    }
}

impl FlashblocksLocalNode {
    /// Launch a flashblocks-enabled node using the default configuration.
    pub async fn new() -> Result<Self> {
        Self::with_options(true).await
    }

    /// Builds a flashblocks-enabled node with canonical block streaming disabled so tests can call
    /// `FlashblocksState::on_canonical_block_received` at precise points.
    pub async fn manual_canonical() -> Result<Self> {
        Self::with_options(false).await
    }

    async fn with_options(process_canonical: bool) -> Result<Self> {
        // Build default chain spec programmatically
        let genesis = build_test_genesis();
        let chain_spec = Arc::new(OpChainSpec::from_genesis(genesis));

        let extension = FlashblocksTestExtension::new(process_canonical);
        let parts_source = extension.clone();
        let node = LocalNode::new(vec![Box::new(extension)], chain_spec).await?;
        let parts = parts_source.parts()?;
        Ok(Self { node, parts })
    }

    /// Access the shared Flashblocks state for assertions or manual driving.
    pub fn flashblocks_state(&self) -> Arc<FlashblocksState> {
        self.parts.state()
    }

    /// Send a flashblock through the background processor and await completion.
    pub async fn send_flashblock(&self, flashblock: Flashblock) -> Result<()> {
        self.parts.send(flashblock).await
    }

    /// Split the wrapper into the underlying node plus flashblocks parts.
    pub fn into_parts(self) -> (LocalNode, FlashblocksParts) {
        (self.node, self.parts)
    }
}

/// Helper that exposes [`TestHarness`] conveniences plus Flashblocks helpers.
#[derive(Debug, Deref)]
pub struct FlashblocksHarness {
    #[deref]
    inner: TestHarness,
    parts: FlashblocksParts,
}

impl FlashblocksHarness {
    /// Launch a flashblocks-enabled harness with automatic canonical processing.
    pub async fn new() -> Result<Self> {
        Self::with_options(true).await
    }

    /// Launch the harness configured for manual canonical progression.
    pub async fn manual_canonical() -> Result<Self> {
        Self::with_options(false).await
    }

    /// Get a handle to the in-memory Flashblocks state backing the harness.
    pub fn flashblocks_state(&self) -> Arc<FlashblocksState> {
        self.parts.state()
    }

    /// Send a single flashblock through the harness.
    pub async fn send_flashblock(&self, flashblock: Flashblock) -> Result<()> {
        self.parts.send(flashblock).await
    }

    async fn with_options(process_canonical: bool) -> Result<Self> {
        init_silenced_tracing();

        // Build default chain spec programmatically
        let genesis = build_test_genesis();
        let chain_spec = Arc::new(OpChainSpec::from_genesis(genesis));

        // Create the extension and keep a reference to get parts after launch
        let extension = FlashblocksTestExtension::new(process_canonical);
        let parts_source = extension.clone();

        // Launch the node with the flashblocks extension
        let node = LocalNode::new(vec![Box::new(extension)], chain_spec).await?;
        let engine = node.engine_api()?;

        tokio::time::sleep(Duration::from_millis(NODE_STARTUP_DELAY_MS)).await;

        // Get the parts from the extension after node launch
        let parts = parts_source.parts()?;

        // Create harness by building it directly (avoiding TestHarnessBuilder since we already have node)
        let inner = TestHarness::from_parts(node, engine);

        Ok(Self { inner, parts })
    }
}

/// Test harness builder for building flashblocks.
#[derive(Debug)]
pub struct FlashblocksBuilderTestHarness {
    /// The flashblocks harness.
    pub node: FlashblocksHarness,
    /// The blockchain provider.
    pub provider: LocalNodeProvider,
    /// The flashblocks state.
    pub flashblocks: Arc<FlashblocksState>,
}

impl FlashblocksBuilderTestHarness {
    /// Launch a new flashblocks builder test harness.
    pub async fn new() -> Self {
        // These tests simulate pathological timing (missing receipts, reorgs, etc.), so we disable
        // the automatic canonical listener and only apply blocks when the test explicitly requests it.
        let node = FlashblocksHarness::manual_canonical()
            .await
            .expect("able to launch flashblocks harness");
        let provider = node.blockchain_provider();
        let flashblocks = node.flashblocks_state();

        let genesis_block = provider
            .block(BlockHashOrNumber::Number(0))
            .expect("able to load block")
            .expect("block exists")
            .try_into_recovered()
            .expect("able to recover block");
        flashblocks.on_canonical_block_received(genesis_block);

        Self { node, provider, flashblocks }
    }

    /// Decode a private key from an account.
    pub fn decode_private_key(account: Account) -> B256 {
        B256::from_hex(account.private_key()).expect("valid hex-encoded key")
    }

    /// Get the canonical account state.
    pub fn canonical_account(&self, account: Account) -> RethAccount {
        self.provider
            .basic_account(&account.address())
            .expect("can lookup account state")
            .expect("should be existing account state")
    }

    /// Get the canonical account balance.
    pub fn canonical_balance(&self, account: Account) -> U256 {
        self.canonical_account(account).balance
    }

    /// Get the expected pending balance.
    pub fn expected_pending_balance(&self, account: Account, delta: u128) -> U256 {
        self.canonical_balance(account) + U256::from(delta)
    }

    /// Get the account state.
    pub fn account_state(&self, account: Account) -> RethAccount {
        let basic_account = self.canonical_account(account);

        let nonce = self
            .flashblocks
            .get_pending_blocks()
            .get_transaction_count(account.address())
            .to::<u64>();
        let balance = self
            .flashblocks
            .get_pending_blocks()
            .get_balance(account.address())
            .unwrap_or(basic_account.balance);

        RethAccount {
            nonce: nonce + basic_account.nonce,
            balance,
            bytecode_hash: basic_account.bytecode_hash,
        }
    }

    /// Build a transaction to send ETH from one account to another.
    pub fn build_transaction_to_send_eth(
        &self,
        from: Account,
        to: Account,
        amount: u128,
    ) -> OpTransactionSigned {
        let txn = TransactionBuilder::default()
            .signer(Self::decode_private_key(from))
            .chain_id(self.provider.chain_spec().chain_id())
            .to(to.address())
            .nonce(self.account_state(from).nonce)
            .value(amount)
            .gas_limit(21_000)
            .max_fee_per_gas(1_000_000_000)
            .max_priority_fee_per_gas(1_000_000_000)
            .into_eip1559()
            .as_eip1559()
            .unwrap()
            .clone();

        OpTransactionSigned::Eip1559(txn)
    }

    /// Build a transaction to send ETH from one account to another with a specific nonce.
    pub fn build_transaction_to_send_eth_with_nonce(
        &self,
        from: Account,
        to: Account,
        amount: u128,
        nonce: u64,
    ) -> OpTransactionSigned {
        let txn = TransactionBuilder::default()
            .signer(Self::decode_private_key(from))
            .chain_id(self.provider.chain_spec().chain_id())
            .to(to.address())
            .nonce(nonce)
            .value(amount)
            .gas_limit(21_000)
            .max_fee_per_gas(1_000_000_000)
            .max_priority_fee_per_gas(1_000_000_000)
            .into_eip1559()
            .as_eip1559()
            .unwrap()
            .clone();

        OpTransactionSigned::Eip1559(txn)
    }

    /// Send a flashblock through the harness.
    pub async fn send_flashblock(&self, flashblock: Flashblock) {
        self.node
            .send_flashblock(flashblock)
            .await
            .expect("flashblocks channel should accept payload");
        sleep(Duration::from_millis(SLEEP_TIME)).await;
    }

    /// Build a new canonical block without processing.
    pub async fn new_canonical_block_without_processing(
        &mut self,
        user_transactions: Vec<OpTransactionSigned>,
    ) -> RecoveredBlock<OpBlock> {
        let previous_tip =
            self.provider.best_block_number().expect("able to read best block number");
        let txs: Vec<Bytes> =
            user_transactions.into_iter().map(|tx| tx.encoded_2718().into()).collect();
        self.node.build_block_from_transactions(txs).await.expect("able to build block");
        let target_block_number = previous_tip + 1;

        let block = self
            .provider
            .block(BlockHashOrNumber::Number(target_block_number))
            .expect("able to load block")
            .expect("new canonical block should be available after building payload");

        block.try_into_recovered().expect("able to recover newly built block")
    }

    /// Build a new canonical block with processing.
    pub async fn new_canonical_block(&mut self, user_transactions: Vec<OpTransactionSigned>) {
        let block = self.new_canonical_block_without_processing(user_transactions).await;
        self.flashblocks.on_canonical_block_received(block);
        sleep(Duration::from_millis(SLEEP_TIME)).await;
    }
}

/// Test helper for building flashblocks.
#[derive(Debug)]
pub struct FlashblockBuilder<'a> {
    /// The transactions to include in the flashblock.
    transactions: Vec<Bytes>,
    /// The receipts to include in the flashblock.
    receipts: Option<HashMap<B256, OpReceipt>>,
    /// The harness to use for building the flashblock.
    harness: &'a FlashblocksBuilderTestHarness,
    /// The canonical block number to use for the flashblock.
    canonical_block_number: Option<BlockNumber>,
    /// The index of the flashblock.
    index: u64,
}

impl<'a> FlashblockBuilder<'a> {
    /// Create a new base flashblock builder.
    pub fn new_base(harness: &'a FlashblocksBuilderTestHarness) -> Self {
        Self {
            canonical_block_number: None,
            transactions: vec![L1_BLOCK_INFO_DEPOSIT_TX],
            receipts: Some({
                let mut receipts = alloy_primitives::map::HashMap::default();
                receipts.insert(
                    L1_BLOCK_INFO_DEPOSIT_TX_HASH,
                    OpReceipt::Deposit(OpDepositReceipt {
                        inner: Receipt {
                            status: true.into(),
                            cumulative_gas_used: 10000,
                            logs: vec![],
                        },
                        deposit_nonce: Some(4012991u64),
                        deposit_receipt_version: None,
                    }),
                );
                receipts
            }),
            index: 0,
            harness,
        }
    }

    /// Create a new flashblock builder.
    pub fn new(harness: &'a FlashblocksBuilderTestHarness, index: u64) -> Self {
        Self {
            canonical_block_number: None,
            transactions: Vec::new(),
            receipts: Some(HashMap::default()),
            harness,
            index,
        }
    }

    /// Set the receipts for the flashblock.
    pub fn with_receipts(&mut self, receipts: Option<HashMap<B256, OpReceipt>>) -> &mut Self {
        self.receipts = receipts;
        self
    }

    /// Set the transactions for the flashblock.
    pub fn with_transactions(&mut self, transactions: Vec<OpTransactionSigned>) -> &mut Self {
        assert_ne!(self.index, 0, "Cannot set txns for initial flashblock");
        self.transactions.clear();

        let mut cumulative_gas_used = 0;
        for txn in &transactions {
            cumulative_gas_used += txn.gas_limit();
            self.transactions.push(txn.encoded_2718().into());
            if let Some(ref mut receipts) = self.receipts {
                receipts.insert(
                    *txn.hash(),
                    OpReceipt::Eip1559(Receipt {
                        status: true.into(),
                        cumulative_gas_used,
                        logs: vec![],
                    }),
                );
            }
        }
        self
    }

    /// Set the canonical block number for the flashblock.
    pub const fn with_canonical_block_number(&mut self, num: BlockNumber) -> &mut Self {
        self.canonical_block_number = Some(num);
        self
    }

    /// Build the flashblock.
    pub fn build(&self) -> Flashblock {
        let current_block = self.harness.node.latest_block();
        let canonical_block_num =
            self.canonical_block_number.unwrap_or_else(|| current_block.number) + 1;

        let base = if self.index == 0 {
            Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: current_block.hash(),
                parent_hash: current_block.hash(),
                fee_recipient: Address::random(),
                prev_randao: B256::random(),
                block_number: canonical_block_num,
                gas_limit: current_block.gas_limit,
                timestamp: current_block.timestamp + 2,
                extra_data: Bytes::new(),
                base_fee_per_gas: U256::from(100),
            })
        } else {
            None
        };

        Flashblock {
            payload_id: PayloadId::default(),
            index: self.index,
            base,
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::default(),
                receipts_root: B256::default(),
                block_hash: B256::default(),
                gas_used: 0,
                withdrawals: Vec::new(),
                logs_bloom: Default::default(),
                withdrawals_root: Default::default(),
                transactions: self.transactions.clone(),
                blob_gas_used: Default::default(),
            },
            metadata: Metadata { block_number: canonical_block_num },
        }
    }
}
