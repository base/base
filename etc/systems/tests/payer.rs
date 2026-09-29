//! System test for the ERC-8168 token payer served by the sequencer's builder.

#![cfg(feature = "payer")]

use std::time::Duration;

use alloy_consensus::Typed2718;
use alloy_network::{EthereumWallet, ReceiptResponse};
use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_client::RpcClient;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use base_common_consensus::Call;
use base_execution_payer::{
    IERC20, PayerConfig, PayerExtension, PayerExtensionConfig, PayerSponsor,
};
use base_execution_txpool::{DEFAULT_MAX_VALIDITY_EXPIRY_SECS, DEFAULT_MAX_VALIDITY_PREDICATES};
use base_node_runner::FromExtensionConfig;
use base_system_tests::{
    ANVIL_ACCOUNT_3, ANVIL_ACCOUNT_4, ANVIL_ACCOUNT_6, ANVIL_ACCOUNT_7, PayerDemoCli,
    PayerFixtures, PayerSendArgs, PayerSetupArgs, SystemTestProviderExt, SystemTestStack,
    SystemTestStackBuilder, TokenPayerWallet,
};
use eyre::{OptionExt, Result, ensure};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const EIP8130_TX_TYPE: u8 = 0x79;
const TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);
/// 1,000 USDC.
const WALLET_USDC: U256 = U256::from_limbs([1_000_000_000, 0, 0, 0]);
/// 10 USDC.
const TRANSFER_USDC: U256 = U256::from_limbs([10_000_000, 0, 0, 0]);

async fn start_payer_system(sponsor: PayerSponsor) -> Result<SystemTestStack> {
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_base_cobalt_activation_block(0)
        .with_base_denim_activation_block(0)
        .with_base_everest_activation_block(0)
        .with_payload_builder_cutover()
        .with_builder_extension(Box::new(PayerExtension::from_config(
            PayerExtensionConfig::Sponsor(Box::new(sponsor)),
        )))
        .build()
        .await?;
    system.l2_builder_provider()?.wait_for_block(3, Duration::from_secs(15)).await?;
    Ok(system)
}

/// A wallet holding USDC and no ETH sends USDC to a recipient, paying the
/// sequencer's payer for gas in USDC through `payer_sendTransaction`.
#[tokio::test]
async fn payer_sponsors_eip8130_transaction_paid_in_token() -> Result<()> {
    let deployer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_6.private_key)?;
    let payer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_7.private_key)?;
    let recipient = Address::repeat_byte(0xbe);
    let fixtures = PayerFixtures::new(deployer.address(), 0);
    let sponsor = PayerSponsor {
        config: fixtures.payer_config(payer.address()),
        signer: payer.clone(),
        max_validity_predicates: DEFAULT_MAX_VALIDITY_PREDICATES,
        max_validity_expiry_secs: DEFAULT_MAX_VALIDITY_EXPIRY_SECS,
        experimental_override: false,
    };
    let system = start_payer_system(sponsor).await?;
    let l2_url = system.l2_rpc_url()?;
    let provider = system.l2_builder_provider()?;
    provider.wait_for_balance(deployer.address(), Duration::from_secs(15)).await?;
    provider.wait_for_balance(payer.address(), Duration::from_secs(15)).await?;

    ensure!(
        provider.get_transaction_count(deployer.address()).await? == 0,
        "fixture addresses assume a fresh deployer"
    );
    let deployer_provider =
        ProviderBuilder::new().wallet(EthereumWallet::from(deployer)).connect_http(l2_url.clone());
    fixtures.deploy(&deployer_provider).await?;
    let wallet = TokenPayerWallet { signer: PrivateKeySigner::random(), chain_id: L2_CHAIN_ID };
    let sender = wallet.signer.address();
    PayerFixtures::mint(&deployer_provider, fixtures.token, sender, WALLET_USDC).await?;

    let transfer = Call {
        to: fixtures.token,
        value: U256::ZERO,
        data: IERC20::transferCall { to: recipient, amount: TRANSFER_USDC }.abi_encode().into(),
    };
    let rpc = RpcClient::builder().http(l2_url);
    let submission = wallet.send(&rpc, fixtures.token, vec![transfer], 0).await?;
    let charged = submission.choice.payment_amount;
    ensure!(submission.payer == payer.address(), "offer must name the configured payer");
    ensure!(charged > U256::ZERO, "offer must charge for gas");
    ensure!(
        submission.result.token_charged.token == fixtures.token
            && submission.result.token_charged.amount == charged,
        "charge must equal the phase-0 transfer"
    );

    let tx_hash = submission.result.transaction_hash;
    let receipt = provider.wait_for_receipt(tx_hash, TX_RECEIPT_TIMEOUT).await?;
    ensure!(receipt.status(), "sponsored transaction must succeed");
    ensure!(receipt.inner.inner.receipt.ty() == EIP8130_TX_TYPE, "receipt must be type 0x79");
    ensure!(receipt.payer == Some(payer.address()), "receipt must name the payer");

    let block = receipt.block_number().ok_or_eyre("receipt must be mined")?;
    let payer_before = provider.get_balance(payer.address()).number(block - 1).await?;
    let payer_after = provider.get_balance(payer.address()).number(block).await?;
    let execution_fee = U256::from(receipt.gas_used()) * U256::from(receipt.effective_gas_price());
    ensure!(payer_before - payer_after >= execution_fee, "payer must pay the transaction's gas");
    ensure!(provider.get_balance(sender).await? == U256::ZERO, "wallet must not spend ETH");

    let balance = |account| PayerFixtures::balance_of(&deployer_provider, fixtures.token, account);
    ensure!(balance(payer.address()).await? == charged, "payer must receive the payment");
    ensure!(balance(recipient).await? == TRANSFER_USDC, "recipient must receive the transfer");
    ensure!(
        balance(sender).await? == WALLET_USDC - charged - TRANSFER_USDC,
        "wallet must pay for both phases"
    );

    system.shutdown().await?;
    Ok(())
}

