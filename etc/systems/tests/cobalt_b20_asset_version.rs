//! System tests pinning the B-20 asset version to the active Base upgrade.
//!
//! The B-20 asset precompile resolves its logic version per call from the block's active upgrade
//! rather than storing a version per token: at Beryl an asset routes to `AssetV1`, and at Cobalt
//! the same surface routes to `AssetV2`, which adds the ERC-8056 scheduled-multiplier methods
//! (`uiMultiplier`, `newUIMultiplier`, `effectiveAt`, `balanceOfUI`, `totalSupplyUI`,
//! `setUIMultiplier`, `cancelScheduledMultiplier`, `supportsInterface`).
//!
//! These tests activate the Cobalt upgrade end-to-end over Base node RPC and assert:
//!   * before Cobalt (Beryl): the asset behaves as `AssetV1` and every ERC-8056 selector reverts as
//!     an unknown selector, and
//!   * at Cobalt: the asset behaves as `AssetV2`, advertising the ERC-8056 interface IDs and
//!     executing the new scheduled-multiplier functionality.

mod common;

use alloy_primitives::{Address, B256, FixedBytes, U256, keccak256};
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolValue};
use base_common_network::Base;
use base_common_precompiles::{
    ActivationFeature, B20Variant, ERC8056_INTERFACE_IDS, IB20, IB20Asset,
};
use base_system_tests::{
    ANVIL_ACCOUNT_5, B20PrecompileClient, SystemTestStack, SystemTestStackBuilder,
};
use eyre::{Result, WrapErr};

/// Block at which Cobalt activates for the Cobalt system stack. Chosen after Beryl (block 3) so the
/// pre-Cobalt window still exercises `AssetV1` during warm-up.
const BASE_COBALT_ACTIVATION_BLOCK: u64 = 5;
/// Initial supply minted to the token admin on creation.
const INITIAL_SUPPLY: u64 = 1_000_000_000;
/// One WAD (`1e18`), the fixed-point precision for the asset multiplier.
const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
/// Two WAD (`2e18`), the target of the scheduled multiplier update.
const DOUBLE_WAD: U256 = U256::from_limbs([2_000_000_000_000_000_000, 0, 0, 0]);
/// A deterministic far-future `effectiveAt` (2100-01-01T00:00:00Z). Comfortably in the future for a
/// freshly started devnet yet well within the `u64` field size, so the schedule stays pending for
/// the duration of the test and never matures into the active multiplier.
const SCHEDULED_EFFECTIVE_AT: u64 = 4_102_444_800;

/// Boots a system stack with Cobalt active at [`BASE_COBALT_ACTIVATION_BLOCK`] and waits until the
/// chain has advanced past it, so all subsequent calls resolve against the Cobalt upgrade.
async fn start_cobalt_system() -> Result<(SystemTestStack, RootProvider<Base>)> {
    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(common::L1_CHAIN_ID)
        .with_l2_chain_id(common::L2_CHAIN_ID)
        .with_base_azul_activation_block(common::BASE_AZUL_ACTIVATION_BLOCK)
        .with_base_beryl_activation_block(common::BASE_BERYL_ACTIVATION_BLOCK)
        .with_base_cobalt_activation_block(BASE_COBALT_ACTIVATION_BLOCK)
        .build()
        .await?;
    let provider = system.l2_builder_provider()?;
    common::wait_for_block(&provider, BASE_COBALT_ACTIVATION_BLOCK + 1).await?;
    Ok((system, provider))
}

