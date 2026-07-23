use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fmt::Debug,
    sync::{Arc, RwLock},
    time::Instant,
};

use alloy_consensus::{Header, Sealed};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use base_flashblocks::{FlashblocksAPI, FlashblocksConfig, FlashblocksState, PendingBlocks};
use base_mev_trader::{
    A1Status, BlinkFeedClient, BlinkIngressConfig, BundleVisitor, MevTraderRuntime,
    MevTraderRuntimeConfig, PayloadVisitor, PendingSnapshotView, PortError, SnapshotHandle,
    SnapshotHandleFactory, TraderSnapshotPort, TransactionVisitor, VisitControl, VisitSummary,
};
use base_node_runner::{BaseNodeExtension, BaseNodeRunner, FromExtensionConfig, NodeHooks};
use reth_provider::{HeaderProvider, StateProviderBox, StateProviderFactory};
use tracing::info;

// B5-1a dormant-preparation tier: a private default-off child compiled only under
// `b5-dormant-presign`, never registered or re-exported at the crate root.
#[cfg(feature = "b5-dormant-presign")]
mod b5_dormant;

#[derive(Debug)]
struct PendingSnapshotViewAdapter {
    pending: Arc<PendingBlocks>,
}

impl PendingSnapshotView for PendingSnapshotViewAdapter {
    fn parent_hash(&self) -> B256 {
        self.pending.parent_hash()
    }

    fn latest_block_number(&self) -> u64 {
        self.pending.latest_block_number()
    }

    fn canonical_block_number(&self) -> u64 {
        match self.pending.canonical_block_number() {
            BlockNumberOrTag::Number(number) => number,
            _ => 0,
        }
    }

    fn latest_flashblock_index(&self) -> u64 {
        self.pending.latest_flashblock_index()
    }

    fn latest_header(&self) -> Sealed<Header> {
        self.pending.latest_header()
    }

    fn latest_block_transaction_count(&self) -> usize {
        self.pending.latest_block_transaction_count()
    }

    fn has_transaction_hash(&self, transaction_hash: B256) -> bool {
        self.pending.has_transaction_hash(&transaction_hash)
    }

    fn transaction_position(&self, block_number: u64, transaction_hash: B256) -> Option<usize> {
        self.pending.transaction_position(block_number, &transaction_hash)
    }

    fn visit_latest_block_payloads(
        &self,
        visitor: &mut dyn PayloadVisitor,
    ) -> Result<VisitSummary, PortError> {
        let mut visited = 0u32;
        for flashblock in self.pending.latest_block_flashblocks_iter() {
            visited = visited.checked_add(1).ok_or(PortError::LimitExceeded)?;
            if visitor.visit(flashblock.payload_id, flashblock.index)? == VisitControl::Stop {
                return Ok(VisitSummary { visited, complete: false });
            }
        }
        Ok(VisitSummary { visited, complete: true })
    }

    fn visit_transactions_for_block(
        &self,
        block_number: u64,
        start: usize,
        limit: usize,
        visitor: &mut dyn TransactionVisitor,
    ) -> Result<VisitSummary, PortError> {
        let mut transactions = self.pending.get_transactions_for_block(block_number).skip(start);
        let mut visited = 0u32;
        for position in start..start.saturating_add(limit) {
            let Some(transaction) = transactions.next() else {
                return Ok(VisitSummary { visited, complete: true });
            };
            visited = visited.checked_add(1).ok_or(PortError::LimitExceeded)?;
            if visitor.visit(position, transaction.inner.inner.inner())? == VisitControl::Stop {
                return Ok(VisitSummary { visited, complete: false });
            }
        }
        Ok(VisitSummary { visited, complete: transactions.next().is_none() })
    }

