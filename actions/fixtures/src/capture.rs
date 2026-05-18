use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use alloy_consensus::{Receipt, Transaction as _, TxEnvelope, constants::EMPTY_TRANSACTIONS};
use alloy_eips::Encodable2718;
use alloy_primitives::{B256, Bytes};
use alloy_rlp::Decodable;
use base_common_chains::ChainConfig as BaseChainConfig;
use base_common_consensus::{BaseBlock, BaseReceiptEnvelope};
use base_common_genesis::RollupConfig;
use base_common_rpc_types::{BaseTransactionReceipt, Transaction};
use base_consensus_derive::{PipelineError, PipelineErrorKind, ResetError};
use base_protocol::{BlockInfo, L2BlockInfo, to_system_config};
use clap::Parser;
use futures::{StreamExt, stream};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tracing::info;

use crate::{
    ActionFixture, DerivationFixture, ExpectedOutcome, ExpectedPayload, FixtureKind,
    FixtureKindParseError, FixtureL1Block, FixtureL1DiskBlockError, FixtureL1DiskCodec,
    FixtureL2Block, FixtureLoader, FixtureLoaderError, FixtureManifest, FixturePaths,
    FixtureReplayError, StateRoot,
};

/// Concurrent L1 requests used while scanning derivation windows.
pub const L1_DERIVATION_CAPTURE_CONCURRENCY: usize = 16;

/// L1 blocks to fetch before probing whether derivation can complete.
pub const L1_DERIVATION_CAPTURE_CHUNK_SIZE: u64 = 512;

/// JSON-RPC request timeout used during fixture capture.
pub const FIXTURE_CAPTURE_RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// CLI input options for fixture capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureInput {
    /// Network name to record in the manifest.
    pub network: String,
    /// Fixture name to record in the manifest.
    pub name: String,
    /// Fixture kind to capture.
    pub kind: FixtureKind,
    /// Optional L1 RPC URL.
    pub l1_rpc_url: Option<String>,
    /// Optional L2 RPC URL.
    pub l2_rpc_url: Option<String>,
    /// Optional beacon API URL.
    pub beacon_url: Option<String>,
    /// Inclusive L1 start block number.
    pub l1_start: Option<u64>,
    /// Inclusive L1 end block number.
    pub l1_end: Option<u64>,
    /// Inclusive L2 start block number.
    pub l2_start: Option<u64>,
    /// Inclusive L2 end block number.
    pub l2_end: Option<u64>,
}

/// CLI output options for fixture capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureOutput {
    /// Output directory for the generated fixture.
    pub output: PathBuf,
    /// Whether to replace an existing fixture directory.
    pub overwrite: bool,
}

/// Capture command parsed by the localized fixture binary.
#[derive(Debug, Clone, Parser)]
#[command(name = "base-action-fixture-capture", about = "Capture real-chain action-test fixtures")]
pub struct CaptureCommand {
    /// Network name to record in the manifest.
    #[arg(long, env = "BASE_ACTION_FIXTURE_NETWORK", hide_env_values = true)]
    pub network: String,
    /// Fixture name to record in the manifest.
    #[arg(long, env = "BASE_ACTION_FIXTURE_NAME", hide_env_values = true)]
    pub name: String,
    /// Fixture kind: derivation or execution.
    #[arg(
        long,
        env = "BASE_ACTION_FIXTURE_KIND",
        default_value = "derivation",
        hide_env_values = true
    )]
    pub kind: String,
    /// L1 RPC URL.
    #[arg(long, env = "BASE_ACTION_FIXTURE_L1_RPC_URL", hide_env_values = true)]
    pub l1_rpc_url: Option<String>,
    /// L2 RPC URL.
    #[arg(long, env = "BASE_ACTION_FIXTURE_L2_RPC_URL", hide_env_values = true)]
    pub l2_rpc_url: Option<String>,
    /// Beacon API URL.
    #[arg(long, env = "BASE_ACTION_FIXTURE_BEACON_URL", hide_env_values = true)]
    pub beacon_url: Option<String>,
    /// Inclusive L1 start block number.
    #[arg(long, env = "BASE_ACTION_FIXTURE_L1_START", hide_env_values = true)]
    pub l1_start: Option<u64>,
    /// Inclusive L1 end block number.
    #[arg(long, env = "BASE_ACTION_FIXTURE_L1_END", hide_env_values = true)]
    pub l1_end: Option<u64>,
    /// Inclusive L2 start block number.
    #[arg(long, env = "BASE_ACTION_FIXTURE_L2_START", hide_env_values = true)]
    pub l2_start: Option<u64>,
    /// Inclusive L2 end block number.
    #[arg(long, env = "BASE_ACTION_FIXTURE_L2_END", hide_env_values = true)]
    pub l2_end: Option<u64>,
    /// Output directory for the generated fixture.
    ///
    /// Supports `{network}`, `{name}`, `{kind}`, `{l2-start}`, and `{l2-end}` placeholders.
    #[arg(
        long,
        env = "BASE_ACTION_FIXTURE_OUTPUT",
        default_value = "fixtures/{network}/{name}-l2-{l2-start}-{l2-end}",
        hide_env_values = true
    )]
    pub output: PathBuf,
    /// Replace an existing fixture directory.
    #[arg(long, env = "BASE_ACTION_FIXTURE_OVERWRITE", hide_env_values = true)]
    pub overwrite: bool,
}

impl CaptureCommand {
    /// Convert the parsed command to strongly typed input and output values.
    pub fn into_parts(self) -> Result<(CaptureInput, CaptureOutput), CaptureError> {
        let kind = self.kind.parse::<FixtureKind>()?;
        let input = CaptureInput {
            network: self.network,
            name: self.name,
            kind,
            l1_rpc_url: self.l1_rpc_url,
            l2_rpc_url: self.l2_rpc_url,
            beacon_url: self.beacon_url,
            l1_start: self.l1_start,
            l1_end: self.l1_end,
            l2_start: self.l2_start,
            l2_end: self.l2_end,
        };
        let output = CaptureOutput::new(self.output, &input, self.overwrite)?;
        Ok((input, output))
    }

    /// Execute fixture capture.
    pub async fn run(self) -> Result<(), CaptureError> {
        let (input, output) = self.into_parts()?;
        input.validate_ranges()?;
        info!(
            network = %input.network,
            name = %input.name,
            kind = %input.kind,
            output = ?output.output,
            "capturing fixture data"
        );
        let output_path = output.output.clone();
        let fixture = RpcFixtureCapture::capture(input, output).await?;
        info!(
            l1_blocks = fixture.l1_blocks.len(),
            l2_blocks = fixture.l2_blocks.len(),
            "captured fixture data"
        );
        println!(
            "captured fixture: {} (l1_blocks={}, l2_blocks={})",
            output_path.display(),
            fixture.l1_blocks.len(),
            fixture.l2_blocks.len()
        );
        Ok(())
    }
}

