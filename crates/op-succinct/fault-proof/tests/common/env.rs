//! Common test environment setup utilities.
use std::{
    str::FromStr,
    sync::{Arc, OnceLock},
    time::Duration,
};

use alloy_network::EthereumWallet;
use alloy_primitives::{hex, Address, Bytes, FixedBytes, Uint, B256, U256};
use alloy_provider::ProviderBuilder;
use alloy_rpc_types_eth::TransactionReceipt;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolValue};
use alloy_transport_http::reqwest::Url;
use anyhow::{Context, Result};
use op_succinct_bindings::{
    anchor_state_registry::AnchorStateRegistry::{self, AnchorStateRegistryInstance},
    dispute_game_factory::DisputeGameFactory::{self, DisputeGameFactoryInstance},
    mock_optimism_portal2::MockOptimismPortal2::{self, MockOptimismPortal2Instance},
    op_succinct_fault_dispute_game::OPSuccinctFaultDisputeGame::{
        self, OPSuccinctFaultDisputeGameInstance,
    },
};
use op_succinct_client_utils::{boot::hash_rollup_config, types::u32_to_u8};
use op_succinct_elfs::AGGREGATION_ELF;
use op_succinct_host_utils::{
    fetcher::{get_rpcs_from_env, OPSuccinctDataFetcher, RPCConfig},
    host::OPSuccinctHost,
    OP_SUCCINCT_FAULT_DISPUTE_GAME_CONFIG_PATH,
};
use op_succinct_proof_utils::get_range_elf_embedded;
use op_succinct_signer_utils::{Signer, SignerLock};
use sp1_sdk::{Elf, HashableKey, Prover, ProvingKey, ProverClient};
use tokio::task::JoinHandle;
use tracing::{info, Level};

use fault_proof::{
    challenger::OPSuccinctChallenger, config::FaultDisputeGameConfig, proposer::OPSuccinctProposer,
    L2ProviderTrait,
};
use tracing_subscriber::{filter::Targets, fmt, prelude::*, util::SubscriberInitExt};

use crate::common::{
    constants::*,
    contracts::{deploy_mock_permissioned_game, send_contract_transaction},
    new_challenger, new_proposer, start_challenger, start_proposer, warp_time, ANVIL,
};

use super::{
    anvil::{setup_anvil_chain, AnvilFork},
    contracts::{deploy_test_contracts, DeployedContracts},
};

/// Role for selecting which private key to use when creating providers
#[derive(Debug, Clone, Copy)]
pub enum Role {
    Proposer,
    Challenger,
    Deployer,
}

/// Common test environment setup
pub struct TestEnvironment {
    /// Game type
    pub game_type: u32,
    /// Private keys
    pub private_keys: TestPrivateKeys,
    /// RPC configuration
    pub rpc_config: RPCConfig,
    /// Data fetcher
    pub fetcher: OPSuccinctDataFetcher,
    /// Anvil fork
    pub anvil: AnvilFork,
    /// Deployed contracts
    pub deployed: DeployedContracts,
}

impl Drop for TestEnvironment {
    fn drop(&mut self) {
        let mut anvil_lock = ANVIL.lock().unwrap();
        if let Some(anvil_instance) = anvil_lock.take() {
            info!("Stopping Anvil instance");
            drop(anvil_instance);
        }
    }
}

/// Compute vkeys from ELF programs.
/// Returns (aggregation_vkey, range_vkey_commitment) as B256 values.
/// Uses the same computation as `proposer.rs` and `config_common.rs`.
pub async fn compute_vkeys() -> anyhow::Result<(B256, B256)> {
    let prover = ProverClient::builder().cpu().build().await;

    let agg_pk = prover.setup(Elf::Static(AGGREGATION_ELF)).await?;
    let agg_vk = agg_pk.verifying_key();
    let aggregation_vkey = {
        let hex_str = agg_vk.bytes32();
        B256::from_slice(&hex::decode(hex_str.trim_start_matches("0x")).unwrap())
    };

    let range_pk = prover.setup(Elf::Static(get_range_elf_embedded())).await?;
    let range_vk = range_pk.verifying_key();
    let range_vkey_commitment = B256::from(u32_to_u8(range_vk.hash_u32()));

    Ok((aggregation_vkey, range_vkey_commitment))
}

