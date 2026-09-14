//! Reth-free EIP-8130 RPC request conversion.

use alloc::vec;

use alloy_evm::FromRecoveredTx;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_rpc_types_eth::state::StateOverride;
use base_common_consensus::{
    BaseTxEnvelope, Eip8130Constants, Eip8130Contracts, Eip8130Signed, TxEip8130,
};
use base_common_evm::{BaseTransaction as BaseRevm, Eip8130ExecutionMode};
use revm::context::TxEnv;

use crate::{BaseTransactionRequest, Eip8130AuthScheme};

/// Filler byte for synthesized authentication stubs.
pub(crate) const STUB_AUTH_FILL: u8 = 0xff;

/// Length of the authenticator selector on a prefixed authentication blob.
const AUTHENTICATOR_SELECTOR_LEN: usize = 20;

/// Maximum caller-supplied authentication payload length.
pub(crate) const MAX_AUTH_SIZE: u32 = 8_192;

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

impl BaseTransactionRequest {
    /// Builds an unsigned EIP-8130 simulation transaction.
    ///
    /// Returns `None` when the request carries no EIP-8130 fields, does not resolve a sender,
    /// contains conflicting `sender` and `from` values, or supplies an invalid authentication
    /// blob. The returned transaction uses [`Eip8130ExecutionMode::Simulate`] so callers can run
    /// `eth_call` and `eth_estimateGas` without signature verification or committed state.
    pub fn to_eip8130_simulation_tx(
        &self,
        chain_id: u64,
        gas_limit_cap: u64,
    ) -> Option<BaseRevm<TxEnv>> {
        let aa = self.as_eip8130()?;
        let req = self.as_ref();

        let account = match (aa.sender, req.from) {
            (Some(sender), Some(from)) if sender != from => return None,
            (Some(sender), _) => sender,
            (None, Some(from)) => from,
            (None, None) => return None,
        };
        let sender_declared = aa.sender.is_some();

        let (sender, sender_auth) = match &aa.sender_auth {
            Some(blob) => {
                let prefixed = Self::is_prefixed_auth(blob);
                Self::check_auth_len(blob, prefixed)?;
                (prefixed.then_some(account), blob.clone())
            }
            None if sender_declared => (
                Some(account),
                Self::stub_prefixed_auth(
                    Eip8130AuthScheme::Secp256k1,
                    Eip8130AuthScheme::Secp256k1.default_data_len(),
                ),
            ),
            None => (None, Self::default_bare_auth()),
        };

        let (payer, payer_auth) = match aa.payer {
            None => (None, Bytes::new()),
            Some(payer) => {
                let blob = match &aa.payer_auth {
                    Some(blob) => {
                        if !Self::is_prefixed_auth(blob) {
                            return None;
                        }
                        Self::check_auth_len(blob, true)?;
                        blob.clone()
                    }
                    None => Self::stub_prefixed_auth(
                        Eip8130AuthScheme::Secp256k1,
                        Eip8130AuthScheme::Secp256k1.default_data_len(),
                    ),
                };
                (Some(payer), blob)
            }
        };

        let tx = TxEip8130 {
            chain_id,
            sender,
            nonce_key: aa.nonce_key.unwrap_or(U256::ZERO),
            nonce_sequence: 0,
            valid_after: aa.valid_after.unwrap_or_default(),
            valid_before: aa.valid_before.unwrap_or_default(),
            max_priority_fee_per_gas: req.max_priority_fee_per_gas.unwrap_or_default(),
            max_fee_per_gas: req.max_fee_per_gas.unwrap_or_default(),
            gas_limit: req.gas.unwrap_or(gas_limit_cap),
            account_changes: aa.account_changes.clone().unwrap_or_default(),
            calls: aa.calls.clone().unwrap_or_default(),
            metadata: aa.metadata.clone().unwrap_or_default(),
            payer,
        };

        let envelope = BaseTxEnvelope::Eip8130(Eip8130Signed::new(tx, sender_auth, payer_auth));
        let mut simulation = BaseRevm::from_recovered_tx(&envelope, account);
        if let Some(parts) = simulation.eip8130.as_mut() {
            parts.mode = Eip8130ExecutionMode::Simulate;
            parts.simulation_sender_actor_id = aa.sender_actor_id;
        }
        Some(simulation)
    }

    fn default_bare_auth() -> Bytes {
        Bytes::from(vec![STUB_AUTH_FILL; Eip8130AuthScheme::Secp256k1.default_data_len()])
    }

    fn check_auth_len(blob: &Bytes, prefixed: bool) -> Option<()> {
        let data_len = if prefixed {
            blob.len().saturating_sub(AUTHENTICATOR_SELECTOR_LEN)
        } else {
            blob.len()
        };
        (data_len as u64 <= u64::from(MAX_AUTH_SIZE)).then_some(())
    }

    fn is_prefixed_auth(blob: &Bytes) -> bool {
        if blob.len() < AUTHENTICATOR_SELECTOR_LEN {
            return false;
        }
        let selector = Address::from_slice(&blob[..AUTHENTICATOR_SELECTOR_LEN]);
        selector == Eip8130Constants::K1_AUTHENTICATOR
            || Eip8130Contracts::is_canonical_authenticator(&selector)
    }