impl CaptureOutput {
    /// Create capture output options, anchoring relative paths at this crate.
    pub fn new(
        output: PathBuf,
        input: &CaptureInput,
        overwrite: bool,
    ) -> Result<Self, CaptureError> {
        let output = Self::expand_template(output, input)?;
        Ok(Self { output: Self::resolve_output(output), overwrite })
    }

    /// Resolve a capture output path.
    pub fn resolve_output(output: PathBuf) -> PathBuf {
        if output.is_absolute() {
            return output;
        }
        Path::new(env!("CARGO_MANIFEST_DIR")).join(output)
    }

    /// Expand supported placeholders in an output path template.
    pub fn expand_template(output: PathBuf, input: &CaptureInput) -> Result<PathBuf, CaptureError> {
        let Some(output) = output.to_str() else {
            return Err(CaptureError::NonUtf8OutputPath);
        };

        let output = output
            .replace("{network}", &input.network)
            .replace("{name}", &input.name)
            .replace("{kind}", &input.kind.to_string());
        let output = Self::replace_optional_placeholder(output, "{l2-start}", input.l2_start)?;
        let output = Self::replace_optional_placeholder(output, "{l2-end}", input.l2_end)?;
        Ok(PathBuf::from(output))
    }

    /// Replace one optional numeric placeholder.
    pub fn replace_optional_placeholder(
        output: String,
        placeholder: &'static str,
        value: Option<u64>,
    ) -> Result<String, CaptureError> {
        if !output.contains(placeholder) {
            return Ok(output);
        }
        let value = value.ok_or(CaptureError::MissingOutputPlaceholderValue { placeholder })?;
        Ok(output.replace(placeholder, &value.to_string()))
    }
}

impl CaptureInput {
    /// Validate block-range arguments.
    pub const fn validate_ranges(&self) -> Result<(), CaptureError> {
        match (self.l1_start, self.l1_end) {
            (Some(start), Some(end)) if end < start => {
                return Err(CaptureError::InvalidRange { chain: "l1", start, end });
            }
            _ => {}
        }
        match (self.l2_start, self.l2_end) {
            (Some(start), Some(end)) if end < start => {
                return Err(CaptureError::InvalidRange { chain: "l2", start, end });
            }
            _ => {}
        }
        Ok(())
    }
}

/// Captures checked-in action fixtures from live JSON-RPC data.
#[derive(Debug, Clone, Copy, Default)]
pub struct RpcFixtureCapture;

impl RpcFixtureCapture {
    /// Capture a fixture and write it to the requested output directory.
    pub async fn capture(
        input: CaptureInput,
        output: CaptureOutput,
    ) -> Result<ActionFixture, CaptureError> {
        let fixture = Self::capture_fixture(&input).await?;
        Self::write_fixture(&output.output, &fixture, output.overwrite)?;
        FixtureLoader::load_dir(&output.output)?;
        Ok(fixture)
    }

    /// Capture the typed fixture data without writing files.
    pub async fn capture_fixture(input: &CaptureInput) -> Result<ActionFixture, CaptureError> {
        let l2_rpc_url = input
            .l2_rpc_url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
            .ok_or(CaptureError::MissingRpcUrl { chain: "l2" })?;
        let (l2_start, l2_end) = Self::required_range("l2", input.l2_start, input.l2_end)?;

        let client = reqwest::Client::builder()
            .timeout(FIXTURE_CAPTURE_RPC_TIMEOUT)
            .build()
            .map_err(|source| CaptureError::RpcClient { error: source.to_string() })?;
        let rollup_config = Self::rollup_config_for_network(&input.network)?;
        let mut l2_blocks = Self::capture_l2_range(&client, l2_rpc_url, l2_start, l2_end).await?;
        Self::populate_l2_origins(&mut l2_blocks, &rollup_config)?;
        let derivation =
            Self::capture_derivation_anchor(&client, l2_rpc_url, l2_start, &rollup_config).await?;
        let l1_blocks = Self::capture_l1_blocks_for_derivation(
            &client,
            input,
            &derivation,
            &l2_blocks,
            &rollup_config,
        )
        .await?;
        let mut manifest =
            FixtureManifest::new(input.name.clone(), input.network.clone(), input.kind);
        manifest.source = "rpc-capture".to_owned();
        manifest.l1_start = l1_blocks.first().map(FixtureL1Block::id);
        manifest.l1_end = l1_blocks.last().map(FixtureL1Block::id);
        manifest.l2_start = l2_blocks.first().map(FixtureL2Block::id);
        manifest.l2_end = l2_blocks.last().map(FixtureL2Block::id);

        let expected = Self::expected_outcome(&l2_blocks);
        Ok(ActionFixture::new(manifest, l1_blocks, l2_blocks, expected).with_derivation(derivation))
    }

    /// Return a required inclusive block range.
    pub const fn required_range(
        chain: &'static str,
        start: Option<u64>,
        end: Option<u64>,
    ) -> Result<(u64, u64), CaptureError> {
        match (start, end) {
            (Some(start), Some(end)) => Ok((start, end)),
            _ => Err(CaptureError::MissingRange { chain }),
        }
    }

    /// Return the rollup config for a supported fixture network.
    pub fn rollup_config_for_network(network: &str) -> Result<RollupConfig, CaptureError> {
        let chain_id = FixtureManifest::chain_id_for_network(network)
            .ok_or_else(|| CaptureError::UnsupportedNetwork { network: network.to_owned() })?;
        BaseChainConfig::by_chain_id(chain_id)
            .map(BaseChainConfig::rollup_config)
            .ok_or(CaptureError::MissingRollupConfig { chain_id })
    }

    /// Populate each L2 fixture block with its decoded L1 origin.
    pub fn populate_l2_origins(
        blocks: &mut [FixtureL2Block],
        rollup_config: &RollupConfig,
    ) -> Result<Vec<L2BlockInfo>, CaptureError> {
        let mut infos = Vec::with_capacity(blocks.len());
        for block in blocks {
            let base_block = crate::ActionFixtureAdapter::l2_block(block)?;
            let info = L2BlockInfo::from_block_and_genesis(&base_block, &rollup_config.genesis)
                .map_err(|source| CaptureError::L2BlockInfo {
                    block_number: block.header.number,
                    error: source.to_string(),
                })?;
            block.l1_origin =
                Some(crate::BlockId { number: info.l1_origin.number, hash: info.l1_origin.hash });
            infos.push(info);
        }
        Ok(infos)
    }

