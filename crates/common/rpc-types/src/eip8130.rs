//! EIP-8130 nonce and RPC activation helpers.

use alloy_primitives::{Address, B256, U256};

use crate::state::StateOverride;

/// Canonical invalid-params message for EIP-8130 RPC reads before Zenith.
pub const EIP8130_PRE_ZENITH_RPC_ERROR: &str = "EIP-8130 RPC features are not active before the Zenith hard fork; the `nonce_key` parameter is not supported at this block";

/// Reth-free EIP-8130 channel-nonce helpers.
#[derive(Clone, Copy, Debug, Default)]
pub struct Eip8130Nonce;

impl Eip8130Nonce {
    /// Looks up a Nonce Manager storage slot in a state override.
    ///
    /// A full `state` replacement takes precedence and returns zero for a missing slot. A
    /// `state_diff` only returns values explicitly present in the diff.
    pub fn override_for_slot(
        state_overrides: Option<&StateOverride>,
        address: Address,
        slot: B256,
    ) -> Option<U256> {
        let account_override = state_overrides?.get(&address)?;
        if let Some(state) = account_override.state.as_ref() {
            return Some(
                state
                    .get(&slot)
                    .copied()
                    .map(|value| U256::from_be_bytes(value.0))
                    .unwrap_or_default(),
            );
        }
        account_override
            .state_diff
            .as_ref()?
            .get(&slot)
            .copied()
            .map(|value| U256::from_be_bytes(value.0))
    }

    /// Decodes the Solidity-packed `u64` channel nonce from an EVM storage word.
    pub fn decode_channel_nonce(slot_value: U256) -> U256 {
        slot_value & U256::from(u64::MAX)
    }
}
