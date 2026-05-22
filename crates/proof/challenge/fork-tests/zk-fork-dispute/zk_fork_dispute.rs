//! Manual fork test for disputing an invalid game with a ZK proof.
//!
//! This test is ignored because it requires an externally prepared fork:
//! - an L1 fork RPC with a dispute game containing a bad intermediate root
//! - an L2 RPC or rollup RPC that can return canonical output roots for the same L2 blocks
//! - a running `base-prover-zk` gRPC service
//!
//! By default, config is read from `fork-tests/zk-fork-dispute/<chain>.yaml`.
//! Each field can be overridden with its matching `BASE_ZK_FORK_*` env var.

use std::{
    env,
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use base_challenger::{ChallengeSubmitter, DisputeIntent};
use base_common_consensus::Predeploys;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, DisputeGameFactoryClient,
    DisputeGameFactoryContractClient, GameStatus, encode_extra_data,
};
use base_proof_primitives::PROOF_TYPE_ZK;
use base_proof_rpc::{L2HttpProvider, RollupClient, RollupClientConfig, RollupProvider};
use base_protocol::OutputRoot;
use base_tx_manager::{NoopTxMetrics, SignerConfig, SimpleTxManager, TxManagerConfig};
use base_zk_client::{
    GetProofRequest, ProofJobStatus, ProofType, ProveBlockRequest, ReceiptType, ZkProofClient,
    ZkProofClientConfig,
};
use eyre::{Context, Result, bail, eyre};
use serde_yaml::Value;
use url::Url;

const DEFAULT_POLL_INTERVAL_SECS: u64 = 30;
const DEFAULT_POLL_TIMEOUT_SECS: u64 = 4 * 60 * 60;
const DEFAULT_CONFIG_DIR: &str = "fork-tests/zk-fork-dispute";

#[derive(Debug)]
struct ForkDisputeConfig {
    l1_rpc_url: Url,
    l2_rpc_url: Option<Url>,
    rollup_rpc_url: Option<Url>,
    prover_grpc_url: Url,
    dispute_game_factory: Address,
    game_address: Address,
    game_type: Option<u32>,
    private_key: PrivateKeySigner,
    intent: DisputeIntent,
    invalid_index: Option<u64>,
    patch_invalid_game: bool,
    poll_interval: Duration,
    poll_timeout: Duration,
}

#[derive(Debug, Clone, Copy)]
enum ForkDisputeChain {
    Mainnet,
    Sepolia,
    Zeronet,
}

impl ForkDisputeChain {
    fn from_env() -> Result<Self> {
        let chain = env::var("BASE_ZK_FORK_CHAIN").unwrap_or_else(|_| "sepolia".to_string());
        Self::parse(&chain)
    }

    fn parse(chain: &str) -> Result<Self> {
        match chain {
            "mainnet" => Ok(Self::Mainnet),
            "sepolia" => Ok(Self::Sepolia),
            "zeronet" => Ok(Self::Zeronet),
            other => bail!("BASE_ZK_FORK_CHAIN must be mainnet, sepolia, or zeronet; got {other}"),
        }
    }