    fn stub_prefixed_auth(scheme: Eip8130AuthScheme, data_len: usize) -> Bytes {
        let mut blob = scheme.authenticator().to_vec();
        blob.resize(blob.len() + data_len, STUB_AUTH_FILL);
        Bytes::from(blob)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256};
    use alloy_rpc_types_eth::state::AccountOverride;
    use base_common_consensus::{Eip8130Constants, Eip8130Signed};
    use serde_json::json;

    use super::*;

    const CHAIN_ID: u64 = 8453;
    const GAS_CAP: u64 = 30_000_000;
    const SENDER: Address = address!("00000000000000000000000000000000000000a1");
    const FROM: Address = address!("00000000000000000000000000000000000000c3");

    fn simulation(request: serde_json::Value) -> BaseRevm<TxEnv> {
        let request = serde_json::from_value::<BaseTransactionRequest>(request).unwrap();
        request.to_eip8130_simulation_tx(CHAIN_ID, GAS_CAP).unwrap()
    }

    fn signed(tx: &BaseRevm<TxEnv>) -> &Eip8130Signed {
        &tx.eip8130.as_ref().unwrap().signed
    }

    #[test]
    fn configured_sender_gets_prefixed_k1_stub() {
        let tx = simulation(json!({ "sender": SENDER, "calls": [] }));
        assert_eq!(signed(&tx).tx().sender, Some(SENDER));
        assert_eq!(&signed(&tx).sender_auth()[..20], Eip8130Constants::K1_AUTHENTICATOR.as_slice());
    }

    #[test]
    fn from_only_request_uses_eoa_path() {
        let tx = simulation(json!({ "from": FROM, "calls": [] }));
        assert!(signed(&tx).tx().sender.is_none());
        assert_eq!(
            signed(&tx).sender_auth().len(),
            Eip8130AuthScheme::Secp256k1.default_data_len()
        );
    }

    #[test]
    fn mismatched_sender_and_from_are_rejected() {
        let request = serde_json::from_value::<BaseTransactionRequest>(json!({
            "from": FROM,
            "sender": SENDER,
            "calls": []
        }))
        .unwrap();
        assert!(request.to_eip8130_simulation_tx(CHAIN_ID, GAS_CAP).is_none());
    }

    #[test]
    fn missing_sender_is_rejected() {
        let request =
            serde_json::from_value::<BaseTransactionRequest>(json!({ "calls": [] })).unwrap();
        assert!(request.to_eip8130_simulation_tx(CHAIN_ID, GAS_CAP).is_none());
    }

    #[test]
    fn declared_payer_gets_prefixed_auth() {
        let payer = address!("00000000000000000000000000000000000000b2");
        let tx = simulation(json!({ "sender": SENDER, "calls": [], "payer": payer }));
        assert_eq!(signed(&tx).tx().payer, Some(payer));
        assert_eq!(&signed(&tx).payer_auth()[..20], Eip8130Constants::K1_AUTHENTICATOR.as_slice());
    }

    #[test]
    fn oversized_auth_is_rejected() {
        let auth =
            alloy_primitives::hex::encode_prefixed([STUB_AUTH_FILL; MAX_AUTH_SIZE as usize + 1]);
        let request = serde_json::from_value::<BaseTransactionRequest>(json!({
            "from": FROM,
            "calls": [],
            "senderAuth": auth
        }))
        .unwrap();
        assert!(request.to_eip8130_simulation_tx(CHAIN_ID, GAS_CAP).is_none());
    }

    #[test]
    fn actor_hint_and_simulation_mode_are_retained() {
        let actor = b256!("30df39d5edcf9ed82b6d77d27bff1192ac265918000000000000000000000000");
        let tx = simulation(json!({ "sender": SENDER, "calls": [], "senderActorId": actor }));
        let parts = tx.eip8130.unwrap();
        assert_eq!(parts.mode, Eip8130ExecutionMode::Simulate);
        assert_eq!(parts.simulation_sender_actor_id, Some(actor));
    }

    #[test]
    fn channel_nonce_decodes_low_u64() {
        let slot = (U256::ONE << 200) | U256::from(42);
        assert_eq!(Eip8130Nonce::decode_channel_nonce(slot), U256::from(42));
    }

    #[test]
    fn channel_nonce_reads_state_diff_override() {
        let slot = B256::repeat_byte(0xab);
        let value = B256::from(U256::from(9).to_be_bytes::<32>());
        let overrides = [(
            SENDER,
            AccountOverride {
                state_diff: Some([(slot, value)].into_iter().collect()),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect();
        assert_eq!(
            Eip8130Nonce::override_for_slot(Some(&overrides), SENDER, slot),
            Some(U256::from(9))
        );
    }

    #[test]
    fn channel_nonce_full_state_miss_is_zero() {
        let slot = B256::repeat_byte(0xab);
        let overrides =
            [(SENDER, AccountOverride { state: Some(Default::default()), ..Default::default() })]
                .into_iter()
                .collect();
        assert_eq!(
            Eip8130Nonce::override_for_slot(Some(&overrides), SENDER, slot),
            Some(U256::ZERO)
        );
    }
}
