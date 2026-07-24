//! Version 2 scaffolding for the asset B-20 precompile, activated at Cobalt.
//!
//! This version delegates all existing behavior to the frozen [`AssetV1`] via [`delegate_asset!`].
//! ERC-8056 behavior is introduced by a follow-up PR, which drops the diverging methods from the
//! delegation list and supplies their real implementations through the macro's override section.
//!
//! In particular, `update_multiplier` is delegated to V1 here; a follow-up PR overrides it to
//! clear any pending schedule and emit the ERC-8056 events (`MultiplierUpdateCancelled` /
//! `UIMultiplierUpdated`). Delegating to V1 is behavior-correct on this scaffolding because no
//! pending schedule can exist yet: `set_ui_multiplier` is a frozen default until that PR, so
//! nothing can populate the pending slot for `update_multiplier` to clear.

use crate::{AssetV1, macros::delegate_asset};

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
