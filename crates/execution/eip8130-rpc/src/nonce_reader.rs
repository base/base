//! 2D channel-nonce reader used by `eth_getTransactionCount` extensions.

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types::state::StateOverride;
use base_common_consensus::Eip8130Constants;
use base_common_network::Base;
use base_common_precompiles::NonceManagerStorage;
use jsonrpsee_types::{ErrorObjectOwned, error::INVALID_PARAMS_CODE};
use reth_provider::StateProvider;
use reth_rpc_eth_api::helpers::{EthState, FullEthApi};
use reth_rpc_eth_types::EthApiError;

/// Reads 2D channel nonces (`nonces[account][nonce_key]`) from the Nonce Manager
/// precompile, with optional state-override support.
///
/// See the crate-level docs for behavior across the three `nonce_key` regimes
/// (protocol nonce, expiring-nonce sentinel, real 2D channel).
///
/// **Fork-agnostic on purpose.** This helper does not check that the
/// Cobalt fork has activated. Callers must enforce that themselves
/// (typically via [`crate::Eip8130CobaltGate`]) before invoking
/// [`Self::read`].
#[derive(Debug)]
pub struct ChannelNonceReader;

impl ChannelNonceReader {
    /// Resolves the channel nonce for `(address, nonce_key)` at `block_id`,
    /// honoring `state_overrides` if provided.
    ///
    /// `state_overrides` lets callers stack pending-state writes on top of the
    /// canonical block — e.g. flashblocks passes its accumulated
    /// [`StateOverride`] here so a 2D nonce incremented inside the pending
    /// flashblock is visible. The override's `state` field, if present,
    /// fully replaces the precompile's storage view; otherwise `state_diff`
    /// is consulted slot-by-slot and the canonical storage is used for slots
    /// the diff doesn't mention.
    ///
    /// # Errors
    /// - [`Eip8130Constants::NONCE_KEY_MAX`] returns an `INVALID_PARAMS` RPC
    ///   error: the expiring-nonce channel has no per-channel counter and
    ///   replay protection there relies on `expiry`, not a sequence number.
    /// - Any error from the underlying `eth_api` (e.g. unknown block, state
    ///   read failure) propagates as an `ErrorObjectOwned`.
    pub async fn read<Eth>(
        eth_api: &Eth,
        address: Address,
        nonce_key: U256,
        block_id: BlockId,
        state_overrides: Option<&StateOverride>,
    ) -> Result<U256, ErrorObjectOwned>
    where
        Eth: FullEthApi<NetworkTypes = Base> + Send + Sync + 'static,
        ErrorObjectOwned: From<Eth::Error>,
    {
        // Protocol nonce. Lives in account state, not the precompile.
        // Delegate to the standard `eth_getTransactionCount` resolution path.
        if nonce_key == U256::ZERO {
            return EthState::transaction_count(eth_api, address, Some(block_id))
                .await
                .map_err(Into::into);
        }

        // Expiring-nonce sentinel. No per-channel counter exists for this key.
        if nonce_key == Eip8130Constants::NONCE_KEY_MAX {
            return Err(ErrorObjectOwned::owned(
                INVALID_PARAMS_CODE,
                "nonce_key NONCE_KEY_MAX selects the expiring-nonce channel which has no per-channel counter",
                None::<()>,
            ));
        }

        // Real 2D channel. Derive slot, consult overrides, then fall back to
        // the canonical state at `block_id`.
        let slot = NonceManagerStorage::nonce_slot(address, nonce_key).map_err(|err| {
            ErrorObjectOwned::owned(
                INVALID_PARAMS_CODE,
                format!("failed to derive nonce slot for nonce_key: {err}"),
                None::<()>,
            )
        })?;
        let slot_b256 = B256::from(slot);

        if let Some(value) =
            Self::override_for_slot(state_overrides, NonceManagerStorage::ADDRESS, slot_b256)
        {
            return Ok(Self::decode_channel_nonce(value));
        }

        let state = eth_api.state_at_block_id(block_id).await.map_err(Into::into)?;
        let word = state
            .storage(NonceManagerStorage::ADDRESS, slot_b256)
            .map_err(|err| EthApiError::from(err).into())?
            .unwrap_or_default();
        Ok(Self::decode_channel_nonce(word))
    }

