use std::{env, time::Duration};

use alloy_eips::BlockNumberOrTag;
use alloy_network::Ethereum;
use alloy_primitives::{Address, U256};
use alloy_provider::{fillers::TxFiller, Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolValue;
use alloy_transport_http::reqwest::Url;
use anyhow::{Context, Result};
use clap::Parser;
use op_alloy_network::EthereumWallet;
use tokio::time;

use fault_proof::{
    config::ProposerConfig,
    contract::{DisputeGameFactory, DisputeGameFactory::DisputeGameFactoryInstance},
    utils::setup_logging,
    FactoryTrait, L1ProviderWithWallet, L2Provider, L2ProviderTrait, Mode, NUM_CONFIRMATIONS,
    TIMEOUT_SECONDS,
};

#[derive(Parser)]
struct Args {
    #[clap(long, default_value = ".env.proposer")]
    env_file: String,
}

struct OPSuccinctProposer<F, P>
where
    F: TxFiller<Ethereum> + Send + Sync,
    P: Provider<Ethereum> + Clone + Send + Sync,
{
    config: ProposerConfig,
    l1_provider_with_wallet: L1ProviderWithWallet<F, P>,
    l2_provider: L2Provider,
    factory: DisputeGameFactoryInstance<(), L1ProviderWithWallet<F, P>>,
    init_bond: U256,
}

impl<F, P> OPSuccinctProposer<F, P>
where
    F: TxFiller<Ethereum> + Send + Sync,
    P: Provider<Ethereum> + Clone + Send + Sync,
{
    /// Creates a new challenger instance with the provided L1 provider with wallet and factory contract instance.
    pub async fn new(
        l1_provider_with_wallet: L1ProviderWithWallet<F, P>,
        factory: DisputeGameFactoryInstance<(), L1ProviderWithWallet<F, P>>,
    ) -> Result<Self> {
        let config = ProposerConfig::from_env()?;

        Ok(Self {
            config: config.clone(),
            l1_provider_with_wallet: l1_provider_with_wallet.clone(),
            l2_provider: ProviderBuilder::default().on_http(config.l2_rpc),
            factory: factory.clone(),
            init_bond: factory.fetch_init_bond(config.game_type).await?,
        })
    }

    /// Creates a new game with the given parameters.
    ///
    /// `l2_block_number`: the L2 block number we are proposing the output root for.
    /// `parent_game_index`: the index of the parent game.
    async fn create_game(&self, l2_block_number: U256, parent_game_index: u32) -> Result<()> {
        tracing::info!(
            "Creating game at L2 block number: {:?}, with parent game index: {:?}",
            l2_block_number,
            parent_game_index
        );

        let extra_data = <(U256, u32)>::abi_encode_packed(&(l2_block_number, parent_game_index));

        // TODO(fakedev9999): Potentially need to add a gas provider.
        let receipt = self
            .factory
            .create(
                self.config.game_type,
                self.l2_provider
                    .compute_output_root_at_block(l2_block_number)
                    .await?,
                extra_data.into(),
            )
            .value(self.init_bond)
            .send()
            .await
            .context("Failed to send create transaction")?
            .with_required_confirmations(NUM_CONFIRMATIONS)
            .with_timeout(Some(Duration::from_secs(TIMEOUT_SECONDS)))
            .get_receipt()
            .await?;

        let game_address =
            Address::from_slice(&receipt.inner.logs()[0].inner.data.topics()[1][12..]);
        tracing::info!(
            "\x1b[1mNew game at address {:?} created with tx {:?}\x1b[0m",
            game_address,
            receipt.transaction_hash
        );

        Ok(())
    }

    /// Handles the creation of a new game if conditions are met.
    async fn handle_game_creation(&self) -> Result<()> {
        let _span = tracing::info_span!("[[Proposing]]").entered();

        // Get the safe L2 head block number
        let safe_l2_head_block_number = self
            .l2_provider
            .get_l2_block_by_number(BlockNumberOrTag::Safe)
            .await?
            .header
            .number;
        tracing::debug!("Safe L2 head block number: {:?}", safe_l2_head_block_number);

        // Get the latest valid proposal.
        let latest_valid_proposal = self
            .factory
            .get_latest_valid_proposal(self.l2_provider.clone())
            .await?;

        // Determine next block number and parent game index.
        //
        // Two cases based on the result of `get_latest_valid_proposal`:
        // 1. With existing valid proposal:
        //    - Block number = latest valid proposal's block + proposal interval.
        //    - Parent = latest valid game's index.
        //
        // 2. Without valid proposal (first game or all existing games being faulty):
        //    - Block number = anchor L2 block number + proposal interval.
        //    - Parent = u32::MAX (special value indicating no parent).
        let (next_l2_block_number_for_proposal, parent_game_index) = match latest_valid_proposal {
            Some((latest_block, latest_game_idx)) => (
                latest_block + U256::from(self.config.proposal_interval_in_blocks),
                latest_game_idx.to::<u32>(),
            ),
            None => {
                let anchor_l2_block_number = self
                    .factory
                    .get_anchor_l2_block_number(self.config.game_type)
                    .await?;
                tracing::info!("Anchor L2 block number: {:?}", anchor_l2_block_number);
                (
                    anchor_l2_block_number
                        .checked_add(U256::from(self.config.proposal_interval_in_blocks))
                        .unwrap(),
                    u32::MAX,
                )
            }
        };

        // There's always a new game to propose, as the chain is always moving forward from the genesis block set for the game type.
        // Only create a new game if the safe L2 head block number is greater than the next L2 block number for proposal.
        if U256::from(safe_l2_head_block_number) > next_l2_block_number_for_proposal {
            self.create_game(next_l2_block_number_for_proposal, parent_game_index)
                .await?;
        }

        Ok(())
    }

    /// Handles the resolution of all eligible unchallenged games.
    async fn handle_game_resolution(&self) -> Result<()> {
        let _span = tracing::info_span!("[[Resolving]]").entered();

        self.factory
            .resolve_games(
                Mode::Proposer,
                self.config.max_games_to_check_for_resolution,
                self.l1_provider_with_wallet.clone(),
                self.l2_provider.clone(),
            )
            .await
    }

    /// Runs the proposer indefinitely.
    async fn run(&self) -> Result<()> {
        tracing::info!("OP Succinct Proposer running...");
        let mut interval = time::interval(Duration::from_secs(self.config.fetch_interval));

        loop {
            interval.tick().await;

            if let Err(e) = self.handle_game_creation().await {
                tracing::warn!("Failed to handle game creation: {:?}", e);
            }

            if let Err(e) = self.handle_game_resolution().await {
                tracing::warn!("Failed to handle game resolution: {:?}", e);
            }
        }
    }
}

#[tokio::main]
async fn main() {
    setup_logging();

    let args = Args::parse();
    dotenv::from_filename(args.env_file).ok();

    let wallet = EthereumWallet::from(
        env::var("PRIVATE_KEY")
            .expect("PRIVATE_KEY must be set")
            .parse::<PrivateKeySigner>()
            .unwrap(),
    );

    let l1_provider_with_wallet = ProviderBuilder::new()
        .wallet(wallet.clone())
        .on_http(env::var("L1_RPC").unwrap().parse::<Url>().unwrap());

    let factory = DisputeGameFactory::new(
        env::var("FACTORY_ADDRESS")
            .expect("FACTORY_ADDRESS must be set")
            .parse::<Address>()
            .unwrap(),
        l1_provider_with_wallet.clone(),
    );

    let proposer = OPSuccinctProposer::new(l1_provider_with_wallet, factory)
        .await
        .unwrap();
    proposer.run().await.expect("Runs in an infinite loop");
}
