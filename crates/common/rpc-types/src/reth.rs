//! Reth compatibility implementations for RPC types.

use alloc::vec;
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
    /// carries no EIP-8130 fields or omits the required `from` sender.
    ///
    /// Estimation runs without a signature. The caller passes the raw
    /// authentication blob it intends to sign (`sender_auth`, and for sponsored
    /// transactions `payer_auth`); the intrinsic-gas schedule prices that blob's
    /// authentication gas (the authenticator's execution gas, selected by the
    /// leading 20-byte authenticator address, plus the EIP-2028 calldata cost of
    /// the whole blob). The blob is never recovered —
    /// [`base_common_evm::Eip8130Executor::simulate`] simulates from `from`
    /// without verification. `gas_limit_cap` bounds execution when the request
    /// omits `gas`.
    ///
    /// The `from` account is the sender; the `sender_auth` blob's form selects
    /// the authentication path, mirroring the on-wire transaction:
    ///
    /// - A bare or absent `sender_auth` prices the default-EOA bare-signature
    ///   (secp256k1) path for `from`, exactly as a 1559 transaction — an absent
    ///   blob defaults to a representative 65-byte stub.
    /// - A `sender_auth` prefixed with an enshrined authenticator
    ///   (`authenticator(20) || data`) prices the configured-account path for
    ///   `from`.
    /// - A declared `payer` adds payer authentication, priced from `payer_auth`
    ///   (defaulting to a representative secp256k1 authorization). Unlike
    ///   `sender_auth`, a supplied `payer_auth` is always the prefixed
    ///   configured-account form and must carry a recognized enshrined
    ///   authenticator selector.
    ///
    /// `from` is mandatory for EIP-8130: the sender identity drives actor
    /// resolution, policy lookup, and auto-delegation, so a missing `from`
    /// returns `None` (surfaced as `INVALID_PARAMS`) rather than silently
    /// falling back to the zero address.
    ///
    /// A `payer_auth` whose leading 20 bytes are not a recognized enshrined
    /// authenticator selector returns `None` (surfaced as `INVALID_PARAMS`)
    /// rather than pricing an authenticator the intrinsic-gas schedule doesn't
    /// recognize (which could under-price the estimate). A `sender_auth` /
    /// `payer_auth` blob whose data exceeds [`MAX_AUTH_SIZE`] bytes (excluding
    /// the 20-byte authenticator selector on the configured path) is rejected
    /// the same way, rather than pricing an unbounded payload.
    pub fn to_eip8130_simulation_tx(
        &self,
        chain_id: u64,
        gas_limit_cap: u64,
    ) -> Option<BaseRevm<TxEnv>> {
        let aa = self.as_eip8130()?;
        let req = self.as_ref();
        let from = req.from?;

        // The `from` account is the sender; the `sender_auth` blob's form selects
        // the authentication path, mirroring the wire. A blob prefixed with an
        // enshrined authenticator selector (`authenticator(20) || data`) is the
        // configured-account path (`sender` set to `from`); a bare or absent blob
        // is the default-EOA path (`sender` unset), where `from` authenticates
        // with a k1 signature exactly as a 1559 transaction. The blob is priced
        // verbatim (never verified); an absent blob synthesizes a bare k1 stub.
        let (sender, sender_auth) = match &aa.sender_auth {
            None => (None, Self::default_bare_auth()),
            Some(blob) => {
                let prefixed = Self::is_prefixed_auth(blob);
                Self::check_auth_len(blob, prefixed)?;
                (prefixed.then_some(from), blob.clone())
            }
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
        let mut sim_tx = BaseRevm::from_recovered_tx(&envelope, from);
        // Route to the unverified `Eip8130Executor::simulate` path rather than
        // the verifying `execute` path.
        if let Some(parts) = sim_tx.eip8130.as_mut() {
            parts.mode = Eip8130ExecutionMode::Simulate;
        }
        Some(sim_tx)
    }

    /// The default-EOA bare secp256k1 authentication stub: a representative
    /// `r || s || v`-shaped blob filled with a non-zero byte so its EIP-2028
    /// calldata cost matches a real signature. Never recovered.
    fn default_bare_auth() -> Bytes {
        Bytes::from(vec![STUB_AUTH_FILL; Eip8130AuthScheme::Secp256k1.default_data_len()])
    }

    /// Rejects (as `None`, surfaced to the caller as `INVALID_PARAMS`) an
    /// authentication blob whose *data* exceeds [`MAX_AUTH_SIZE`] bytes,
    /// excluding the 20-byte authenticator selector for a `prefixed`
    /// (`authenticator(20) || data`) blob, bounding the calldata the estimate
    /// prices.
    fn check_auth_len(blob: &Bytes, prefixed: bool) -> Option<()> {
        let data_len = if prefixed {
            blob.len().saturating_sub(AUTHENTICATOR_SELECTOR_LEN)
        } else {
            blob.len()
        };
        (data_len as u64 <= u64::from(MAX_AUTH_SIZE)).then_some(())
    }

    /// Whether a `sender_auth` blob is in the prefixed configured-account form
    /// (`authenticator(20) || data`) rather than a bare EOA signature: true when
    /// it begins with an enshrined authenticator selector. This mirrors the wire
    /// authentication form the intrinsic-gas schedule prices the blob under, so a
    /// prefixed blob simulates on the configured-account path (`sender` set) and
    /// a bare or absent blob on the default-EOA path (`sender` unset).
    fn is_prefixed_auth(blob: &Bytes) -> bool {
        if blob.len() < AUTHENTICATOR_SELECTOR_LEN {
            return false;
        }
        let selector = Address::from_slice(&blob[..AUTHENTICATOR_SELECTOR_LEN]);
        selector == Eip8130AuthScheme::Secp256k1.authenticator()
            || selector == Eip8130AuthScheme::P256.authenticator()
            || selector == Eip8130AuthScheme::WebAuthn.authenticator()
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
    const FROM: Address = address!("0x00000000000000000000000000000000000000a1");

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
    fn absent_auth_builds_bare_secp256k1_stub() {
        let tx = sim_tx(json!({ "from": FROM, "calls": [] }));
        let s = signed(&tx);
        assert!(s.tx().sender.is_none(), "an absent auth blob uses the default-EOA bare form");
        assert_eq!(
            s.sender_auth().len(),
            Eip8130AuthScheme::Secp256k1.default_data_len(),
            "the bare secp256k1 stub is the scheme's default length",
        );
        assert!(s.payer_auth().is_empty(), "no declared payer means no payer auth");
    }

    #[test]
    fn prefixed_p256_auth_selects_the_configured_path() {
        let tx = sim_tx(json!({
            "from": FROM,
            "calls": [],
            "senderAuth": blob(Some(Eip8130Contracts::P256_AUTHENTICATOR), 128),
        }));
        let s = signed(&tx);
        assert_eq!(s.tx().sender, Some(FROM), "a prefixed blob selects the configured path");
        let auth = s.sender_auth();
        assert_eq!(
            &auth[..20],
            Eip8130Contracts::P256_AUTHENTICATOR.as_slice(),
            "the caller's blob is priced verbatim, prefix intact",
        );
        assert_eq!(auth.len(), 20 + 128, "selector + supplied data length");
    }

    #[test]
    fn prefixed_webauthn_auth_selects_the_configured_path() {
        let tx = sim_tx(json!({
            "from": FROM,
            "calls": [],
            "senderAuth": blob(Some(Eip8130Contracts::WEBAUTHN_AUTHENTICATOR), 512),
        }));
        let s = signed(&tx);
        assert_eq!(s.tx().sender, Some(FROM));
        let auth = s.sender_auth();
        assert_eq!(&auth[..20], Eip8130Contracts::WEBAUTHN_AUTHENTICATOR.as_slice());
        assert_eq!(auth.len(), 20 + 512, "the WebAuthn blob is priced at its supplied size");
    }

    #[test]
    fn prefixed_k1_auth_selects_the_configured_path() {
        // A k1-prefixed blob is the configured-account path (`sender` set),
        // distinct from a bare EOA k1 signature — the 20-byte selector is what
        // distinguishes them.
        let tx = sim_tx(json!({
            "from": FROM,
            "calls": [],
            "senderAuth": blob(Some(Eip8130Constants::K1_AUTHENTICATOR), 65),
        }));
        let s = signed(&tx);
        assert_eq!(s.tx().sender, Some(FROM), "a k1-prefixed blob selects the configured path");
        assert_eq!(s.sender_auth().len(), 20 + 65);
    }

    #[test]
    fn bare_auth_stays_on_the_eoa_path() {
        // An unprefixed blob is the default-EOA bare-signature path (`sender`
        // unset), priced verbatim.
        let tx = sim_tx(json!({
            "from": FROM,
            "calls": [],
            "senderAuth": blob(None, 65),
        }));
        let s = signed(&tx);
        assert!(s.tx().sender.is_none(), "an unprefixed blob stays on the EOA path");
        assert_eq!(s.sender_auth().len(), 65, "the bare blob is priced verbatim");
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
        // On the configured (prefixed) path the 20-byte selector is excluded from
        // the cap, so `MAX_AUTH_SIZE` data bytes are honoured (total = selector +
        // data).
        let tx = sim_tx(json!({
            "from": FROM,
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
    fn bare_sender_auth_data_at_the_cap_is_accepted() {
        // On the EOA path there is no selector, so the whole blob is the data.
        let tx = sim_tx(json!({
            "from": FROM,
            "calls": [],
            "senderAuth": blob(None, MAX_AUTH_SIZE as usize),
        }));
        assert_eq!(signed(&tx).sender_auth().len(), MAX_AUTH_SIZE as usize);
    }

    #[test]
    fn oversize_sender_auth_data_is_rejected() {
        let req: BaseTransactionRequest = serde_json::from_value(json!({
            "from": FROM,
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
    fn oversize_bare_sender_auth_is_rejected() {
        // A bare blob has no selector, so its whole length is capped at
        // MAX_AUTH_SIZE (no 20-byte headroom).
        let req: BaseTransactionRequest = serde_json::from_value(json!({
            "from": FROM,
            "calls": [],
            "senderAuth": blob(None, MAX_AUTH_SIZE as usize + 1),
        }))
        .expect("valid request");
        assert!(req.to_eip8130_simulation_tx(CHAIN_ID, GAS_CAP).is_none());
    }

    #[test]
    fn oversize_payer_auth_is_rejected() {
        let payer = address!("0x00000000000000000000000000000000000000b2");
        let req: BaseTransactionRequest = serde_json::from_value(json!({
            "from": FROM,
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
            "from": FROM,
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
            "from": FROM,
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
        let tx = sim_tx(json!({ "from": FROM, "calls": [], "payer": payer }));
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
