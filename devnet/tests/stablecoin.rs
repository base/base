//! End-to-end tests for the stablecoin B-20 variant over Base node RPC.

mod common;

use alloy_primitives::{B256, U256};
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use base_common_network::Base;
use base_common_precompiles::{ActivationFeature, B20TokenRole, B20Variant, IB20};
use devnet::{
    B20PrecompileClient,
    config::{ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6},
};
use eyre::{Result, WrapErr};

const INITIAL_SUPPLY: u64 = 1_000_000;
const STABLECOIN_CURRENCY: &str = "USD";
const SUPPLY_CAP: u64 = 1_000_000;
const RAISED_SUPPLY_CAP: u64 = 2_000_000;
const PAUSE_TRANSFER_AMOUNT: u64 = 10_000;

async fn activated_stablecoin_client<'a>(
    provider: &'a RootProvider<Base>,
    admin: &'a PrivateKeySigner,
) -> Result<B20PrecompileClient<'a>> {
    let b20 = B20PrecompileClient::new(provider, admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    b20.activate_feature(ActivationFeature::B20Factory.id()).await?;
    b20.activate_feature(ActivationFeature::B20Stablecoin.id()).await?;
    Ok(b20)
}

#[tokio::test]
async fn test_b20_stablecoin_factory_create_and_views() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;

    common::wait_for_balance(&provider, admin.address()).await?;

    let b20 = activated_stablecoin_client(&provider, &admin).await?;
    let salt = B256::repeat_byte(0x60);
    let params = B20PrecompileClient::stablecoin_token_params(
        "USD Stablecoin",
        "USDS",
        admin.address(),
        STABLECOIN_CURRENCY,
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );

    let token = b20.create_stablecoin_token(params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    assert!(b20.is_b20(token).await?, "stablecoin must be recognised as B-20");
    assert!(b20.is_b20_initialized(token).await?, "stablecoin must be initialized");
    assert_eq!(
        b20.currency(token).await?,
        STABLECOIN_CURRENCY,
        "currency must match creation params",
    );
    assert_eq!(b20.name(token).await?, "USD Stablecoin", "name must match creation params");
    assert_eq!(b20.symbol(token).await?, "USDS", "symbol must match creation params");
    assert_eq!(
        b20.variant_of(token).await?,
        B20Variant::Stablecoin,
        "token variant must be Stablecoin",
    );
    assert_eq!(b20.decimals_of(token).await?, 6, "stablecoin decimals must be 6");
    assert_eq!(
        b20.total_supply(token).await?,
        U256::from(INITIAL_SUPPLY),
        "total supply must equal the initial mint",
    );
    assert_eq!(
        b20.balance_of(token, admin.address()).await?,
        U256::from(INITIAL_SUPPLY),
        "admin must hold the full initial supply",
    );

    Ok(())
}

#[tokio::test]
async fn test_b20_stablecoin_supply_cap_enforcement() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;

    common::wait_for_balance(&provider, admin.address()).await?;

    let b20 = activated_stablecoin_client(&provider, &admin).await?;
    let salt = B256::repeat_byte(0x61);
    let mut params = B20PrecompileClient::stablecoin_token_params(
        "Capped USD",
        "CUSD",
        admin.address(),
        STABLECOIN_CURRENCY,
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );
    // Set the supply cap equal to the initial mint so the token starts at capacity.
    params.supply_cap = U256::from(SUPPLY_CAP);

    let token = b20.create_stablecoin_token(params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    assert_eq!(
        b20.supply_cap(token).await?,
        U256::from(SUPPLY_CAP),
        "supply cap must match creation params",
    );
    assert_eq!(
        b20.currency(token).await?,
        STABLECOIN_CURRENCY,
        "currency must remain accessible throughout the test",
    );

    // Grant MINT_ROLE so admin can attempt additional mints.
    b20.send_call(
        token,
        IB20::grantRoleCall { role: B20TokenRole::Mint.id(), account: admin.address() },
        "grant MINT_ROLE",
    )
    .await?;

    // Minting past the cap must revert.
    let mint_past_cap_succeeded = b20
        .try_send_call(
            token,
            IB20::mintCall { to: admin.address(), amount: U256::from(1u64) },
            "mint past supply cap",
        )
        .await?;
    assert!(!mint_past_cap_succeeded, "mint past supply cap must revert");
    assert_eq!(
        b20.total_supply(token).await?,
        U256::from(INITIAL_SUPPLY),
        "total supply must not change after failed mint",
    );

    // Raise the supply cap and verify the mint now succeeds.
    b20.update_supply_cap(token, U256::from(RAISED_SUPPLY_CAP)).await?;
    assert_eq!(
        b20.supply_cap(token).await?,
        U256::from(RAISED_SUPPLY_CAP),
        "supply cap must reflect the update",
    );

    b20.mint(token, admin.address(), U256::from(1u64)).await?;
    assert_eq!(
        b20.total_supply(token).await?,
        U256::from(INITIAL_SUPPLY) + U256::ONE,
        "total supply must increase after successful mint",
    );
    assert_eq!(
        b20.currency(token).await?,
        STABLECOIN_CURRENCY,
        "currency must remain accessible after supply cap changes",
    );

    Ok(())
}