    const fn file_stem(self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Sepolia => "sepolia",
            Self::Zeronet => "zeronet",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct InvalidCheckpoint {
    index: u64,
    start_block: u64,
    target_block: u64,
    canonical_root: B256,
    onchain_root: B256,
}

#[derive(Debug, Clone, Copy)]
struct ResolvedGame {
    address: Address,
    game_type: Option<u32>,
}

struct CanonicalRootClient {
    l2_provider: Option<L2HttpProvider>,
    rollup: Option<RollupClient>,
}

impl CanonicalRootClient {
    fn new(config: &ForkDisputeConfig) -> Result<Self> {
        Self::from_urls(config.l2_rpc_url.clone(), config.rollup_rpc_url.clone())
    }

    fn from_urls(l2_rpc_url: Option<Url>, rollup_rpc_url: Option<Url>) -> Result<Self> {
        let l2_provider = l2_rpc_url.map(RootProvider::new_http);
        let rollup =
            rollup_rpc_url.map(RollupClientConfig::new).map(RollupClient::new).transpose()?;

        if l2_provider.is_none() && rollup.is_none() {
            bail!("set l2_rpc_url or rollup_rpc_url in config");
        }

        Ok(Self { l2_provider, rollup })
    }

    async fn output_root_at_block(&self, block_number: u64) -> Result<B256> {
        if let Some(provider) = &self.l2_provider {
            let block = provider
                .get_block_by_number(block_number.into())
                .await?
                .ok_or_else(|| eyre!("L2 block {block_number} not found"))?;
            let proof = provider
                .get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, Vec::new())
                .block_id(block_number.into())
                .await?;
            return Ok(OutputRoot::from_parts(
                block.header.state_root,
                proof.storage_hash,
                block.header.hash,
            )
            .hash());
        }

        let rollup = self.rollup.as_ref().expect("checked in constructor");
        Ok(rollup.fresh_output_at_block(block_number).await?.output_root)
    }
}

#[tokio::test]
#[ignore = "requires BASE_ZK_FORK_* env vars and an externally prepared L1 fork"]
async fn zk_proof_disputes_invalid_intermediate_root_on_fork() -> Result<()> {
    let config = ForkDisputeConfig::from_env().await?;

    let verifier = AggregateVerifierContractClient::new(config.l1_rpc_url.clone())?;
    let canonical_roots = CanonicalRootClient::new(&config)?;
    let checkpoint = if config.patch_invalid_game {
        patch_invalid_intermediate_root(&config, &verifier).await?
    } else {
        find_invalid_checkpoint(&config, &verifier, &canonical_roots).await?
    };
    let l1_head = verifier.l1_head(config.game_address).await?;
    let interval = checkpoint
        .target_block
        .checked_sub(checkpoint.start_block)
        .ok_or_else(|| eyre!("target block precedes start block"))?;

    let provider: RootProvider = RootProvider::new_http(config.l1_rpc_url.clone());
    let chain_id = provider.get_chain_id().await?;
    let tx_manager = SimpleTxManager::new(
        provider,
        SignerConfig::local(config.private_key.clone()),
        tx_manager_config(),
        chain_id,
        Arc::new(NoopTxMetrics),
    )
    .await?;
    let submitter = ChallengeSubmitter::new(tx_manager);

    let proof_bytes = request_onchain_zk_proof(
        &config,
        submitter.sender_address(),
        l1_head,
        checkpoint,
        interval,
    )
    .await?;

    let before_zk_prover = verifier.zk_prover(config.game_address).await?;
    let before_tee_prover = verifier.tee_prover(config.game_address).await?;
    let before_countered_index = verifier.countered_index(config.game_address).await?;

    let tx_hash = submitter
        .submit_dispute(
            config.game_address,
            proof_bytes,
            checkpoint.index,
            checkpoint.canonical_root,
            config.intent,
        )
        .await?;

    let status = verifier.status(config.game_address).await?;
    if status != GameStatus::InProgress {
        bail!("expected game to remain in progress after dispute tx, got {status}");
    }

    match config.intent {
        DisputeIntent::Nullify => {
            let after_zk_prover = verifier.zk_prover(config.game_address).await?;
            if before_zk_prover == Address::ZERO {
                bail!("nullify test requires an existing ZK prover on the game");
            }
            if after_zk_prover != Address::ZERO {
                bail!("expected ZK prover to be cleared after nullify, got {after_zk_prover}");
            }
        }
        DisputeIntent::Challenge => {
            let after_zk_prover = verifier.zk_prover(config.game_address).await?;
            let after_countered_index = verifier.countered_index(config.game_address).await?;
            if before_tee_prover == Address::ZERO {
                bail!("challenge test requires an existing TEE prover on the game");
            }
            if before_countered_index != 0 {
                bail!("challenge test requires an unchallenged game");
            }
            if after_zk_prover != submitter.sender_address() {
                bail!(
                    "expected ZK prover to be challenger {}, got {after_zk_prover}",
                    submitter.sender_address()
                );
            }
            if after_countered_index != checkpoint.index + 1 {
                bail!(
                    "expected countered index {}, got {after_countered_index}",
                    checkpoint.index + 1
                );
            }
        }
    }

    eprintln!(
        "submitted {:?} for game {} at intermediate index {} in tx {}",
        config.intent, config.game_address, checkpoint.index, tx_hash
    );

    Ok(())
}

impl ForkDisputeConfig {
    async fn from_env() -> Result<Self> {
        let chain = ForkDisputeChain::from_env()?;
        let file_config = ForkDisputeFileConfig::load(chain)?;

        let l1_rpc_url = env_optional_parse("BASE_ZK_FORK_L1_RPC_URL")?
            .or(file_config.l1_rpc_url)
            .ok_or_else(|| eyre!("set l1_rpc_url in config or BASE_ZK_FORK_L1_RPC_URL"))?;
        let l2_rpc_url = env_optional_parse("BASE_ZK_FORK_L2_RPC_URL")?.or(file_config.l2_rpc_url);
        let prover_l2_node_rpc_url = env_optional_parse("BASE_ZK_FORK_PROVER_L2_NODE_RPC_URL")?
            .or(file_config.prover_l2_node_rpc_url);
        let rollup_rpc_url = env_optional_parse("BASE_ZK_FORK_ROLLUP_RPC_URL")?
            .or(file_config.rollup_rpc_url)
            .or_else(|| prover_l2_node_rpc_url.clone());
        let dispute_game_factory = env_optional_parse("BASE_ZK_FORK_DISPUTE_GAME_FACTORY")?
            .or(file_config.dispute_game_factory)
            .ok_or_else(|| {
                eyre!("set dispute_game_factory in config or BASE_ZK_FORK_DISPUTE_GAME_FACTORY")
            })?;
        let explicit_game_address =
            env_optional_parse("BASE_ZK_FORK_GAME_ADDRESS")?.or(file_config.game_address);
        let explicit_game_index =
            env_optional_parse("BASE_ZK_FORK_GAME_INDEX")?.or(file_config.game_index);
        let resolved_game = resolve_game_address(
            &l1_rpc_url,
            dispute_game_factory,
            explicit_game_address,
            explicit_game_index,
        )
        .await?;

        Ok(Self {
            l1_rpc_url,
            l2_rpc_url,
            rollup_rpc_url,
            prover_grpc_url: env_optional_parse("BASE_ZK_FORK_PROVER_GRPC_URL")?
                .or(file_config.prover_grpc_url)
                .unwrap_or_else(|| Url::parse("http://localhost:9090").unwrap()),
            dispute_game_factory,
            game_address: resolved_game.address,
            game_type: resolved_game.game_type,
            private_key: env_optional_parse("BASE_ZK_FORK_PRIVATE_KEY")?
                .ok_or_else(|| eyre!("set BASE_ZK_FORK_PRIVATE_KEY"))?,
            intent: env_optional_intent("BASE_ZK_FORK_DISPUTE_INTENT")?
                .or(file_config.intent)
                .unwrap_or(DisputeIntent::Nullify),
            invalid_index: env_optional_parse("BASE_ZK_FORK_INVALID_INDEX")?
                .or(file_config.invalid_index),
            patch_invalid_game: explicit_game_address.is_none() && explicit_game_index.is_none(),
            poll_interval: Duration::from_secs(
                env_optional_parse("BASE_ZK_FORK_POLL_INTERVAL_SECS")?
                    .or(file_config.poll_interval_secs)
                    .unwrap_or(DEFAULT_POLL_INTERVAL_SECS),
            ),
            poll_timeout: Duration::from_secs(
                env_optional_parse("BASE_ZK_FORK_POLL_TIMEOUT_SECS")?
                    .or(file_config.poll_timeout_secs)
                    .unwrap_or(DEFAULT_POLL_TIMEOUT_SECS),
            ),
        })
    }
}

#[derive(Debug, Default)]
struct ForkDisputeFileConfig {
    l1_rpc_url: Option<Url>,
    l2_rpc_url: Option<Url>,
    rollup_rpc_url: Option<Url>,
    prover_l2_node_rpc_url: Option<Url>,
    prover_grpc_url: Option<Url>,
    dispute_game_factory: Option<Address>,
    game_address: Option<Address>,
    game_index: Option<u64>,
    intent: Option<DisputeIntent>,
    invalid_index: Option<u64>,
    poll_interval_secs: Option<u64>,
    poll_timeout_secs: Option<u64>,
}

impl ForkDisputeFileConfig {
    fn load(chain: ForkDisputeChain) -> Result<Self> {
        let Some(path) = Self::path(chain)? else {
            return Ok(Self::default());
        };
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let value: Value = serde_yaml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?;

        Ok(Self {
            l1_rpc_url: parse_config_field(&value, "l1_rpc_url")?,
            l2_rpc_url: parse_config_field(&value, "l2_rpc_url")?,
            rollup_rpc_url: parse_config_field(&value, "rollup_rpc_url")?,
            prover_l2_node_rpc_url: parse_config_field(&value, "prover_l2_node_rpc_url")?,
            prover_grpc_url: parse_config_field(&value, "prover_grpc_url")?,
            dispute_game_factory: parse_config_field(&value, "dispute_game_factory")?,
            game_address: parse_config_field(&value, "game_address")?,
            game_index: parse_config_field(&value, "game_index")?,
            intent: parse_config_intent(&value, "intent")?,
            invalid_index: parse_config_field(&value, "invalid_index")?,
            poll_interval_secs: parse_config_field(&value, "poll_interval_secs")?,
            poll_timeout_secs: parse_config_field(&value, "poll_timeout_secs")?,
        })
    }

