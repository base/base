//! End-to-end tests for B-20 precompiles over Base node RPC.

mod common;

use alloy_primitives::{Address, B256, U256};
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolValue;
use base_common_network::Base;
use base_common_precompiles::{
    ActivationFeature, B20FactoryStorage, B20TokenRole, B20Variant, IB20, IB20Factory,
};
use devnet::{
    B20PrecompileClient,
    config::{ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, ANVIL_ACCOUNT_7},
};
use eyre::{Result, WrapErr};

const TOKEN_DECIMALS: u8 = 18;
const INITIAL_SUPPLY: u64 = 1_000_000_000;
const TRANSFER_AMOUNT: u64 = 100_000_000;
const MINT_AMOUNT: u64 = 500_000;
const BURN_AMOUNT: u64 = 200_000;
const APPROVE_AMOUNT: u64 = 50_000_000;
const SPENDER_TRANSFER_AMOUNT: u64 = 30_000_000;
const MEMO_TRANSFER_AMOUNT: u64 = 111_000;
const INITIAL_SUPPLY_CAP: u64 = 2_000_000_000;
const PAUSE_TRANSFER_AMOUNT: u64 = 10_000;

async fn activated_b20_client<'a>(
    provider: &'a RootProvider<Base>,
    admin: &'a PrivateKeySigner,
) -> Result<B20PrecompileClient<'a>> {
    let b20 = B20PrecompileClient::new(provider, admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    b20.activate_feature(ActivationFeature::B20Factory.id()).await?;
    b20.activate_feature(ActivationFeature::B20Token.id()).await?;
    Ok(b20)
}

