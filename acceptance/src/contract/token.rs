//! B-20 token acceptance workloads migrated from the RPC system tests.

use std::time::Duration;

use alloy_primitives::{Address, B256, Bytes, LogData, U256, keccak256};
use alloy_provider::Provider;
use alloy_sol_types::{SolCall, SolEvent, SolValue};
use base_common_precompiles::{
    ActivationFeature, B20FactoryStorage, B20TokenRole, B20Variant, IB20, IB20Asset, IB20Factory,
};
use base_common_rpc_types::BaseTransactionReceipt;
use eyre::{Result, WrapErr, bail, ensure};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep};

use crate::{B20PrecompileClient, ContractCase, WorkloadContext};

const TOKEN_DECIMALS: u8 = 6;
const INITIAL_SUPPLY: u64 = 1_000_000_000;
const TRANSFER_AMOUNT: u64 = 100_000_000;
const MINT_AMOUNT: u64 = 500_000;
const BURN_AMOUNT: u64 = 200_000;
const APPROVE_AMOUNT: u64 = 50_000_000;
const SPENDER_TRANSFER_AMOUNT: u64 = 30_000_000;
const INITIAL_SUPPLY_CAP: u64 = 2_000_000_000;
const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
const UPDATED_MULTIPLIER: U256 = U256::from_limbs([2_000_000_000_000_000_000, 0, 0, 0]);

/// Executes the fourteen B-20 token behaviors from `b20_precompile.rs`.
#[derive(Debug)]
pub struct TokenWorkload;

impl TokenWorkload {
    /// Runs one token case against the managed builder RPC.
    pub async fn execute(case: ContractCase, context: &WorkloadContext<'_>) -> Result<Value> {
        match case {
            ContractCase::B20FactoryCreateAndTransferViaRpc => Self::create_transfer(context).await,
            ContractCase::B20TokenMetadata => Self::metadata(context).await,
            ContractCase::B20ApproveAndTransferFrom => Self::allowance(context).await,
            ContractCase::B20MintAndBurn => Self::mint_burn(context).await,
            ContractCase::B20StablecoinCreateAndCurrencyViaRpc => {
                Self::stablecoin_currency(context).await
            }
            ContractCase::B20AssetExtensionViaRpc => Self::asset_extension(context).await,
            ContractCase::B20TransferWithMemo => Self::memo(context).await,
            ContractCase::B20SupplyCap => Self::supply_cap(context).await,
            ContractCase::B20MetadataUpdates => Self::metadata_updates(context).await,
            ContractCase::B20PauseAndUnpause => Self::pause(context).await,
            ContractCase::B20FactoryPredictAndIsB20 => Self::prediction(context).await,
            ContractCase::B20StablecoinVariantCreateViaRpc => {
                Self::stablecoin_variant(context).await
            }
            ContractCase::BerylPrecompilesDoNotExecuteBeforeActivationBlock => {
                Self::pre_beryl(context).await
            }
            ContractCase::B20CreateTokenDuplicateReverts => Self::duplicate(context).await,
            _ => bail!("contract case is not a B-20 token workload"),
        }
    }

    /// Activates a feature using the canonical administrator.
    pub async fn setup(
        context: &WorkloadContext<'_>,
        feature: ActivationFeature,
    ) -> Result<(
        alloy_provider::RootProvider<base_common_network::Base>,
        alloy_signer_local::PrivateKeySigner,
    )> {
        let provider = context.provider("builder")?;
        let admin = WorkloadContext::signer(5)?;
        let client = Self::client(context, &provider, &admin);
        client.activate_feature(feature.id()).await?;
        Ok((provider, admin))
    }