    /// Capture the derivation safe-head anchor immediately before the L2 range.
    pub async fn capture_derivation_anchor(
        client: &reqwest::Client,
        l2_rpc_url: &str,
        l2_start: u64,
        rollup_config: &RollupConfig,
    ) -> Result<DerivationFixture, CaptureError> {
        if l2_start <= rollup_config.genesis.l2.number {
            return Err(CaptureError::InvalidDerivationStart { l2_start });
        }

        if l2_start == rollup_config.genesis.l2.number + 1 {
            let safe_head = L2BlockInfo {
                block_info: BlockInfo {
                    hash: rollup_config.genesis.l2.hash,
                    number: rollup_config.genesis.l2.number,
                    parent_hash: B256::ZERO,
                    timestamp: rollup_config.genesis.l2_time,
                },
                l1_origin: rollup_config.genesis.l1,
                seq_num: 0,
            };
            let system_config = rollup_config
                .genesis
                .system_config
                .ok_or(CaptureError::MissingGenesisSystemConfig)?;
            return Ok(DerivationFixture { safe_head, system_config, l2_history: vec![] });
        }

        let parent = Self::capture_l2_block(client, l2_rpc_url, l2_start - 1).await?;
        let parent_block = crate::ActionFixtureAdapter::l2_block(&parent)?;
        let safe_head = L2BlockInfo::from_block_and_genesis(&parent_block, &rollup_config.genesis)
            .map_err(|source| CaptureError::L2BlockInfo {
                block_number: parent.header.number,
                error: source.to_string(),
            })?;
        let system_config = to_system_config(&parent_block, rollup_config).map_err(|source| {
            CaptureError::SystemConfig {
                block_number: parent.header.number,
                error: source.to_string(),
            }
        })?;
        Ok(DerivationFixture { safe_head, system_config, l2_history: vec![parent] })
    }

    /// Capture the L1 block range needed to derive the expected L2 blocks.
    pub async fn capture_l1_blocks_for_derivation(
        client: &reqwest::Client,
        input: &CaptureInput,
        derivation: &DerivationFixture,
        l2_blocks: &[FixtureL2Block],
        rollup_config: &RollupConfig,
    ) -> Result<Vec<FixtureL1Block>, CaptureError> {
        let l1_rpc_url = input
            .l1_rpc_url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
            .ok_or(CaptureError::MissingRpcUrl { chain: "l1" })?;
        let default_start = derivation.safe_head.l1_origin.number;
        let start = input.l1_start.unwrap_or(default_start);
        if let Some(end) = input.l1_end {
            return Self::capture_l1_derivation_range(
                client,
                l1_rpc_url,
                start,
                end,
                rollup_config,
            )
            .await;
        }

        let scan_end =
            default_start.saturating_add(rollup_config.seq_window_size.saturating_add(1));
        let mut blocks = Vec::new();
        let mut next_start = start;
        while next_start <= scan_end {
            let next_end = next_start
                .saturating_add(L1_DERIVATION_CAPTURE_CHUNK_SIZE.saturating_sub(1))
                .min(scan_end);
            blocks.extend(
                Self::capture_l1_derivation_range(
                    client,
                    l1_rpc_url,
                    next_start,
                    next_end,
                    rollup_config,
                )
                .await?,
            );

            if let Some(required_end) =
                Self::derived_l1_end(input, derivation, l2_blocks, &blocks, rollup_config).await?
            {
                blocks.retain(|block| block.header.number <= required_end);
                return Ok(blocks);
            }

            next_start = next_end + 1;
        }

        Err(CaptureError::DerivationReplayIncomplete {
            start,
            end: scan_end,
            l2_start: l2_blocks.first().map(|block| block.header.number),
            l2_end: l2_blocks.last().map(|block| block.header.number),
        })
    }

    /// Return the highest L1 block needed by a successful replay, or `None` if more L1 data is needed.
    pub async fn derived_l1_end(
        input: &CaptureInput,
        derivation: &DerivationFixture,
        l2_blocks: &[FixtureL2Block],
        l1_blocks: &[FixtureL1Block],
        rollup_config: &RollupConfig,
    ) -> Result<Option<u64>, CaptureError> {
        let mut manifest =
            FixtureManifest::new(input.name.clone(), input.network.clone(), input.kind);
        manifest.source = "rpc-capture-probe".to_owned();
        manifest.l1_start = l1_blocks.first().map(FixtureL1Block::id);
        manifest.l1_end = l1_blocks.last().map(FixtureL1Block::id);
        manifest.l2_start = l2_blocks.first().map(FixtureL2Block::id);
        manifest.l2_end = l2_blocks.last().map(FixtureL2Block::id);

        let fixture = ActionFixture::new(
            manifest,
            l1_blocks.to_vec(),
            l2_blocks.to_vec(),
            Self::expected_outcome(l2_blocks),
        )
        .with_derivation(derivation.clone());

        match crate::DerivationFixtureReplayer::derive_payloads_with_rollup_config(
            &fixture,
            rollup_config.clone(),
        )
        .await
        {
            Ok(payloads) => {
                let end = payloads
                    .iter()
                    .filter_map(|payload| payload.derived_from())
                    .map(|block| block.number)
                    .max()
                    .unwrap_or_else(|| {
                        l1_blocks.last().map_or(derivation.safe_head.l1_origin.number, |block| {
                            block.header.number
                        })
                    });
                Ok(Some(end))
            }
            Err(error) if Self::replay_needs_more_l1(&error) => Ok(None),
            Err(error) => Err(CaptureError::DerivationReplay { error }),
        }
    }

    /// Return whether a replay error means the scanner has not captured enough L1 blocks yet.
    pub fn replay_needs_more_l1(error: &FixtureReplayError) -> bool {
        match error {
            FixtureReplayError::Pipeline { source, .. } => matches!(
                source.as_ref(),
                PipelineErrorKind::Temporary(
                    PipelineError::Eof
                        | PipelineError::NotEnoughData
                        | PipelineError::ChannelReaderEmpty
                        | PipelineError::Provider(_)
                ) | PipelineErrorKind::Reset(ResetError::BlockNotFound(_))
            ),
            _ => false,
        }
    }

    /// Return the capacity for an inclusive block range.
    pub const fn inclusive_range_capacity(start: u64, end: u64) -> usize {
        end.saturating_sub(start).saturating_add(1) as usize
    }

