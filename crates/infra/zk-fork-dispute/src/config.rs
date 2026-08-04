//! Fork dispute configuration resolved from CLI arguments.

use alloy_primitives::{Address, B256, keccak256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use alloy_trie::{Nibbles, TrieAccount, proof::verify_proof};
use base_challenger::DisputeIntent;
use base_common_consensus::Predeploys;
use base_proof_contracts::{DisputeGameFactoryClient, DisputeGameFactoryContractClient};
use base_proof_rpc::L2HttpProvider;
use base_protocol::OutputRoot;
use base_prover_service_protocol::ZkBackend;
use eyre::{Context, Result, bail, eyre};
use tracing::info;
use url::Url;

use crate::cli::Cli;

/// Runtime configuration for the ZK fork dispute workflow.
#[derive(Debug)]
pub struct Config {
    /// Anvil (or other) L1 fork RPC.
    pub l1_rpc_url: Url,
    /// L2 archive RPC used to compute canonical output roots.
    pub l2_provider: L2HttpProvider,
    /// Prover-service JSON-RPC endpoint.
    pub prover_service_url: Url,
    /// `DisputeGameFactory` address.
    pub dispute_game_factory: Address,
    /// Selected dispute game proxy.
    pub game_address: Address,
    /// Game type for factory registration patching.
    pub game_type: u32,
    /// Signer used for dispute transactions.
    pub private_key: PrivateKeySigner,
    /// Explicit dispute intent, if provided.
    ///
    /// When `None`, [`crate::ZkForkDispute`] infers challenge for TEE-only games and
    /// nullify when a ZK prover is already set.
    pub intent: Option<DisputeIntent>,
    /// ZK proving backend for the prover-service request.
    pub zk_backend: ZkBackend,
    /// Optional invalid intermediate index override.
    pub invalid_index: Option<u64>,
    /// When true, mutate the selected game on the fork.
    pub patch_invalid_game: bool,
    /// Poll interval while waiting for a proof.
    pub poll_interval: std::time::Duration,
    /// Proof poll timeout.
    pub poll_timeout: std::time::Duration,
}

impl Config {
    /// Builds config from clap CLI args, resolving the game on L1 via the factory.
    pub async fn from_cli(cli: Cli) -> Result<Self> {
        let args = cli.fork;

        let explicit_game = args.game_address;
        let explicit_index = args.game_index;
        let (game_address, game_type) = Self::resolve_game(
            &args.l1_rpc_url,
            args.dispute_game_factory,
            explicit_game,
            explicit_index,
        )
        .await?;

        Ok(Self {
            l1_rpc_url: args.l1_rpc_url,
            l2_provider: RootProvider::new_http(args.l2_rpc_url),
            prover_service_url: args.prover_service_url,
            dispute_game_factory: args.dispute_game_factory,
            game_address,
            game_type,
            private_key: args.private_key,
            intent: args.intent.map(Into::into),
            zk_backend: args.zk_backend.into(),
            invalid_index: args.invalid_index,
            // Only an explicit game address opts into find-mode; `--game-index` still patches.
            patch_invalid_game: explicit_game.is_none(),
            poll_interval: args.poll_interval,
            poll_timeout: args.poll_timeout,
        })
    }

    /// Returns the canonical output root at `block_number` via verified L2 state proof.
    pub async fn output_root_at_block(&self, block_number: u64) -> Result<B256> {
        let block = self
            .l2_provider
            .get_block_by_number(block_number.into())
            .await?
            .ok_or_else(|| eyre!("L2 block {block_number} not found"))?;

        let rpc_hash = block.header.hash;
        let computed_hash = block.header.inner.hash_slow();
        if rpc_hash != computed_hash {
            bail!(
                "header hash mismatch at block {block_number}: rpc={rpc_hash}, computed={computed_hash}"
            );
        }

        let account_proof = self
            .l2_provider
            .get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, Vec::new())
            .hash(rpc_hash)
            .await
            .with_context(|| {
                format!("failed to fetch L2ToL1MessagePasser proof at {block_number}")
            })?;

        if account_proof.address != Predeploys::L2_TO_L1_MESSAGE_PASSER {
            bail!(
                "account proof address mismatch at block {block_number}: expected {}, got {}",
                Predeploys::L2_TO_L1_MESSAGE_PASSER,
                account_proof.address
            );
        }

        let account = TrieAccount {
            nonce: account_proof.nonce,
            balance: account_proof.balance,
            storage_root: account_proof.storage_hash,
            code_hash: account_proof.code_hash,
        };
        verify_proof(
            block.header.inner.state_root,
            Nibbles::unpack(keccak256(account_proof.address)),
            Some(alloy_rlp::encode(account)),
            &account_proof.account_proof,
        )
        .with_context(|| {
            format!("account proof verification failed for L2 block {block_number}")
        })?;

        Ok(OutputRoot::from_parts(
            block.header.inner.state_root,
            account_proof.storage_hash,
            computed_hash,
        )
        .hash())
    }

    async fn resolve_game(
        l1_rpc_url: &Url,
        factory_address: Address,
        game_address: Option<Address>,
        game_index: Option<u64>,
    ) -> Result<(Address, u32)> {
        let provider: RootProvider = RootProvider::new_http(l1_rpc_url.clone());
        if provider.get_code_at(factory_address).await?.is_empty() {
            bail!(
                "no DisputeGameFactory bytecode at {factory_address} on {l1_rpc_url}; ensure the L1 RPC is a fork of the selected chain"
            );
        }

        let factory = DisputeGameFactoryContractClient::new(factory_address, provider);

        if let Some(game_address) = game_address {
            let game_type = Self::game_type_for_proxy(&factory, game_address).await?;
            return Ok((game_address, game_type));
        }

        let index = match game_index {
            Some(index) => index,
            None => {
                let count = factory.game_count().await?;
                if count == 0 {
                    bail!("factory {factory_address} has no games");
                }
                count - 1
            }
        };
        let game = factory.game_at_index(index).await?;
        info!(game = %game.proxy, factory_index = index, "selected dispute game");
        Ok((game.proxy, game.game_type))
    }

    /// Resolves `gameType` by scanning factory indices newest-first.
    ///
    /// One RPC per index; fine for one-shot Anvil fork init, not for mainnet-scale scans.
    async fn game_type_for_proxy(
        factory: &DisputeGameFactoryContractClient,
        game_address: Address,
    ) -> Result<u32> {
        let count = factory.game_count().await?;
        for index in (0..count).rev() {
            let game = factory.game_at_index(index).await?;
            if game.proxy == game_address {
                return Ok(game.game_type);
            }
        }
        bail!("game {game_address} not found in DisputeGameFactory")
    }
}
