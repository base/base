//! End-to-end tests for the security B-20 variant over Base node RPC.

mod common;

use alloy_primitives::{B256, U256};
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use base_common_network::Base;
use base_common_precompiles::{
    ActivationFeature, B20SecurityToken, B20TokenRole, B20Variant, IB20, IB20Security,
    InMemoryPolicy, InMemoryTokenAccounting,
};
use devnet::{
    B20PrecompileClient,
    config::{ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, ANVIL_ACCOUNT_7},
};
use eyre::{Result, WrapErr};

/// Concrete instantiation used only to access compile-time role constants on the generic type.
type B20SecurityConsts = B20SecurityToken<InMemoryTokenAccounting, InMemoryPolicy>;

const INITIAL_SUPPLY: u64 = 1_000_000;
const SECURITY_ISIN: &str = "US0231351067";
const BATCH_MINT_BOB: u64 = 300;
const BATCH_MINT_CAROL: u64 = 500;
const BATCH_BURN_BOB: u64 = 100;
const BATCH_BURN_CAROL: u64 = 200;
const REDEEM_MINIMUM: u64 = 20;
const REDEEM_BELOW: u64 = 19;
const REDEEM_AT_MINIMUM: u64 = 20;

/// WAD precision for share ratio arithmetic: 1e18.
const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

/// Two times WAD, used in share ratio update tests.
const TWO_WAD: U256 = U256::from_limbs([2_000_000_000_000_000_000, 0, 0, 0]);

async fn activated_security_client<'a>(
    provider: &'a RootProvider<Base>,
    admin: &'a PrivateKeySigner,
) -> Result<B20PrecompileClient<'a>> {
    let b20 = B20PrecompileClient::new(provider, admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    b20.activate_feature(ActivationFeature::B20Factory.id()).await?;
    b20.activate_feature(ActivationFeature::B20Security.id()).await?;
    b20.activate_feature(ActivationFeature::PolicyRegistry.id()).await?;
    Ok(b20)
}

#[tokio::test]
async fn test_b20_security_factory_create_and_views() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;

    common::wait_for_balance(&provider, admin.address()).await?;

    let b20 = activated_security_client(&provider, &admin).await?;
    let salt = B256::repeat_byte(0x50);
    let params = B20PrecompileClient::security_token_params(
        "Security Token",
        "STOK",
        admin.address(),
        SECURITY_ISIN,
        U256::ZERO,
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );

    let token = b20.create_security_token(params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    assert!(b20.is_b20(token).await?, "security token must be recognised as B-20");
    assert!(b20.is_b20_initialized(token).await?, "security token must be initialized");
    assert_eq!(
        b20.shares_to_tokens_ratio(token).await?,
        WAD,
        "initial sharesToTokensRatio must be WAD",
    );
    assert_eq!(
        b20.minimum_redeemable(token).await?,
        U256::ZERO,
        "initial minimumRedeemable must be zero",
    );
    assert_eq!(
        b20.security_identifier(token, "ISIN").await?,
        SECURITY_ISIN,
        "ISIN must match creation params",
    );
    assert_eq!(
        b20.balance_of(token, admin.address()).await?,
        U256::from(INITIAL_SUPPLY),
        "admin must hold the full initial supply",
    );
    assert_eq!(
        b20.to_shares(token, U256::from(100u64)).await?,
        U256::from(100u64),
        "toShares at 1:1 ratio must equal the balance",
    );
    assert_eq!(
        b20.variant_of(token).await?,
        B20Variant::Security,
        "token variant must be Security"
    );
    assert_eq!(b20.decimals_of(token).await?, 6, "security token decimals must be 6");

    Ok(())
}