    fn path(chain: ForkDisputeChain) -> Result<Option<PathBuf>> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(DEFAULT_CONFIG_DIR)
            .join(format!("{}.yaml", chain.file_stem()));
        Ok(path.exists().then_some(path))
    }
}

async fn resolve_game_address(
    l1_rpc_url: &Url,
    dispute_game_factory: Address,
    game_address: Option<Address>,
    game_index: Option<u64>,
) -> Result<ResolvedGame> {
    if let Some(game_address) = game_address {
        return Ok(ResolvedGame { address: game_address, game_type: None });
    }

    let provider: RootProvider = RootProvider::new_http(l1_rpc_url.clone());
    let factory_code = provider.get_code_at(dispute_game_factory).await?;
    if factory_code.is_empty() {
        bail!(
            "no DisputeGameFactory bytecode at {dispute_game_factory} on {l1_rpc_url}; ensure the L1 RPC is a fork of the selected chain"
        );
    }

    let factory = DisputeGameFactoryContractClient::new(dispute_game_factory, l1_rpc_url.clone())?;
    if let Some(index) = game_index {
        let game = factory.game_at_index(index).await?;
        return Ok(ResolvedGame { address: game.proxy, game_type: Some(game.game_type) });
    }

    let game_count = factory.game_count().await?;
    if game_count == 0 {
        bail!("factory {dispute_game_factory} has no games");
    }
    let index = game_count - 1;
    let game = factory.game_at_index(index).await?;
    eprintln!("auto-selected game {} at factory index {index}", game.proxy);
    Ok(ResolvedGame { address: game.proxy, game_type: Some(game.game_type) })
}