/// The test configuration, used for integration tests.
pub fn test_config(
    starting_l2_block_number: u64,
    starting_root: String,
    aggregation_vkey: B256,
    range_vkey_commitment: B256,
    rollup_config_hash: B256,
) -> FaultDisputeGameConfig {
    FaultDisputeGameConfig {
        aggregation_vkey: aggregation_vkey.to_string(),
        challenger_addresses: vec![CHALLENGER_ADDRESS.to_string()],
        challenger_bond_wei: CHALLENGER_BOND.to::<u64>(),
        dispute_game_finality_delay_seconds: DISPUTE_GAME_FINALITY_DELAY_SECONDS,
        fallback_timeout_fp_secs: FALLBACK_TIMEOUT.to::<u64>(),
        game_type: TEST_GAME_TYPE,
        initial_bond_wei: INIT_BOND.to::<u64>(),
        max_challenge_duration: MAX_CHALLENGE_DURATION,
        max_prove_duration: MAX_PROVE_DURATION,
        optimism_portal2_address: Address::ZERO.to_string(),
        permissionless_mode: false,
        proposer_addresses: vec![PROPOSER_ADDRESS.to_string()],
        range_vkey_commitment: range_vkey_commitment.to_string(),
        rollup_config_hash: rollup_config_hash.to_string(),
        starting_l2_block_number,
        starting_root,
        system_config_address: Address::ZERO.to_string(),
        use_sp1_mock_verifier: true,
        verifier_address: Address::ZERO.to_string(),
    }
}

impl TestEnvironment {
    /// Create a new test environment with common setup
    pub async fn setup() -> Result<Self> {
        init_logging();

        // Compute vkeys from ELFs - these will match the proposer's computed vkeys
        let (aggregation_vkey, range_vkey_commitment) = compute_vkeys().await?;

        // Get environment variables
        let mut rpc_config = get_rpcs_from_env();

        let fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;

        // Compute rollup_config_hash from fetcher's chain config - matches proposer's computation
        let rollup_config_hash = hash_rollup_config(
            fetcher.rollup_config.as_ref().context("rollup_config required for test setup")?,
        );

        // Setup fresh Anvil chain
        let anvil = setup_anvil_chain().await?;

        // Put the test config into ../contracts/opsuccinctfdgconfig.json
        let test_config: FaultDisputeGameConfig = test_config(
            anvil.starting_l2_block_number,
            anvil.starting_root.clone(),
            aggregation_vkey,
            range_vkey_commitment,
            rollup_config_hash,
        );
        let json = serde_json::to_string_pretty(&test_config)?;
        std::fs::write(OP_SUCCINCT_FAULT_DISPUTE_GAME_CONFIG_PATH.clone(), json)?;

        // Update RPC config with Anvil endpoint
        rpc_config.l1_rpc = Url::parse(&anvil.endpoint.clone())?;

        let game_type = TEST_GAME_TYPE;

        let private_keys = TestPrivateKeys::default();

        // Deploy contracts
        info!("=== Deploying Contracts ===");
        let deployed = deploy_test_contracts(&anvil.endpoint, private_keys.deployer).await?;

        Ok(Self { game_type, private_keys, rpc_config, fetcher, anvil, deployed })
    }