/// Activates the B-20 asset feature and deploys a fresh asset token whose admin holds
/// [`INITIAL_SUPPLY`], returning the RPC client bound to `provider`/`admin` alongside the token
/// address.
async fn create_asset_token<'a>(
    provider: &'a RootProvider<Base>,
    admin: &'a PrivateKeySigner,
    salt: B256,
    name: &str,
    symbol: &str,
) -> Result<(B20PrecompileClient<'a>, Address)> {
    let b20 = B20PrecompileClient::new(provider, admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(common::TX_RECEIPT_TIMEOUT);
    b20.activate_feature(ActivationFeature::B20Asset.id()).await?;
    let params = B20PrecompileClient::token_params(
        name,
        symbol,
        admin.address(),
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );
    let token = b20.create_token(B20Variant::Asset, params, salt).await?;
    b20.wait_for_token_code(token, common::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL).await?;
    Ok((b20, token))
}

#[tokio::test]
async fn test_b20_asset_is_v1_before_cobalt() -> Result<()> {
    let (_system, provider) = common::start_beryl_system().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    common::wait_for_balance(&provider, admin.address()).await?;

    let (b20, token) =
        create_asset_token(&provider, &admin, B256::repeat_byte(0x51), "Beryl Asset", "BASST")
            .await?;

    assert_eq!(asset_word(&b20, token, IB20Asset::multiplierCall {}).await?, WAD);

    // The ERC-8056 read selectors do not exist on AssetV1 and must revert as unknown selectors.
    assert!(
        b20.call(token, IB20Asset::uiMultiplierCall {}).await.is_err(),
        "uiMultiplier must revert before Cobalt",
    );
    assert!(
        b20.call(token, IB20Asset::newUIMultiplierCall {}).await.is_err(),
        "newUIMultiplier must revert before Cobalt",
    );
    assert!(
        b20.call(token, IB20Asset::effectiveAtCall {}).await.is_err(),
        "effectiveAt must revert before Cobalt",
    );
    assert!(
        b20.call(token, IB20Asset::balanceOfUICall { account: admin.address() }).await.is_err(),
        "balanceOfUI must revert before Cobalt",
    );
    assert!(
        b20.call(token, IB20Asset::totalSupplyUICall {}).await.is_err(),
        "totalSupplyUI must revert before Cobalt",
    );
    assert!(
        b20.call(token, IB20Asset::supportsInterfaceCall { interfaceId: ERC8056_INTERFACE_IDS[0] })
            .await
            .is_err(),
        "supportsInterface must revert before Cobalt",
    );

    // The ERC-8056 write selectors likewise do not exist on AssetV1 and must revert on-chain.
    assert!(
        !b20.try_send_call(
            token,
            IB20Asset::setUIMultiplierCall {
                newMultiplier: DOUBLE_WAD,
                effectiveAt: U256::from(SCHEDULED_EFFECTIVE_AT),
            },
            "setUIMultiplier before Cobalt",
        )
        .await?,
        "setUIMultiplier must revert before Cobalt",
    );
    assert!(
        !b20.try_send_call(
            token,
            IB20Asset::cancelScheduledMultiplierCall {},
            "cancelScheduledMultiplier before Cobalt",
        )
        .await?,
        "cancelScheduledMultiplier must revert before Cobalt",
    );

    Ok(())
}

#[tokio::test]
async fn test_b20_asset_is_v2_at_cobalt() -> Result<()> {
    let (_system, provider) = start_cobalt_system().await?;
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse admin key")?;
    common::wait_for_balance(&provider, admin.address()).await?;

    let (b20, token) =
        create_asset_token(&provider, &admin, B256::repeat_byte(0x52), "Cobalt Asset", "CASST")
            .await?;

    // AssetV2 advertises the ERC-165 + ERC-8056 interface IDs.
    for interface_id in ERC8056_INTERFACE_IDS {
        assert!(
            asset_bool(&b20, token, IB20Asset::supportsInterfaceCall { interfaceId: interface_id })
                .await?,
            "supportsInterface must be true for advertised id {interface_id}",
        );
    }
    // The ERC-8056 Conversion extension is deliberately not advertised, and neither is a random id.
    assert!(
        !asset_bool(
            &b20,
            token,
            IB20Asset::supportsInterfaceCall {
                interfaceId: FixedBytes::new([0x57, 0x85, 0x4f, 0xc3])
            },
        )
        .await?,
        "conversion-extension interface id must not be advertised",
    );
    assert!(
        !asset_bool(
            &b20,
            token,
            IB20Asset::supportsInterfaceCall {
                interfaceId: FixedBytes::new([0xde, 0xad, 0xbe, 0xef])
            },
        )
        .await?,
        "an unrelated interface id must not be advertised",
    );

    assert_eq!(asset_word(&b20, token, IB20Asset::uiMultiplierCall {}).await?, WAD);
    assert_eq!(asset_word(&b20, token, IB20Asset::multiplierCall {}).await?, WAD);
    assert_eq!(asset_word(&b20, token, IB20Asset::newUIMultiplierCall {}).await?, WAD);
    assert_eq!(asset_word(&b20, token, IB20Asset::effectiveAtCall {}).await?, U256::ZERO);
    // The ERC-8056 balance views mirror the raw balances while the multiplier is WAD.
    assert_eq!(
        asset_word(&b20, token, IB20Asset::balanceOfUICall { account: admin.address() }).await?,
        U256::from(INITIAL_SUPPLY),
    );
    assert_eq!(
        asset_word(&b20, token, IB20Asset::totalSupplyUICall {}).await?,
        U256::from(INITIAL_SUPPLY),
    );

    // Grant the operator role so the admin may schedule multiplier updates.
    b20.send_call(
        token,
        IB20::grantRoleCall { role: operator_role(), account: admin.address() },
        "grant B-20 operator role",
    )
    .await?;

    // Schedule a far-future multiplier update: the pending target is observable via newUIMultiplier
    // and effectiveAt, while the effective uiMultiplier stays at WAD until the timestamp is reached.
    let effective_at = U256::from(SCHEDULED_EFFECTIVE_AT);
    b20.send_call(
        token,
        IB20Asset::setUIMultiplierCall { newMultiplier: DOUBLE_WAD, effectiveAt: effective_at },
        "schedule B-20 UI multiplier",
    )
    .await?;
    assert_eq!(asset_word(&b20, token, IB20Asset::newUIMultiplierCall {}).await?, DOUBLE_WAD);
    assert_eq!(asset_word(&b20, token, IB20Asset::effectiveAtCall {}).await?, effective_at);
    assert_eq!(
        asset_word(&b20, token, IB20Asset::uiMultiplierCall {}).await?,
        WAD,
        "a pending update must not take effect before its timestamp",
    );

    // A second overlapping schedule must revert while the first is still live.
    assert!(
        !b20.try_send_call(
            token,
            IB20Asset::setUIMultiplierCall { newMultiplier: DOUBLE_WAD, effectiveAt: effective_at },
            "overlapping B-20 UI multiplier schedule",
        )
        .await?,
        "an overlapping schedule must revert while a pending update is live",
    );

    // Cancelling the live pending update restores the no-pending state.
    b20.send_call(
        token,
        IB20Asset::cancelScheduledMultiplierCall {},
        "cancel scheduled B-20 UI multiplier",
    )
    .await?;
    assert_eq!(asset_word(&b20, token, IB20Asset::effectiveAtCall {}).await?, U256::ZERO);
    assert_eq!(
        asset_word(&b20, token, IB20Asset::newUIMultiplierCall {}).await?,
        WAD,
        "cancelling must clear the pending target",
    );

    Ok(())
}

async fn asset_word<C>(client: &B20PrecompileClient<'_>, token: Address, call: C) -> Result<U256>
where
    C: SolCall,
{
    let output = client.call(token, call).await?;
    U256::abi_decode(output.as_ref()).wrap_err("Failed to decode asset word")
}

async fn asset_bool<C>(client: &B20PrecompileClient<'_>, token: Address, call: C) -> Result<bool>
where
    C: SolCall,
{
    let output = client.call(token, call).await?;
    bool::abi_decode(output.as_ref()).wrap_err("Failed to decode asset bool")
}

fn operator_role() -> B256 {
    keccak256("OPERATOR_ROLE")
}
