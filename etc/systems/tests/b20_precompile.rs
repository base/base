//! System tests for B-20 precompiles over Base node RPC.

mod common;

use alloy_primitives::{Address, B256, Bytes, LogData, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolEvent, SolValue};
use base_common_network::Base;
use base_common_precompiles::{
    ActivationFeature, B20FactoryStorage, B20TokenRole, B20Variant, IB20, IB20Factory,
};
use base_common_rpc_types::BaseTransactionReceipt;
use base_system_tests::{
    ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, ANVIL_ACCOUNT_7, B20PrecompileClient, SystemTestStack,
    SystemTestStackBuilder,
};
use eyre::{Result, WrapErr, ensure};

const TOKEN_DECIMALS: u8 = 6;
const INITIAL_SUPPLY: u64 = 1_000_000_000;
const TRANSFER_AMOUNT: u64 = 100_000_000;
const MINT_AMOUNT: u64 = 500_000;
const BURN_AMOUNT: u64 = 200_000;
const APPROVE_AMOUNT: u64 = 50_000_000;
const SPENDER_TRANSFER_AMOUNT: u64 = 30_000_000;
const MEMO_TRANSFER_AMOUNT: u64 = 111_000;
const INITIAL_SUPPLY_CAP: u64 = 2_000_000_000;
const PAUSE_TRANSFER_AMOUNT: u64 = 10_000;
const STABLECOIN_CURRENCY: &str = "USD";
const PRE_BERYL_TEST_ACTIVATION_BLOCK: u64 = 8;

async fn start_beryl_system_before_activation() -> Result<(SystemTestStack, RootProvider<Base>)> {
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(common::L1_CHAIN_ID)
        .with_l2_chain_id(common::L2_CHAIN_ID)
        .with_base_azul_activation_block(common::BASE_AZUL_ACTIVATION_BLOCK)
        .with_base_beryl_activation_block(PRE_BERYL_TEST_ACTIVATION_BLOCK)
        .build()
        .await?;
    let provider = system.l2_builder_provider()?;
    let block = provider.get_block_number().await?;
    ensure!(
        block < PRE_BERYL_TEST_ACTIVATION_BLOCK,
        "system test stack already reached Beryl activation block: {block}"
    );
    Ok((system, provider))
}

async fn activated_b20_client<'a>(
    provider: &'a RootProvider<Base>,
    admin: &'a PrivateKeySigner,
) -> Result<B20PrecompileClient<'a>> {
    activated_feature_client(provider, admin, [ActivationFeature::B20Asset]).await
}

async fn activated_feature_client<'a>(
    provider: &'a RootProvider<Base>,
    admin: &'a PrivateKeySigner,
    features: impl IntoIterator<Item = ActivationFeature>,
) -> Result<B20PrecompileClient<'a>> {
    let b20 = B20PrecompileClient::new(provider, admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    for feature in features {
        b20.activate_feature(feature.id()).await?;
    }
    Ok(b20)
}

