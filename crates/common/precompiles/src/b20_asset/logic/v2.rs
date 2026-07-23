//! Version 2 scaffolding for the asset B-20 precompile, activated at Cobalt.
//!
//! This version initially delegates existing behavior to the frozen [`AssetV1`] via
//! [`delegate_asset!`]. ERC-8056 behavior is introduced separately by overriding the
//! interface defaults; those overrides are dropped from the delegation list and written by
//! hand as the surface diverges from V1.

use crate::AssetV1;

/// Second B-20 Asset precompile implementation, introduced at Cobalt.
#[derive(Debug, Default, Clone, Copy)]
pub struct AssetV2;

delegate_asset!(AssetV2 => AssetV1, {
    transfer,
    transfer_from,
    approve,
    emit_memo,
    mint,
    burn,
    burn_blocked,
    pause,
    unpause,
    update_supply_cap,
    update_name,
    update_symbol,
    update_contract_uri,
    grant_role,
    revoke_role,
    renounce_role,
    renounce_last_admin,
    set_role_admin,
    update_policy,
    permit,
    update_multiplier,
    update_extra_metadata,
    batch_mint,
    begin_announce,
    end_announce,
    is_paused,
    paused_features,
    policy_id,
    domain_separator,
    eip712_domain,
    to_scaled_balance,
    to_raw_balance,
    scaled_balance_of,
    operator_role,
});