#[tokio::test]
async fn test_b20_factory_create_and_transfer_via_rpc() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse devnet private key")?;
    let recipient = ANVIL_ACCOUNT_6.address;

    common::wait_for_balance(&provider, admin.address()).await?;

    let b20 = activated_b20_client(&provider, &admin).await?;
    let salt = B256::repeat_byte(0x42);
    let params = B20PrecompileClient::token_params(
        "Devnet B20",
        "DB20",
        admin.address(),
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );

    let token = b20.create_token(B20Variant::B20, params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    assert_eq!(b20.variant_of(token).await?, B20Variant::B20);
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

#[tokio::test]
async fn test_b20_token_metadata() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    common::wait_for_balance(&provider, admin.address()).await?;

    let b20 = activated_b20_client(&provider, &admin).await?;
    let salt = B256::repeat_byte(0x10);
    let params = B20PrecompileClient::token_params(
        "Metadata Token",
        "META",
        admin.address(),
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );

    let token = b20.create_token(B20Variant::B20, params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    assert_eq!(b20.name(token).await?, "Metadata Token");
    assert_eq!(b20.symbol(token).await?, "META");
    assert_eq!(b20.total_supply(token).await?, U256::from(INITIAL_SUPPLY));

    Ok(())
}

#[tokio::test]
async fn test_b20_approve_and_transfer_from() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    let spender =
        PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_7.private_key).wrap_err("spender key")?;
    let recipient = ANVIL_ACCOUNT_6.address;
    common::wait_for_balance(&provider, admin.address()).await?;
    common::wait_for_balance(&provider, spender.address()).await?;

    let b20_admin = activated_b20_client(&provider, &admin).await?;
    let b20_spender = B20PrecompileClient::new(&provider, &spender, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);

    let salt = B256::repeat_byte(0x11);
    let params = B20PrecompileClient::token_params(
        "Allowance Token",
        "ALLW",
        admin.address(),
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );
    let token = b20_admin.create_token(B20Variant::B20, params, salt).await?;
    b20_admin
        .wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL)
        .await?;

    let approve_amount = U256::from(APPROVE_AMOUNT);
    let transfer_amount = U256::from(SPENDER_TRANSFER_AMOUNT);

    b20_admin.approve(token, spender.address(), approve_amount).await?;
    assert_eq!(
        b20_admin.allowance(token, admin.address(), spender.address()).await?,
        approve_amount
    );

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
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    common::wait_for_balance(&provider, admin.address()).await?;

    let b20 = activated_b20_client(&provider, &admin).await?;
    let salt = B256::repeat_byte(0x12);
    let params = B20PrecompileClient::token_params(
        "Mintable Token",
        "MINT",
        admin.address(),
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );
    let token = b20.create_token(B20Variant::B20, params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    let supply_before = b20.total_supply(token).await?;

    b20.send_call(
        token,
        IB20::grantRoleCall { role: B20TokenRole::Mint.id(), account: admin.address() },
        "grant B-20 mint role",
    )
    .await?;
    b20.send_call(
        token,
        IB20::grantRoleCall { role: B20TokenRole::Burn.id(), account: admin.address() },
        "grant B-20 burn role",
    )
    .await?;

    let zero_mint_succeeded = b20
        .try_send_call(
            token,
            IB20::mintCall { to: admin.address(), amount: U256::ZERO },
            "zero amount B-20 mint",
        )
        .await?;
    assert!(!zero_mint_succeeded, "zero amount B-20 mint should revert");

    let zero_burn_succeeded = b20
        .try_send_call(token, IB20::burnCall { amount: U256::ZERO }, "zero amount B-20 burn")
        .await?;
    assert!(!zero_burn_succeeded, "zero amount B-20 burn should revert");
    assert_eq!(b20.total_supply(token).await?, supply_before);

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
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    let recipient = ANVIL_ACCOUNT_6.address;
    common::wait_for_balance(&provider, admin.address()).await?;

    let b20 = activated_b20_client(&provider, &admin).await?;
    let salt = B256::repeat_byte(0x13);
    let params = B20PrecompileClient::token_params(
        "Memo Token",
        "MEMO",
        admin.address(),
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );
    let token = b20.create_token(B20Variant::B20, params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    let memo = B256::repeat_byte(0xde);
    let amount = U256::from(MEMO_TRANSFER_AMOUNT);
    b20.transfer_with_memo(token, recipient, amount, memo).await?;

    assert_eq!(b20.balance_of(token, recipient).await?, amount);
    assert_eq!(b20.balance_of(token, admin.address()).await?, U256::from(INITIAL_SUPPLY) - amount,);

    Ok(())
}

#[tokio::test]
async fn test_b20_supply_cap() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    common::wait_for_balance(&provider, admin.address()).await?;

    let b20 = activated_b20_client(&provider, &admin).await?;
    let salt = B256::repeat_byte(0x14);
    let mut params = B20PrecompileClient::token_params(
        "Capped Token",
        "CAP",
        admin.address(),
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );
    params.supply_cap = U256::from(INITIAL_SUPPLY_CAP);

    let token = b20.create_token(B20Variant::B20, params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    assert_eq!(b20.supply_cap(token).await?, U256::from(INITIAL_SUPPLY_CAP));

    // Cap below current total supply reverts.
    assert!(
        !b20.try_send_call(
            token,
            IB20::updateSupplyCapCall { newSupplyCap: U256::from(INITIAL_SUPPLY - 1) },
            "updateSupplyCap below current supply",
        )
        .await?,
        "updateSupplyCap below total supply should revert",
    );

    // Tighten cap to exactly the current supply.
    b20.update_supply_cap(token, U256::from(INITIAL_SUPPLY)).await?;
    assert_eq!(b20.supply_cap(token).await?, U256::from(INITIAL_SUPPLY));

    // Minting past the cap reverts.
    assert!(
        !b20.try_send_call(
            token,
            IB20::mintCall { to: admin.address(), amount: U256::from(1) },
            "mint past supply cap",
        )
        .await?,
        "mint past supply cap should revert",
    );

    Ok(())
}

#[tokio::test]
async fn test_b20_metadata_updates() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    common::wait_for_balance(&provider, admin.address()).await?;

    let b20 = activated_b20_client(&provider, &admin).await?;
    let salt = B256::repeat_byte(0x15);
    let params = B20PrecompileClient::token_params(
        "Old Name",
        "OLD",
        admin.address(),
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );
    let token = b20.create_token(B20Variant::B20, params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    b20.update_name(token, "New Name").await?;
    b20.update_symbol(token, "NEW").await?;
    b20.update_contract_uri(token, "ipfs://QmTest").await?;

    assert_eq!(b20.name(token).await?, "New Name");
    assert_eq!(b20.symbol(token).await?, "NEW");
    assert_eq!(b20.contract_uri(token).await?, "ipfs://QmTest");

    Ok(())
}

#[tokio::test]
async fn test_b20_pause_and_unpause() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    let recipient = ANVIL_ACCOUNT_6.address;
    common::wait_for_balance(&provider, admin.address()).await?;

    let b20 = activated_b20_client(&provider, &admin).await?;
    let salt = B256::repeat_byte(0x16);
    let params = B20PrecompileClient::token_params(
        "Pausable Token",
        "PAUS",
        admin.address(),
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );
    let token = b20.create_token(B20Variant::B20, params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    // Transfer succeeds before pause.
    b20.transfer(token, recipient, U256::from(PAUSE_TRANSFER_AMOUNT)).await?;
    assert_eq!(b20.balance_of(token, recipient).await?, U256::from(PAUSE_TRANSFER_AMOUNT));

    b20.send_call(
        token,
        IB20::grantRoleCall { role: B20TokenRole::Pause.id(), account: admin.address() },
        "grant B-20 pause role",
    )
    .await?;
    b20.send_call(
        token,
        IB20::grantRoleCall { role: B20TokenRole::Unpause.id(), account: admin.address() },
        "grant B-20 unpause role",
    )
    .await?;

    b20.pause(token, U256::from(1)).await?;
    assert_ne!(b20.paused(token).await?, U256::ZERO, "token should be paused");

    // Transfer reverts while paused.
    assert!(
        !b20.try_send_call(
            token,
            IB20::transferCall { to: recipient, amount: U256::from(PAUSE_TRANSFER_AMOUNT) },
            "transfer while paused",
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
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    common::wait_for_balance(&provider, admin.address()).await?;

    let b20 = activated_b20_client(&provider, &admin).await?;
    let salt = B256::repeat_byte(0x17);
    let params = B20PrecompileClient::token_params(
        "Predict Token",
        "PRD",
        admin.address(),
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );

    let local_prediction = b20.predict_token_address(B20Variant::B20, salt);
    let rpc_prediction =
        b20.predict_token_address_rpc(admin.address(), B20Variant::B20, salt).await?;
    assert_eq!(local_prediction, rpc_prediction, "local and RPC predictions should match");

    let token = b20.create_token(B20Variant::B20, params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    assert_eq!(token, rpc_prediction, "created token address should match prediction");

    assert!(b20.is_b20(token).await?, "created token should be recognised as B-20");
    assert!(!b20.is_b20(B20FactoryStorage::ADDRESS).await?, "factory address is not a B-20 token",);
    assert!(
        !b20.is_b20(Address::repeat_byte(0xab)).await?,
        "arbitrary address is not a B-20 token",
    );

    Ok(())
}

#[tokio::test]
async fn test_b20_create_token_duplicate_reverts() -> Result<()> {
    let (_devnet, provider) = common::start_beryl_devnet().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    common::wait_for_balance(&provider, admin.address()).await?;

    let b20 = activated_b20_client(&provider, &admin).await?;
    let salt = B256::repeat_byte(0x18);
    let params = B20PrecompileClient::token_params(
        "Dup Token",
        "DUP",
        admin.address(),
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );

    let token = b20.create_token(B20Variant::B20, params.clone(), salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    let succeeded = b20
        .try_send_call(
            B20FactoryStorage::ADDRESS,
            IB20Factory::createB20Call {
                variant: IB20Factory::B20Variant::DEFAULT,
                salt,
                params: params.create.abi_encode().into(),
                initCalls: Vec::new(),
            },
            "createB20 (duplicate salt)",
        )
        .await?;
    assert!(!succeeded, "creating a token with the same salt should revert on-chain");

    Ok(())
}