fn parse_config_field<T>(value: &Value, field: &'static str) -> Result<Option<T>>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    let Some(raw) = config_field(value, field)? else {
        return Ok(None);
    };
    let rendered = match raw {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Null => return Ok(None),
        _ => bail!("config field {field} must be a string or number"),
    };

    rendered
        .parse()
        .map(Some)
        .map_err(|error| eyre!("failed to parse config field {field}: {error}"))
}

fn parse_config_intent(value: &Value, field: &'static str) -> Result<Option<DisputeIntent>> {
    let Some(raw) = config_field(value, field)? else {
        return Ok(None);
    };
    let Value::String(intent) = raw else {
        bail!("config field {field} must be a string");
    };

    parse_intent(intent).map(Some)
}

fn config_field<'a>(value: &'a Value, field: &'static str) -> Result<Option<&'a Value>> {
    let Value::Mapping(mapping) = value else {
        bail!("fork dispute config must be a YAML mapping");
    };
    Ok(mapping.get(Value::String(field.to_string())))
}

async fn patch_invalid_intermediate_root(
    config: &ForkDisputeConfig,
    verifier: &AggregateVerifierContractClient,
) -> Result<InvalidCheckpoint> {
    let starting_block = verifier.starting_block_number(config.game_address).await?;
    let onchain_roots = verifier.intermediate_output_roots(config.game_address).await?;
    if onchain_roots.is_empty() {
        bail!(
            "auto-selected game {} has no intermediate output roots; pass a game_address or game_index",
            config.game_address
        );
    }

    let interval =
        infer_intermediate_interval_for_game(config.game_address, verifier, onchain_roots.len())
            .await?;
    let index = config.invalid_index.unwrap_or(0);
    let canonical_root = *onchain_roots
        .get(index as usize)
        .ok_or_else(|| eyre!("invalid index {index} out of range {}", onchain_roots.len()))?;
    let offset = interval
        .checked_mul(index + 1)
        .ok_or_else(|| eyre!("intermediate block offset overflow"))?;
    let target_block =
        starting_block.checked_add(offset).ok_or_else(|| eyre!("target block overflow"))?;
    let start_block =
        target_block.checked_sub(interval).ok_or_else(|| eyre!("start block underflow"))?;
    let patched_root = patched_invalid_root(canonical_root);
    patch_factory_registration_for_patched_root(
        config,
        verifier,
        &onchain_roots,
        index,
        patched_root,
    )
    .await?;
    patch_game_code_root(config, canonical_root, patched_root).await?;

    let patched = verifier.intermediate_output_root(config.game_address, index).await?;
    if patched != patched_root {
        bail!(
            "patched game code but intermediate root {} stayed {patched}; expected {patched_root}",
            index
        );
    }

    eprintln!(
        "patched game {} intermediate index {} from {} to {} on fork",
        config.game_address, index, canonical_root, patched_root
    );

    Ok(InvalidCheckpoint {
        index,
        start_block,
        target_block,
        canonical_root,
        onchain_root: patched_root,
    })
}