    /// Creates a client bounded by the remaining workload deadline.
    pub fn client<'a>(
        context: &WorkloadContext<'_>,
        provider: &'a alloy_provider::RootProvider<base_common_network::Base>,
        signer: &'a alloy_signer_local::PrivateKeySigner,
    ) -> B20PrecompileClient<'a> {
        B20PrecompileClient::new(provider, signer, context.config.devnet.l2.chain_id)
            .with_receipt_timeout(context.deadline.saturating_duration_since(Instant::now()))
    }

    /// Creates a funded asset token for a workload.
    pub async fn token(
        context: &WorkloadContext<'_>,
        feature: ActivationFeature,
        salt: u8,
        name: &str,
        symbol: &str,
    ) -> Result<(
        alloy_provider::RootProvider<base_common_network::Base>,
        alloy_signer_local::PrivateKeySigner,
        Address,
    )> {
        let (provider, admin) = Self::setup(context, feature).await?;
        let client = Self::client(context, &provider, &admin);
        let params = B20PrecompileClient::token_params(
            name,
            symbol,
            admin.address(),
            U256::from(INITIAL_SUPPLY),
            admin.address(),
        );
        let token = client.create_token(B20Variant::Asset, params, B256::repeat_byte(salt)).await?;
        client
            .wait_for_token_code(token, Duration::from_secs(60), Duration::from_millis(250))
            .await?;
        Ok((provider, admin, token))
    }

    /// Verifies creation events, metadata, and transfer balances.
    pub async fn create_transfer(context: &WorkloadContext<'_>) -> Result<Value> {
        let (provider, admin) = Self::setup(context, ActivationFeature::B20Asset).await?;
        let client = Self::client(context, &provider, &admin);
        let recipient = WorkloadContext::signer(6)?.address();
        let params = B20PrecompileClient::token_params(
            "System Test B20",
            "DB20",
            admin.address(),
            U256::from(INITIAL_SUPPLY),
            admin.address(),
        );
        let (token, receipt) = client
            .create_token_with_receipt(B20Variant::Asset, params, B256::repeat_byte(0x42))
            .await?;
        client
            .wait_for_token_code(token, Duration::from_secs(60), Duration::from_millis(250))
            .await?;
        Self::expected_log(
            &receipt,
            B20FactoryStorage::ADDRESS,
            IB20Factory::B20Created {
                token,
                variant: B20Variant::Asset.abi(),
                name: "System Test B20".into(),
                symbol: "DB20".into(),
                decimals: TOKEN_DECIMALS,
                variantParams: Bytes::new(),
            }
            .encode_log_data(),
        )?;
        Self::transfer_log(&receipt, token, Address::ZERO, admin.address(), INITIAL_SUPPLY)?;
        ensure!(client.variant_of(token).await? == B20Variant::Asset, "asset variant mismatch");
        ensure!(client.decimals_of(token).await? == TOKEN_DECIMALS, "decimals mismatch");
        let before = client.balance_of(token, admin.address()).await?;
        ensure!(before == U256::from(INITIAL_SUPPLY), "initial balance mismatch");
        let transfer = client
            .send_call_receipt(
                token,
                IB20::transferCall { to: recipient, amount: U256::from(TRANSFER_AMOUNT) },
                "transfer B-20 token",
            )
            .await?;
        Self::transfer_log(&transfer, token, admin.address(), recipient, TRANSFER_AMOUNT)?;
        let after = client.balance_of(token, admin.address()).await?;
        let received = client.balance_of(token, recipient).await?;
        ensure!(received == U256::from(TRANSFER_AMOUNT), "recipient balance mismatch");
        ensure!(before - after == U256::from(TRANSFER_AMOUNT), "sender debit mismatch");
        Ok(json!({"token": token, "sender_balance": after, "recipient_balance": received}))
    }

    /// Verifies immutable token metadata and initial supply.
    pub async fn metadata(context: &WorkloadContext<'_>) -> Result<Value> {
        let (provider, admin, token) =
            Self::token(context, ActivationFeature::B20Asset, 0x10, "Metadata Token", "META")
                .await?;
        let client = Self::client(context, &provider, &admin);
        let name = client.name(token).await?;
        let symbol = client.symbol(token).await?;
        let supply = client.total_supply(token).await?;
        ensure!(name == "Metadata Token" && symbol == "META", "token metadata mismatch");
        ensure!(supply == U256::from(INITIAL_SUPPLY), "total supply mismatch");
        Ok(json!({"token": token, "name": name, "symbol": symbol, "total_supply": supply}))
    }

    /// Verifies delegated transfer balances and allowance consumption.
    pub async fn allowance(context: &WorkloadContext<'_>) -> Result<Value> {
        let (provider, admin, token) =
            Self::token(context, ActivationFeature::B20Asset, 0x11, "Allowance Token", "ALLW")
                .await?;
        let owner = Self::client(context, &provider, &admin);
        let spender = WorkloadContext::signer(7)?;
        let spender_client = Self::client(context, &provider, &spender);
        let recipient = WorkloadContext::signer(6)?.address();
        let approved = U256::from(APPROVE_AMOUNT);
        let transferred = U256::from(SPENDER_TRANSFER_AMOUNT);
        owner.approve(token, spender.address(), approved).await?;
        ensure!(
            owner.allowance(token, admin.address(), spender.address()).await? == approved,
            "approval mismatch"
        );
        spender_client.transfer_from(token, admin.address(), recipient, transferred).await?;
        ensure!(
            owner.balance_of(token, admin.address()).await?
                == U256::from(INITIAL_SUPPLY) - transferred,
            "owner balance mismatch"
        );
        ensure!(
            owner.balance_of(token, recipient).await? == transferred,
            "recipient balance mismatch"
        );
        let remaining = owner.allowance(token, admin.address(), spender.address()).await?;
        ensure!(remaining == approved - transferred, "allowance was not decreased");
        Ok(json!({"token": token, "transferred": transferred, "remaining_allowance": remaining}))
    }

    /// Verifies mint and burn supply and balance changes.
    pub async fn mint_burn(context: &WorkloadContext<'_>) -> Result<Value> {
        let (provider, admin, token) =
            Self::token(context, ActivationFeature::B20Asset, 0x12, "Mintable Token", "MINT")
                .await?;
        let client = Self::client(context, &provider, &admin);
        let before = client.total_supply(token).await?;
        for role in [B20TokenRole::Mint, B20TokenRole::Burn] {
            client
                .send_call(
                    token,
                    IB20::grantRoleCall { role: role.id(), account: admin.address() },
                    "grant B-20 role",
                )
                .await?;
        }
        client.mint(token, admin.address(), U256::ZERO).await?;
        client.burn(token, U256::ZERO).await?;
        ensure!(client.total_supply(token).await? == before, "zero mint/burn changed supply");
        client.mint(token, admin.address(), U256::from(MINT_AMOUNT)).await?;
        ensure!(
            client.total_supply(token).await? == before + U256::from(MINT_AMOUNT),
            "mint supply mismatch"
        );
        ensure!(
            client.balance_of(token, admin.address()).await?
                == U256::from(INITIAL_SUPPLY + MINT_AMOUNT),
            "mint balance mismatch"
        );
        client.burn(token, U256::from(BURN_AMOUNT)).await?;
        let expected = before + U256::from(MINT_AMOUNT) - U256::from(BURN_AMOUNT);
        ensure!(client.total_supply(token).await? == expected, "burn supply mismatch");
        ensure!(
            client.balance_of(token, admin.address()).await?
                == U256::from(INITIAL_SUPPLY + MINT_AMOUNT - BURN_AMOUNT),
            "burn balance mismatch"
        );
        Ok(
            json!({"token": token, "total_supply": expected, "minted": MINT_AMOUNT, "burned": BURN_AMOUNT}),
        )
    }

    /// Verifies stablecoin currency validation.
    pub async fn stablecoin_currency(context: &WorkloadContext<'_>) -> Result<Value> {
        let (provider, admin) = Self::setup(context, ActivationFeature::B20Stablecoin).await?;
        let client = Self::client(context, &provider, &admin);
        let salt = B256::repeat_byte(0x19);
        let token = client.predict_token_address(B20Variant::Stablecoin, salt);
        let params = IB20Factory::B20StablecoinCreateParams {
            version: B20Variant::Stablecoin.supported_version(),
            name: "System USD".into(),
            symbol: "SUSD".into(),
            initialAdmin: admin.address(),
            currency: "USD".into(),
        };
        client
            .send_call(
                B20FactoryStorage::ADDRESS,
                IB20Factory::createB20Call {
                    variant: IB20Factory::B20Variant::STABLECOIN,
                    salt,
                    params: params.abi_encode().into(),
                    initCalls: vec![
                        IB20::mintCall { to: admin.address(), amount: U256::from(INITIAL_SUPPLY) }
                            .abi_encode()
                            .into(),
                    ],
                },
                "create B-20 stablecoin",
            )
            .await?;
        client
            .wait_for_token_code(token, Duration::from_secs(60), Duration::from_millis(250))
            .await?;
        ensure!(client.currency(token).await? == "USD", "currency mismatch");
        ensure!(client.name(token).await? == "System USD", "stablecoin name mismatch");
        ensure!(
            client.total_supply(token).await? == U256::from(INITIAL_SUPPLY),
            "stablecoin supply mismatch"
        );
        ensure!(
            client.is_b20(token).await? && client.is_b20_initialized(token).await?,
            "stablecoin factory state mismatch"
        );
        let invalid_salt = B256::repeat_byte(0x1a);
        let invalid = client.predict_token_address(B20Variant::Stablecoin, invalid_salt);
        let bad = IB20Factory::B20StablecoinCreateParams {
            version: B20Variant::Stablecoin.supported_version(),
            name: "Invalid USD".into(),
            symbol: "IUSD".into(),
            initialAdmin: admin.address(),
            currency: "usd".into(),
        };
        let succeeded = client
            .try_send_call(
                B20FactoryStorage::ADDRESS,
                IB20Factory::createB20Call {
                    variant: IB20Factory::B20Variant::STABLECOIN,
                    salt: invalid_salt,
                    params: bad.abi_encode().into(),
                    initCalls: vec![],
                },
                "create invalid stablecoin",
            )
            .await?;
        ensure!(!succeeded, "lowercase currency did not revert");
        ensure!(
            provider.get_code_at(invalid).await?.is_empty(),
            "invalid stablecoin deployed code"
        );
        Ok(json!({"token": token, "currency": "USD", "invalid_currency_reverted": true}))
    }

    /// Verifies asset metadata, multiplier, and announcement behavior.
    pub async fn asset_extension(context: &WorkloadContext<'_>) -> Result<Value> {
        let (provider, admin, token) =
            Self::token(context, ActivationFeature::B20Asset, 0x1b, "System Asset", "ASST").await?;
        let client = Self::client(context, &provider, &admin);
        let bob = WorkloadContext::signer(6)?.address();
        let carol = WorkloadContext::signer(7)?.address();
        ensure!(
            Self::asset_word(&client, token, IB20Asset::multiplierCall {}).await? == WAD,
            "initial multiplier mismatch"
        );
        ensure!(
            Self::asset_word(
                &client,
                token,
                IB20Asset::toRawBalanceCall { scaledBalance: U256::from(100) }
            )
            .await?
                == U256::from(100),
            "initial conversion mismatch"
        );
        for role in
            [keccak256("OPERATOR_ROLE"), B20TokenRole::Metadata.id(), B20TokenRole::Mint.id()]
        {
            client
                .send_call(
                    token,
                    IB20::grantRoleCall { role, account: admin.address() },
                    "grant asset role",
                )
                .await?;
        }
        client
            .send_call(
                token,
                IB20Asset::updateMultiplierCall { newMultiplier: UPDATED_MULTIPLIER },
                "update multiplier",
            )
            .await?;
        ensure!(
            Self::asset_word(
                &client,
                token,
                IB20Asset::toScaledBalanceCall { rawBalance: U256::from(100) }
            )
            .await?
                == U256::from(200),
            "scaled conversion mismatch"
        );
        ensure!(
            Self::asset_word(
                &client,
                token,
                IB20Asset::toRawBalanceCall { scaledBalance: U256::from(200) }
            )
            .await?
                == U256::from(100),
            "raw conversion mismatch"
        );
        client
            .send_call(
                token,
                IB20Asset::updateExtraMetadataCall {
                    key: "CUSIP".into(),
                    value: "123456789".into(),
                },
                "update asset metadata",
            )
            .await?;
        ensure!(
            Self::asset_string(&client, token, "CUSIP").await? == "123456789",
            "CUSIP mismatch"
        );
        client
            .send_call(
                token,
                IB20Asset::batchMintCall {
                    recipients: vec![bob, carol],
                    amounts: vec![U256::from(100), U256::from(200)],
                },
                "batch mint",
            )
            .await?;
        ensure!(
            client.balance_of(token, bob).await? == U256::from(100),
            "bob batch balance mismatch"
        );
        ensure!(
            client.balance_of(token, carol).await? == U256::from(200),
            "carol batch balance mismatch"
        );
        let update =
            IB20Asset::updateExtraMetadataCall { key: "FIGI".into(), value: "BBG000000001".into() };
        client
            .send_call(
                token,
                IB20Asset::announceCall {
                    internalCalls: vec![update.abi_encode().into()],
                    id: "asset-rpc-1".into(),
                    description: "update FIGI".into(),
                    uri: "ipfs://asset-rpc".into(),
                },
                "announce asset update",
            )
            .await?;
        ensure!(
            Self::asset_string(&client, token, "FIGI").await? == "BBG000000001",
            "FIGI mismatch"
        );
        ensure!(
            Self::asset_bool(
                &client,
                token,
                IB20Asset::isAnnouncementIdUsedCall { id: "asset-rpc-1".into() }
            )
            .await?,
            "announcement id not marked used"
        );
        Ok(
            json!({"token": token, "multiplier": UPDATED_MULTIPLIER, "cusip": "123456789", "figi": "BBG000000001", "bob_balance": 100, "carol_balance": 200}),
        )
    }

    /// Verifies balance changes from a memo transfer.
    pub async fn memo(context: &WorkloadContext<'_>) -> Result<Value> {
        let (provider, admin, token) =
            Self::token(context, ActivationFeature::B20Asset, 0x13, "Memo Token", "MEMO").await?;
        let client = Self::client(context, &provider, &admin);
        let recipient = WorkloadContext::signer(6)?.address();
        let amount = U256::from(111_000);
        client.transfer_with_memo(token, recipient, amount, B256::repeat_byte(0xde)).await?;
        ensure!(
            client.balance_of(token, recipient).await? == amount,
            "memo recipient balance mismatch"
        );
        ensure!(
            client.balance_of(token, admin.address()).await? == U256::from(INITIAL_SUPPLY) - amount,
            "memo sender balance mismatch"
        );
        Ok(json!({"token": token, "memo": B256::repeat_byte(0xde), "amount": amount}))
    }

    /// Verifies supply-cap enforcement and rejected minting.
    pub async fn supply_cap(context: &WorkloadContext<'_>) -> Result<Value> {
        let (provider, admin) = Self::setup(context, ActivationFeature::B20Asset).await?;
        let client = Self::client(context, &provider, &admin);
        let mut params = B20PrecompileClient::token_params(
            "Capped Token",
            "CAP",
            admin.address(),
            U256::from(INITIAL_SUPPLY),
            admin.address(),
        );
        params.supply_cap = U256::from(INITIAL_SUPPLY_CAP);
        let token = client.create_token(B20Variant::Asset, params, B256::repeat_byte(0x14)).await?;
        client
            .wait_for_token_code(token, Duration::from_secs(60), Duration::from_millis(250))
            .await?;
        ensure!(
            client.supply_cap(token).await? == U256::from(INITIAL_SUPPLY_CAP),
            "initial cap mismatch"
        );
        ensure!(
            !client
                .try_send_call(
                    token,
                    IB20::updateSupplyCapCall { newSupplyCap: U256::from(INITIAL_SUPPLY - 1) },
                    "cap below supply"
                )
                .await?,
            "cap below supply succeeded"
        );
        client.update_supply_cap(token, U256::from(INITIAL_SUPPLY)).await?;
        ensure!(
            client.supply_cap(token).await? == U256::from(INITIAL_SUPPLY),
            "tightened cap mismatch"
        );
        ensure!(
            !client
                .try_send_call(
                    token,
                    IB20::mintCall { to: admin.address(), amount: U256::from(1) },
                    "mint past cap"
                )
                .await?,
            "mint past cap succeeded"
        );
        Ok(json!({"token": token, "supply_cap": INITIAL_SUPPLY, "reverts_verified": 2}))
    }

    /// Verifies authorized metadata updates.
    pub async fn metadata_updates(context: &WorkloadContext<'_>) -> Result<Value> {
        let (provider, admin, token) =
            Self::token(context, ActivationFeature::B20Asset, 0x15, "Old Name", "OLD").await?;
        let client = Self::client(context, &provider, &admin);
        client
            .send_call(
                token,
                IB20::grantRoleCall { role: B20TokenRole::Metadata.id(), account: admin.address() },
                "grant metadata role",
            )
            .await?;
        client.update_name(token, "New Name").await?;
        client.update_symbol(token, "NEW").await?;
        client.update_contract_uri(token, "ipfs://QmTest").await?;
        ensure!(client.name(token).await? == "New Name", "updated name mismatch");
        ensure!(client.symbol(token).await? == "NEW", "updated symbol mismatch");
        ensure!(client.contract_uri(token).await? == "ipfs://QmTest", "updated URI mismatch");
        Ok(
            json!({"token": token, "name": "New Name", "symbol": "NEW", "contract_uri": "ipfs://QmTest"}),
        )
    }

    /// Verifies rejection while paused and resumed transfers after unpause.
    pub async fn pause(context: &WorkloadContext<'_>) -> Result<Value> {
        let (provider, admin, token) =
            Self::token(context, ActivationFeature::B20Asset, 0x16, "Pausable Token", "PAUS")
                .await?;
        let client = Self::client(context, &provider, &admin);
        let recipient = WorkloadContext::signer(6)?.address();
        let amount = U256::from(10_000);
        client.transfer(token, recipient, amount).await?;
        ensure!(
            client.balance_of(token, recipient).await? == amount,
            "pre-pause transfer mismatch"
        );
        for role in [B20TokenRole::Pause, B20TokenRole::Unpause] {
            client
                .send_call(
                    token,
                    IB20::grantRoleCall { role: role.id(), account: admin.address() },
                    "grant pause role",
                )
                .await?;
        }
        client.pause(token, U256::from(1)).await?;
        ensure!(client.paused(token).await? != U256::ZERO, "token not paused");
        ensure!(
            !client
                .try_send_call(
                    token,
                    IB20::transferCall { to: recipient, amount },
                    "transfer while paused"
                )
                .await?,
            "paused transfer succeeded"
        );
        ensure!(
            client.balance_of(token, recipient).await? == amount,
            "reverted transfer changed balance"
        );
        client.unpause(token).await?;
        ensure!(client.paused(token).await? == U256::ZERO, "token still paused");
        client.transfer(token, recipient, amount).await?;
        ensure!(
            client.balance_of(token, recipient).await? == amount * U256::from(2),
            "post-unpause transfer mismatch"
        );
        Ok(
            json!({"token": token, "recipient_balance": amount * U256::from(2), "paused_transfer_reverted": true}),
        )
    }

    /// Verifies local and RPC address predictions and token recognition.
    pub async fn prediction(context: &WorkloadContext<'_>) -> Result<Value> {
        let (provider, admin) = Self::setup(context, ActivationFeature::B20Asset).await?;
        let client = Self::client(context, &provider, &admin);
        let salt = B256::repeat_byte(0x17);
        let local = client.predict_token_address(B20Variant::Asset, salt);
        let rpc =
            client.predict_token_address_rpc(admin.address(), B20Variant::Asset, salt).await?;
        ensure!(local == rpc, "local/RPC prediction mismatch");
        let params = B20PrecompileClient::token_params(
            "Predict Token",
            "PRD",
            admin.address(),
            U256::from(INITIAL_SUPPLY),
            admin.address(),
        );
        let token = client.create_token(B20Variant::Asset, params, salt).await?;
        client
            .wait_for_token_code(token, Duration::from_secs(60), Duration::from_millis(250))
            .await?;
        ensure!(token == rpc, "created address mismatch");
        ensure!(client.is_b20(token).await?, "created token not recognized");
        ensure!(!client.is_b20(B20FactoryStorage::ADDRESS).await?, "factory recognized as token");
        ensure!(!client.is_b20(Address::repeat_byte(0xab)).await?, "arbitrary address recognized");
        Ok(json!({"token": token, "local_prediction": local, "rpc_prediction": rpc}))
    }

    /// Verifies stablecoin variant metadata, events, and balances.
    pub async fn stablecoin_variant(context: &WorkloadContext<'_>) -> Result<Value> {
        let (provider, admin) = Self::setup(context, ActivationFeature::B20Stablecoin).await?;
        let client = Self::client(context, &provider, &admin);
        let salt = B256::repeat_byte(0x19);
        let name = "System Test USD Stablecoin";
        let params = B20PrecompileClient::stablecoin_params(
            name,
            "SUSD",
            admin.address(),
            U256::from(INITIAL_SUPPLY),
            admin.address(),
            "USD",
        );
        let local = client.predict_token_address(B20Variant::Stablecoin, salt);
        let rpc =
            client.predict_token_address_rpc(admin.address(), B20Variant::Stablecoin, salt).await?;
        ensure!(local == rpc, "stablecoin prediction mismatch");
        let (token, receipt) =
            client.create_token_with_receipt(B20Variant::Stablecoin, params, salt).await?;
        client
            .wait_for_token_code(token, Duration::from_secs(60), Duration::from_millis(250))
            .await?;
        ensure!(token == rpc, "stablecoin created address mismatch");
        Self::expected_log(
            &receipt,
            B20FactoryStorage::ADDRESS,
            IB20Factory::B20Created {
                token,
                variant: B20Variant::Stablecoin.abi(),
                name: name.into(),
                symbol: "SUSD".into(),
                decimals: TOKEN_DECIMALS,
                variantParams: IB20Factory::B20StablecoinEventParams {
                    version: 1,
                    currency: "USD".into(),
                }
                .abi_encode()
                .into(),
            }
            .encode_log_data(),
        )?;
        Self::transfer_log(&receipt, token, Address::ZERO, admin.address(), INITIAL_SUPPLY)?;
        ensure!(
            client.is_b20(token).await? && client.is_b20_initialized(token).await?,
            "stablecoin factory state mismatch"
        );
        ensure!(
            client.variant_of(token).await? == B20Variant::Stablecoin,
            "stablecoin variant mismatch"
        );
        ensure!(client.decimals_of(token).await? == TOKEN_DECIMALS, "stablecoin decimals mismatch");
        ensure!(client.currency(token).await? == "USD", "stablecoin currency mismatch");
        ensure!(
            client.name(token).await? == name && client.symbol(token).await? == "SUSD",
            "stablecoin metadata mismatch"
        );
        ensure!(
            client.total_supply(token).await? == U256::from(INITIAL_SUPPLY),
            "stablecoin supply mismatch"
        );
        ensure!(
            client.balance_of(token, admin.address()).await? == U256::from(INITIAL_SUPPLY),
            "stablecoin balance mismatch"
        );
        Ok(json!({"token": token, "currency": "USD", "total_supply": INITIAL_SUPPLY}))
    }

    /// Verifies the same factory call before and after Beryl activation.
    pub async fn pre_beryl(context: &WorkloadContext<'_>) -> Result<Value> {
        let provider = context.provider("builder")?;
        let admin = WorkloadContext::signer(5)?;
        let client = Self::client(context, &provider, &admin);
        let activation = context
            .config
            .devnet
            .l2
            .forks
            .get("beryl")
            .and_then(crate::ForkActivation::block)
            .ok_or_else(|| eyre::eyre!("Beryl must have block activation"))?;
        let before = provider.get_block_number().await?;
        ensure!(
            before.saturating_add(5) < activation,
            "insufficient pre-Beryl startup margin: activation={activation}, current={before}"
        );
        let salt = B256::repeat_byte(0x1a);
        let params = B20PrecompileClient::token_params(
            "Pre-Beryl Token",
            "PRE",
            admin.address(),
            U256::ZERO,
            admin.address(),
        );
        let token = client.predict_token_address(B20Variant::Asset, salt);
        let receipt = client
            .send_call_unchecked_receipt(
                B20FactoryStorage::ADDRESS,
                IB20Factory::createB20Call {
                    variant: IB20Factory::B20Variant::ASSET,
                    salt,
                    params: params.encoded_params.clone(),
                    initCalls: vec![],
                },
                "pre-Beryl createB20",
            )
            .await?;
        ensure!(receipt.inner.logs().is_empty(), "pre-Beryl create emitted logs");
        ensure!(provider.get_code_at(token).await?.is_empty(), "pre-Beryl create deployed code");
        Self::wait_for_block(&provider, activation + 1, context.deadline).await?;
        client.activate_feature(ActivationFeature::B20Asset.id()).await?;
        let after = client.create_token(B20Variant::Asset, params, salt).await?;
        ensure!(after == token, "post-Beryl address changed");
        client
            .wait_for_token_code(
                token,
                context.deadline.saturating_duration_since(Instant::now()),
                Duration::from_millis(250),
            )
            .await?;
        Ok(
            json!({"token": token, "pre_block": before, "post_block": provider.get_block_number().await?, "pre_logs": 0, "post_code": true}),
        )
    }

    /// Verifies duplicate token creation reverts.
    pub async fn duplicate(context: &WorkloadContext<'_>) -> Result<Value> {
        let (provider, admin) = Self::setup(context, ActivationFeature::B20Asset).await?;
        let client = Self::client(context, &provider, &admin);
        let salt = B256::repeat_byte(0x18);
        let params = B20PrecompileClient::token_params(
            "Dup Token",
            "DUP",
            admin.address(),
            U256::from(INITIAL_SUPPLY),
            admin.address(),
        );
        let token = client.create_token(B20Variant::Asset, params.clone(), salt).await?;
        client
            .wait_for_token_code(token, Duration::from_secs(60), Duration::from_millis(250))
            .await?;
        let succeeded = client
            .try_send_call(
                B20FactoryStorage::ADDRESS,
                IB20Factory::createB20Call {
                    variant: IB20Factory::B20Variant::ASSET,
                    salt,
                    params: params.encoded_params,
                    initCalls: vec![],
                },
                "duplicate createB20",
            )
            .await?;
        ensure!(!succeeded, "duplicate token creation succeeded");
        Ok(json!({"token": token, "duplicate_reverted": true}))
    }

    /// Requires an exact log and returns concrete observed log evidence.
    pub fn expected_log(
        receipt: &BaseTransactionReceipt,
        address: Address,
        expected: LogData,
    ) -> Result<Value> {
        let observed = receipt
            .inner
            .logs()
            .iter()
            .find(|log| log.address() == address && log.data() == &expected);
        ensure!(
            observed.is_some(),
            "receipt missing expected log at {address}; expected={expected:?}; logs={:?}",
            receipt.inner.logs()
        );
        let observed = observed.expect("log presence checked above");
        Ok(json!({
            "emitter": observed.address(),
            "topics": observed.data().topics(),
            "data": observed.data().data,
        }))
    }

    /// Requires an exact ERC-20 transfer log and returns its observed evidence.
    pub fn transfer_log(
        receipt: &BaseTransactionReceipt,
        token: Address,
        from: Address,
        to: Address,
        amount: u64,
    ) -> Result<Value> {
        Self::expected_log(
            receipt,
            token,
            IB20::Transfer { from, to, amount: U256::from(amount) }.encode_log_data(),
        )
    }

    /// Executes and decodes an asset call returning a word.
    pub async fn asset_word<C: SolCall>(
        client: &B20PrecompileClient<'_>,
        token: Address,
        call: C,
    ) -> Result<U256> {
        U256::abi_decode(client.call(token, call).await?.as_ref()).wrap_err("decode asset word")
    }

    /// Reads and decodes one asset extra-metadata string.
    pub async fn asset_string(
        client: &B20PrecompileClient<'_>,
        token: Address,
        key: &str,
    ) -> Result<String> {
        String::abi_decode(
            client.call(token, IB20Asset::extraMetadataCall { key: key.into() }).await?.as_ref(),
        )
        .wrap_err("decode asset string")
    }

    /// Executes and decodes an asset call returning a boolean.
    pub async fn asset_bool<C: SolCall>(
        client: &B20PrecompileClient<'_>,
        token: Address,
        call: C,
    ) -> Result<bool> {
        bool::abi_decode(client.call(token, call).await?.as_ref()).wrap_err("decode asset bool")
    }

    /// Polls until the chain reaches `target` or the workload deadline expires.
    pub async fn wait_for_block(
        provider: &alloy_provider::RootProvider<base_common_network::Base>,
        target: u64,
        deadline: Instant,
    ) -> Result<()> {
        loop {
            let block = provider.get_block_number().await?;
            if block >= target {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "deadline elapsed waiting for block {target}; current={block}"
            );
            sleep(Duration::from_millis(250)).await;
        }
    }
}
