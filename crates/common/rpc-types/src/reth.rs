//! Reth compatibility implementations for RPC types.

use core::convert::Infallible;

use alloy_consensus::{SignableTransaction, error::ValueError};
use alloy_evm::{
    EvmEnv, FromRecoveredTx,
    env::BlockEnvironment,
    rpc::{EthTxEnvError, TryIntoTxEnv},
};
use alloy_network::TxSigner;
use alloy_primitives::{Address, Bytes, U256};
use alloy_signer::Signature;
use base_common_consensus::{BaseTransactionInfo, BaseTxEnvelope, Eip8130Signed, TxEip8130};
use base_common_evm::{BaseTransaction as BaseRevm, Eip8130ExecutionMode};
use reth_rpc_convert::{FromConsensusTx, SignTxRequestError, SignableTxRequest, TryIntoSimTx};
use revm::context::TxEnv;

use crate::{BaseTransactionRequest, Eip8130AuthScheme, Transaction};

/// Filler byte for synthesized authentication stubs. Non-zero so the EIP-2028
/// calldata cost of the stub matches a real (high-entropy) signature rather
/// than under-pricing it as zero bytes; the bytes are never recovered.
const STUB_AUTH_FILL: u8 = 0xff;

/// Length (in bytes) of the leading authenticator-address selector on a prefixed
/// (`authenticator(20) || data`) authentication blob.
const AUTHENTICATOR_SELECTOR_LEN: usize = 20;

/// Upper bound (in bytes) on the caller-supplied authentication-payload data
/// (the `sender_auth` / `payer_auth` bytes after any 20-byte selector). Real
/// authenticator payloads are at most a few hundred bytes (e.g. a `WebAuthn`
/// assertion with its client-data JSON), so 8 `KiB` is generous. The cap bounds
/// the calldata the estimate has to hash and price; an over-cap blob is rejected
/// (surfaced as `INVALID_PARAMS`) rather than priced.
const MAX_AUTH_SIZE: u32 = 8_192;