async fn patch_factory_registration_for_patched_root(
    config: &ForkDisputeConfig,
    verifier: &AggregateVerifierContractClient,
    original_roots: &[B256],
    index: u64,
    patched_root: B256,
) -> Result<()> {
    let game_type = config.game_type.ok_or_else(|| {
        eyre!(
            "cannot patch factory registration without a factory game type; pass game_index instead of game_address or let the test auto-select a game"
        )
    })?;
    let root_index = usize::try_from(index).context("invalid root index does not fit usize")?;
    if root_index >= original_roots.len() {
        bail!("invalid index {index} out of range {}", original_roots.len());
    }

    let info = verifier.game_info(config.game_address).await?;
    let original_extra_data =
        encode_extra_data(info.l2_block_number, info.parent_address, original_roots);
    let original_uuid = game_uuid(game_type, info.root_claim, &original_extra_data);

    let mut patched_roots = original_roots.to_vec();
    patched_roots[root_index] = patched_root;
    let patched_extra_data =
        encode_extra_data(info.l2_block_number, info.parent_address, &patched_roots);
    let patched_uuid = game_uuid(game_type, info.root_claim, &patched_extra_data);

    let factory = DisputeGameFactoryContractClient::new(
        config.dispute_game_factory,
        config.l1_rpc_url.clone(),
    )?;
    let original_lookup = factory
        .games(game_type, info.root_claim, original_extra_data)
        .await
        .context("failed to look up original factory game")?;
    if original_lookup != config.game_address {
        bail!(
            "factory lookup for original game data returned {original_lookup}, expected {}",
            config.game_address
        );
    }

    let provider: RootProvider = RootProvider::new_http(config.l1_rpc_url.clone());
    let (mapping_slot, packed_game_id) = find_dispute_games_mapping_slot(
        &provider,
        config.dispute_game_factory,
        original_uuid,
        game_type,
        config.game_address,
    )
    .await?;
    let patched_storage_key = mapping_storage_key(patched_uuid, mapping_slot);

    let storage_updated = provider
        .client()
        .request::<_, bool>(
            "anvil_setStorageAt",
            (config.dispute_game_factory, patched_storage_key, packed_game_id),
        )
        .await
        .context(
            "anvil_setStorageAt failed; ensure BASE_ZK_FORK_L1_RPC_URL points to an Anvil fork",
        )?;
    if !storage_updated {
        bail!("anvil_setStorageAt returned false for patched factory registration");
    }

    let patched_lookup = factory
        .games(game_type, info.root_claim, patched_extra_data)
        .await
        .context("failed to look up patched factory game")?;
    if patched_lookup != config.game_address {
        bail!("patched factory lookup returned {patched_lookup}, expected {}", config.game_address);
    }

    eprintln!(
        "patched factory registration for game {} at _disputeGames slot {}",
        config.game_address, mapping_slot
    );

    Ok(())
}