    fn visit_bundle(&self, visitor: &mut dyn BundleVisitor) -> Result<VisitSummary, PortError> {
        let bundle = self.pending.get_bundle_state();
        let mut visited = 0u32;
        let mut code_hashes = BTreeSet::new();
        for (address, account) in bundle.state() {
            visited = visited.checked_add(1).ok_or(PortError::LimitExceeded)?;
            if visitor.visit_account(*address, account)? == VisitControl::Stop {
                return Ok(VisitSummary { visited, complete: false });
            }
            if let Some(info) = account.account_info()
                && !info.code_hash.is_zero()
            {
                code_hashes.insert(info.code_hash);
            }
        }
        for code_hash in code_hashes {
            let Some(bytecode) = bundle.bytecode(&code_hash) else { continue };
            visited = visited.checked_add(1).ok_or(PortError::LimitExceeded)?;
            if visitor.visit_contract(code_hash, &bytecode)? == VisitControl::Stop {
                return Ok(VisitSummary { visited, complete: false });
            }
        }
        Ok(VisitSummary { visited, complete: true })
    }
}

#[derive(Debug)]
struct PendingSnapshotRecord {
    view: Arc<dyn PendingSnapshotView + Send + Sync>,
    pending: Arc<PendingBlocks>,
    received_at: Instant,
}

#[derive(Debug)]
pub(crate) struct CliTraderSnapshotPort<Provider> {
    flashblocks: Arc<FlashblocksState>,
    provider: Provider,
    current_record: RwLock<Option<Arc<PendingSnapshotRecord>>>,
}

impl<Provider> CliTraderSnapshotPort<Provider> {
    pub(crate) const fn new(flashblocks: Arc<FlashblocksState>, provider: Provider) -> Self {
        Self { flashblocks, provider, current_record: RwLock::new(None) }
    }

    pub(crate) fn record_pending_snapshot(
        &self,
        pending: Arc<PendingBlocks>,
        received_at: Instant,
    ) {
        let view: Arc<dyn PendingSnapshotView + Send + Sync> =
            Arc::new(PendingSnapshotViewAdapter { pending: Arc::clone(&pending) });
        let record = Arc::new(PendingSnapshotRecord { view, pending, received_at });
        *self.current_record.write().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(record);
    }

    fn current_record(&self) -> Option<Arc<PendingSnapshotRecord>> {
        self.current_record.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    fn record_is_current(&self, record: &PendingSnapshotRecord) -> bool {
        let current = self.flashblocks.get_pending_blocks();
        current.as_ref().is_some_and(|pending| Arc::ptr_eq(pending, &record.pending))
    }
}

impl<Provider> TraderSnapshotPort for CliTraderSnapshotPort<Provider>
where
    Provider: StateProviderFactory
        + HeaderProvider<Header = Header>
        + Clone
        + Debug
        + Send
        + Sync
        + 'static,
{
    fn capture_latest(
        &self,
        factory: &SnapshotHandleFactory,
    ) -> Result<Option<SnapshotHandle>, PortError> {
        let Some(record) = self.current_record() else { return Ok(None) };
        if !self.record_is_current(&record) {
            return Ok(None);
        }
        factory.issue(Arc::clone(&record.view), record.received_at).map(Some)
    }

    fn is_current_authoritative(&self, handle: &SnapshotHandle) -> bool {
        let Some(record) = self.current_record() else { return false };
        self.record_is_current(&record) && handle.matches_capture(&record.view, record.received_at)
    }

    fn state_at_hash(&self, block_hash: B256) -> Result<StateProviderBox, PortError> {
        self.provider.state_by_block_hash(block_hash).map_err(|_| PortError::ProviderUnavailable)
    }

    fn sealed_header_at_hash(&self, block_hash: B256) -> Result<Sealed<Header>, PortError> {
        let header = self
            .provider
            .sealed_header_by_hash(block_hash)
            .map_err(|_| PortError::HeaderUnavailable)?
            .ok_or(PortError::HeaderUnavailable)?;
        Ok(Sealed::new_unchecked(header.clone_header(), header.hash()))
    }
}

/// Exact-1 Phase A extension configuration with post-gate receive credential input.
#[derive(Debug, Clone)]
pub struct BaseNodeTraderConfig {
    flashblocks: Arc<FlashblocksState>,
    credential_file: Option<OsString>,
    t4a_shadow: bool,
}

impl BaseNodeTraderConfig {
    /// Returns true only for the exact native `OsStr` bytes `1`.
    pub fn enabled(env: Option<&OsStr>) -> bool {
        env == Some(OsStr::new("1"))
    }