#[tokio::test]
async fn test_b20_security_batch_mint_and_burn() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    let bob = ANVIL_ACCOUNT_6.address;
    let carol = ANVIL_ACCOUNT_7.address;

    common::wait_for_balance(&provider, admin.address()).await?;

    let b20 = activated_security_client(&provider, &admin).await?;
    let salt = B256::repeat_byte(0x51);
    let params = B20PrecompileClient::security_token_params(
        "Batch Token",
        "BTCH",
        admin.address(),
        SECURITY_ISIN,
        U256::ZERO,
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );

    let token = b20.create_security_token(params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    let mint_role = B20TokenRole::Mint.id();
    let burn_from_role = B20SecurityConsts::BURN_FROM_ROLE;

    b20.send_call(
        token,
        IB20::grantRoleCall { role: mint_role, account: admin.address() },
        "grant MINT_ROLE",
    )
    .await?;
    b20.send_call(
        token,
        IB20::grantRoleCall { role: burn_from_role, account: admin.address() },
        "grant BURN_FROM_ROLE",
    )
    .await?;

    b20.batch_mint(
        token,
        vec![bob, carol],
        vec![U256::from(BATCH_MINT_BOB), U256::from(BATCH_MINT_CAROL)],
    )
    .await?;

    assert_eq!(
        b20.balance_of(token, bob).await?,
        U256::from(BATCH_MINT_BOB),
        "Bob must receive the minted amount",
    );
    assert_eq!(
        b20.balance_of(token, carol).await?,
        U256::from(BATCH_MINT_CAROL),
        "Carol must receive the minted amount",
    );
    assert_eq!(
        b20.total_supply(token).await?,
        U256::from(INITIAL_SUPPLY) + U256::from(BATCH_MINT_BOB) + U256::from(BATCH_MINT_CAROL),
        "total supply must reflect batch mint",
    );

    b20.batch_burn(
        token,
        vec![bob, carol],
        vec![U256::from(BATCH_BURN_BOB), U256::from(BATCH_BURN_CAROL)],
    )
    .await?;

    assert_eq!(
        b20.balance_of(token, bob).await?,
        U256::from(BATCH_MINT_BOB - BATCH_BURN_BOB),
        "Bob balance must decrease after batch burn",
    );
    assert_eq!(
        b20.balance_of(token, carol).await?,
        U256::from(BATCH_MINT_CAROL - BATCH_BURN_CAROL),
        "Carol balance must decrease after batch burn",
    );
    assert_eq!(
        b20.total_supply(token).await?,
        U256::from(INITIAL_SUPPLY)
            + U256::from(BATCH_MINT_BOB - BATCH_BURN_BOB)
            + U256::from(BATCH_MINT_CAROL - BATCH_BURN_CAROL),
        "total supply must reflect batch burn",
    );

    Ok(())
}

#[tokio::test]
async fn test_b20_security_redeem_and_share_ratio() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;

    common::wait_for_balance(&provider, admin.address()).await?;

    let b20 = activated_security_client(&provider, &admin).await?;
    let salt = B256::repeat_byte(0x52);
    let params = B20PrecompileClient::security_token_params(
        "Redeem Token",
        "RDMT",
        admin.address(),
        SECURITY_ISIN,
        U256::ZERO,
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );

    let token = b20.create_security_token(params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    let supply_before = b20.total_supply(token).await?;
    assert_eq!(supply_before, U256::from(INITIAL_SUPPLY));

    // Admin has DEFAULT_ADMIN_ROLE and can set the minimum redeemable threshold.
    b20.update_minimum_redeemable(token, U256::from(REDEEM_MINIMUM)).await?;
    assert_eq!(
        b20.minimum_redeemable(token).await?,
        U256::from(REDEEM_MINIMUM),
        "minimumRedeemable must update to the new value",
    );

    // Redeem below the minimum must revert (19 shares < 20).
    let below_minimum_succeeded = b20
        .try_send_call(
            token,
            IB20Security::redeemCall { amount: U256::from(REDEEM_BELOW) },
            "redeem below minimum",
        )
        .await?;
    assert!(!below_minimum_succeeded, "redeem below minimum must revert");
    assert_eq!(
        b20.total_supply(token).await?,
        supply_before,
        "supply must not change on failed redeem"
    );

    // Redeem at the minimum must succeed (20 shares == 20 at 1:1 ratio).
    b20.redeem(token, U256::from(REDEEM_AT_MINIMUM)).await?;
    assert_eq!(
        b20.total_supply(token).await?,
        supply_before - U256::from(REDEEM_AT_MINIMUM),
        "total supply must decrease after successful redeem",
    );
    assert_eq!(
        b20.balance_of(token, admin.address()).await?,
        U256::from(INITIAL_SUPPLY) - U256::from(REDEEM_AT_MINIMUM),
        "admin balance must decrease after redeem",
    );

    // Grant SECURITY_OPERATOR_ROLE so admin can update the share ratio.
    let security_operator_role = B20SecurityConsts::SECURITY_OPERATOR_ROLE;
    b20.send_call(
        token,
        IB20::grantRoleCall { role: security_operator_role, account: admin.address() },
        "grant SECURITY_OPERATOR_ROLE",
    )
    .await?;

    // Update to a 2:1 ratio (2 shares per token).
    b20.update_share_ratio(token, TWO_WAD).await?;
    assert_eq!(
        b20.shares_to_tokens_ratio(token).await?,
        TWO_WAD,
        "sharesToTokensRatio must update to TWO_WAD",
    );

    // At 2:1 ratio, 50 tokens = 100 shares.
    assert_eq!(
        b20.to_shares(token, U256::from(50u64)).await?,
        U256::from(100u64),
        "toShares must reflect the new ratio",
    );

    Ok(())
}
