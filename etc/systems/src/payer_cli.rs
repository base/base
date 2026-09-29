//! Command-line demo of the ERC-8168 token payer on a running devnet.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use alloy_network::{EthereumWallet, ReceiptResponse};
use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use base_common_consensus::Call;
use base_common_network::Base;
use base_execution_payer::{IERC20, PayerConfig};
use clap::{Args, Parser, Subcommand};
use eyre::{OptionExt, Result, WrapErr, ensure};
use tracing::info;
use url::Url;

use crate::{
    ANVIL_ACCOUNT_2, ANVIL_ACCOUNT_3, ANVIL_ACCOUNT_4, PayerFixtures, SystemTestProviderExt,
    TokenPayerWallet,
};

/// Token-payer demo against a running devnet.
#[derive(Debug, Parser)]
#[command(author, version, about = "Demo the ERC-8168 token payer on a running devnet")]
pub struct PayerDemoCli {
    /// Demo step.
    #[command(subcommand)]
    pub command: PayerDemoCommand,
}

/// Demo steps.
#[derive(Debug, Subcommand)]
pub enum PayerDemoCommand {
    /// Deploy mock feeds and a token, and write the payer's config for the
    /// sequencer.
    Setup(PayerSetupArgs),
    /// Send a token transfer from a wallet with no ETH, paying gas in the
    /// token.
    Send(PayerSendArgs),
}

/// Arguments for [`PayerDemoCommand::Setup`].
#[derive(Debug, Args)]
pub struct PayerSetupArgs {
    /// RPC endpoint of the sequencer that will run the payer.
    #[arg(long, env = "L2_BUILDER_RPC_URL", default_value = "http://localhost:7545")]
    pub rpc_url: Url,
    /// Host directory the sequencer's container mounts at `--container-dir`.
    #[arg(long, default_value = ".devnet/l2/configs/payer")]
    pub out_dir: PathBuf,
    /// Path of `--out-dir` inside the sequencer's container.
    #[arg(long, default_value = "/genesis/l2/payer")]
    pub container_dir: PathBuf,
}

/// Arguments for [`PayerDemoCommand::Send`].
#[derive(Debug, Args)]
pub struct PayerSendArgs {
    /// RPC endpoint serving `payer_*`.
    #[arg(long, env = "L2_BUILDER_RPC_URL", default_value = "http://localhost:7545")]
    pub rpc_url: Url,
    /// Directory `setup` wrote the payer's config to.
    #[arg(long, default_value = ".devnet/l2/configs/payer")]
    pub config_dir: PathBuf,
    /// Recipient of the wallet's transfer.
    #[arg(long, default_value_t = ANVIL_ACCOUNT_2.address)]
    pub recipient: Address,
    /// Amount the wallet transfers, in token atomic units.
    #[arg(long, default_value_t = U256::from(10_000_000u64))]
    pub amount: U256,
}

impl PayerDemoCli {
    /// Payer config file name within the config directory.
    pub const CONFIG_FILE: &str = "payer.toml";
    /// Payer key file name within the config directory.
    pub const KEY_FILE: &str = "payer.key";
    /// Environment file the node entrypoint sources to enable the payer.
    pub const ENV_FILE: &str = "payer.env";
    /// Token minted to the demo wallet: 100 USDC.
    pub const WALLET_FUNDING: U256 = U256::from_limbs([100_000_000, 0, 0, 0]);

    /// Runs the selected step.
    pub async fn run(self) -> Result<()> {
        match self.command {
            PayerDemoCommand::Setup(args) => args.run().await,
            PayerDemoCommand::Send(args) => args.run().await,
        }
    }

    fn wallet_provider(rpc_url: &Url) -> Result<impl Provider + Clone + use<>> {
        let deployer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_4.private_key)?;
        Ok(ProviderBuilder::new()
            .wallet(EthereumWallet::from(deployer))
            .connect_http(rpc_url.clone()))
    }
}