/// `base-payer-demo setup` writes a config the payer accepts, and `send`
/// lands a token-paid transfer through it.
#[tokio::test]
async fn payer_demo_cli_sets_up_and_sends() -> Result<()> {
    let payer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_3.private_key)?;
    let fixtures = PayerFixtures::new(ANVIL_ACCOUNT_4.address, 0);
    let sponsor = PayerSponsor {
        config: fixtures.payer_config(payer.address()),
        signer: payer.clone(),
        max_validity_predicates: DEFAULT_MAX_VALIDITY_PREDICATES,
        max_validity_expiry_secs: DEFAULT_MAX_VALIDITY_EXPIRY_SECS,
        experimental_override: false,
    };
    let system = start_payer_system(sponsor).await?;
    let rpc_url = system.l2_rpc_url()?;
    let provider = system.l2_builder_provider()?;
    provider.wait_for_balance(ANVIL_ACCOUNT_4.address, Duration::from_secs(15)).await?;
    provider.wait_for_balance(payer.address(), Duration::from_secs(15)).await?;
    let dir = tempfile::tempdir()?;

    PayerSetupArgs {
        rpc_url: rpc_url.clone(),
        out_dir: dir.path().to_owned(),
        container_dir: "/genesis/l2/payer".into(),
    }
    .run()
    .await?;
    let written = PayerConfig::load(&dir.path().join(PayerDemoCli::CONFIG_FILE))?;
    ensure!(written == fixtures.payer_config(payer.address()), "setup must predict the fixtures");
    let env = std::fs::read_to_string(dir.path().join(PayerDemoCli::ENV_FILE))?;
    ensure!(
        env.contains("BASE_PAYER_KEY_PATH=/genesis/l2/payer/payer.key"),
        "env must point the node at the key"
    );

    let recipient = Address::repeat_byte(0xbf);
    PayerSendArgs {
        rpc_url: rpc_url.clone(),
        config_dir: dir.path().to_owned(),
        recipient,
        amount: TRANSFER_USDC,
    }
    .run()
    .await?;
    let reader = ProviderBuilder::new().connect_http(rpc_url);
    ensure!(
        PayerFixtures::balance_of(&reader, fixtures.token, recipient).await? == TRANSFER_USDC,
        "recipient must receive the transfer"
    );

    system.shutdown().await?;
    Ok(())
}