    /// Looks up a single storage slot in a [`StateOverride`], if one was
    /// provided and overrides `address`.
    ///
    /// Honors `state` (full replacement: missing slots read as zero) ahead of
    /// `state_diff` (partial merge: missing slots fall through). Returns
    /// `None` if no override applies; callers should fall back to the
    /// canonical state in that case.
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
        let state_diff = account_override.state_diff.as_ref()?;
        state_diff.get(&slot).copied().map(|value| U256::from_be_bytes(value.0))
    }

    /// Decodes a Solidity-packed `u64` (the channel nonce) from the low 64
    /// bits of an EVM storage slot, returning it widened to [`U256`].
    ///
    /// Widening to `U256` matches the return type of
    /// [`EthState::transaction_count`] so all three `nonce_key` branches of
    /// [`Self::read`] return the same shape.
    pub fn decode_channel_nonce(slot_value: U256) -> U256 {
        slot_value & U256::from(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use alloy_rpc_types::state::AccountOverride;

    use super::*;

    const ADDR: Address = address!("0x1111111111111111111111111111111111111111");
    const OTHER_ADDR: Address = address!("0x2222222222222222222222222222222222222222");
    const SLOT: B256 = B256::repeat_byte(0xAB);
    const OTHER_SLOT: B256 = B256::repeat_byte(0xCD);

    fn override_with(account_override: AccountOverride) -> StateOverride {
        let mut o = StateOverride::default();
        o.insert(ADDR, account_override);
        o
    }

    fn slot_to_b256(value: u64) -> B256 {
        let mut bytes = [0u8; 32];
        bytes[24..32].copy_from_slice(&value.to_be_bytes());
        B256::from(bytes)
    }

    #[test]
    fn decode_zero_slot() {
        assert_eq!(ChannelNonceReader::decode_channel_nonce(U256::ZERO), U256::ZERO);
    }

    #[test]
    fn decode_low_byte_value() {
        // Slot containing the u64 `42` right-aligned (Solidity packing).
        let slot_value = U256::from(42u64);
        assert_eq!(ChannelNonceReader::decode_channel_nonce(slot_value), U256::from(42u64));
    }

    #[test]
    fn decode_at_u64_max() {
        let slot_value = U256::from(u64::MAX);
        assert_eq!(ChannelNonceReader::decode_channel_nonce(slot_value), U256::from(u64::MAX));
    }

    #[test]
    fn decode_ignores_high_bits() {
        // A correctly-packed u64 only uses the low 8 bytes. Bits above bit 64
        // are not part of the channel-nonce value; the decoder should ignore
        // them so a malformed slot can't pollute the result.
        let slot_value = (U256::from(1u64) << 200) | U256::from(7u64);
        assert_eq!(ChannelNonceReader::decode_channel_nonce(slot_value), U256::from(7u64));
    }

    #[test]
    fn override_lookup_returns_none_when_no_overrides() {
        assert_eq!(ChannelNonceReader::override_for_slot(None, ADDR, SLOT), None);
    }

    #[test]
    fn override_lookup_returns_none_when_address_not_overridden() {
        let overrides = override_with(AccountOverride {
            state_diff: Some([(SLOT, slot_to_b256(5))].into_iter().collect()),
            ..Default::default()
        });
        assert_eq!(ChannelNonceReader::override_for_slot(Some(&overrides), OTHER_ADDR, SLOT), None);
    }

    #[test]
    fn override_lookup_reads_state_diff_hit() {
        let overrides = override_with(AccountOverride {
            state_diff: Some([(SLOT, slot_to_b256(9))].into_iter().collect()),
            ..Default::default()
        });
        assert_eq!(
            ChannelNonceReader::override_for_slot(Some(&overrides), ADDR, SLOT),
            Some(U256::from(9u64))
        );
    }

    #[test]
    fn override_lookup_falls_through_on_state_diff_miss() {
        // state_diff is a merge: missing slots fall through to the canonical
        // state, not zero.
        let overrides = override_with(AccountOverride {
            state_diff: Some([(OTHER_SLOT, slot_to_b256(5))].into_iter().collect()),
            ..Default::default()
        });
        assert_eq!(ChannelNonceReader::override_for_slot(Some(&overrides), ADDR, SLOT), None);
    }

    #[test]
    fn override_lookup_reads_full_state_hit() {
        let overrides = override_with(AccountOverride {
            state: Some([(SLOT, slot_to_b256(11))].into_iter().collect()),
            ..Default::default()
        });
        assert_eq!(
            ChannelNonceReader::override_for_slot(Some(&overrides), ADDR, SLOT),
            Some(U256::from(11u64))
        );
    }

    #[test]
    fn override_lookup_returns_zero_on_full_state_miss() {
        // `state` is a *replacement* — missing slots are zero, NOT a fall-through.
        let overrides = override_with(AccountOverride {
            state: Some([(OTHER_SLOT, slot_to_b256(5))].into_iter().collect()),
            ..Default::default()
        });
        assert_eq!(
            ChannelNonceReader::override_for_slot(Some(&overrides), ADDR, SLOT),
            Some(U256::ZERO)
        );
    }
}