    /// Capture an inclusive L2 block range.
    pub async fn capture_l2_range(
        client: &reqwest::Client,
        rpc_url: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<FixtureL2Block>, CaptureError> {
        let mut blocks = Vec::with_capacity(Self::inclusive_range_capacity(start, end));
        for number in start..=end {
            blocks.push(Self::capture_l2_block(client, rpc_url, number).await?);
        }
        Ok(blocks)
    }

    /// Capture one L2 block and its receipts.
    pub async fn capture_l2_block(
        client: &reqwest::Client,
        rpc_url: &str,
        number: u64,
    ) -> Result<FixtureL2Block, CaptureError> {
        let block = Self::fetch_l2_block(client, rpc_url, number).await?;
        let alloy_rpc_types_eth::Block { header, transactions, .. } = block;
        let rpc_hash = header.hash;
        let header = header.into_consensus();
        Self::validate_header_hash("l2", number, rpc_hash, header.hash_slow())?;

        let transactions =
            Self::resolve_l2_transactions(client, rpc_url, number, transactions).await?;
        if transactions.is_empty() && header.transactions_root != EMPTY_TRANSACTIONS {
            return Self::capture_l2_block_from_raw(client, rpc_url, number, rpc_hash).await;
        }
        let mut raw_transactions = Vec::with_capacity(transactions.len());
        let mut receipts = Vec::with_capacity(transactions.len());
        for transaction in transactions {
            let tx_hash = transaction.as_ref().tx_hash();
            let raw = transaction.as_ref().encoded_2718();
            let receipt = Self::fetch_l2_receipt(client, rpc_url, tx_hash).await?;
            raw_transactions.push(Bytes::from(raw));
            receipts.push(receipt);
        }

        Ok(FixtureL2Block { header, transactions: raw_transactions, receipts, l1_origin: None })
    }

    /// Capture one L2 block by decoding `debug_getRawBlock`.
    pub async fn capture_l2_block_from_raw(
        client: &reqwest::Client,
        rpc_url: &str,
        number: u64,
        rpc_hash: B256,
    ) -> Result<FixtureL2Block, CaptureError> {
        let raw_block = Self::fetch_raw_l2_block(client, rpc_url, number).await?;
        let mut raw_block = raw_block.as_ref();
        let block =
            BaseBlock::decode(&mut raw_block).map_err(|source| CaptureError::RawBlockDecode {
                chain: "l2",
                block_number: number,
                error: source.to_string(),
            })?;
        let header = block.header;
        Self::validate_header_hash("l2", number, rpc_hash, header.hash_slow())?;

        let mut raw_transactions = Vec::with_capacity(block.body.transactions.len());
        let mut receipts = Vec::with_capacity(block.body.transactions.len());
        for transaction in block.body.transactions {
            let tx_hash = transaction.tx_hash();
            let raw = transaction.encoded_2718();
            let receipt = Self::fetch_l2_receipt(client, rpc_url, tx_hash).await?;
            raw_transactions.push(Bytes::from(raw));
            receipts.push(receipt);
        }

        Ok(FixtureL2Block { header, transactions: raw_transactions, receipts, l1_origin: None })
    }

    /// Resolve full L2 transaction objects from a block response.
    pub async fn resolve_l2_transactions(
        client: &reqwest::Client,
        rpc_url: &str,
        block_number: u64,
        transactions: alloy_rpc_types_eth::BlockTransactions<Transaction>,
    ) -> Result<Vec<Transaction>, CaptureError> {
        match transactions {
            alloy_rpc_types_eth::BlockTransactions::Full(transactions) => Ok(transactions),
            alloy_rpc_types_eth::BlockTransactions::Hashes(hashes) => {
                let mut transactions = Vec::with_capacity(hashes.len());
                for hash in hashes {
                    transactions.push(Self::fetch_l2_transaction(client, rpc_url, hash).await?);
                }
                Ok(transactions)
            }
            alloy_rpc_types_eth::BlockTransactions::Uncle => {
                Err(CaptureError::BlockTransactionsUnavailable { chain: "l2", block_number })
            }
        }
    }

    /// Capture an inclusive L1 block range.
    pub async fn capture_l1_range(
        client: &reqwest::Client,
        rpc_url: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<FixtureL1Block>, CaptureError> {
        let mut blocks = Vec::with_capacity(Self::inclusive_range_capacity(start, end));
        for number in start..=end {
            blocks.push(Self::capture_l1_block(client, rpc_url, number).await?);
        }
        Ok(blocks)
    }

    /// Capture an inclusive L1 range, retaining bodies only for derivation-relevant blocks.
    pub async fn capture_l1_derivation_range(
        client: &reqwest::Client,
        rpc_url: &str,
        start: u64,
        end: u64,
        rollup_config: &RollupConfig,
    ) -> Result<Vec<FixtureL1Block>, CaptureError> {
        let mut blocks = Vec::with_capacity(Self::inclusive_range_capacity(start, end));
        let captures = stream::iter(start..=end)
            .map(|number| Self::capture_l1_derivation_block(client, rpc_url, number, rollup_config))
            .buffered(L1_DERIVATION_CAPTURE_CONCURRENCY);
        futures::pin_mut!(captures);
        while let Some(block) = captures.next().await {
            blocks.push(block?);
        }
        Ok(blocks)
    }

    /// Capture one L1 derivation block, pruning bodies without derivation inputs.
    pub async fn capture_l1_derivation_block(
        client: &reqwest::Client,
        rpc_url: &str,
        number: u64,
        rollup_config: &RollupConfig,
    ) -> Result<FixtureL1Block, CaptureError> {
        let block = Self::fetch_l1_block(client, rpc_url, number).await?;
        let alloy_rpc_types_eth::Block { header, transactions, .. } = block;
        let rpc_hash = header.hash;
        let header = header.into_consensus();
        Self::validate_header_hash("l1", number, rpc_hash, header.hash_slow())?;

        let transactions =
            Self::resolve_l1_transactions(client, rpc_url, number, transactions).await?;
        if !Self::contains_derivation_l1_transaction(&transactions, rollup_config) {
            return Ok(FixtureL1Block {
                header,
                transactions: vec![],
                receipts: vec![],
                blobs: vec![],
            });
        }

        Self::capture_l1_derivation_block_from_parts(
            client,
            rpc_url,
            header,
            transactions,
            rollup_config,
        )
        .await
    }

    /// Capture an inclusive L1 header range without transaction or receipt bodies.
    pub async fn capture_l1_header_range(
        client: &reqwest::Client,
        rpc_url: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<FixtureL1Block>, CaptureError> {
        let mut blocks = Vec::with_capacity(Self::inclusive_range_capacity(start, end));
        for number in start..=end {
            blocks.push(Self::capture_l1_header_block(client, rpc_url, number).await?);
        }
        Ok(blocks)
    }

    /// Capture one L1 header without transaction or receipt bodies.
    pub async fn capture_l1_header_block(
        client: &reqwest::Client,
        rpc_url: &str,
        number: u64,
    ) -> Result<FixtureL1Block, CaptureError> {
        let block = Self::fetch_l1_header_block(client, rpc_url, number).await?;
        let rpc_hash = block.header.hash;
        let header = block.header.into_consensus();
        Self::validate_header_hash("l1", number, rpc_hash, header.hash_slow())?;
        Ok(FixtureL1Block { header, transactions: vec![], receipts: vec![], blobs: vec![] })
    }

    /// Capture one L1 block and its receipts.
    pub async fn capture_l1_block(
        client: &reqwest::Client,
        rpc_url: &str,
        number: u64,
    ) -> Result<FixtureL1Block, CaptureError> {
        let block = Self::fetch_l1_block(client, rpc_url, number).await?;
        let alloy_rpc_types_eth::Block { header, transactions, .. } = block;
        let rpc_hash = header.hash;
        let header = header.into_consensus();
        Self::validate_header_hash("l1", number, rpc_hash, header.hash_slow())?;

        let transactions =
            Self::resolve_l1_transactions(client, rpc_url, number, transactions).await?;
        Self::capture_l1_block_from_parts(client, rpc_url, header, transactions).await
    }

    /// Build a full L1 fixture block from an RPC header and full transactions.
    pub async fn capture_l1_block_from_parts(
        client: &reqwest::Client,
        rpc_url: &str,
        header: alloy_consensus::Header,
        transactions: Vec<alloy_rpc_types_eth::Transaction<TxEnvelope>>,
    ) -> Result<FixtureL1Block, CaptureError> {
        let mut raw_transactions = Vec::with_capacity(transactions.len());
        let mut receipts = Vec::with_capacity(transactions.len());
        for transaction in transactions {
            let tx_hash = *transaction.as_ref().tx_hash();
            let raw = transaction.as_ref().encoded_2718();
            let receipt = Self::fetch_l1_receipt(client, rpc_url, tx_hash).await?;
            raw_transactions.push(Bytes::from(raw));
            receipts.push(receipt);
        }

        Ok(FixtureL1Block { header, transactions: raw_transactions, receipts, blobs: vec![] })
    }

    /// Build a derivation fixture L1 block from only derivation-relevant transactions and receipts.
    pub async fn capture_l1_derivation_block_from_parts(
        client: &reqwest::Client,
        rpc_url: &str,
        header: alloy_consensus::Header,
        transactions: Vec<alloy_rpc_types_eth::Transaction<TxEnvelope>>,
        rollup_config: &RollupConfig,
    ) -> Result<FixtureL1Block, CaptureError> {
        let relevant = transactions
            .into_iter()
            .filter(|transaction| Self::is_derivation_l1_transaction(transaction, rollup_config));
        let mut raw_transactions = Vec::new();
        let mut receipts = Vec::new();
        for transaction in relevant {
            let tx_hash = *transaction.as_ref().tx_hash();
            let raw = transaction.as_ref().encoded_2718();
            let receipt = Self::fetch_l1_receipt(client, rpc_url, tx_hash).await?;
            raw_transactions.push(Bytes::from(raw));
            receipts.push(receipt);
        }

        Ok(FixtureL1Block { header, transactions: raw_transactions, receipts, blobs: vec![] })
    }

    /// Return whether any L1 transaction can affect derivation.
    pub fn contains_derivation_l1_transaction(
        transactions: &[alloy_rpc_types_eth::Transaction<TxEnvelope>],
        rollup_config: &RollupConfig,
    ) -> bool {
        transactions
            .iter()
            .any(|transaction| Self::is_derivation_l1_transaction(transaction, rollup_config))
    }

    /// Return whether an L1 transaction can affect derivation.
    pub fn is_derivation_l1_transaction(
        transaction: &alloy_rpc_types_eth::Transaction<TxEnvelope>,
        rollup_config: &RollupConfig,
    ) -> bool {
        let Some(to) = transaction.as_ref().to() else {
            return false;
        };
        to == rollup_config.batch_inbox_address
            || to == rollup_config.deposit_contract_address
            || to == rollup_config.l1_system_config_address
    }

    /// Resolve full L1 transaction objects from a block response.
    pub async fn resolve_l1_transactions(
        client: &reqwest::Client,
        rpc_url: &str,
        block_number: u64,
        transactions: alloy_rpc_types_eth::BlockTransactions<
            alloy_rpc_types_eth::Transaction<TxEnvelope>,
        >,
    ) -> Result<Vec<alloy_rpc_types_eth::Transaction<TxEnvelope>>, CaptureError> {
        match transactions {
            alloy_rpc_types_eth::BlockTransactions::Full(transactions) => Ok(transactions),
            alloy_rpc_types_eth::BlockTransactions::Hashes(hashes) => {
                let mut transactions = Vec::with_capacity(hashes.len());
                for hash in hashes {
                    transactions.push(Self::fetch_l1_transaction(client, rpc_url, hash).await?);
                }
                Ok(transactions)
            }
            alloy_rpc_types_eth::BlockTransactions::Uncle => {
                Err(CaptureError::BlockTransactionsUnavailable { chain: "l1", block_number })
            }
        }
    }

    /// Fetch a full L2 block by number.
    pub async fn fetch_l2_block(
        client: &reqwest::Client,
        rpc_url: &str,
        number: u64,
    ) -> Result<alloy_rpc_types_eth::Block<Transaction>, CaptureError> {
        let block_tag = format!("{number:#x}");
        Self::call_rpc(client, rpc_url, "eth_getBlockByNumber", json!([block_tag, true])).await
    }

    /// Fetch a raw Base L2 block by number.
    pub async fn fetch_raw_l2_block(
        client: &reqwest::Client,
        rpc_url: &str,
        number: u64,
    ) -> Result<Bytes, CaptureError> {
        let block_tag = format!("{number:#x}");
        Self::call_rpc(client, rpc_url, "debug_getRawBlock", json!([block_tag])).await
    }

    /// Fetch a full L2 transaction by hash.
    pub async fn fetch_l2_transaction(
        client: &reqwest::Client,
        rpc_url: &str,
        tx_hash: B256,
    ) -> Result<Transaction, CaptureError> {
        Self::call_rpc(client, rpc_url, "eth_getTransactionByHash", json!([tx_hash])).await
    }

    /// Fetch a full L1 block by number.
    pub async fn fetch_l1_block(
        client: &reqwest::Client,
        rpc_url: &str,
        number: u64,
    ) -> Result<
        alloy_rpc_types_eth::Block<alloy_rpc_types_eth::Transaction<TxEnvelope>>,
        CaptureError,
    > {
        let block_tag = format!("{number:#x}");
        Self::call_rpc(client, rpc_url, "eth_getBlockByNumber", json!([block_tag, true])).await
    }

    /// Fetch an L1 block header by number.
    pub async fn fetch_l1_header_block(
        client: &reqwest::Client,
        rpc_url: &str,
        number: u64,
    ) -> Result<
        alloy_rpc_types_eth::Block<alloy_rpc_types_eth::Transaction<TxEnvelope>>,
        CaptureError,
    > {
        let block_tag = format!("{number:#x}");
        Self::call_rpc(client, rpc_url, "eth_getBlockByNumber", json!([block_tag, false])).await
    }

    /// Fetch a full L1 transaction by hash.
    pub async fn fetch_l1_transaction(
        client: &reqwest::Client,
        rpc_url: &str,
        tx_hash: B256,
    ) -> Result<alloy_rpc_types_eth::Transaction<TxEnvelope>, CaptureError> {
        Self::call_rpc(client, rpc_url, "eth_getTransactionByHash", json!([tx_hash])).await
    }

    /// Fetch a Base L2 receipt by transaction hash and convert it to a consensus receipt.
    pub async fn fetch_l2_receipt(
        client: &reqwest::Client,
        rpc_url: &str,
        tx_hash: B256,
    ) -> Result<Receipt, CaptureError> {
        let receipt: BaseTransactionReceipt =
            Self::call_rpc(client, rpc_url, "eth_getTransactionReceipt", json!([tx_hash])).await?;
        let receipt_envelope = BaseReceiptEnvelope::from(receipt);
        Ok(receipt_envelope.into())
    }

    /// Fetch an L1 receipt by transaction hash and convert it to a consensus receipt.
    pub async fn fetch_l1_receipt(
        client: &reqwest::Client,
        rpc_url: &str,
        tx_hash: B256,
    ) -> Result<Receipt, CaptureError> {
        let receipt: alloy_rpc_types_eth::TransactionReceipt =
            Self::call_rpc(client, rpc_url, "eth_getTransactionReceipt", json!([tx_hash])).await?;
        Ok(receipt.into_primitives_receipt().into_inner().into_receipt())
    }

    /// Call a JSON-RPC method and deserialize its non-null `result`.
    pub async fn call_rpc<T>(
        client: &reqwest::Client,
        rpc_url: &str,
        method: &'static str,
        params: Value,
    ) -> Result<T, CaptureError>
    where
        T: DeserializeOwned,
    {
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let response = client.post(rpc_url).json(&payload).send().await.map_err(|source| {
            CaptureError::RpcRequest { method, error: source.without_url().to_string() }
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(CaptureError::RpcStatus { method, status: status.as_u16() });
        }

        let body = response.text().await.map_err(|source| CaptureError::RpcRequest {
            method,
            error: source.without_url().to_string(),
        })?;
        let response: Value = serde_json::from_str(&body)
            .map_err(|source| CaptureError::RpcJson { method, source })?;
        if let Some(error) = response.get("error") {
            return Err(CaptureError::RpcResponse { method, error: error.to_string() });
        }
        let result =
            response.get("result").ok_or(CaptureError::RpcMissingResult { method })?.clone();
        if result.is_null() {
            return Err(CaptureError::RpcNullResult { method });
        }
        serde_json::from_value(result).map_err(|source| CaptureError::RpcJson { method, source })
    }

    /// Validate that the RPC block hash matches the decoded consensus header.
    pub fn validate_header_hash(
        chain: &'static str,
        block_number: u64,
        rpc_hash: B256,
        computed_hash: B256,
    ) -> Result<(), CaptureError> {
        if rpc_hash != computed_hash {
            return Err(CaptureError::HeaderHashMismatch {
                chain,
                block_number,
                rpc_hash,
                computed_hash,
            });
        }
        Ok(())
    }

    /// Build expected outcomes from captured L2 blocks.
    pub fn expected_outcome(l2_blocks: &[FixtureL2Block]) -> ExpectedOutcome {
        ExpectedOutcome {
            safe_head: l2_blocks.last().map(FixtureL2Block::id),
            derived_payloads: l2_blocks
                .iter()
                .map(|block| {
                    let id = block.id();
                    ExpectedPayload {
                        number: id.number,
                        block_hash: Some(id.hash),
                        state_root: Some(block.header.state_root),
                    }
                })
                .collect(),
            state_roots: l2_blocks
                .iter()
                .map(|block| StateRoot {
                    number: block.header.number,
                    state_root: block.header.state_root,
                })
                .collect(),
        }
    }

    /// Write a fixture directory and its component files.
    pub fn write_fixture(
        path: &Path,
        fixture: &ActionFixture,
        overwrite: bool,
    ) -> Result<(), CaptureError> {
        if !overwrite && Self::contains_fixture_files(path) {
            return Err(CaptureError::OutputExists { path: path.to_path_buf() });
        }
        fs::create_dir_all(path)
            .map_err(|source| CaptureError::CreateOutput { path: path.to_path_buf(), source })?;
        Self::write_toml(&path.join(FixturePaths::MANIFEST), &fixture.manifest)?;
        let l1_blocks = FixtureL1DiskCodec::encode_blocks(&fixture.l1_blocks)?;
        Self::write_snappy_bincode(&path.join(FixturePaths::L1), &l1_blocks)?;
        Self::remove_legacy_file(&path.join(FixturePaths::L1_JSON_SNAP))?;
        Self::remove_legacy_file(&path.join(FixturePaths::L1_JSON))?;
        Self::write_json(&path.join(FixturePaths::L2), &fixture.l2_blocks)?;
        Self::write_json(&path.join(FixturePaths::EXPECTED), &fixture.expected)?;
        if let Some(derivation) = &fixture.derivation {
            Self::write_json(&path.join(FixturePaths::DERIVATION), derivation)?;
        }
        Ok(())
    }

    /// Return whether a path already contains fixture files.
    pub fn contains_fixture_files(path: &Path) -> bool {
        [
            FixturePaths::MANIFEST,
            FixturePaths::L1,
            FixturePaths::L1_JSON_SNAP,
            FixturePaths::L1_JSON,
            FixturePaths::L2,
            FixturePaths::EXPECTED,
            FixturePaths::DERIVATION,
        ]
        .iter()
        .any(|file| path.join(file).exists())
    }

    /// Remove an obsolete fixture file if it is present.
    pub fn remove_legacy_file(path: &Path) -> Result<(), CaptureError> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(CaptureError::Write { path: path.to_path_buf(), source }),
        }
    }

    /// Write one JSON file.
    pub fn write_json<T>(path: &Path, value: &T) -> Result<(), CaptureError>
    where
        T: Serialize,
    {
        let data = serde_json::to_vec_pretty(value)
            .map_err(|source| CaptureError::JsonSerialize { path: path.to_path_buf(), source })?;
        fs::write(path, data)
            .map_err(|source| CaptureError::Write { path: path.to_path_buf(), source })
    }

    /// Write one Snappy-compressed compact JSON file.
    pub fn write_snappy_json<T>(path: &Path, value: &T) -> Result<(), CaptureError>
    where
        T: Serialize,
    {
        let data = serde_json::to_vec(value)
            .map_err(|source| CaptureError::JsonSerialize { path: path.to_path_buf(), source })?;
        let data = snap::raw::Encoder::new()
            .compress_vec(&data)
            .map_err(|source| CaptureError::Snap { path: path.to_path_buf(), source })?;
        fs::write(path, data)
            .map_err(|source| CaptureError::Write { path: path.to_path_buf(), source })
    }

    /// Write one Snappy-compressed bincode file.
    pub fn write_snappy_bincode<T>(path: &Path, value: &T) -> Result<(), CaptureError>
    where
        T: Serialize,
    {
        let data = bincode::serde::encode_to_vec(value, bincode::config::standard())
            .map_err(|source| CaptureError::BincodeEncode { path: path.to_path_buf(), source })?;
        let data = snap::raw::Encoder::new()
            .compress_vec(&data)
            .map_err(|source| CaptureError::Snap { path: path.to_path_buf(), source })?;
        fs::write(path, data)
            .map_err(|source| CaptureError::Write { path: path.to_path_buf(), source })
    }

    /// Write one TOML file.
    pub fn write_toml<T>(path: &Path, value: &T) -> Result<(), CaptureError>
    where
        T: Serialize,
    {
        let data = toml::to_string_pretty(value)
            .map_err(|source| CaptureError::TomlSerialize { path: path.to_path_buf(), source })?;
        fs::write(path, data)
            .map_err(|source| CaptureError::Write { path: path.to_path_buf(), source })
    }
}

/// Fixture capture failure.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// Fixture kind parsing failed.
    #[error(transparent)]
    FixtureKind(#[from] FixtureKindParseError),
    /// The provided block range is invalid.
    #[error("{chain} range is invalid: end block {end} is before start block {start}")]
    InvalidRange {
        /// Chain label.
        chain: &'static str,
        /// Start block number.
        start: u64,
        /// End block number.
        end: u64,
    },
    /// A required RPC URL is missing.
    #[error("missing {chain} RPC URL")]
    MissingRpcUrl {
        /// Chain label.
        chain: &'static str,
    },
    /// A required block range is missing.
    #[error("missing {chain} block range")]
    MissingRange {
        /// Chain label.
        chain: &'static str,
    },
    /// The requested network is not supported by fixture capture.
    #[error("unsupported fixture network: {network}")]
    UnsupportedNetwork {
        /// Fixture network.
        network: String,
    },
    /// The rollup config registry is missing a chain ID.
    #[error("missing rollup config for chain ID {chain_id}")]
    MissingRollupConfig {
        /// L2 chain ID.
        chain_id: u64,
    },
    /// The requested derivation range starts at genesis rather than after a safe head.
    #[error("derivation fixtures must start after genesis, got L2 start {l2_start}")]
    InvalidDerivationStart {
        /// L2 start block.
        l2_start: u64,
    },
    /// The selected rollup config does not contain a genesis system config.
    #[error("rollup config is missing genesis system config")]
    MissingGenesisSystemConfig,
    /// Scanned the maximum derivation window without deriving the requested L2 range.
    #[error(
        "captured L1 blocks {start}..={end} did not derive requested L2 range {l2_start:?}..={l2_end:?}"
    )]
    DerivationReplayIncomplete {
        /// First scanned L1 block.
        start: u64,
        /// Last scanned L1 block.
        end: u64,
        /// First requested L2 block.
        l2_start: Option<u64>,
        /// Last requested L2 block.
        l2_end: Option<u64>,
    },
    /// Captured L1 data failed derivation replay for a non-retryable reason.
    #[error(transparent)]
    DerivationReplay {
        /// Replay failure.
        error: FixtureReplayError,
    },
    /// Output path could not be represented as UTF-8.
    #[error("fixture output path must be UTF-8 when it contains placeholders")]
    NonUtf8OutputPath,
    /// A path template placeholder requires a missing value.
    #[error("fixture output template uses {placeholder}, but no value was provided")]
    MissingOutputPlaceholderValue {
        /// Missing placeholder.
        placeholder: &'static str,
    },
    /// Output directory already contains fixture files.
    #[error("fixture output already exists at {path:?}; pass --overwrite to replace it")]
    OutputExists {
        /// Existing output path.
        path: PathBuf,
    },
    /// A captured L2 block could not be converted to `L2BlockInfo`.
    #[error("l2 block {block_number} failed to convert to L2BlockInfo: {error}")]
    L2BlockInfo {
        /// L2 block number.
        block_number: u64,
        /// Conversion error text.
        error: String,
    },
    /// A captured L2 block could not be converted to a system config.
    #[error("l2 block {block_number} failed to convert to SystemConfig: {error}")]
    SystemConfig {
        /// L2 block number.
        block_number: u64,
        /// Conversion error text.
        error: String,
    },
    /// A block response cannot provide full transactions.
    #[error("{chain} block {block_number} does not contain retrievable transactions")]
    BlockTransactionsUnavailable {
        /// Chain label.
        chain: &'static str,
        /// Block number.
        block_number: u64,
    },
    /// Raw block RLP could not be decoded.
    #[error("{chain} raw block {block_number} failed to decode: {error}")]
    RawBlockDecode {
        /// Chain label.
        chain: &'static str,
        /// Block number.
        block_number: u64,
        /// Decode error text.
        error: String,
    },
    /// RPC transport failed.
    #[error("RPC request failed for {method}: {error}")]
    RpcRequest {
        /// RPC method.
        method: &'static str,
        /// Redacted request error.
        error: String,
    },
    /// RPC client construction failed.
    #[error("failed to construct RPC client: {error}")]
    RpcClient {
        /// Client construction error.
        error: String,
    },
    /// RPC returned a non-success status.
    #[error("RPC request for {method} failed with HTTP status {status}")]
    RpcStatus {
        /// RPC method.
        method: &'static str,
        /// HTTP status code.
        status: u16,
    },
    /// RPC response JSON could not be decoded.
    #[error("failed to decode RPC response for {method}: {source}")]
    RpcJson {
        /// RPC method.
        method: &'static str,
        /// Underlying JSON error.
        source: serde_json::Error,
    },
    /// RPC response contained an error object.
    #[error("RPC method {method} returned an error: {error}")]
    RpcResponse {
        /// RPC method.
        method: &'static str,
        /// JSON-RPC error object.
        error: String,
    },
    /// RPC response did not contain a result.
    #[error("RPC method {method} returned no result")]
    RpcMissingResult {
        /// RPC method.
        method: &'static str,
    },
    /// RPC response contained a null result.
    #[error("RPC method {method} returned null")]
    RpcNullResult {
        /// RPC method.
        method: &'static str,
    },
    /// The RPC block hash did not match the decoded consensus header.
    #[error(
        "{chain} block {block_number} header hash mismatch: RPC {rpc_hash}, computed {computed_hash}"
    )]
    HeaderHashMismatch {
        /// Chain label.
        chain: &'static str,
        /// Block number.
        block_number: u64,
        /// RPC block hash.
        rpc_hash: B256,
        /// Locally computed header hash.
        computed_hash: B256,
    },
    /// Failed to create the output directory.
    #[error("failed to create fixture output directory {path:?}: {source}")]
    CreateOutput {
        /// Output path.
        path: PathBuf,
        /// Underlying IO error.
        source: std::io::Error,
    },
    /// Failed to write a fixture file.
    #[error("failed to write fixture file {path:?}: {source}")]
    Write {
        /// Output path.
        path: PathBuf,
        /// Underlying IO error.
        source: std::io::Error,
    },
    /// Failed to serialize JSON.
    #[error("failed to serialize fixture JSON {path:?}: {source}")]
    JsonSerialize {
        /// Output path.
        path: PathBuf,
        /// Underlying JSON error.
        source: serde_json::Error,
    },
    /// Failed to compress Snappy data.
    #[error("failed to compress fixture file {path:?}: {source}")]
    Snap {
        /// Output path.
        path: PathBuf,
        /// Underlying Snappy error.
        source: snap::Error,
    },
    /// Failed to encode bincode.
    #[error("failed to serialize fixture bincode {path:?}: {source}")]
    BincodeEncode {
        /// Output path.
        path: PathBuf,
        /// Underlying bincode error.
        source: bincode::error::EncodeError,
    },
    /// Failed to serialize TOML.
    #[error("failed to serialize fixture TOML {path:?}: {source}")]
    TomlSerialize {
        /// Output path.
        path: PathBuf,
        /// Underlying TOML error.
        source: toml::ser::Error,
    },
    /// Written fixture failed loader validation.
    #[error(transparent)]
    Loader(#[from] FixtureLoaderError),
    /// L1 disk block encoding failed.
    #[error(transparent)]
    L1Disk(#[from] FixtureL1DiskBlockError),
    /// Fixture adapter conversion failed.
    #[error(transparent)]
    Adapter(#[from] crate::FixtureAdapterError),
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;

    use crate::{
        CaptureInput, CaptureOutput, FixtureKind, FixtureL2Block, FixtureManifest, FixturePaths,
        RpcFixtureCapture,
    };

    #[test]
    fn rejects_inverted_ranges() {
        let input = CaptureInput {
            network: "base-mainnet".to_owned(),
            name: "window".to_owned(),
            kind: FixtureKind::Derivation,
            l1_rpc_url: None,
            l2_rpc_url: None,
            beacon_url: None,
            l1_start: Some(2),
            l1_end: Some(1),
            l2_start: None,
            l2_end: None,
        };
        assert!(input.validate_ranges().is_err());
    }

    #[test]
    fn requires_complete_ranges() {
        assert!(RpcFixtureCapture::required_range("l2", Some(1), None).is_err());
        assert_eq!(RpcFixtureCapture::required_range("l2", Some(1), Some(2)).unwrap(), (1, 2));
    }

    #[test]
    fn builds_expected_outcome_from_l2_blocks() {
        let blocks = vec![FixtureL2Block {
            header: Header { number: 1, ..Default::default() },
            transactions: vec![],
            receipts: vec![],
            l1_origin: None,
        }];
        let expected = RpcFixtureCapture::expected_outcome(&blocks);
        assert_eq!(expected.safe_head.unwrap().number, 1);
        assert_eq!(expected.derived_payloads.len(), 1);
        assert_eq!(expected.state_roots.len(), 1);
    }

    #[test]
    fn resolves_relative_output_under_crate() {
        let input = CaptureInput {
            network: "base-mainnet".to_owned(),
            name: "window".to_owned(),
            kind: FixtureKind::Derivation,
            l1_rpc_url: None,
            l2_rpc_url: None,
            beacon_url: None,
            l1_start: None,
            l1_end: None,
            l2_start: Some(1),
            l2_end: Some(2),
        };
        let output = CaptureOutput::new("fixtures/window".into(), &input, false).unwrap();
        assert!(output.output.ends_with("actions/fixtures/fixtures/window"));
    }

    #[test]
    fn expands_output_template_with_l2_range() {
        let input = CaptureInput {
            network: "base-mainnet".to_owned(),
            name: "window".to_owned(),
            kind: FixtureKind::Derivation,
            l1_rpc_url: None,
            l2_rpc_url: None,
            beacon_url: None,
            l1_start: None,
            l1_end: None,
            l2_start: Some(1),
            l2_end: Some(2),
        };
        let output = CaptureOutput::new(
            "fixtures/{network}/{name}-l2-{l2-start}-{l2-end}".into(),
            &input,
            false,
        )
        .unwrap();
        assert!(output.output.ends_with("fixtures/base-mainnet/window-l2-1-2"));
    }

    #[test]
    fn rejects_missing_output_template_value() {
        let input = CaptureInput {
            network: "base-mainnet".to_owned(),
            name: "window".to_owned(),
            kind: FixtureKind::Derivation,
            l1_rpc_url: None,
            l2_rpc_url: None,
            beacon_url: None,
            l1_start: None,
            l1_end: None,
            l2_start: Some(1),
            l2_end: None,
        };
        assert!(
            CaptureOutput::new(
                "fixtures/{network}/{name}-l2-{l2-start}-{l2-end}".into(),
                &input,
                false
            )
            .is_err()
        );
    }

    #[test]
    fn refuses_to_overwrite_existing_fixture_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FixturePaths::MANIFEST), "").unwrap();
        let fixture = crate::ActionFixture::new(
            FixtureManifest::new("window", "base-mainnet", FixtureKind::Derivation),
            vec![],
            vec![],
            Default::default(),
        );
        assert!(RpcFixtureCapture::write_fixture(dir.path(), &fixture, false).is_err());
    }
}
