//! End-to-end tests for B-20 precompiles over Base node RPC.

use std::time::Duration;

use alloy_primitives::{B256, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use base_common_network::Base;
use base_common_precompiles::TokenVariant;
use devnet::{
    B20PrecompileClient, Devnet, DevnetBuilder,
    config::{ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6},
};
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const BASE_AZUL_ACTIVATION_BLOCK: u64 = 0;
const BASE_BERYL_ACTIVATION_BLOCK: u64 = 3;
const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(30);
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);
const TX_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);
const TOKEN_DECIMALS: u8 = 6;
const INITIAL_SUPPLY: u64 = 1_000_000_000;
const TRANSFER_AMOUNT: u64 = 100_000_000;

#[tokio::test]
async fn test_b20_factory_create_and_transfer_via_rpc() -> Result<()> {
    let devnet = B20Devnet::start().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse devnet private key")?;
    let recipient = ANVIL_ACCOUNT_6.address;

    devnet.wait_for_balance(admin.address()).await?;

    let b20 = B20PrecompileClient::new(devnet.provider(), &admin, L2_CHAIN_ID)
        .with_receipt_timeout(TX_RECEIPT_TIMEOUT);
    let salt = B256::repeat_byte(0x42);
    let params = B20PrecompileClient::token_params(
        "Devnet B20",
        "DB20",
        TOKEN_DECIMALS,
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );

    let token = b20.create_token(TokenVariant::B20, params, salt).await?;
    b20.wait_for_token_code(token, TX_RECEIPT_TIMEOUT, BLOCK_POLL_INTERVAL).await?;

    assert_eq!(b20.variant_of(token).await?, TokenVariant::B20.discriminant());
    assert_eq!(b20.decimals_of(token).await?, TOKEN_DECIMALS);

    let admin_balance_before = b20.balance_of(token, admin.address()).await?;
    assert_eq!(admin_balance_before, U256::from(INITIAL_SUPPLY));

    b20.transfer(token, recipient, U256::from(TRANSFER_AMOUNT)).await?;

    let admin_balance_after = b20.balance_of(token, admin.address()).await?;
    let recipient_balance = b20.balance_of(token, recipient).await?;

    assert_eq!(recipient_balance, U256::from(TRANSFER_AMOUNT));
    assert_eq!(admin_balance_before - admin_balance_after, U256::from(TRANSFER_AMOUNT));

    Ok(())
}

struct B20Devnet {
    _devnet: Devnet,
    provider: RootProvider<Base>,
}

impl B20Devnet {
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

    async fn wait_for_balance(&self, address: alloy_primitives::Address) -> Result<()> {
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
}