async fn find_dispute_games_mapping_slot(
    provider: &RootProvider,
    factory_address: Address,
    original_uuid: B256,
    game_type: u32,
    game_address: Address,
) -> Result<(u64, B256)> {
    for mapping_slot in 0..256u64 {
        let storage_key = mapping_storage_key(original_uuid, mapping_slot);
        let value = provider
            .get_storage_at(factory_address, U256::from_be_slice(storage_key.as_slice()))
            .await
            .with_context(|| {
                format!("failed to read factory storage slot candidate {mapping_slot}")
            })?;
        if packed_game_id_matches(value, game_type, game_address) {
            return Ok((mapping_slot, u256_to_b256(value)));
        }
    }

    bail!(
        "could not discover DisputeGameFactory _disputeGames mapping slot for game {}",
        game_address
    )
}

fn packed_game_id_matches(
    value: U256,
    expected_game_type: u32,
    expected_game_address: Address,
) -> bool {
    if value == U256::ZERO {
        return false;
    }

    let bytes = value.to_be_bytes::<32>();
    let game_type = u32::from_be_bytes(bytes[..4].try_into().expect("4-byte game type"));
    let game_address = Address::from_slice(&bytes[12..]);

    game_type == expected_game_type && game_address == expected_game_address
}

fn game_uuid(game_type: u32, root_claim: B256, extra_data: &Bytes) -> B256 {
    keccak256(abi_encode_game_uuid_preimage(game_type, root_claim, extra_data))
}

fn abi_encode_game_uuid_preimage(game_type: u32, root_claim: B256, extra_data: &Bytes) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(128 + extra_data.len().div_ceil(32) * 32);
    encoded.extend_from_slice(&U256::from(game_type).to_be_bytes::<32>());
    encoded.extend_from_slice(root_claim.as_slice());
    encoded.extend_from_slice(&U256::from(96).to_be_bytes::<32>());
    encoded.extend_from_slice(&U256::from(extra_data.len()).to_be_bytes::<32>());
    encoded.extend_from_slice(extra_data);
    let padding = (32 - extra_data.len() % 32) % 32;
    encoded.resize(encoded.len() + padding, 0);
    encoded
}

fn mapping_storage_key(key: B256, mapping_slot: u64) -> B256 {
    let mut encoded = Vec::with_capacity(64);
    encoded.extend_from_slice(key.as_slice());
    encoded.extend_from_slice(&U256::from(mapping_slot).to_be_bytes::<32>());
    keccak256(encoded)
}

fn u256_to_b256(value: U256) -> B256 {
    B256::from_slice(&value.to_be_bytes::<32>())
}

fn patched_invalid_root(canonical_root: B256) -> B256 {
    for byte in [0x42, 0x43, 0x44] {
        let candidate = B256::repeat_byte(byte);
        if candidate != canonical_root {
            return candidate;
        }
    }
    B256::ZERO
}