    /// Setup test environment with starting L2 block offset from actual finalized block
    ///
    /// # Arguments
    /// * `offset` - Positive offset creates future block (for testing misconfiguration). Negative
    ///   offset creates past block (for testing valid scenarios). Zero offset is equivalent to
    ///   normal setup().
    ///
    /// # Examples
    /// * `setup_with_starting_block_offset(1_000_000)` - 1M blocks ahead (misconfigured)
    /// * `setup_with_starting_block_offset(-100)` - 100 blocks behind (valid)
    /// * `setup_with_starting_block_offset(0)` - same as setup()
    pub async fn setup_with_starting_block_offset(offset: i64) -> Result<Self> {
        init_logging();

        // Compute vkeys from ELFs - these will match the proposer's computed vkeys
        let (aggregation_vkey, range_vkey_commitment) = compute_vkeys().await?;

        let mut rpc_config = get_rpcs_from_env();
        let fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;

        // Compute rollup_config_hash from fetcher's chain config - matches proposer's computation
        let rollup_config_hash = hash_rollup_config(
            fetcher.rollup_config.as_ref().context("rollup_config required for test setup")?,
        );

        // Setup anvil chain - gets actual L2 finalized block
        let anvil = setup_anvil_chain().await?;

        // Apply offset to get custom starting block
        let custom_starting_block = if offset >= 0 {
            anvil.starting_l2_block_number.saturating_add(offset as u64)
        } else {
            anvil.starting_l2_block_number.saturating_sub(offset.unsigned_abs())
        };

        // For future blocks, use dummy root (can't compute root for blocks that don't exist)
        // For past/current blocks, reuse the actual root from anvil setup
        let starting_root = if offset > 0 {
            // Dummy root for future blocks
            "0x0000000000000000000000000000000000000000000000000000000000000000".to_string()
        } else {
            anvil.starting_root.clone()
        };

        // Create config with offset starting block
        let test_config = test_config(
            custom_starting_block,
            starting_root,
            aggregation_vkey,
            range_vkey_commitment,
            rollup_config_hash,
        );
        let json = serde_json::to_string_pretty(&test_config)?;
        std::fs::write(OP_SUCCINCT_FAULT_DISPUTE_GAME_CONFIG_PATH.clone(), json)?;

        // Update RPC config with Anvil endpoint
        rpc_config.l1_rpc = Url::parse(&anvil.endpoint)?;

        let game_type = TEST_GAME_TYPE;
        let private_keys = TestPrivateKeys::default();

        // Deploy contracts with the offset starting block
        info!("=== Deploying Contracts with starting block offset: {} ===", offset);
        let deployed = deploy_test_contracts(&anvil.endpoint, private_keys.deployer).await?;

        Ok(Self { game_type, private_keys, rpc_config, fetcher, anvil, deployed })
    }

    pub async fn new_proposer(
        &self,
    ) -> Result<OPSuccinctProposer<fault_proof::L1Provider, impl OPSuccinctHost + Clone>> {
        new_proposer(
            &self.rpc_config,
            self.private_keys.proposer,
            &self.deployed.anchor_state_registry,
            &self.deployed.factory,
            self.game_type,
            None,
        )
        .await
    }

    pub async fn init_proposer(
        &self,
    ) -> Result<OPSuccinctProposer<fault_proof::L1Provider, impl OPSuccinctHost + Clone>> {
        let proposer = self.new_proposer().await?;
        proposer.try_init().await?;
        info!("✓ Proposer initialized");
        Ok(proposer)
    }

    pub async fn new_challenger(&self) -> Result<OPSuccinctChallenger<fault_proof::L1Provider>> {
        new_challenger(
            &self.rpc_config,
            self.private_keys.challenger,
            &self.deployed.anchor_state_registry,
            &self.deployed.factory,
            self.game_type,
            None,
        )
        .await
    }

    pub async fn init_challenger(&self) -> Result<OPSuccinctChallenger<fault_proof::L1Provider>> {
        let challenger = self.new_challenger().await?;
        challenger.try_init().await?;
        info!("✓ Challenger initialized");
        Ok(challenger)
    }

    pub async fn start_proposer(&self) -> Result<JoinHandle<Result<()>>> {
        let handle = start_proposer(
            &self.rpc_config,
            self.private_keys.proposer,
            &self.deployed.anchor_state_registry,
            &self.deployed.factory,
            self.game_type,
        )
        .await?;
        info!("✓ Proposer service started");
        Ok(handle)
    }