    fn t4a_shadow_enabled() -> bool {
        #[cfg(feature = "t4a-shadow")]
        {
            return Self::enabled(std::env::var_os("MEV_TRADER_T4A_SHADOW").as_deref());
        }
        #[cfg(not(feature = "t4a-shadow"))]
        {
            false
        }
    }

    /// Applies exact-1 and flashblocks-present gates before consulting the credential environment.
    pub fn from_inputs(
        flashblocks_config: &Option<FlashblocksConfig>,
        env: Option<&OsStr>,
    ) -> Option<Self> {
        if !Self::enabled(env) {
            return None;
        }
        let config = flashblocks_config.as_ref()?;
        let credential_file = std::env::var_os("MEV_TRADER_BLINK_CREDENTIAL_FILE");
        Some(Self {
            flashblocks: Arc::clone(&config.state),
            credential_file,
            t4a_shadow: Self::t4a_shadow_enabled(),
        })
    }

    /// Creates the sole snapshot subscription and receive-only A1 runtime.
    pub fn start_idle(self) -> eyre::Result<BaseNodeTraderStart> {
        let receiver = self.flashblocks.subscribe_to_flashblocks();
        let runtime_config = t4a_runtime_config(self.t4a_shadow)?;
        let runtime = Arc::new(MevTraderRuntime::start(runtime_config)?);
        let client = self.credential_file.and_then(|credential_file| {
            BlinkFeedClient::new(
                BlinkIngressConfig::production(credential_file),
                Arc::clone(&runtime),
            )
        });
        if client.is_none() {
            runtime.set_a1_status(A1Status::DisabledNoConnect);
        }
        Ok(BaseNodeTraderStart { flashblocks: self.flashblocks, receiver, runtime, client })
    }
}

/// Provider-independent node-start resources for the receive-only runtime.
#[derive(Debug)]
pub struct BaseNodeTraderStart {
    flashblocks: Arc<FlashblocksState>,
    receiver: tokio::sync::broadcast::Receiver<Arc<PendingBlocks>>,
    runtime: Arc<MevTraderRuntime>,
    client: Option<BlinkFeedClient>,
}

impl BaseNodeTraderStart {
    /// Returns the exact one existing flashblock broadcast subscription.
    pub const fn subscriber_count(&self) -> usize {
        1
    }

    /// Returns the exact one sole consumer.
    pub fn worker_count(&self) -> usize {
        if self.runtime.worker_is_claimed() { 1 } else { 0 }
    }

    /// Returns the exact one dedicated Rayon4 analysis pool.
    pub const fn pool_count(&self) -> usize {
        1
    }

    /// Returns the exact one separate watchdog/control domain.
    pub const fn watchdog_count(&self) -> usize {
        1
    }

    /// Returns whether the measurement-only registry is disabled.
    pub fn registry_is_empty(&self) -> bool {
        self.runtime.registry_is_empty()
    }

    /// Returns whether a valid receive-only ingress client was constructed.
    pub const fn has_live_victim_producer(&self) -> bool {
        self.client.is_some()
    }
}

/// CLI-owned node extension for snapshot observation and receive-only A1 ownership.
#[derive(Debug)]
pub struct BaseNodeTraderExtension {
    config: BaseNodeTraderConfig,
}

impl BaseNodeTraderExtension {
    /// Creates an extension from exact-1 configuration.
    pub const fn new(config: BaseNodeTraderConfig) -> Self {
        Self { config }
    }
}

impl FromExtensionConfig for BaseNodeTraderExtension {
    type Config = BaseNodeTraderConfig;