async fn patch_game_code_root(
    config: &ForkDisputeConfig,
    original_root: B256,
    patched_root: B256,
) -> Result<()> {
    let provider: RootProvider = RootProvider::new_http(config.l1_rpc_url.clone());
    let code = provider.get_code_at(config.game_address).await?;
    let mut patched_code = code.to_vec();
    let original = original_root.as_slice();
    let mut replacements = 0usize;

    for offset in 0..=patched_code.len().saturating_sub(original.len()) {
        if &patched_code[offset..offset + original.len()] == original {
            patched_code[offset..offset + original.len()].copy_from_slice(patched_root.as_slice());
            replacements += 1;
        }
    }

    if replacements == 0 {
        bail!(
            "could not find intermediate root {original_root} in game {} bytecode; pass an existing invalid game_address or game_index",
            config.game_address
        );
    }

    provider
        .client()
        .request::<_, ()>("anvil_setCode", (config.game_address, Bytes::from(patched_code)))
        .await
        .context("anvil_setCode failed; ensure BASE_ZK_FORK_L1_RPC_URL points to an Anvil fork")?;

    Ok(())
}

async fn find_invalid_checkpoint(
    config: &ForkDisputeConfig,
    verifier: &AggregateVerifierContractClient,
    canonical_roots: &CanonicalRootClient,
) -> Result<InvalidCheckpoint> {
    let starting_block = verifier.starting_block_number(config.game_address).await?;
    let onchain_roots = verifier.intermediate_output_roots(config.game_address).await?;
    if onchain_roots.is_empty() {
        bail!("game {} has no intermediate output roots", config.game_address);
    }

    let interval =
        infer_intermediate_interval_for_game(config.game_address, verifier, onchain_roots.len())
            .await?;

    if let Some(index) = config.invalid_index {
        return checkpoint_at_index(
            starting_block,
            interval,
            index,
            &onchain_roots,
            canonical_roots,
        )
        .await;
    }

    for index in 0..onchain_roots.len() as u64 {
        let checkpoint =
            checkpoint_at_index(starting_block, interval, index, &onchain_roots, canonical_roots)
                .await?;
        if checkpoint.onchain_root != checkpoint.canonical_root {
            return Ok(checkpoint);
        }
    }

    bail!("no invalid intermediate root found for game {}", config.game_address)
}

async fn infer_intermediate_interval_for_game(
    game_address: Address,
    verifier: &AggregateVerifierContractClient,
    root_count: usize,
) -> Result<u64> {
    let info = verifier.game_info(game_address).await?;
    let starting_block = verifier.starting_block_number(game_address).await?;
    let span = info
        .l2_block_number
        .checked_sub(starting_block)
        .ok_or_else(|| eyre!("game target block precedes starting block"))?;

    if root_count == 0 {
        bail!("cannot infer interval for a game with no intermediate roots");
    }
    if !span.is_multiple_of(root_count as u64) {
        bail!(
            "cannot infer intermediate interval: span {span} is not divisible by root count {root_count}"
        );
    }

    Ok(span / root_count as u64)
}

async fn checkpoint_at_index(
    starting_block: u64,
    interval: u64,
    index: u64,
    onchain_roots: &[B256],
    canonical_roots: &CanonicalRootClient,
) -> Result<InvalidCheckpoint> {
    let onchain_root = *onchain_roots
        .get(index as usize)
        .ok_or_else(|| eyre!("invalid index {index} out of range {}", onchain_roots.len()))?;
    let offset = interval
        .checked_mul(index + 1)
        .ok_or_else(|| eyre!("intermediate block offset overflow"))?;
    let target_block =
        starting_block.checked_add(offset).ok_or_else(|| eyre!("target block overflow"))?;
    let start_block =
        target_block.checked_sub(interval).ok_or_else(|| eyre!("start block underflow"))?;
    let canonical_root = canonical_roots.output_root_at_block(target_block).await?;

    Ok(InvalidCheckpoint { index, start_block, target_block, canonical_root, onchain_root })
}

