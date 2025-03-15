use std::env;
use std::sync::Arc;

use alloy_primitives::Address;
use alloy_provider::ProviderBuilder;
use alloy_signer_local::PrivateKeySigner;
use alloy_transport_http::reqwest::Url;
use anyhow::Context;
use anyhow::Result;
use op_alloy_network::EthereumWallet;
use op_succinct_host_utils::fetcher::OPSuccinctDataFetcher;
use op_succinct_host_utils::hosts::default::SingleChainOPSuccinctHost;
use tokio::time::Duration;

use fault_proof::{
    contract::{DisputeGameFactory, OPSuccinctFaultDisputeGame, ProposalStatus},
    proposer::OPSuccinctProposer,
    utils::setup_logging,
    FactoryTrait,
};

#[tokio::test(flavor = "multi_thread")]
async fn test_proposer_defends_successfully() -> Result<()> {
    setup_logging();
    let _span = tracing::info_span!("[[TEST]]").entered();

    dotenv::from_filename(".env.proposer").ok();

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

    let fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;
    let proposer = OPSuccinctProposer::new(
        wallet.default_signer().address(),
        l1_provider_with_wallet.clone(),
        factory.clone(),
        Arc::new(SingleChainOPSuccinctHost {
            fetcher: Arc::new(fetcher),
        }),
    )
    .await
    .unwrap();
    let game_address = proposer.handle_game_creation().await?.unwrap();

    // Malicious challenger challenging a valid game
    tracing::info!("Malicious challenger challenging a valid game");
    let game = OPSuccinctFaultDisputeGame::new(game_address, l1_provider_with_wallet.clone());
    let challenger_bond = factory
        .fetch_challenger_bond(proposer.config.game_type)
        .await?;
    let challenge_receipt = game
        .challenge()
        .value(challenger_bond)
        .send()
        .await
        .context("Failed to send challenge transaction")?
        .with_required_confirmations(1)
        .with_timeout(Some(Duration::from_secs(60)))
        .get_receipt()
        .await
        .context("Failed to get transaction receipt for challenge")?;
    tracing::info!(
        "\x1b[1mSuccessfully challenged game {:?} with tx {:?}\x1b[0m",
        game_address,
        challenge_receipt.transaction_hash
    );

    // Proposer defending the game with a valid proof
    tracing::info!("Proposer defending the game with a valid proof");
    let tx_hash = proposer.prove_game(game_address).await?;
    tracing::info!(
        "\x1b[1mSuccessfully defended game {:?} with tx {:?}\x1b[0m",
        game_address,
        tx_hash
    );

    let status = game.claimData().call().await?.claimData_.status;
    assert_eq!(
        status,
        ProposalStatus::ChallengedAndValidProofProvided,
        "Game was not successfully defended"
    );

    Ok(())
}