#[tokio::test]
async fn test_b20_stablecoin_pause_and_transfer() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    let recipient = ANVIL_ACCOUNT_6.address;

    common::wait_for_balance(&provider, admin.address()).await?;

    let b20 = activated_stablecoin_client(&provider, &admin).await?;
    let salt = B256::repeat_byte(0x62);
    let params = B20PrecompileClient::stablecoin_token_params(
        "EUR Stablecoin",
        "EURS",
        admin.address(),
        "EUR",
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );

    let token = b20.create_stablecoin_token(params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    assert_eq!(b20.currency(token).await?, "EUR", "currency must be EUR before pause");

    // Transfer succeeds before pause.
    b20.transfer(token, recipient, U256::from(PAUSE_TRANSFER_AMOUNT)).await?;
    assert_eq!(
        b20.balance_of(token, recipient).await?,
        U256::from(PAUSE_TRANSFER_AMOUNT),
        "recipient balance must increase after transfer",
    );

    // Grant PAUSE_ROLE and UNPAUSE_ROLE so admin can toggle.
    b20.send_call(
        token,
        IB20::grantRoleCall { role: B20TokenRole::Pause.id(), account: admin.address() },
        "grant PAUSE_ROLE",
    )
    .await?;
    b20.send_call(
        token,
        IB20::grantRoleCall { role: B20TokenRole::Unpause.id(), account: admin.address() },
        "grant UNPAUSE_ROLE",
    )
    .await?;

    // Pause the TRANSFER feature (vector bit 1).
    b20.pause(token, U256::from(1u64)).await?;
    assert_ne!(b20.paused(token).await?, U256::ZERO, "token must report paused");
    assert_eq!(b20.currency(token).await?, "EUR", "currency must be readable while paused");

    // Transfer reverts while paused.
    let transfer_while_paused_succeeded = b20
        .try_send_call(
            token,
            IB20::transferCall { to: recipient, amount: U256::from(PAUSE_TRANSFER_AMOUNT) },
            "transfer while paused",
        )
        .await?;
    assert!(!transfer_while_paused_succeeded, "transfer must revert while paused");
    assert_eq!(
        b20.balance_of(token, recipient).await?,
        U256::from(PAUSE_TRANSFER_AMOUNT),
        "recipient balance must be unchanged while paused",
    );

    // Unpause and verify transfer succeeds again.
    b20.unpause(token).await?;
    assert_eq!(b20.paused(token).await?, U256::ZERO, "token must report unpaused");

    b20.transfer(token, recipient, U256::from(PAUSE_TRANSFER_AMOUNT)).await?;
    assert_eq!(
        b20.balance_of(token, recipient).await?,
        U256::from(PAUSE_TRANSFER_AMOUNT * 2),
        "recipient balance must increase after transfer following unpause",
    );
    assert_eq!(
        b20.balance_of(token, admin.address()).await?,
        U256::from(INITIAL_SUPPLY) - U256::from(PAUSE_TRANSFER_AMOUNT * 2),
        "admin balance must reflect both transfers",
    );
    assert_eq!(b20.currency(token).await?, "EUR", "currency must be readable after unpause");

    Ok(())
}