    pub fn stop_proposer(&self, handle: JoinHandle<Result<()>>) {
        handle.abort();
        info!("✓ Proposer service stopped");
    }

    pub async fn start_challenger(
        &self,
        malicious_percentage: Option<f64>,
    ) -> Result<JoinHandle<Result<()>>> {
        let handle = start_challenger(
            &self.rpc_config,
            self.private_keys.challenger,
            &self.deployed.anchor_state_registry,
            &self.deployed.factory,
            self.game_type,
            malicious_percentage,
        )
        .await?;
        info!("✓ Challenger service started with malicious percentage: {malicious_percentage:?}");
        Ok(handle)
    }

    pub fn stop_challenger(&self, handle: JoinHandle<Result<()>>) {
        handle.abort();
        info!("✓ Challenger service stopped");
    }

    pub async fn deploy_mock_permissioned_game(&self) -> Result<Address> {
        let proposer_signer =
            SignerLock::new(Signer::new_local_signer(self.private_keys.proposer)?);
        let factory = self.factory()?;
        let address = deploy_mock_permissioned_game(
            &proposer_signer,
            &self.rpc_config.l1_rpc,
            *factory.address(),
        )
        .await?;
        info!("✓ Deployed mock permissioned implementation at {address}");
        Ok(address)
    }

    /// Setup a legacy game type with init bond and implementation.
    /// Returns the deployed implementation address.
    pub async fn setup_legacy_game_type(
        &self,
        game_type: u32,
        init_bond: Uint<256, 4>,
    ) -> Result<Address> {
        let legacy_impl = self.deploy_mock_permissioned_game().await?;

        let set_init_call =
            DisputeGameFactory::setInitBondCall { _gameType: game_type, _initBond: init_bond };
        self.send_factory_tx(set_init_call.abi_encode(), None).await?;

        let set_impl_call = DisputeGameFactory::setImplementation_0Call {
            _gameType: game_type,
            _impl: legacy_impl,
        };
        self.send_factory_tx(set_impl_call.abi_encode(), None).await?;

        info!("✓ Setup legacy game type {game_type} with implementation at {legacy_impl}");
        Ok(legacy_impl)
    }

    /// Set the respected game type on the AnchorStateRegistry.
    /// In v5.0.0, respected game type is stored in AnchorStateRegistry, not OptimismPortal2.
    pub async fn set_respected_game_type(&self, game_type: u32) -> Result<()> {
        let set_type_call = AnchorStateRegistry::setRespectedGameTypeCall { _gameType: game_type };
        self.send_anchor_state_registry_tx(set_type_call.abi_encode(), None).await?;
        info!("✓ Set respected game type to {game_type}");
        Ok(())
    }

    pub async fn send_anchor_state_registry_tx(
        &self,
        call: Vec<u8>,
        value: Option<Uint<256, 4>>,
    ) -> Result<()> {
        let proposer_signer =
            SignerLock::new(Signer::new_local_signer(self.private_keys.proposer)?);
        send_contract_transaction(
            &proposer_signer,
            &self.rpc_config.l1_rpc,
            self.deployed.anchor_state_registry,
            Bytes::from(call),
            value,
        )
        .await
    }

    pub async fn send_factory_tx(&self, call: Vec<u8>, value: Option<Uint<256, 4>>) -> Result<()> {
        let proposer_signer =
            SignerLock::new(Signer::new_local_signer(self.private_keys.proposer)?);
        send_contract_transaction(
            &proposer_signer,
            &self.rpc_config.l1_rpc,
            self.deployed.factory,
            Bytes::from(call),
            value,
        )
        .await
    }

    pub async fn send_portal_tx(&self, call: Vec<u8>, value: Option<Uint<256, 4>>) -> Result<()> {
        let proposer_signer =
            SignerLock::new(Signer::new_local_signer(self.private_keys.proposer)?);
        send_contract_transaction(
            &proposer_signer,
            &self.rpc_config.l1_rpc,
            self.deployed.portal,
            Bytes::from(call),
            value,
        )
        .await
    }