async fn request_onchain_zk_proof(
    config: &ForkDisputeConfig,
    prover_address: Address,
    l1_head: B256,
    checkpoint: InvalidCheckpoint,
    interval: u64,
) -> Result<Bytes> {
    if config.prover_grpc_url.scheme() == "http" {
        let host = config.prover_grpc_url.host_str().unwrap_or("<host>");
        let is_local = matches!(host, "localhost" | "127.0.0.1" | "::1");
        if !is_local || config.prover_grpc_url.port_or_known_default() == Some(443) {
            bail!(
                "BASE_ZK_FORK_PROVER_GRPC_URL uses http for remote prover {host}; use https://{host} instead"
            );
        }
    }

    let client = ZkProofClient::new(&ZkProofClientConfig {
        endpoint: config.prover_grpc_url.clone(),
        connect_timeout: Duration::from_secs(10),
        request_timeout: Duration::from_secs(60),
    })?;
    let target_block = checkpoint
        .start_block
        .checked_add(interval)
        .ok_or_else(|| eyre!("proof target block overflow"))?;
    eprintln!(
        "requesting ZK proof from {} for L2 blocks {}..={} with L1 head {}",
        config.prover_grpc_url, checkpoint.start_block, target_block, l1_head
    );

    let response = client
        .prove_block(ProveBlockRequest {
            start_block_number: checkpoint.start_block,
            number_of_blocks_to_prove: interval,
            sequence_window: None,
            proof_type: ProofType::SnarkGroth16.into(),
            session_id: None,
            prover_address: Some(format!("{prover_address:#x}")),
            l1_head: Some(format!("{l1_head:#x}")),
            intermediate_root_interval: Some(interval),
        })
        .await?;

    let started_at = Instant::now();
    loop {
        if started_at.elapsed() > config.poll_timeout {
            bail!(
                "timed out after {:?} waiting for ZK proof session {}",
                config.poll_timeout,
                response.session_id
            );
        }

        tokio::time::sleep(config.poll_interval).await;
        let proof = client
            .get_proof(GetProofRequest {
                session_id: response.session_id.clone(),
                receipt_type: Some(ReceiptType::OnChainSnark as i32),
            })
            .await?;

        match ProofJobStatus::try_from(proof.status) {
            Ok(ProofJobStatus::Succeeded) => {
                if proof.receipt.is_empty() {
                    bail!("ZK proof session {} succeeded with empty receipt", response.session_id);
                }
                let mut proof_bytes = Vec::with_capacity(1 + proof.receipt.len());
                proof_bytes.push(PROOF_TYPE_ZK);
                proof_bytes.extend_from_slice(&proof.receipt);
                return Ok(Bytes::from(proof_bytes));
            }
            Ok(ProofJobStatus::Failed) => {
                let error_message =
                    proof.error_message.unwrap_or_else(|| "no error message".to_string());
                if error_message.contains("Block not found for block number") {
                    bail!(
                        "ZK proof session {} failed because the prover backend cannot read L2 block data for range {}..={}. The prover endpoint is likely not configured for Base Sepolia or its L2 RPC is behind/missing archive data. Backend error: {}",
                        response.session_id,
                        checkpoint.start_block,
                        target_block,
                        error_message
                    );
                }
                bail!("ZK proof session {} failed: {}", response.session_id, error_message);
            }
            Ok(
                ProofJobStatus::Created
                | ProofJobStatus::Pending
                | ProofJobStatus::Running
                | ProofJobStatus::Unspecified,
            )
            | Err(_) => {}
        }
    }
}

fn tx_manager_config() -> TxManagerConfig {
    TxManagerConfig {
        num_confirmations: 1,
        resubmission_timeout: Duration::from_secs(10),
        receipt_query_interval: Duration::from_secs(1),
        tx_send_timeout: Duration::from_secs(180),
        tx_not_in_mempool_timeout: Duration::from_secs(30),
        confirmation_timeout: Duration::from_secs(120),
        ..Default::default()
    }
}

fn env_optional_parse<T>(name: &'static str) -> Result<Option<T>>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    env::var(name)
        .ok()
        .map(|value| value.parse().map_err(|e| eyre!("failed to parse {name}: {e}")))
        .transpose()
}

fn env_optional_intent(name: &'static str) -> Result<Option<DisputeIntent>> {
    env::var(name).ok().map(|value| parse_intent(&value)).transpose()
}

fn parse_intent(intent: &str) -> Result<DisputeIntent> {
    match intent {
        "nullify" => Ok(DisputeIntent::Nullify),
        "challenge" => Ok(DisputeIntent::Challenge),
        other => bail!("intent must be either 'nullify' or 'challenge', got {other}"),
    }
}
