//! Common test environment setup utilities.
use std::{
    str::FromStr,
    sync::{Arc, OnceLock},
    time::Duration,
};

use alloy_network::EthereumWallet;
use alloy_primitives::{Address, Bytes, FixedBytes, Uint, U256};
use alloy_provider::ProviderBuilder;
use alloy_rpc_types_eth::TransactionReceipt;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolValue};
use alloy_transport_http::reqwest::Url;
use anyhow::Result;
use op_succinct_bindings::{
    anchor_state_registry::AnchorStateRegistry::{self, AnchorStateRegistryInstance},
    dispute_game_factory::DisputeGameFactory::{self, DisputeGameFactoryInstance},
    mock_optimism_portal2::MockOptimismPortal2::{self, MockOptimismPortal2Instance},
    op_succinct_fault_dispute_game::OPSuccinctFaultDisputeGame::{
        self, OPSuccinctFaultDisputeGameInstance,
    },
};
use op_succinct_host_utils::{
    fetcher::{get_rpcs_from_env, OPSuccinctDataFetcher, RPCConfig},
    host::OPSuccinctHost,
    OP_SUCCINCT_FAULT_DISPUTE_GAME_CONFIG_PATH,
};
use op_succinct_signer_utils::{Signer, SignerLock};
use tokio::task::JoinHandle;
use tracing::{info, Level};

use fault_proof::{config::FaultDisputeGameConfig, proposer::OPSuccinctProposer, L2ProviderTrait};
use tracing_subscriber::{filter::Targets, fmt, prelude::*, util::SubscriberInitExt};

use crate::common::{
    constants::*,
    contracts::{deploy_mock_permissioned_game, send_contract_transaction},
    init_proposer, start_challenger, start_proposer, warp_time, ANVIL,
};

use super::{
    anvil::{setup_anvil_chain, AnvilFork},
    contracts::{deploy_test_contracts, DeployedContracts},
};

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

/// The test configuration, used for integration tests.
pub fn test_config(starting_l2_block_number: u64, starting_root: String) -> FaultDisputeGameConfig {
    FaultDisputeGameConfig {
        aggregation_vkey: AGGREGATION_VKEY.to_string(),
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
        range_vkey_commitment: RANGE_VKEY_COMMITMENT.to_string(),
        rollup_config_hash: ROLLUP_CONFIG_HASH.to_string(),
        starting_l2_block_number,
        starting_root,
        use_sp1_mock_verifier: true,
        verifier_address: Address::ZERO.to_string(),
    }
}

impl TestEnvironment {
    /// Create a new test environment with common setup
    pub async fn setup() -> Result<Self> {
        init_logging();

        // Get environment variables
        let mut rpc_config = get_rpcs_from_env();

        let fetcher = OPSuccinctDataFetcher::new();

        // Setup fresh Anvil chain
        let anvil = setup_anvil_chain().await?;

        // Put the test config into ../contracts/opsuccinctfdgconfig.json
        let test_config: FaultDisputeGameConfig =
            test_config(anvil.starting_l2_block_number, anvil.starting_root.clone());
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

    pub async fn init_proposer(
        &self,
    ) -> Result<OPSuccinctProposer<fault_proof::L1Provider, impl OPSuccinctHost + Clone>> {
        let proposer = init_proposer(
            &self.rpc_config,
            self.private_keys.proposer,
            &self.deployed.factory,
            self.game_type,
        )
        .await?;
        info!("✓ Proposer initialized");
        Ok(proposer)
    }

    pub async fn start_proposer(&self) -> Result<JoinHandle<Result<()>>> {
        let handle = start_proposer(
            &self.rpc_config,
            self.private_keys.proposer,
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

        let set_impl_call =
            DisputeGameFactory::setImplementationCall { _gameType: game_type, _impl: legacy_impl };
        self.send_factory_tx(set_impl_call.abi_encode(), None).await?;

        info!("✓ Setup legacy game type {game_type} with implementation at {legacy_impl}");
        Ok(legacy_impl)
    }

    /// Set the respected game type on the OptimismPortal2.
    pub async fn set_respected_game_type(&self, game_type: u32) -> Result<()> {
        let set_type_call = MockOptimismPortal2::setRespectedGameTypeCall { _gameType: game_type };
        self.send_portal_tx(set_type_call.abi_encode(), None).await?;
        info!("✓ Set respected game type to {game_type}");
        Ok(())
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

    pub fn provider_with_signer(&self) -> Result<Arc<impl alloy_provider::Provider + Clone>> {
        let wallet = PrivateKeySigner::from_str(self.private_keys.proposer)?;
        let provider_with_signer = ProviderBuilder::new()
            .wallet(EthereumWallet::from(wallet))
            .connect_http(self.anvil.endpoint.parse::<Url>()?);
        Ok(Arc::new(provider_with_signer))
    }

    pub async fn compute_output_root_at_block(&self, block: u64) -> Result<FixedBytes<32>> {
        self.fetcher.l2_provider.compute_output_root_at_block(U256::from(block)).await
    }

    pub fn factory(
        &self,
    ) -> Result<DisputeGameFactoryInstance<impl alloy_provider::Provider + Clone>> {
        let provider_with_signer = self.provider_with_signer()?;
        let factory = DisputeGameFactory::new(self.deployed.factory, provider_with_signer.clone());
        Ok(factory)
    }

    pub async fn anchor_registry_address(&self, game_address: Address) -> Result<Address> {
        let provider_with_signer = self.provider_with_signer()?;
        let anchor_registry_addr =
            OPSuccinctFaultDisputeGame::new(game_address, provider_with_signer.clone())
                .anchorStateRegistry()
                .call()
                .await?;
        Ok(anchor_registry_addr)
    }

    pub async fn anchor_registry(
        &self,
        game_address: Address,
    ) -> Result<AnchorStateRegistryInstance<impl alloy_provider::Provider + Clone>> {
        let provider_with_signer = self.provider_with_signer()?;
        let anchor_registry_addr = self.anchor_registry_address(game_address).await?;
        let anchor_registry =
            AnchorStateRegistry::new(anchor_registry_addr, provider_with_signer.clone());
        Ok(anchor_registry)
    }

    pub async fn mock_optimism_portal2(
        &self,
        anchor_registry: AnchorStateRegistryInstance<impl alloy_provider::Provider + Clone>,
    ) -> Result<MockOptimismPortal2Instance<impl alloy_provider::Provider + Clone>> {
        let provider_with_signer = self.provider_with_signer()?;
        let portal_addr = anchor_registry.portal().call().await?;
        let portal = MockOptimismPortal2::new(portal_addr, provider_with_signer.clone());
        Ok(portal)
    }

    pub async fn fault_dispute_game(
        &self,
        game_address: Address,
    ) -> Result<OPSuccinctFaultDisputeGameInstance<impl alloy_provider::Provider + Clone>> {
        let provider_with_signer = self.provider_with_signer()?;
        let fault_dispute_game =
            OPSuccinctFaultDisputeGame::new(game_address, provider_with_signer.clone());
        Ok(fault_dispute_game)
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
            ("e2e", level),
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