    /// Create a provider with a signer for the specified role
    pub fn provider_with_role(
        &self,
        role: Role,
    ) -> Result<Arc<impl alloy_provider::Provider + Clone>> {
        let key = match role {
            Role::Proposer => self.private_keys.proposer,
            Role::Challenger => self.private_keys.challenger,
            Role::Deployer => self.private_keys.deployer,
        };

        let wallet = PrivateKeySigner::from_str(key)?;
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(wallet))
            .connect_http(self.anvil.endpoint.parse::<Url>()?);
        Ok(Arc::new(provider))
    }

    pub async fn compute_output_root_at_block(&self, block: u64) -> Result<FixedBytes<32>> {
        self.fetcher.l2_provider.compute_output_root_at_block(U256::from(block)).await
    }

    pub fn factory(
        &self,
    ) -> Result<DisputeGameFactoryInstance<impl alloy_provider::Provider + Clone>> {
        let provider = self.provider_with_role(Role::Proposer)?;
        let factory = DisputeGameFactory::new(self.deployed.factory, provider);
        Ok(factory)
    }

    pub async fn anchor_registry_address(&self, game_address: Address) -> Result<Address> {
        let provider = self.provider_with_role(Role::Proposer)?;
        let anchor_registry_addr = OPSuccinctFaultDisputeGame::new(game_address, provider)
            .anchorStateRegistry()
            .call()
            .await?;
        Ok(anchor_registry_addr)
    }

    pub async fn anchor_registry(
        &self,
        game_address: Address,
    ) -> Result<AnchorStateRegistryInstance<impl alloy_provider::Provider + Clone>> {
        let provider = self.provider_with_role(Role::Proposer)?;
        let anchor_registry_addr = self.anchor_registry_address(game_address).await?;
        let anchor_registry = AnchorStateRegistry::new(anchor_registry_addr, provider);
        Ok(anchor_registry)
    }

    pub async fn mock_optimism_portal2(
        &self,
    ) -> Result<MockOptimismPortal2Instance<impl alloy_provider::Provider + Clone>> {
        let provider = self.provider_with_role(Role::Proposer)?;
        // In v5.0.0, AnchorStateRegistry no longer has a portal() method.
        // Use the portal address from deployed contracts instead.
        let portal_addr = self.deployed.portal;
        let portal = MockOptimismPortal2::new(portal_addr, provider);
        Ok(portal)
    }

    /// Create a fault dispute game instance as the proposer by default
    pub async fn fault_dispute_game(
        &self,
        game_address: Address,
    ) -> Result<OPSuccinctFaultDisputeGameInstance<impl alloy_provider::Provider + Clone>> {
        self.fault_dispute_game_with_role(game_address, Role::Proposer).await
    }

    /// Create a fault dispute game instance with a provider for the specified role
    pub async fn fault_dispute_game_with_role(
        &self,
        game_address: Address,
        role: Role,
    ) -> Result<OPSuccinctFaultDisputeGameInstance<impl alloy_provider::Provider + Clone>> {
        let provider = self.provider_with_role(role)?;
        let game = OPSuccinctFaultDisputeGame::new(game_address, provider);
        Ok(game)
    }

    pub async fn create_game(
        &self,
        root_claim: FixedBytes<32>,
        block: u64,
        parent_id: u32,
        init_bond: Uint<256, 4>,
    ) -> Result<TransactionReceipt> {
        let factory = self.factory()?;
        let extra_data = (U256::from(block), parent_id).abi_encode_packed();
        let receipt = factory
            .create(self.game_type, root_claim, extra_data.into())
            .value(init_bond)
            .send()
            .await?
            .with_required_confirmations(1)
            .get_receipt()
            .await?;
        Ok(receipt)
    }

    pub async fn resolve_game(&self, address: Address) -> Result<TransactionReceipt> {
        let game = self.fault_dispute_game(address).await?;
        let receipt =
            game.resolve().send().await?.with_required_confirmations(1).get_receipt().await?;
        Ok(receipt)
    }

    pub async fn claim_bond(
        &self,
        game_address: Address,
        recipient: Address,
    ) -> Result<TransactionReceipt> {
        let game = self.fault_dispute_game(game_address).await?;
        let receipt = game
            .claimCredit(recipient)
            .send()
            .await?
            .with_required_confirmations(1)
            .get_receipt()
            .await?;
        Ok(receipt)
    }

    pub async fn get_credit(&self, game_address: Address, recipient: Address) -> Result<U256> {
        let provider = &self.anvil.provider;
        let game = OPSuccinctFaultDisputeGame::new(game_address, provider);
        let normal_credit = game.normalModeCredit(recipient).call().await?;
        let refund_credit = game.refundModeCredit(recipient).call().await?;
        Ok(normal_credit + refund_credit)
    }

    pub async fn last_game_info(&self) -> Result<(Uint<256, 4>, Address)> {
        let factory = self.factory()?;
        let game_count = factory.gameCount().call().await?;
        let index = game_count.saturating_sub(U256::from(1));
        let game_info = factory.gameAtIndex(index).call().await?;
        Ok((index, game_info.proxy_))
    }

    pub async fn set_anchor_state(&self, game_address: Address) -> Result<TransactionReceipt> {
        let anchor_registry = self.anchor_registry(game_address).await?;
        let receipt = anchor_registry
            .setAnchorState(game_address)
            .send()
            .await?
            .with_required_confirmations(1)
            .get_receipt()
            .await?;
        Ok(receipt)
    }

    pub async fn warp_time(&self, secs: u64) -> Result<()> {
        warp_time(&self.anvil.provider, Duration::from_secs(secs)).await
    }

    pub async fn challenge_game(&self, address: Address) -> Result<TransactionReceipt> {
        let game = self.fault_dispute_game_with_role(address, Role::Challenger).await?;
        let challenger_bond = game.challengerBond().call().await?;
        let receipt = game
            .challenge()
            .value(challenger_bond)
            .send()
            .await?
            .with_required_confirmations(1)
            .get_receipt()
            .await?;

        Ok(receipt)
    }

    pub async fn prove_game(&self, address: Address) -> Result<TransactionReceipt> {
        let game = self.fault_dispute_game(address).await?;
        let receipt = game
            .prove(Bytes::new())
            .send()
            .await?
            .with_required_confirmations(1)
            .get_receipt()
            .await?;

        Ok(receipt)
    }

    /// Deploy a new game implementation with custom vkeys via forge script and set it in the
    /// factory. This simulates a hardfork where the game implementation changes.
    /// Note: This also sets the implementation in the factory, so calling set_game_implementation
    /// afterwards is not required.
    pub async fn deploy_game_impl_with_vkeys(
        &self,
        aggregation_vkey: B256,
        range_vkey_commitment: B256,
    ) -> Result<Address> {
        use std::process::Command;

        // Get the SP1 verifier address from the existing game implementation
        let provider = self.provider_with_role(Role::Proposer)?;
        let existing_game =
            OPSuccinctFaultDisputeGame::new(self.deployed.game_implementation, provider);
        let verifier_address = existing_game.sp1Verifier().call().await?;

        // Compute rollup_config_hash from the fetcher's chain config
        // This stays the same during hardforks (only vkeys change)
        let rollup_config_hash = hash_rollup_config(
            self.fetcher.rollup_config.as_ref().context("rollup_config required")?,
        );

        let contracts_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("contracts");

        // Build environment variables for the forge script
        // Uses UpgradeOPSuccinctFDG.s.sol which deploys impl and sets it in factory
        let output = Command::new("forge")
            .arg("script")
            .arg("script/fp/UpgradeOPSuccinctFDG.s.sol")
            .arg("--broadcast")
            .arg("--rpc-url")
            .arg(&self.anvil.endpoint)
            .arg("--private-key")
            .arg(self.private_keys.deployer)
            .arg("--json")
            .env("FACTORY_ADDRESS", self.deployed.factory.to_string())
            .env("GAME_TYPE", TEST_GAME_TYPE.to_string())
            .env("VERIFIER_ADDRESS", verifier_address.to_string())
            .env("ANCHOR_STATE_REGISTRY", self.deployed.anchor_state_registry.to_string())
            .env("ACCESS_MANAGER", self.deployed.access_manager.to_string())
            .env("AGGREGATION_VKEY", aggregation_vkey.to_string())
            .env("RANGE_VKEY_COMMITMENT", range_vkey_commitment.to_string())
            .env("ROLLUP_CONFIG_HASH", rollup_config_hash.to_string())
            .env("MAX_CHALLENGE_DURATION", MAX_CHALLENGE_DURATION.to_string())
            .env("MAX_PROVE_DURATION", MAX_PROVE_DURATION.to_string())
            .env("CHALLENGER_BOND_WEI", CHALLENGER_BOND.to_string())
            .current_dir(&contracts_dir)
            .output()
            .context("Failed to execute forge script")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Forge script failed: {}", stderr);
        }

        // Parse JSON output to extract gameImpl address
        let stdout = String::from_utf8_lossy(&output.stdout);
        let impl_address = parse_game_impl_address(&stdout)?;

        info!("✓ Deployed and set game implementation with custom vkeys at {impl_address}");
        Ok(impl_address)
    }

    /// Set the game implementation for a specific game type.
    /// This upgrades the factory to use a new implementation (hardfork).
    pub async fn set_game_implementation(&self, game_type: u32, new_impl: Address) -> Result<()> {
        let set_impl_call =
            DisputeGameFactory::setImplementation_0Call { _gameType: game_type, _impl: new_impl };
        self.send_factory_tx(set_impl_call.abi_encode(), None).await?;

        info!("✓ Set game implementation for type {game_type} to {new_impl}");
        Ok(())
    }
}