#[tokio::test]
async fn test_b20_factory_create_and_transfer_via_rpc() -> Result<()> {
    let (_system, provider) = common::start_beryl_system().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse system test private key")?;
    let recipient = ANVIL_ACCOUNT_6.address;

    common::wait_for_balance(&provider, admin.address()).await?;

    let b20 = activated_b20_client(&provider, &admin).await?;
    let salt = B256::repeat_byte(0x42);
    let name = "System Test B20";
    let symbol = "DB20";
    let params = B20PrecompileClient::token_params(
        name,
        symbol,
        admin.address(),
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );

    let (token, create_receipt) =
        b20.create_token_with_receipt(B20Variant::Asset, params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;
    assert_b20_created_log(
        &create_receipt,
        token,
        B20Variant::Asset,
        name,
        symbol,
        TOKEN_DECIMALS,
        Bytes::new(),
    );
    assert_transfer_log(&create_receipt, token, Address::ZERO, admin.address(), INITIAL_SUPPLY);

    assert_eq!(b20.variant_of(token).await?, B20Variant::Asset);
    assert_eq!(b20.decimals_of(token).await?, TOKEN_DECIMALS);

    let admin_balance_before = b20.balance_of(token, admin.address()).await?;
    assert_eq!(admin_balance_before, U256::from(INITIAL_SUPPLY));

    let transfer_receipt = b20
        .send_call_receipt(
            token,
            IB20::transferCall { to: recipient, amount: U256::from(TRANSFER_AMOUNT) },
            "transfer B-20 token",
        )
        .await?;
    assert_transfer_log(&transfer_receipt, token, admin.address(), recipient, TRANSFER_AMOUNT);

    let admin_balance_after = b20.balance_of(token, admin.address()).await?;
    let recipient_balance = b20.balance_of(token, recipient).await?;

    assert_eq!(recipient_balance, U256::from(TRANSFER_AMOUNT));
    assert_eq!(admin_balance_before - admin_balance_after, U256::from(TRANSFER_AMOUNT));

    Ok(())
}

#[tokio::test]
async fn test_b20_token_metadata() -> Result<()> {
    let (_system, provider) = common::start_beryl_system().await?;
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

    let token = b20.create_token(B20Variant::Asset, params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    assert_eq!(b20.name(token).await?, "Metadata Token");
    assert_eq!(b20.symbol(token).await?, "META");
    assert_eq!(b20.total_supply(token).await?, U256::from(INITIAL_SUPPLY));

    Ok(())
}

#[tokio::test]
async fn test_b20_approve_and_transfer_from() -> Result<()> {
    let (_system, provider) = common::start_beryl_system().await?;
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
    let token = b20_admin.create_token(B20Variant::Asset, params, salt).await?;
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
    let (_system, provider) = common::start_beryl_system().await?;
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
    let token = b20.create_token(B20Variant::Asset, params, salt).await?;
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
    let (_system, provider) = common::start_beryl_system().await?;
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
    let token = b20.create_token(B20Variant::Asset, params, salt).await?;
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
    let (_system, provider) = common::start_beryl_system().await?;
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

    let token = b20.create_token(B20Variant::Asset, params, salt).await?;
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
    let (_system, provider) = common::start_beryl_system().await?;
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
    let token = b20.create_token(B20Variant::Asset, params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    b20.send_call(
        token,
        IB20::grantRoleCall { role: B20TokenRole::Metadata.id(), account: admin.address() },
        "grant B-20 metadata role",
    )
    .await?;

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
    let (_system, provider) = common::start_beryl_system().await?;
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
    let token = b20.create_token(B20Variant::Asset, params, salt).await?;
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
    let (_system, provider) = common::start_beryl_system().await?;
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

    let local_prediction = b20.predict_token_address(B20Variant::Asset, salt);
    let rpc_prediction =
        b20.predict_token_address_rpc(admin.address(), B20Variant::Asset, salt).await?;
    assert_eq!(local_prediction, rpc_prediction, "local and RPC predictions should match");

    let token = b20.create_token(B20Variant::Asset, params, salt).await?;
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
async fn test_b20_stablecoin_variant_create_via_rpc() -> Result<()> {
    let (_system, provider) = common::start_beryl_system().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    common::wait_for_balance(&provider, admin.address()).await?;

    let b20 =
        activated_feature_client(&provider, &admin, [ActivationFeature::B20Stablecoin]).await?;
    let salt = B256::repeat_byte(0x19);
    let name = "System Test USD Stablecoin";
    let symbol = "SUSD";
    let params = B20PrecompileClient::stablecoin_params(
        name,
        symbol,
        admin.address(),
        U256::from(INITIAL_SUPPLY),
        admin.address(),
        STABLECOIN_CURRENCY,
    );

    let local_prediction = b20.predict_token_address(B20Variant::Stablecoin, salt);
    let rpc_prediction =
        b20.predict_token_address_rpc(admin.address(), B20Variant::Stablecoin, salt).await?;
    assert_eq!(local_prediction, rpc_prediction, "stablecoin prediction should match RPC");

    let (token, receipt) =
        b20.create_token_with_receipt(B20Variant::Stablecoin, params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    assert_eq!(token, rpc_prediction, "created stablecoin address should match prediction");
    assert_b20_created_log(
        &receipt,
        token,
        B20Variant::Stablecoin,
        name,
        symbol,
        TOKEN_DECIMALS,
        IB20Factory::B20StablecoinEventParams {
            version: 1,
            currency: STABLECOIN_CURRENCY.to_string(),
        }
        .abi_encode()
        .into(),
    );
    assert_transfer_log(&receipt, token, Address::ZERO, admin.address(), INITIAL_SUPPLY);
    assert!(b20.is_b20(token).await?, "created stablecoin should be recognised as B-20");
    assert!(b20.is_b20_initialized(token).await?, "stablecoin should be initialized");
    assert_eq!(b20.variant_of(token).await?, B20Variant::Stablecoin);
    assert_eq!(b20.decimals_of(token).await?, TOKEN_DECIMALS);
    assert_eq!(b20.currency(token).await?, STABLECOIN_CURRENCY);
    assert_eq!(b20.name(token).await?, name);
    assert_eq!(b20.symbol(token).await?, symbol);
    assert_eq!(b20.total_supply(token).await?, U256::from(INITIAL_SUPPLY));
    assert_eq!(b20.balance_of(token, admin.address()).await?, U256::from(INITIAL_SUPPLY));

    Ok(())
}

#[tokio::test]
async fn test_beryl_precompiles_do_not_execute_before_activation_block() -> Result<()> {
    let (_system, provider) = start_beryl_system_before_activation().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    common::wait_for_balance(&provider, admin.address()).await?;
    let b20 = B20PrecompileClient::new(&provider, &admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    let salt = B256::repeat_byte(0x1a);
    let params = B20PrecompileClient::token_params(
        "Pre-Beryl Token",
        "PRE",
        admin.address(),
        U256::ZERO,
        admin.address(),
    );
    let token = b20.predict_token_address(B20Variant::Asset, salt);

    let receipt = b20
        .send_call_unchecked_receipt(
            B20FactoryStorage::ADDRESS,
            IB20Factory::createB20Call {
                variant: IB20Factory::B20Variant::ASSET,
                salt,
                params: params.encoded_params.clone(),
                initCalls: Vec::new(),
            },
            "pre-Beryl createB20",
        )
        .await?;
    assert!(receipt.inner.logs().is_empty(), "pre-Beryl createB20 must not emit logs");
    assert!(
        provider.get_code_at(token).await?.is_empty(),
        "pre-Beryl createB20 must not deploy code"
    );

    common::wait_for_block(&provider, PRE_BERYL_TEST_ACTIVATION_BLOCK + 1).await?;
    b20.activate_feature(ActivationFeature::B20Asset.id()).await?;

    let token_after_beryl = b20.create_token(B20Variant::Asset, params, salt).await?;
    assert_eq!(token_after_beryl, token, "post-Beryl creation should use the same address");
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    Ok(())
}

#[tokio::test]
async fn test_b20_create_token_duplicate_reverts() -> Result<()> {
    let (_system, provider) = common::start_beryl_system().await?;
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

    let token = b20.create_token(B20Variant::Asset, params.clone(), salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;

    let succeeded = b20
        .try_send_call(
            B20FactoryStorage::ADDRESS,
            IB20Factory::createB20Call {
                variant: IB20Factory::B20Variant::ASSET,
                salt,
                params: params.encoded_params,
                initCalls: Vec::new(),
            },
            "createB20 (duplicate salt)",
        )
        .await?;
    assert!(!succeeded, "creating a token with the same salt should revert on-chain");

    Ok(())
}

fn assert_b20_created_log(
    receipt: &BaseTransactionReceipt,
    token: Address,
    variant: B20Variant,
    name: &str,
    symbol: &str,
    decimals: u8,
    variant_params: Bytes,
) {
    assert_receipt_log(
        receipt,
        B20FactoryStorage::ADDRESS,
        IB20Factory::B20Created {
            token,
            variant: variant.abi(),
            name: name.to_string(),
            symbol: symbol.to_string(),
            decimals,
            variantParams: variant_params,
        }
        .encode_log_data(),
    );
}

fn assert_transfer_log(
    receipt: &BaseTransactionReceipt,
    token: Address,
    from: Address,
    to: Address,
    amount: u64,
) {
    assert_receipt_log(
        receipt,
        token,
        IB20::Transfer { from, to, amount: U256::from(amount) }.encode_log_data(),
    );
}

fn assert_receipt_log(receipt: &BaseTransactionReceipt, address: Address, expected: LogData) {
    assert!(
        receipt.inner.logs().iter().any(|log| log.address() == address && log.data() == &expected),
        "receipt must contain expected log at {address}; expected={expected:?}, logs={:?}",
        receipt.inner.logs()
    );
}
