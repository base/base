//! 2D channel-nonce reader used by `eth_getTransactionCount` extensions.

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, U256};
use base_common_consensus::Eip8130Constants;
use base_common_precompiles::NonceManagerStorage;
use base_common_rpc_types::{BaseRpcTypes, Eip8130Nonce};
use jsonrpsee_types::{ErrorObjectOwned, error::INVALID_PARAMS_CODE};
use reth_provider::StateProvider;
use reth_rpc_eth_api::helpers::{EthState, FullEthApi};
use reth_rpc_eth_types::EthApiError;

/// Reads 2D channel nonces (`nonces[account][nonce_key]`) from the Nonce Manager
/// precompile.
///
/// See the crate-level docs for behavior across the three `nonce_key` regimes
/// (protocol nonce, expiring-nonce sentinel, real 2D channel).
///
/// **Fork-agnostic on purpose.** This helper does not check that the
/// Everest fork has activated. Callers must enforce that themselves
/// (typically via [`crate::Eip8130EverestGate`]) before invoking
/// [`Self::read`].
#[derive(Debug)]
pub struct ChannelNonceReader;

impl ChannelNonceReader {
    /// Resolves the channel nonce for `(address, nonce_key)` at `block_id`.
    ///
    /// # Errors
    /// - [`Eip8130Constants::NONCE_KEY_MAX`] returns an `INVALID_PARAMS` RPC
    ///   error: the expiring-nonce channel has no per-channel counter and
    ///   replay protection there relies on `valid_before`, not a sequence number.
    /// - Any error from the underlying `eth_api` (e.g. unknown block, state
    ///   read failure) propagates as an `ErrorObjectOwned`.
    pub async fn read<Eth>(
        eth_api: &Eth,
        address: Address,
        nonce_key: U256,
        block_id: BlockId,
    ) -> Result<U256, ErrorObjectOwned>
    where
        Eth: FullEthApi<NetworkTypes = BaseRpcTypes> + Send + Sync + 'static,
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

        // Real 2D channel. Derive slot and read it from the state at `block_id`.
        let slot = NonceManagerStorage::nonce_slot(address, nonce_key).map_err(|err| {
            ErrorObjectOwned::owned(
                INVALID_PARAMS_CODE,
                format!("failed to derive nonce slot for nonce_key: {err}"),
                None::<()>,
            )
        })?;
        let slot_b256 = B256::from(slot);

        let state = eth_api.state_at_block_id(block_id).await.map_err(Into::into)?;
        let word = state
            .storage(NonceManagerStorage::ADDRESS, slot_b256)
            .map_err(|err| EthApiError::from(err).into())?
            .unwrap_or_default();
        Ok(Self::decode_channel_nonce(word))
    }

    /// Decodes a Solidity-packed `u64` (the channel nonce) from the low 64
    /// bits of an EVM storage slot, returning it widened to [`U256`].
    ///
    /// Widening to `U256` matches the return type of
    /// [`EthState::transaction_count`] so all three `nonce_key` branches of
    /// [`Self::read`] return the same shape.
    pub fn decode_channel_nonce(slot_value: U256) -> U256 {
        Eip8130Nonce::decode_channel_nonce(slot_value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