    fn from_config(config: Self::Config) -> Self {
        Self::new(config)
    }
}

impl BaseNodeExtension for BaseNodeTraderExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.add_node_started_hook(move |node| {
            let BaseNodeTraderStart {
                flashblocks,
                mut receiver,
                runtime,
                client,
            } = self.config.start_idle()?;
            let port = Arc::new(CliTraderSnapshotPort::new(flashblocks, node.provider().clone()));
            let consumer_port: Arc<dyn TraderSnapshotPort> = port.clone();
            let chain_spec = node.chain_spec();
            let executor = node.task_executor;
            let startup_status = runtime.a1_status();
            let registry_empty = runtime.registry_is_empty();
            executor.spawn_with_graceful_shutdown_signal(move |signal| {
                Box::pin(async move {
                    let snapshot_runtime = Arc::clone(&runtime);
                    let snapshot_handle = tokio::spawn(async move {
                        loop {
                            tokio::select! {
                                () = snapshot_runtime.shutdown().wait_cancelled() => return,
                                pending = receiver.recv() => match pending {
                                    Ok(pending) => port.record_pending_snapshot(pending, Instant::now()),
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                                }
                            }
                        }
                    });
                    let consumer_runtime = Arc::clone(&runtime);
                    let consumer_handle = tokio::spawn(async move {
                        consumer_runtime.run_consumer(consumer_port, chain_spec).await;
                    });
                    let control_runtime = Arc::clone(&runtime);
                    let control_handle =
                        tokio::spawn(async move { control_runtime.run_control().await });
                    let ingress_handle = client.map(|client| tokio::spawn(client.run()));

                    let _guard = signal.await;
                    runtime.close();
                    let _ = snapshot_handle.await;
                    let _ = consumer_handle.await;
                    let _ = control_handle.await;
                    if let Some(handle) = ingress_handle {
                        let _ = handle.await;
                    }
                })
            });
            info!(
                status = ?startup_status,
                registry_empty,
                receive_only = true,
                "MEV trader Phase A receive-only runtime started"
            );
            Ok(())
        })
    }
}

/// Public exact-1 installer called from standard-node assembly before flashblocks config is moved.
#[derive(Debug, Default, Clone, Copy)]
pub struct MevTraderPhaseAInstaller;

impl MevTraderPhaseAInstaller {
    /// Installs exactly one extension only for `MEV_TRADER_PHASE_A=1` plus flashblocks `Some`.
    pub fn maybe_install(
        runner: &mut BaseNodeRunner,
        flashblocks_config: &Option<FlashblocksConfig>,
        env: Option<&OsStr>,
    ) {
        if let Some(config) = BaseNodeTraderConfig::from_inputs(flashblocks_config, env) {
            runner.install_ext::<BaseNodeTraderExtension>(config);
        }
    }
}

#[cfg(feature = "t4a-shadow")]
mod t4a_provisioning {
    use std::{
        ffi::OsStr,
        fs::{File, OpenOptions},
        io::{Read, Take},
        os::unix::{
            fs::{MetadataExt, OpenOptionsExt},
            io::AsRawFd,
        },
        path::PathBuf,
        str::FromStr,
    };

    use base_mev_trader::{
        AuditedWriteKey, BitmapWordRead, DescriptorPlanDigest, ExactProtocol, FieldKind, FieldRead,
        InitializedTickRead, MevTraderRuntimeConfig, PoolDescriptor, PoolUniverseSnapshot,
        ProvisionedPoolRegistry, RegistryDigest, StorageReadPlan, V3ReadPlan,
    };
    use eyre::{WrapErr, bail, eyre};
    use serde::Deserialize;