impl BaseTransactionRequest {
    /// Builds the unsigned simulation transaction for an EIP-8130
    /// `eth_estimateGas` / `eth_call` request, or `None` when the request
    /// carries no EIP-8130 fields or omits the required `sender`.
    ///
    /// Estimation runs without a signature. The caller passes the raw
    /// authentication blob it intends to sign (`sender_auth`, and for sponsored
    /// transactions `payer_auth`); the intrinsic-gas schedule prices that blob's
    /// authentication gas (the authenticator's execution gas, selected by the
    /// leading 20-byte authenticator address, plus the EIP-2028 calldata cost of
    /// the whole blob). The blob is never recovered —
    /// [`base_common_evm::Eip8130Executor::simulate`] simulates from `sender`
    /// without verification. `gas_limit_cap` bounds execution when the request
    /// omits `gas`.
    ///
    /// The 8130 path always targets the configured-account form: `sender` is the
    /// account (`tx.sender` is set), the flattened `from` is ignored, and
    /// `sender_auth` is always a prefixed `authenticator(20) || data` blob:
    ///
    /// - A supplied `sender_auth` must carry a recognized enshrined authenticator
    ///   selector ([`Eip8130AuthScheme::Secp256k1`] / `P256` / `WebAuthn`); an
    ///   absent blob defaults to a representative secp256k1 authorization.
    /// - A declared `payer` adds payer authentication, priced from `payer_auth`
    ///   (defaulting to a representative secp256k1 authorization). Like
    ///   `sender_auth`, a supplied `payer_auth` is the prefixed form and must
    ///   carry a recognized enshrined authenticator selector.
    ///
    /// `sender` is mandatory for EIP-8130: the sender identity drives actor
    /// resolution, policy lookup, and auto-delegation, so a missing `sender`
    /// returns `None` (surfaced as `INVALID_PARAMS`) rather than silently
    /// falling back to the zero address. The default-EOA (bare-signature) path
    /// is intentionally not estimable here — estimation targets configured
    /// accounts.
    ///
    /// A `sender_auth` / `payer_auth` whose leading 20 bytes are not a recognized
    /// enshrined authenticator selector returns `None` (surfaced as
    /// `INVALID_PARAMS`) rather than pricing an authenticator the intrinsic-gas
    /// schedule doesn't recognize (which could under-price the estimate). A blob
    /// whose data exceeds [`MAX_AUTH_SIZE`] bytes (excluding the 20-byte
    /// authenticator selector) is rejected the same way, rather than pricing an
    /// unbounded payload.
    pub fn to_eip8130_simulation_tx(
        &self,
        chain_id: u64,
        gas_limit_cap: u64,
    ) -> Option<BaseRevm<TxEnv>> {
        let aa = self.as_eip8130()?;
        let req = self.as_ref();
        let sender = aa.sender?;

        // The 8130 path always simulates the configured-account form: `sender`
        // is the account and `sender_auth` is always a prefixed
        // `authenticator(20) || data` blob. A supplied blob must carry a
        // recognized enshrined authenticator selector (rejected otherwise, so an
        // unknown authenticator can't under-price the estimate); an absent blob
        // synthesizes a representative secp256k1 authorization. The blob is
        // priced verbatim, never verified.
        let sender_auth = match &aa.sender_auth {
            Some(blob) => {
                if !Self::is_prefixed_auth(blob) {
                    return None;
                }
                Self::check_auth_len(blob)?;
                blob.clone()
            }
            None => Self::stub_prefixed_auth(
                Eip8130AuthScheme::Secp256k1,
                Eip8130AuthScheme::Secp256k1.default_data_len(),
            ),
        };

        // Sponsored payer authentication, priced only when a payer is declared.
        // The payer auth is always a prefixed `authenticator || data` blob, so a
        // supplied blob must carry an enshrined authenticator selector — an
        // unrecognized prefix is rejected rather than silently priced (a
        // selector missing from the intrinsic schedule could under-price the
        // estimate).
        let (payer, payer_auth) = match aa.payer {
            None => (None, Bytes::new()),
            Some(payer) => {
                let blob = match &aa.payer_auth {
                    Some(blob) => {
                        if !Self::is_prefixed_auth(blob) {
                            return None;
                        }
                        Self::check_auth_len(blob)?;
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
            sender: Some(sender),
            nonce_key: aa.nonce_key.unwrap_or(U256::ZERO),
            nonce_sequence: 0,
            expiry: aa.expiry.unwrap_or_default(),
            max_priority_fee_per_gas: req.max_priority_fee_per_gas.unwrap_or_default(),
            max_fee_per_gas: req.max_fee_per_gas.unwrap_or_default(),
            gas_limit: req.gas.unwrap_or(gas_limit_cap),
            account_changes: aa.account_changes.clone().unwrap_or_default(),
            calls: aa.calls.clone().unwrap_or_default(),
            metadata: aa.metadata.clone().unwrap_or_default(),
            payer,
        };

        let envelope = BaseTxEnvelope::Eip8130(Eip8130Signed::new(tx, sender_auth, payer_auth));
        let mut sim_tx = BaseRevm::from_recovered_tx(&envelope, sender);
        // Route to the unverified `Eip8130Executor::simulate` path rather than
        // the verifying `execute` path.
        if let Some(parts) = sim_tx.eip8130.as_mut() {
            parts.mode = Eip8130ExecutionMode::Simulate;
        }
        Some(sim_tx)
    }

    /// Rejects (as `None`, surfaced to the caller as `INVALID_PARAMS`) a
    /// prefixed authentication blob whose *data* (the bytes after the 20-byte
    /// `authenticator(20) || data` selector) exceeds [`MAX_AUTH_SIZE`] bytes,
    /// bounding the calldata the estimate prices.
    fn check_auth_len(blob: &Bytes) -> Option<()> {
        let data_len = blob.len().saturating_sub(AUTHENTICATOR_SELECTOR_LEN);
        (data_len as u64 <= u64::from(MAX_AUTH_SIZE)).then_some(())
    }

    /// Whether an authentication blob carries a recognized enshrined
    /// authenticator selector in its leading 20 bytes (`authenticator(20) ||
    /// data`), checked against [`Eip8130AuthScheme::ALL`] (the single
    /// authoritative list, rather than a second hardcoded address set that could
    /// drift from the enum). A supplied `sender_auth` / `payer_auth` blob that
    /// fails this check is rejected rather than priced, so an unknown
    /// authenticator can't fall through and under-price the estimate.
    fn is_prefixed_auth(blob: &Bytes) -> bool {
        if blob.len() < AUTHENTICATOR_SELECTOR_LEN {
            return false;
        }
        let selector = Address::from_slice(&blob[..AUTHENTICATOR_SELECTOR_LEN]);
        Eip8130AuthScheme::ALL.into_iter().any(|scheme| scheme.authenticator() == selector)
    }

    /// Builds a prefixed stub authentication blob — `authenticator(20) || data`
    /// — for the given scheme, where `data` is `data_len` filler bytes. The
    /// authenticator selector drives the schedule's execution-gas charge and the
    /// total length drives the calldata charge; the bytes are never recovered.
    fn stub_prefixed_auth(scheme: Eip8130AuthScheme, data_len: usize) -> Bytes {
        let mut blob = scheme.authenticator().to_vec();
        // Fill with a non-zero byte (`STUB_AUTH_FILL`) so the EIP-2028 calldata
        // charge matches a real, high-entropy signature (zero bytes are cheaper).
        blob.resize(blob.len() + data_len, STUB_AUTH_FILL);
        Bytes::from(blob)
    }
}

impl FromConsensusTx<BaseTxEnvelope> for Transaction {
    type TxInfo = BaseTransactionInfo;
    type Err = Infallible;

    fn from_consensus_tx(
        tx: BaseTxEnvelope,
        signer: Address,
        tx_info: BaseTransactionInfo,
    ) -> Result<Self, Infallible> {
        Ok(Self::from_transaction(
            alloy_consensus::transaction::Recovered::new_unchecked(tx, signer),
            tx_info,
        ))
    }
}

impl<Spec, Block: BlockEnvironment> TryIntoTxEnv<BaseRevm<TxEnv>, Spec, Block>
    for BaseTransactionRequest
{
    type Err = EthTxEnvError;

    fn try_into_tx_env(self, evm_env: &EvmEnv<Spec, Block>) -> Result<BaseRevm<TxEnv>, Self::Err> {
        Ok(BaseRevm {
            base: self.as_ref().clone().try_into_tx_env(evm_env)?,
            enveloped_tx: Some(Bytes::new()),
            deposit: Default::default(),
            eip8130: None,
        })
    }
}

impl TryIntoSimTx<BaseTxEnvelope> for BaseTransactionRequest {
    fn try_into_sim_tx(self) -> Result<BaseTxEnvelope, ValueError<Self>> {
        let tx = self
            .build_typed_tx()
            .map_err(|request| ValueError::new(request, "Required fields missing"))?;

        // Create an empty signature for the transaction.
        let signature = Signature::new(Default::default(), Default::default(), false);

        Ok(tx.into_signed(signature).into())
    }
}

impl SignableTxRequest<BaseTxEnvelope> for BaseTransactionRequest {
    async fn try_build_and_sign(
        self,
        signer: impl TxSigner<Signature> + Send,
    ) -> Result<BaseTxEnvelope, SignTxRequestError> {
        let mut tx =
            self.build_typed_tx().map_err(|_| SignTxRequestError::InvalidTransactionRequest)?;

        // sanity check: deposit transactions must not be signed by the user
        if tx.is_deposit() {
            return Err(SignTxRequestError::InvalidTransactionRequest);
        }

        let signature = signer.sign_transaction(&mut tx).await?;

        Ok(tx.into_signed(signature).into())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use base_common_consensus::{Eip8130Constants, Eip8130Contracts, Eip8130Signed};
    use serde_json::json;

    use super::*;

    const CHAIN_ID: u64 = 8453;
    const GAS_CAP: u64 = 30_000_000;
    const SENDER: Address = address!("0x00000000000000000000000000000000000000a1");

    fn sim_tx(request: serde_json::Value) -> BaseRevm<TxEnv> {
        let req: BaseTransactionRequest = serde_json::from_value(request).expect("valid request");
        req.to_eip8130_simulation_tx(CHAIN_ID, GAS_CAP).expect("simulation tx")
    }

    fn signed(tx: &BaseRevm<TxEnv>) -> &Eip8130Signed {
        &tx.eip8130.as_ref().expect("eip8130 parts").signed
    }

    /// Builds a hex (`0x`-prefixed) authentication blob: an optional 20-byte
    /// authenticator selector followed by `data_len` filler bytes.
    fn blob(authenticator: Option<Address>, data_len: usize) -> alloc::string::String {
        let mut v = alloc::vec::Vec::new();
        if let Some(a) = authenticator {
            v.extend_from_slice(a.as_slice());
        }
        v.resize(v.len() + data_len, STUB_AUTH_FILL);
        alloy_primitives::hex::encode_prefixed(v)
    }

    #[test]
    fn absent_auth_defaults_to_prefixed_secp256k1() {
        let tx = sim_tx(json!({ "sender": SENDER, "calls": [] }));
        let s = signed(&tx);
        assert_eq!(s.tx().sender, Some(SENDER), "the 8130 path always sets the configured sender");
        let auth = s.sender_auth();
        assert_eq!(
            &auth[..20],
            Eip8130Constants::K1_AUTHENTICATOR.as_slice(),
            "an absent blob defaults to a prefixed secp256k1 authorization",
        );
        assert_eq!(
            auth.len(),
            20 + Eip8130AuthScheme::Secp256k1.default_data_len(),
            "the default stub is selector + the scheme's default data length",
        );
        assert!(s.payer_auth().is_empty(), "no declared payer means no payer auth");
    }

    #[test]
    fn prefixed_p256_auth_is_priced_verbatim() {
        let tx = sim_tx(json!({
            "sender": SENDER,
            "calls": [],
            "senderAuth": blob(Some(Eip8130Contracts::P256_AUTHENTICATOR), 128),
        }));
        let s = signed(&tx);
        assert_eq!(s.tx().sender, Some(SENDER));
        let auth = s.sender_auth();
        assert_eq!(
            &auth[..20],
            Eip8130Contracts::P256_AUTHENTICATOR.as_slice(),
            "the caller's blob is priced verbatim, prefix intact",
        );
        assert_eq!(auth.len(), 20 + 128, "selector + supplied data length");
    }

    #[test]
    fn prefixed_webauthn_auth_is_priced_verbatim() {
        let tx = sim_tx(json!({
            "sender": SENDER,
            "calls": [],
            "senderAuth": blob(Some(Eip8130Contracts::WEBAUTHN_AUTHENTICATOR), 512),
        }));
        let s = signed(&tx);
        assert_eq!(s.tx().sender, Some(SENDER));
        let auth = s.sender_auth();
        assert_eq!(&auth[..20], Eip8130Contracts::WEBAUTHN_AUTHENTICATOR.as_slice());
        assert_eq!(auth.len(), 20 + 512, "the WebAuthn blob is priced at its supplied size");
    }

    #[test]
    fn prefixed_k1_auth_is_priced_verbatim() {
        let tx = sim_tx(json!({
            "sender": SENDER,
            "calls": [],
            "senderAuth": blob(Some(Eip8130Constants::K1_AUTHENTICATOR), 65),
        }));
        let s = signed(&tx);
        assert_eq!(s.tx().sender, Some(SENDER));
        assert_eq!(s.sender_auth().len(), 20 + 65);
    }

    #[test]
    fn missing_sender_is_rejected() {
        // Every 8130 estimate targets a configured account; a request with 8130
        // fields but no `sender` is rejected rather than defaulting the account.
        let req: BaseTransactionRequest =
            serde_json::from_value(json!({ "calls": [] })).expect("valid request");
        assert!(
            req.to_eip8130_simulation_tx(CHAIN_ID, GAS_CAP).is_none(),
            "an 8130 request without a sender is rejected",
        );
    }

    #[test]
    fn bare_sender_auth_is_rejected() {
        // A supplied `sender_auth` must carry a recognized enshrined authenticator
        // selector; an unprefixed (bare) blob is rejected rather than priced.
        let req: BaseTransactionRequest = serde_json::from_value(json!({
            "sender": SENDER,
            "calls": [],
            "senderAuth": blob(None, 65),
        }))
        .expect("valid request");
        assert!(
            req.to_eip8130_simulation_tx(CHAIN_ID, GAS_CAP).is_none(),
            "an unprefixed sender auth blob is rejected rather than priced",
        );
    }

    #[test]
    fn sender_auth_with_unrecognized_authenticator_is_rejected() {
        let unrecognized = address!("0x000000000000000000000000000000000000dead");
        let req: BaseTransactionRequest = serde_json::from_value(json!({
            "sender": SENDER,
            "calls": [],
            "senderAuth": blob(Some(unrecognized), 65),
        }))
        .expect("valid request");
        assert!(
            req.to_eip8130_simulation_tx(CHAIN_ID, GAS_CAP).is_none(),
            "an unrecognized sender authenticator selector is rejected rather than priced",
        );
    }

    #[test]
    fn explicit_secp256k1_scheme_resolves_to_the_k1_selector() {
        // The mapping is pinned so the schedule charges the k1 entry.
        assert_eq!(
            Eip8130AuthScheme::Secp256k1.authenticator(),
            Eip8130Constants::K1_AUTHENTICATOR,
        );
    }

    #[test]
    fn sender_auth_data_at_the_cap_is_accepted() {
        // The 20-byte selector is excluded from the cap, so `MAX_AUTH_SIZE` data
        // bytes are honoured (total = selector + data).
        let tx = sim_tx(json!({
            "sender": SENDER,
            "calls": [],
            "senderAuth": blob(Some(Eip8130Contracts::WEBAUTHN_AUTHENTICATOR), MAX_AUTH_SIZE as usize),
        }));
        let auth = signed(&tx).sender_auth();
        assert_eq!(
            auth.len(),
            20 + MAX_AUTH_SIZE as usize,
            "data at the cap is honoured (selector + data)",
        );
    }

    #[test]
    fn oversize_sender_auth_data_is_rejected() {
        let req: BaseTransactionRequest = serde_json::from_value(json!({
            "sender": SENDER,
            "calls": [],
            "senderAuth": blob(Some(Eip8130Contracts::WEBAUTHN_AUTHENTICATOR), MAX_AUTH_SIZE as usize + 1),
        }))
        .expect("valid request");
        assert!(
            req.to_eip8130_simulation_tx(CHAIN_ID, GAS_CAP).is_none(),
            "an over-cap sender auth blob is rejected rather than priced",
        );
    }

    #[test]
    fn oversize_payer_auth_is_rejected() {
        let payer = address!("0x00000000000000000000000000000000000000b2");
        let req: BaseTransactionRequest = serde_json::from_value(json!({
            "sender": SENDER,
            "calls": [],
            "payer": payer,
            "payerAuth": blob(Some(Eip8130Contracts::P256_AUTHENTICATOR), MAX_AUTH_SIZE as usize + 1),
        }))
        .expect("valid request");
        assert!(
            req.to_eip8130_simulation_tx(CHAIN_ID, GAS_CAP).is_none(),
            "an over-cap payer auth blob is rejected rather than priced",
        );
    }

    #[test]
    fn declared_payer_auth_is_priced_verbatim() {
        let payer = address!("0x00000000000000000000000000000000000000b2");
        let tx = sim_tx(json!({
            "sender": SENDER,
            "calls": [],
            "payer": payer,
            "payerAuth": blob(Some(Eip8130Contracts::P256_AUTHENTICATOR), 128),
        }));
        let s = signed(&tx);
        assert_eq!(s.tx().payer, Some(payer), "the payer is set on the transaction");
        let auth = s.payer_auth();
        assert_eq!(&auth[..20], Eip8130Contracts::P256_AUTHENTICATOR.as_slice());
        assert_eq!(auth.len(), 20 + 128);
    }

    #[test]
    fn payer_auth_with_unrecognized_authenticator_is_rejected() {
        // An arbitrary 20-byte prefix must not be forwarded to the intrinsic
        // schedule verbatim: an unrecognized authenticator is rejected rather
        // than silently priced (potentially as zero, under-pricing the estimate).
        let payer = address!("0x00000000000000000000000000000000000000b2");
        let unrecognized = address!("0x000000000000000000000000000000000000dead");
        let req: BaseTransactionRequest = serde_json::from_value(json!({
            "sender": SENDER,
            "calls": [],
            "payer": payer,
            "payerAuth": blob(Some(unrecognized), 65),
        }))
        .expect("valid request");
        assert!(
            req.to_eip8130_simulation_tx(CHAIN_ID, GAS_CAP).is_none(),
            "an unrecognized payer authenticator selector is rejected rather than priced",
        );
    }

    #[test]
    fn declared_payer_without_auth_defaults_to_secp256k1() {
        let payer = address!("0x00000000000000000000000000000000000000b2");
        let tx = sim_tx(json!({ "sender": SENDER, "calls": [], "payer": payer }));
        let s = signed(&tx);
        assert_eq!(s.tx().payer, Some(payer));
        let auth = s.payer_auth();
        assert_eq!(
            &auth[..20],
            Eip8130Constants::K1_AUTHENTICATOR.as_slice(),
            "the default payer authorization is prefixed with the k1 authenticator",
        );
        assert_eq!(auth.len(), 20 + Eip8130AuthScheme::Secp256k1.default_data_len());
    }
}