pub struct TestPrivateKeys {
    pub deployer: &'static str,
    pub proposer: &'static str,
    pub challenger: &'static str,
}

impl Default for TestPrivateKeys {
    fn default() -> Self {
        Self {
            deployer: DEPLOYER_PRIVATE_KEY,
            proposer: PROPOSER_PRIVATE_KEY,
            challenger: CHALLENGER_PRIVATE_KEY,
        }
    }
}

/// Initialize logging for tests
pub fn init_logging() {
    static INIT: OnceLock<()> = OnceLock::new();

    INIT.get_or_init(|| {
        let level =
            std::env::var("RUST_LOG").unwrap_or("info".to_string()).parse().unwrap_or(Level::INFO);

        let filter = Targets::new().with_targets([
            ("backup", level),
            ("integration", level),
            ("sync", level),
            ("fault_proof", level),
            ("op_succinct_fp", level),
            ("op_succinct_client_utils", level),
            ("op_succinct_ethereum_host_utils", level),
            ("op_succinct_host_utils", level),
            ("op_succinct_proof_utils", level),
            ("op_succinct_signer_utils", level),
        ]);

        tracing_subscriber::registry().with(fmt::layer().compact()).with(filter).init()
    });
}

/// Parse the forge script JSON output to extract the gameImpl address.
fn parse_game_impl_address(output: &str) -> Result<Address> {
    #[derive(serde::Deserialize)]
    struct ForgeOutput {
        returns: ForgeReturns,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ForgeReturns {
        game_impl: AddressField,
    }

    #[derive(serde::Deserialize)]
    struct AddressField {
        value: String,
    }

    let json_line =
        output.lines().next().ok_or_else(|| anyhow::anyhow!("No output from forge script"))?;

    let forge_output: ForgeOutput =
        serde_json::from_str(json_line).context("Failed to parse forge output")?;

    forge_output.returns.game_impl.value.parse().context("Failed to parse gameImpl address")
}