impl PayerSetupArgs {
    /// Deploys the fixtures and writes `payer.toml`, `payer.key`, and
    /// `payer.env` to [`Self::out_dir`].
    pub async fn run(self) -> Result<()> {
        let payer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_3.private_key)?;
        let deployer = ANVIL_ACCOUNT_4.address;
        let wallet = PayerDemoCli::wallet_provider(&self.rpc_url)?;
        ensure!(
            wallet.get_balance(payer.address()).await? > U256::ZERO,
            "payer {} has no ETH on {}",
            payer.address(),
            self.rpc_url
        );

        let nonce = wallet.get_transaction_count(deployer).pending().await?;
        let fixtures = PayerFixtures::new(deployer, nonce);
        fixtures.deploy(&wallet).await?;
        let config = fixtures.payer_config(payer.address());
        config.validate()?;

        fs::create_dir_all(&self.out_dir)?;
        fs::write(self.out_dir.join(PayerDemoCli::CONFIG_FILE), toml::to_string_pretty(&config)?)?;
        let key_path = self.out_dir.join(PayerDemoCli::KEY_FILE);
        fs::write(&key_path, format!("{}\n", ANVIL_ACCOUNT_3.private_key))?;
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))?;
        fs::write(
            self.out_dir.join(PayerDemoCli::ENV_FILE),
            format!(
                "BASE_PAYER_CONFIG={}\nBASE_PAYER_KEY_PATH={}\n",
                self.container_dir.join(PayerDemoCli::CONFIG_FILE).display(),
                self.container_dir.join(PayerDemoCli::KEY_FILE).display(),
            ),
        )?;
        info!(
            payer = %payer.address(),
            token = %fixtures.token,
            eth_usd = %fixtures.eth_usd.proxy,
            usdc_usd = %fixtures.usdc_usd.proxy,
            out_dir = %self.out_dir.display(),
            "deployed payer fixtures; restart the sequencer to serve payer_*"
        );
        Ok(())
    }
}

impl PayerSendArgs {
    /// Funds a fresh wallet with the token and sends [`Self::amount`] to
    /// [`Self::recipient`] with gas paid in the token.
    pub async fn run(self) -> Result<()> {
        let config = Self::load_config(&self.config_dir)?;
        let token = config.tokens.first().ok_or_eyre("payer config lists no token")?.address;
        let chain = RootProvider::<Base>::new_http(self.rpc_url.clone());
        let chain_id = chain.get_chain_id().await?;

        let wallet = TokenPayerWallet { signer: PrivateKeySigner::random(), chain_id };
        let sender = wallet.signer.address();
        let funder = PayerDemoCli::wallet_provider(&self.rpc_url)?;
        PayerFixtures::mint(&funder, token, sender, PayerDemoCli::WALLET_FUNDING).await?;
        info!(wallet = %sender, amount = %PayerDemoCli::WALLET_FUNDING, "funded wallet with tokens only");

        let transfer = Call {
            to: token,
            value: U256::ZERO,
            data: IERC20::transferCall { to: self.recipient, amount: self.amount }
                .abi_encode()
                .into(),
        };
        let rpc = RpcClient::builder().http(self.rpc_url.clone());
        let submission = wallet.send(&rpc, token, vec![transfer], 0).await?;
        let tx_hash = submission.result.transaction_hash;
        info!(tx_hash = %tx_hash, charged = %submission.result.token_charged.amount, "payer co-signed");

        let receipt = chain.wait_for_receipt(tx_hash, Duration::from_secs(60)).await?;
        ensure!(receipt.status(), "sponsored transaction {tx_hash} reverted");
        let fee = U256::from(receipt.gas_used()) * U256::from(receipt.effective_gas_price());
        info!(
            tx_hash = %tx_hash,
            block = ?receipt.block_number(),
            payer = ?receipt.payer,
            fee_wei = %fee,
            wallet_eth = %chain.get_balance(sender).await?,
            wallet_tokens = %PayerFixtures::balance_of(&funder, token, sender).await?,
            recipient_tokens = %PayerFixtures::balance_of(&funder, token, self.recipient).await?,
            "sponsored transfer included"
        );
        Ok(())
    }

    fn load_config(dir: &Path) -> Result<PayerConfig> {
        let path = dir.join(PayerDemoCli::CONFIG_FILE);
        PayerConfig::load(&path)
            .wrap_err_with(|| format!("failed to load {}; run `setup` first", path.display()))
    }
}
