//! End-to-end tests for the native ERC20 precompile over Base node RPC.

use std::time::Duration;

use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use base_common_network::Base;
use devnet::{
    Devnet, DevnetBuilder, NativeErc20Precompile,
    config::{ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6},
};
use eyre::{Result, WrapErr, ensure};
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const BASE_AZUL_ACTIVATION_BLOCK: u64 = 0;
const BASE_BERYL_ACTIVATION_BLOCK: u64 = 3;
const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(30);
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);
const TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);
const MINT_AMOUNT: u64 = 1_000_000_000;
const TRANSFER_AMOUNT: u64 = 100_000_000;

#[tokio::test]
#[ignore = "requires the native ERC20 precompile implementation to be installed"]
async fn test_native_erc20_precompile_transfer_via_rpc() -> Result<()> {
    let devnet = NativeErc20Devnet::start().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse devnet private key")?;
    let recipient = ANVIL_ACCOUNT_6.address;

    devnet.wait_for_balance(admin.address()).await?;
    devnet.wait_for_native_erc20_code(&admin).await?;

    let native_erc20 = NativeErc20Precompile::new(devnet.provider(), &admin, L2_CHAIN_ID)
        .with_receipt_timeout(TX_RECEIPT_TIMEOUT);
    let issuer_role = native_erc20.issuer_role().await?;

    native_erc20.grant_role(issuer_role, admin.address()).await?;
    native_erc20.mint(admin.address(), U256::from(MINT_AMOUNT)).await?;

    let admin_balance_before = native_erc20.balance_of(admin.address()).await?;
    ensure!(
        admin_balance_before >= U256::from(TRANSFER_AMOUNT),
        "admin native ERC20 balance is too low after mint: {admin_balance_before}"
    );

    native_erc20.transfer(recipient, U256::from(TRANSFER_AMOUNT)).await?;

    let admin_balance_after = native_erc20.balance_of(admin.address()).await?;
    let recipient_balance = native_erc20.balance_of(recipient).await?;

    assert_eq!(recipient_balance, U256::from(TRANSFER_AMOUNT));
    assert_eq!(
        admin_balance_before - admin_balance_after,
        U256::from(TRANSFER_AMOUNT),
        "admin balance should decrease by transfer amount"
    );

    Ok(())
}

struct NativeErc20Devnet {
    _devnet: Devnet,
    provider: RootProvider<Base>,
}

impl NativeErc20Devnet {
    async fn start() -> Result<Self> {
        let devnet = DevnetBuilder::new()
            .with_l1_chain_id(L1_CHAIN_ID)
            .with_l2_chain_id(L2_CHAIN_ID)
            .with_base_azul_activation_block(BASE_AZUL_ACTIVATION_BLOCK)
            .with_base_beryl_activation_block(BASE_BERYL_ACTIVATION_BLOCK)
            .build()
            .await?;

        let provider = devnet.l2_builder_provider()?;
        let this = Self { _devnet: devnet, provider };
        this.wait_for_block(BASE_BERYL_ACTIVATION_BLOCK + 1).await?;
        Ok(this)
    }

    const fn provider(&self) -> &RootProvider<Base> {
        &self.provider
    }

    async fn wait_for_block(&self, min_block: u64) -> Result<u64> {
        timeout(BLOCK_PRODUCTION_TIMEOUT, async {
            loop {
                let block = self.provider.get_block_number().await?;
                if block >= min_block {
                    return Ok::<_, eyre::Error>(block);
                }
                sleep(BLOCK_POLL_INTERVAL).await;
            }
        })
        .await
        .wrap_err("Block production timed out")?
    }

    async fn wait_for_balance(&self, address: Address) -> Result<()> {
        timeout(Duration::from_secs(15), async {
            loop {
                let balance = self.provider.get_balance(address).await?;
                if balance > U256::ZERO {
                    return Ok::<_, eyre::Error>(());
                }
                sleep(BLOCK_POLL_INTERVAL).await;
            }
        })
        .await
        .wrap_err("Timed out waiting for funded devnet account")?
    }

    async fn wait_for_native_erc20_code(&self, signer: &PrivateKeySigner) -> Result<()> {
        NativeErc20Precompile::new(&self.provider, signer, L2_CHAIN_ID)
            .wait_for_code(TX_RECEIPT_TIMEOUT, BLOCK_POLL_INTERVAL)
            .await
    }
}