    const POOL_UNIVERSE_PATH: &str = "/home/ubuntu/.config/base-mev/t4a-pool-universe-v1.json";
    const MAX_POOL_UNIVERSE_BYTES: u64 = 8 * 1024 * 1024;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ProvisionedRegistryFile {
        version: u8,
        registry_digest: String,
        descriptors: Vec<PoolDescriptorFile>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PoolDescriptorFile {
        pool: String,
        protocol: ProtocolFile,
        token0: String,
        token1: String,
        decimals0: u8,
        decimals1: u8,
        fee: u32,
        code_hash: String,
        read_plan: StorageReadPlanFile,
        audited_writes: Vec<AuditedWriteKeyFile>,
        descriptor_digest: String,
    }

    #[derive(Debug, Deserialize)]
    enum ProtocolFile {
        UniswapV2,
        AerodromeVolatile,
        AerodromeStable,
        UniswapV3,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    enum AuditedWriteKeyFile {
        AccountBalance { address: String, evidence_digest: String },
        AccountNonce { address: String, evidence_digest: String },
        Storage { address: String, slot: String, evidence_digest: String },
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    enum StorageReadPlanFile {
        ConstantProduct {
            reserve0: FieldReadFile,
            reserve1: FieldReadFile,
        },
        Stable {
            reserve0: FieldReadFile,
            reserve1: FieldReadFile,
            stable: FieldReadFile,
        },
        V3 {
            sqrt_price_x96: FieldReadFile,
            liquidity: FieldReadFile,
            current_tick: FieldReadFile,
            tick_spacing: i32,
            lower_word: i16,
            upper_word: i16,
            words: Vec<BitmapWordReadFile>,
            lower_sentinel: BitmapWordReadFile,
            upper_sentinel: BitmapWordReadFile,
            initialized_ticks: Vec<InitializedTickReadFile>,
            coverage_digest: String,
        },
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct FieldReadFile {
        kind: FieldKindFile,
        slot: String,
        bit_offset: u16,
        bit_width: u16,
        signed: bool,
    }

    #[derive(Debug, Deserialize)]
    enum FieldKindFile {
        Reserve0,
        Reserve1,
        StableFlag,
        SqrtPriceX96,
        Liquidity,
        CurrentTick,
        LiquidityGross,
        LiquidityNet,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct BitmapWordReadFile {
        word_position: i16,
        slot: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct InitializedTickReadFile {
        tick: i32,
        liquidity_gross: FieldReadFile,
        liquidity_net: FieldReadFile,
    }

    pub(super) fn runtime_config() -> eyre::Result<MevTraderRuntimeConfig> {
        let bytes = read_pool_universe()?;
        if bytes.first() != Some(&b'{') || bytes.last() != Some(&b'}') {
            bail!("T4a pool universe must be one exact JSON object without surrounding bytes");
        }
        let file: ProvisionedRegistryFile =
            serde_json::from_slice(&bytes).wrap_err("invalid T4a pool universe schema")?;
        if file.version != 1 {
            bail!("unsupported T4a pool universe version");
        }
        if file.descriptors.is_empty() {
            bail!("T4a pool universe descriptors must be nonempty");
        }

        let descriptors = file
            .descriptors
            .into_iter()
            .map(PoolDescriptor::try_from)
            .collect::<eyre::Result<Vec<_>>>()?;
        let registry = ProvisionedPoolRegistry::new(
            descriptors,
            RegistryDigest(parse_fixed(&file.registry_digest, "registry_digest")?),
        )
        .wrap_err("T4a provisioned registry validation failed")?;
        let snapshot = PoolUniverseSnapshot::capture(&registry)
            .wrap_err("T4a pool universe snapshot validation failed")?;
        MevTraderRuntimeConfig::shadow(snapshot).wrap_err("T4a shadow runtime configuration failed")
    }

    fn read_pool_universe() -> eyre::Result<Vec<u8>> {
        let mut directory = File::open("/").wrap_err("failed to open filesystem root")?;
        for component in ["home", "ubuntu", ".config", "base-mev"] {
            directory = open_child(&directory, component, true)
                .wrap_err_with(|| format!("unsafe T4a pool universe ancestor: {component}"))?;
        }
        let file =
            open_child(&directory, "t4a-pool-universe-v1.json", false).wrap_err_with(|| {
                format!("failed to open {POOL_UNIVERSE_PATH} without following symlinks")
            })?;
        let metadata = file.metadata().wrap_err("failed to inspect T4a pool universe")?;
        if !metadata.file_type().is_file() {
            bail!("T4a pool universe is not a regular file");
        }
        if metadata.mode() & 0o7777 != 0o600 {
            bail!("T4a pool universe mode must be 0600");
        }
        // SAFETY: `geteuid` has no arguments, does not dereference memory, and has no
        // preconditions.
        let service_uid = unsafe { libc::geteuid() };
        if metadata.uid() != service_uid {
            bail!("T4a pool universe is not owned by the service uid");
        }

        let mut bytes = Vec::new();
        let mut bounded: Take<File> = file.take(MAX_POOL_UNIVERSE_BYTES + 1);
        bounded.read_to_end(&mut bytes).wrap_err("failed to read T4a pool universe")?;
        if bytes.len() as u64 > MAX_POOL_UNIVERSE_BYTES {
            bail!("T4a pool universe exceeds the size limit");
        }
        Ok(bytes)
    }

    fn open_child(parent: &File, child: &str, directory: bool) -> std::io::Result<File> {
        let mut path = PathBuf::from("/proc/self/fd");
        path.push(parent.as_raw_fd().to_string());
        path.push(OsStr::new(child));
        let mut flags = libc::O_NOFOLLOW | libc::O_CLOEXEC;
        if directory {
            flags |= libc::O_DIRECTORY;
        } else {
            flags |= libc::O_NONBLOCK;
        }
        OpenOptions::new().read(true).custom_flags(flags).open(path)
    }

    fn parse_fixed<T>(value: &str, field: &'static str) -> eyre::Result<T>
    where
        T: FromStr,
        T::Err: std::fmt::Display,
    {
        value.parse().map_err(|error| eyre!("invalid {field}: {error}"))
    }

    impl TryFrom<PoolDescriptorFile> for PoolDescriptor {
        type Error = eyre::Report;

        fn try_from(value: PoolDescriptorFile) -> Result<Self, Self::Error> {
            Ok(Self {
                pool: parse_fixed(&value.pool, "pool")?,
                protocol: value.protocol.into(),
                token0: parse_fixed(&value.token0, "token0")?,
                token1: parse_fixed(&value.token1, "token1")?,
                decimals0: value.decimals0,
                decimals1: value.decimals1,
                fee: value.fee,
                code_hash: parse_fixed(&value.code_hash, "code_hash")?,
                read_plan: value.read_plan.try_into()?,
                audited_writes: value
                    .audited_writes
                    .into_iter()
                    .map(AuditedWriteKey::try_from)
                    .collect::<eyre::Result<Vec<_>>>()?,
                descriptor_digest: DescriptorPlanDigest(parse_fixed(
                    &value.descriptor_digest,
                    "descriptor_digest",
                )?),
            })
        }
    }

    impl From<ProtocolFile> for ExactProtocol {
        fn from(value: ProtocolFile) -> Self {
            match value {
                ProtocolFile::UniswapV2 => Self::UniswapV2,
                ProtocolFile::AerodromeVolatile => Self::AerodromeVolatile,
                ProtocolFile::AerodromeStable => Self::AerodromeStable,
                ProtocolFile::UniswapV3 => Self::UniswapV3,
            }
        }
    }

    impl TryFrom<AuditedWriteKeyFile> for AuditedWriteKey {
        type Error = eyre::Report;

        fn try_from(value: AuditedWriteKeyFile) -> Result<Self, Self::Error> {
            Ok(match value {
                AuditedWriteKeyFile::AccountBalance { address, evidence_digest } => {
                    Self::AccountBalance {
                        address: parse_fixed(&address, "audited address")?,
                        evidence_digest: parse_fixed(&evidence_digest, "evidence_digest")?,
                    }
                }
                AuditedWriteKeyFile::AccountNonce { address, evidence_digest } => {
                    Self::AccountNonce {
                        address: parse_fixed(&address, "audited address")?,
                        evidence_digest: parse_fixed(&evidence_digest, "evidence_digest")?,
                    }
                }
                AuditedWriteKeyFile::Storage { address, slot, evidence_digest } => Self::Storage {
                    address: parse_fixed(&address, "audited address")?,
                    slot: parse_fixed(&slot, "audited slot")?,
                    evidence_digest: parse_fixed(&evidence_digest, "evidence_digest")?,
                },
            })
        }
    }

    impl TryFrom<StorageReadPlanFile> for StorageReadPlan {
        type Error = eyre::Report;

        fn try_from(value: StorageReadPlanFile) -> Result<Self, Self::Error> {
            Ok(match value {
                StorageReadPlanFile::ConstantProduct { reserve0, reserve1 } => {
                    Self::constant_product(reserve0.try_into()?, reserve1.try_into()?)
                }
                StorageReadPlanFile::Stable { reserve0, reserve1, stable } => {
                    Self::stable(reserve0.try_into()?, reserve1.try_into()?, stable.try_into()?)
                }
                StorageReadPlanFile::V3 {
                    sqrt_price_x96,
                    liquidity,
                    current_tick,
                    tick_spacing,
                    lower_word,
                    upper_word,
                    words,
                    lower_sentinel,
                    upper_sentinel,
                    initialized_ticks,
                    coverage_digest,
                } => Self::v3(V3ReadPlan {
                    sqrt_price_x96: sqrt_price_x96.try_into()?,
                    liquidity: liquidity.try_into()?,
                    current_tick: current_tick.try_into()?,
                    tick_spacing,
                    lower_word,
                    upper_word,
                    words: words
                        .into_iter()
                        .map(BitmapWordRead::try_from)
                        .collect::<Result<_, _>>()?,
                    lower_sentinel: lower_sentinel.try_into()?,
                    upper_sentinel: upper_sentinel.try_into()?,
                    initialized_ticks: initialized_ticks
                        .into_iter()
                        .map(InitializedTickRead::try_from)
                        .collect::<Result<_, _>>()?,
                    coverage_digest: parse_fixed(&coverage_digest, "coverage_digest")?,
                }),
            })
        }
    }

    impl TryFrom<FieldReadFile> for FieldRead {
        type Error = eyre::Report;

        fn try_from(value: FieldReadFile) -> Result<Self, Self::Error> {
            Ok(Self {
                kind: value.kind.into(),
                slot: parse_fixed(&value.slot, "field slot")?,
                bit_offset: value.bit_offset,
                bit_width: value.bit_width,
                signed: value.signed,
            })
        }
    }

    impl From<FieldKindFile> for FieldKind {
        fn from(value: FieldKindFile) -> Self {
            match value {
                FieldKindFile::Reserve0 => Self::Reserve0,
                FieldKindFile::Reserve1 => Self::Reserve1,
                FieldKindFile::StableFlag => Self::StableFlag,
                FieldKindFile::SqrtPriceX96 => Self::SqrtPriceX96,
                FieldKindFile::Liquidity => Self::Liquidity,
                FieldKindFile::CurrentTick => Self::CurrentTick,
                FieldKindFile::LiquidityGross => Self::LiquidityGross,
                FieldKindFile::LiquidityNet => Self::LiquidityNet,
            }
        }
    }

    impl TryFrom<BitmapWordReadFile> for BitmapWordRead {
        type Error = eyre::Report;

        fn try_from(value: BitmapWordReadFile) -> Result<Self, Self::Error> {
            Ok(Self {
                word_position: value.word_position,
                slot: parse_fixed(&value.slot, "bitmap slot")?,
            })
        }
    }

    impl TryFrom<InitializedTickReadFile> for InitializedTickRead {
        type Error = eyre::Report;

        fn try_from(value: InitializedTickReadFile) -> Result<Self, Self::Error> {
            Ok(Self {
                tick: value.tick,
                liquidity_gross: value.liquidity_gross.try_into()?,
                liquidity_net: value.liquidity_net.try_into()?,
            })
        }
    }
}

fn t4a_runtime_config(enabled: bool) -> eyre::Result<MevTraderRuntimeConfig> {
    #[cfg(feature = "t4a-shadow")]
    if enabled {
        return t4a_provisioning::runtime_config();
    }
    let _ = enabled;
    MevTraderRuntimeConfig::empty().map_err(Into::into)
}
#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bloom, Bytes, U256};
    use base_common_flashblocks::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
    };
    use base_flashblocks::PendingBlocksBuilder;

    use super::*;

    fn pending_blocks() -> PendingBlocks {
        let parent_hash = B256::with_last_byte(1);
        let mut builder = PendingBlocksBuilder::default();
        builder.with_flashblocks([Flashblock {
            payload_id: Default::default(),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::ZERO,
                parent_hash,
                fee_recipient: Address::ZERO,
                prev_randao: B256::ZERO,
                block_number: 100,
                gas_limit: 30_000_000,
                timestamp: 1,
                extra_data: Bytes::new(),
                base_fee_per_gas: U256::from(1),
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::default(),
                gas_used: 0,
                block_hash: B256::with_last_byte(2),
                transactions: Vec::new(),
                withdrawals: Vec::new(),
                withdrawals_root: B256::ZERO,
                blob_gas_used: None,
            },
            metadata: Metadata::new(100),
        }]);
        builder.with_header(Sealed::new_unchecked(
            Header { parent_hash, number: 100, ..Default::default() },
            B256::with_last_byte(2),
        ));
        builder.build().expect("pending blocks")
    }

    #[test]
    fn t4a_selected_closure_remains_measurement_only_zero_capability() {
        const CLI_MANIFEST: &str = include_str!("../Cargo.toml");
        const NODE_MANIFEST: &str = include_str!("../../../../bin/node/Cargo.toml");
        const TRADER_MANIFEST: &str = include_str!("../../mev-trader/Cargo.toml");
        const CLI_SOURCE: &str = include_str!("mev_trader.rs");
        const TASK_SPAWN: &str = concat!("tokio", "::spawn");
        const FLASHBLOCK_SUBSCRIBE: &str = concat!("subscribe_to_", "flashblocks");

        let cli_feature = CLI_MANIFEST
            .split_once("t4a-shadow = [")
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(feature, _)| feature)
            .expect("CLI t4a-shadow feature");
        let selected_members = cli_feature
            .split(',')
            .map(|member| member.trim().trim_matches('"'))
            .filter(|member| !member.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(
            selected_members,
            ["base-mev-trader/t4a-shadow", "dep:libc", "dep:serde", "dep:serde_json",]
        );
        assert!(cli_feature.contains("\"base-mev-trader/t4a-shadow\""));
        for forbidden in ["mev-trader-submit", "reqwest", "signer", "assembly", "arm", "egress"] {
            assert!(
                !cli_feature.contains(forbidden),
                "forbidden T4a CLI feature edge: {forbidden}"
            );
        }

        assert!(
            NODE_MANIFEST.contains("t4a-shadow = [ \"base-execution-cli/t4a-shadow\" ]"),
            "node must forward only the T4a measurement feature"
        );
        assert!(
            TRADER_MANIFEST.contains("t4a-shadow = []"),
            "mev-trader T4a leaf feature must add no dependency edge"
        );

        let provisioning = CLI_SOURCE
            .split_once("#[cfg(feature = \"t4a-shadow\")]\nmod t4a_provisioning")
            .and_then(|(_, rest)| rest.split_once("\nfn t4a_runtime_config"))
            .map(|(source, _)| source)
            .expect("isolated T4a provisioning source");
        for forbidden in [
            "send_gated(",
            "mev-trader-submit",
            "reqwest::",
            "signer.",
            TASK_SPAWN,
            FLASHBLOCK_SUBSCRIBE,
        ] {
            assert!(
                !provisioning.contains(forbidden),
                "forbidden T4a provisioning seam: {forbidden}"
            );
        }
        assert_eq!(CLI_SOURCE.matches(TASK_SPAWN).count(), 4);
        assert_eq!(CLI_SOURCE.matches(FLASHBLOCK_SUBSCRIBE).count(), 1);
    }

    #[test]
    fn adapter_requires_live_some_and_pointer_identical_capture() {
        let flashblocks = Arc::new(FlashblocksState::default());
        flashblocks.set_pending_blocks_for_testing(Some(pending_blocks()));
        let current = flashblocks.get_pending_blocks();
        let captured = Arc::clone(current.as_ref().expect("current pending"));
        drop(current);

        let port = CliTraderSnapshotPort::new(Arc::clone(&flashblocks), ());
        port.record_pending_snapshot(captured, Instant::now());
        let record = port.current_record().expect("captured record");
        assert!(port.record_is_current(&record));

        flashblocks.set_pending_blocks_for_testing(None);
        assert!(!port.record_is_current(&record));
    }
}
