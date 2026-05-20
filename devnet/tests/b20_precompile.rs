//! End-to-end tests for B-20 precompiles over Base node RPC.

use std::time::Duration;

use alloy_primitives::{Address, B256, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use base_common_network::Base;
use base_common_precompiles::{
    CAPABILITY_CAP_MUTABLE, CAPABILITY_PAUSABLE, IB20, TokenFactory, TokenVariant,
};
use devnet::{
    B20PrecompileClient, Devnet, DevnetBuilder,
    config::{ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, ANVIL_ACCOUNT_7},
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

const MINT_AMOUNT: u64 = 500_000;
const BURN_AMOUNT: u64 = 200_000;
const APPROVE_AMOUNT: u64 = 50_000_000;
const SPENDER_TRANSFER_AMOUNT: u64 = 30_000_000;
const MEMO_TRANSFER_AMOUNT: u64 = 111_000;
const INITIAL_SUPPLY_CAP: u64 = 2_000_000_000;
const PAUSE_TRANSFER_AMOUNT: u64 = 10_000;

#[tokio::test]
async fn test_b20_token_metadata() -> Result<()> {
    let devnet = B20Devnet::start().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    devnet.wait_for_balance(admin.address()).await?;

    let b20 = B20PrecompileClient::new(devnet.provider(), &admin, L2_CHAIN_ID)
        .with_receipt_timeout(TX_RECEIPT_TIMEOUT);
    let salt = B256::repeat_byte(0x10);
    let params = B20PrecompileClient::token_params(
        "Metadata Token",
        "META",
        TOKEN_DECIMALS,
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );

    let token = b20.create_token(TokenVariant::B20, params, salt).await?;
    b20.wait_for_token_code(token, TX_RECEIPT_TIMEOUT, BLOCK_POLL_INTERVAL).await?;

    assert_eq!(b20.name(token).await?, "Metadata Token");
    assert_eq!(b20.symbol(token).await?, "META");
    assert_eq!(b20.total_supply(token).await?, U256::from(INITIAL_SUPPLY));

    Ok(())
}

#[tokio::test]
async fn test_b20_approve_and_transfer_from() -> Result<()> {
    let devnet = B20Devnet::start().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    let spender =
        PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_7.private_key).wrap_err("spender key")?;
    let recipient = ANVIL_ACCOUNT_6.address;
    devnet.wait_for_balance(admin.address()).await?;
    devnet.wait_for_balance(spender.address()).await?;

    let b20_admin = B20PrecompileClient::new(devnet.provider(), &admin, L2_CHAIN_ID)
        .with_receipt_timeout(TX_RECEIPT_TIMEOUT);
    let b20_spender = B20PrecompileClient::new(devnet.provider(), &spender, L2_CHAIN_ID)
        .with_receipt_timeout(TX_RECEIPT_TIMEOUT);

    let salt = B256::repeat_byte(0x11);
    let params = B20PrecompileClient::token_params(
        "Allowance Token",
        "ALLW",
        TOKEN_DECIMALS,
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );
    let token = b20_admin.create_token(TokenVariant::B20, params, salt).await?;
    b20_admin.wait_for_token_code(token, TX_RECEIPT_TIMEOUT, BLOCK_POLL_INTERVAL).await?;

    let approve_amount = U256::from(APPROVE_AMOUNT);
    let transfer_amount = U256::from(SPENDER_TRANSFER_AMOUNT);

    b20_admin.approve(token, spender.address(), approve_amount).await?;
    assert_eq!(b20_admin.allowance(token, admin.address(), spender.address()).await?, approve_amount);

    b20_spender.transfer_from(token, admin.address(), recipient, transfer_amount).await?;

    assert_eq!(
        b20_admin.balance_of(token, admin.address()).await?,
        U256::from(INITIAL_SUPPLY) - transfer_amount,
    );
    assert_eq!(b20_admin.balance_of(token, recipient).await?, transfer_amount);
    assert_eq!(
        b20_admin.allowance(token, admin.address(), spender.address()).await?,
        approve_amount - transfer_amount,
    );

    Ok(())
}

#[tokio::test]
async fn test_b20_mint_and_burn() -> Result<()> {
    let devnet = B20Devnet::start().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    devnet.wait_for_balance(admin.address()).await?;

    let b20 = B20PrecompileClient::new(devnet.provider(), &admin, L2_CHAIN_ID)
        .with_receipt_timeout(TX_RECEIPT_TIMEOUT);
    let salt = B256::repeat_byte(0x12);
    let params = B20PrecompileClient::token_params(
        "Mintable Token",
        "MINT",
        TOKEN_DECIMALS,
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );
    let token = b20.create_token(TokenVariant::B20, params, salt).await?;
    b20.wait_for_token_code(token, TX_RECEIPT_TIMEOUT, BLOCK_POLL_INTERVAL).await?;

    let supply_before = b20.total_supply(token).await?;

    b20.mint(token, admin.address(), U256::from(MINT_AMOUNT)).await?;
    assert_eq!(b20.total_supply(token).await?, supply_before + U256::from(MINT_AMOUNT));
    assert_eq!(
        b20.balance_of(token, admin.address()).await?,
        U256::from(INITIAL_SUPPLY) + U256::from(MINT_AMOUNT),
    );

    b20.burn(token, U256::from(BURN_AMOUNT)).await?;
    assert_eq!(
        b20.total_supply(token).await?,
        supply_before + U256::from(MINT_AMOUNT) - U256::from(BURN_AMOUNT),
    );
    assert_eq!(
        b20.balance_of(token, admin.address()).await?,
        U256::from(INITIAL_SUPPLY) + U256::from(MINT_AMOUNT) - U256::from(BURN_AMOUNT),
    );

    Ok(())
}

#[tokio::test]
async fn test_b20_transfer_with_memo() -> Result<()> {
    let devnet = B20Devnet::start().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    let recipient = ANVIL_ACCOUNT_6.address;
    devnet.wait_for_balance(admin.address()).await?;

    let b20 = B20PrecompileClient::new(devnet.provider(), &admin, L2_CHAIN_ID)
        .with_receipt_timeout(TX_RECEIPT_TIMEOUT);
    let salt = B256::repeat_byte(0x13);
    let params = B20PrecompileClient::token_params(
        "Memo Token",
        "MEMO",
        TOKEN_DECIMALS,
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );
    let token = b20.create_token(TokenVariant::B20, params, salt).await?;
    b20.wait_for_token_code(token, TX_RECEIPT_TIMEOUT, BLOCK_POLL_INTERVAL).await?;

    let memo = B256::repeat_byte(0xde);
    let amount = U256::from(MEMO_TRANSFER_AMOUNT);
    b20.transfer_with_memo(token, recipient, amount, memo).await?;

    assert_eq!(b20.balance_of(token, recipient).await?, amount);
    assert_eq!(
        b20.balance_of(token, admin.address()).await?,
        U256::from(INITIAL_SUPPLY) - amount,
    );

    Ok(())
}

#[tokio::test]
async fn test_b20_supply_cap() -> Result<()> {
    let devnet = B20Devnet::start().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    devnet.wait_for_balance(admin.address()).await?;

    let b20 = B20PrecompileClient::new(devnet.provider(), &admin, L2_CHAIN_ID)
        .with_receipt_timeout(TX_RECEIPT_TIMEOUT);
    let salt = B256::repeat_byte(0x14);
    let mut params = B20PrecompileClient::token_params(
        "Capped Token",
        "CAP",
        TOKEN_DECIMALS,
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );
    params.capabilities = CAPABILITY_CAP_MUTABLE;
    params.supplyCap = U256::from(INITIAL_SUPPLY_CAP);

    let token = b20.create_token(TokenVariant::B20, params, salt).await?;
    b20.wait_for_token_code(token, TX_RECEIPT_TIMEOUT, BLOCK_POLL_INTERVAL).await?;

    assert_eq!(b20.supply_cap(token).await?, U256::from(INITIAL_SUPPLY_CAP));

    // Cap below current total supply reverts.
    assert!(
        !b20.try_send_call(
            token,
            IB20::setSupplyCapCall { newSupplyCap: U256::from(INITIAL_SUPPLY - 1) },
        )
        .await?,
        "setSupplyCap below total supply should revert",
    );

    // Tighten cap to exactly the current supply.
    b20.set_supply_cap(token, U256::from(INITIAL_SUPPLY)).await?;
    assert_eq!(b20.supply_cap(token).await?, U256::from(INITIAL_SUPPLY));

    // Minting past the cap reverts.
    assert!(
        !b20.try_send_call(token, IB20::mintCall { to: admin.address(), amount: U256::from(1) })
            .await?,
        "mint past supply cap should revert",
    );

    Ok(())
}

#[tokio::test]
async fn test_b20_metadata_updates() -> Result<()> {
    let devnet = B20Devnet::start().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    devnet.wait_for_balance(admin.address()).await?;

    let b20 = B20PrecompileClient::new(devnet.provider(), &admin, L2_CHAIN_ID)
        .with_receipt_timeout(TX_RECEIPT_TIMEOUT);
    let salt = B256::repeat_byte(0x15);
    let params = B20PrecompileClient::token_params(
        "Old Name",
        "OLD",
        TOKEN_DECIMALS,
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );
    let token = b20.create_token(TokenVariant::B20, params, salt).await?;
    b20.wait_for_token_code(token, TX_RECEIPT_TIMEOUT, BLOCK_POLL_INTERVAL).await?;

    b20.set_name(token, "New Name").await?;
    b20.set_symbol(token, "NEW").await?;
    b20.set_contract_uri(token, "ipfs://QmTest").await?;

    assert_eq!(b20.name(token).await?, "New Name");
    assert_eq!(b20.symbol(token).await?, "NEW");
    assert_eq!(b20.contract_uri(token).await?, "ipfs://QmTest");

    Ok(())
}

#[tokio::test]
async fn test_b20_pause_and_unpause() -> Result<()> {
    let devnet = B20Devnet::start().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    let recipient = ANVIL_ACCOUNT_6.address;
    devnet.wait_for_balance(admin.address()).await?;

    let b20 = B20PrecompileClient::new(devnet.provider(), &admin, L2_CHAIN_ID)
        .with_receipt_timeout(TX_RECEIPT_TIMEOUT);
    let salt = B256::repeat_byte(0x16);
    let mut params = B20PrecompileClient::token_params(
        "Pausable Token",
        "PAUS",
        TOKEN_DECIMALS,
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );
    params.capabilities = CAPABILITY_PAUSABLE;

    let token = b20.create_token(TokenVariant::B20, params, salt).await?;
    b20.wait_for_token_code(token, TX_RECEIPT_TIMEOUT, BLOCK_POLL_INTERVAL).await?;

    // Transfer succeeds before pause.
    b20.transfer(token, recipient, U256::from(PAUSE_TRANSFER_AMOUNT)).await?;
    assert_eq!(b20.balance_of(token, recipient).await?, U256::from(PAUSE_TRANSFER_AMOUNT));

    b20.pause(token, U256::from(1)).await?;
    assert_ne!(b20.paused(token).await?, U256::ZERO, "token should be paused");

    // Transfer reverts while paused.
    assert!(
        !b20.try_send_call(
            token,
            IB20::transferCall { to: recipient, amount: U256::from(PAUSE_TRANSFER_AMOUNT) },
        )
        .await?,
        "transfer should revert while paused",
    );
    assert_eq!(b20.balance_of(token, recipient).await?, U256::from(PAUSE_TRANSFER_AMOUNT));

    b20.unpause(token).await?;
    assert_eq!(b20.paused(token).await?, U256::ZERO, "token should be unpaused");

    b20.transfer(token, recipient, U256::from(PAUSE_TRANSFER_AMOUNT)).await?;
    assert_eq!(b20.balance_of(token, recipient).await?, U256::from(PAUSE_TRANSFER_AMOUNT * 2));

    Ok(())
}

#[tokio::test]
async fn test_b20_factory_predict_and_is_b20() -> Result<()> {
    let devnet = B20Devnet::start().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    devnet.wait_for_balance(admin.address()).await?;

    let b20 = B20PrecompileClient::new(devnet.provider(), &admin, L2_CHAIN_ID)
        .with_receipt_timeout(TX_RECEIPT_TIMEOUT);
    let salt = B256::repeat_byte(0x17);
    let params = B20PrecompileClient::token_params(
        "Predict Token",
        "PRD",
        TOKEN_DECIMALS,
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );

    let local_prediction = b20.predict_token_address(TokenVariant::B20, TOKEN_DECIMALS, salt);
    let rpc_prediction =
        b20.predict_token_address_rpc(admin.address(), TokenVariant::B20, TOKEN_DECIMALS, salt)
            .await?;
    assert_eq!(local_prediction, rpc_prediction, "local and RPC predictions should match");

    let token = b20.create_token(TokenVariant::B20, params, salt).await?;
    b20.wait_for_token_code(token, TX_RECEIPT_TIMEOUT, BLOCK_POLL_INTERVAL).await?;

    assert_eq!(token, rpc_prediction, "created token address should match prediction");

    assert!(b20.is_b20(token).await?, "created token should be recognised as B-20");
    assert!(!b20.is_b20(TokenFactory::ADDRESS).await?, "factory address is not a B-20 token");
    assert!(
        !b20.is_b20(Address::repeat_byte(0xab)).await?,
        "arbitrary address is not a B-20 token",
    );

    Ok(())
}

#[tokio::test]
async fn test_b20_create_token_duplicate_reverts() -> Result<()> {
    let devnet = B20Devnet::start().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    devnet.wait_for_balance(admin.address()).await?;

    let b20 = B20PrecompileClient::new(devnet.provider(), &admin, L2_CHAIN_ID)
        .with_receipt_timeout(TX_RECEIPT_TIMEOUT);
    let salt = B256::repeat_byte(0x18);
    let params = B20PrecompileClient::token_params(
        "Dup Token",
        "DUP",
        TOKEN_DECIMALS,
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );

    b20.create_token(TokenVariant::B20, params.clone(), salt).await?;

    let second = b20.create_token(TokenVariant::B20, params, salt).await;
    assert!(second.is_err(), "creating a token with the same salt should fail");

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
