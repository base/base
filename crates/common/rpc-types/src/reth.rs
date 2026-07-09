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
use base_common_consensus::{
    BaseTransactionInfo, BaseTxEnvelope, Eip8130Constants, Eip8130Contracts, Eip8130Signed,
    TxEip8130,
};
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
    /// carries no EIP-8130 fields or resolves no sender account.
    ///
    /// Estimation runs without a signature. The caller passes the raw
    /// authentication blob it intends to sign (`sender_auth`, and for sponsored
    /// transactions `payer_auth`); the intrinsic-gas schedule prices that blob's
    /// authentication gas (the authenticator's execution gas, selected by the
    /// leading 20-byte authenticator address, plus the EIP-2028 calldata cost of
    /// the whole blob). The blob is never recovered —
    /// [`base_common_evm::Eip8130Executor::simulate`] simulates from the account
    /// without verification. `gas_limit_cap` bounds execution when the request
    /// omits `gas`.
    ///
    /// # Account
    ///
    /// The sender account is `sender` (the EIP-8130 field) or the standard
    /// `from`, which must agree when both are present. A request carrying 8130
    /// fields but resolving neither, or one where `sender != from`, returns
    /// `None` (surfaced as `INVALID_PARAMS`) rather than silently falling back to
    /// the zero address — the sender identity drives actor resolution, policy
    /// lookup, and auto-delegation.
    ///
    /// # Path
    ///
    /// A supplied `sender_auth` blob's form selects the authentication path,
    /// mirroring the on-wire transaction:
    ///
    /// - A bare (unprefixed) blob prices the default-EOA path (`tx.sender`
    ///   unset), where the account authenticates with a k1 signature exactly as a
    ///   1559 transaction. Priced verbatim.
    /// - A blob prefixed with a recognized enshrined authenticator selector
    ///   (`authenticator(20) || data`) prices the configured-account path
    ///   (`tx.sender` set to the account). Priced verbatim, including a
    ///   [`base_common_consensus::Eip8130Contracts::DELEGATE_AUTHENTICATOR`]-prefixed
    ///   blob — its nested authenticator is resolved and priced by the same
    ///   intrinsic-gas schedule the verifying `execute` path uses.
    ///
    /// An absent `sender_auth` defaults by intent: a declared `sender`
    /// (configured-account request) synthesizes a k1-prefixed stub on the
    /// configured path, so a configured account isn't under-estimated as a bare
    /// EOA; a `from`-only request synthesizes a bare k1 stub on the default-EOA
    /// path.
    ///
    /// A declared `payer` adds payer authentication, priced from `payer_auth`
    /// (defaulting to a representative secp256k1 authorization). Unlike
    /// `sender_auth`, a supplied `payer_auth` is always the prefixed
    /// configured-account form and must carry a recognized enshrined
    /// authenticator selector.
    ///
    /// A `payer_auth` whose leading 20 bytes are not a recognized enshrined
    /// authenticator selector returns `None` (surfaced as `INVALID_PARAMS`)
    /// rather than pricing an authenticator the intrinsic-gas schedule doesn't
    /// recognize (which could under-price the estimate). A `sender_auth` /
    /// `payer_auth` blob whose data exceeds [`MAX_AUTH_SIZE`] bytes (excluding
    /// the 20-byte authenticator selector on the configured path) is rejected the
    /// same way, rather than pricing an unbounded payload.
    pub fn to_eip8130_simulation_tx(
        &self,
        chain_id: u64,
        gas_limit_cap: u64,
    ) -> Option<BaseRevm<TxEnv>> {
        let aa = self.as_eip8130()?;
        let req = self.as_ref();

        // Account identity: the 8130 `sender` field or the standard `from`, which
        // must agree when both are present. Neither → reject rather than default
        // to the zero address.
        let account = match (aa.sender, req.from) {
            (Some(sender), Some(from)) if sender != from => return None,
            (Some(sender), _) => sender,
            (None, Some(from)) => from,
            (None, None) => return None,
        };
        let sender_declared = aa.sender.is_some();

        // Path + cost follow the `sender_auth` blob's form (mirroring the wire):
        // a prefixed `authenticator(20) || data` blob is the configured-account
        // path (`tx.sender` set to the account); a bare blob is the default-EOA
        // path (`tx.sender` unset). The blob is priced verbatim, never verified.
        // An absent blob defaults by intent: a declared `sender` (configured
        // account) → a k1-prefixed stub on the configured path (so it isn't
        // under-estimated as a bare EOA); a `from`-only request → a bare k1 stub
        // on the EOA path.
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
        let mut sim_tx = BaseRevm::from_recovered_tx(&envelope, account);
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

    /// Whether an authentication blob is in the prefixed configured-account form
    /// (`authenticator(20) || data`) rather than a bare signature: true when its
    /// leading 20 bytes are a recognized enshrined authenticator selector,
    /// checked against the protocol's actual canonical authenticator set
    /// ([`Eip8130Contracts::is_canonical_authenticator`] plus the native k1
    /// sentinel) — the same set the block-validation path accepts — rather than
    /// [`Eip8130AuthScheme::ALL`], which only lists the RPC estimator's own
    /// flat, schedule-priceable *leaf* schemes and omits
    /// [`Eip8130Contracts::DELEGATE_AUTHENTICATOR`] (a recognized prefix, but a
    /// structured 3-segment blob rather than a flat leaf, so it can't be a
    /// [`Eip8130AuthScheme`] variant). This mirrors the wire form the
    /// intrinsic-gas schedule prices the blob under, so a `sender_auth` blob
    /// prefixed this way simulates on the configured-account path (`tx.sender`
    /// set) and a bare one on the default-EOA path (`tx.sender` unset). For
    /// `payer_auth` a `false` result is a rejection (payer auth is always the
    /// prefixed form).
    fn is_prefixed_auth(blob: &Bytes) -> bool {
        if blob.len() < AUTHENTICATOR_SELECTOR_LEN {
            return false;
        }
        let selector = Address::from_slice(&blob[..AUTHENTICATOR_SELECTOR_LEN]);
        selector == Eip8130Constants::K1_AUTHENTICATOR
            || Eip8130Contracts::is_canonical_authenticator(&selector)
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
    const FROM: Address = address!("0x00000000000000000000000000000000000000c3");

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
    fn sender_only_absent_auth_defaults_to_configured_k1() {
        // A declared `sender` with no auth blob is a configured account: the
        // absent blob defaults to a k1-prefixed stub on the configured path,
        // rather than being under-estimated as a bare EOA.
        let tx = sim_tx(json!({ "sender": SENDER, "calls": [] }));
        let s = signed(&tx);
        assert_eq!(s.tx().sender, Some(SENDER), "a declared sender sets the configured account");
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
    fn from_only_absent_auth_is_bare_eoa() {
        // A `from`-only request (no `sender`) with no auth blob prices the
        // default-EOA path: `tx.sender` unset, a bare k1 stub of the scheme's
        // default length.
        let tx = sim_tx(json!({ "from": FROM, "calls": [] }));
        let s = signed(&tx);
        assert!(s.tx().sender.is_none(), "a `from`-only absent-auth request is the EOA path");
        assert_eq!(
            s.sender_auth().len(),
            Eip8130AuthScheme::Secp256k1.default_data_len(),
            "the bare secp256k1 stub is the scheme's default length (no selector)",
        );
    }

    #[test]
    fn from_only_prefixed_auth_is_configured() {
        // `from` is interchangeable with `sender` as the account; a prefixed blob
        // selects the configured path regardless of which field named the account.
        let tx = sim_tx(json!({
            "from": FROM,
            "calls": [],
            "senderAuth": blob(Some(Eip8130Contracts::P256_AUTHENTICATOR), 128),
        }));
        assert_eq!(
            signed(&tx).tx().sender,
            Some(FROM),
            "a prefixed blob selects the configured path"
        );
    }

    #[test]
    fn bare_sender_auth_is_the_eoa_path() {
        // A supplied unprefixed (bare) blob is the default-EOA path, priced
        // verbatim — the blob's form wins even when `sender` named the account.
        let tx = sim_tx(json!({
            "sender": SENDER,
            "calls": [],
            "senderAuth": blob(None, 65),
        }));
        let s = signed(&tx);
        assert!(s.tx().sender.is_none(), "an unprefixed blob stays on the EOA path");
        assert_eq!(s.sender_auth().len(), 65, "the bare blob is priced verbatim");
    }

    #[test]
    fn unrecognized_sender_auth_prefix_is_treated_as_bare_eoa() {
        // An unrecognized 20-byte prefix is not an enshrined authenticator, so the
        // blob is treated as a bare signature (EOA path) and priced verbatim by
        // length — it cannot under-price (no authenticator execution gas applies
        // to the bare path).
        let unrecognized = address!("0x000000000000000000000000000000000000dead");
        let tx = sim_tx(json!({
            "from": FROM,
            "calls": [],
            "senderAuth": blob(Some(unrecognized), 65),
        }));
        let s = signed(&tx);
        assert!(s.tx().sender.is_none(), "an unrecognized prefix falls to the EOA path");
        assert_eq!(s.sender_auth().len(), 20 + 65, "priced verbatim as a bare blob");
    }

    #[test]
    fn delegate_prefixed_sender_auth_is_the_configured_path() {
        // `DELEGATE_AUTHENTICATOR` is a recognized prefix even though it isn't
        // an `Eip8130AuthScheme` variant (it's a structured 3-segment blob, not
        // a flat leaf) — `is_prefixed_auth` must still select the
        // configured-account path for it, so a delegate-authenticated sender
        // isn't misclassified as a bare EOA and flat-priced at k1.
        let delegate_account = address!("0x00000000000000000000000000000000000000d4");
        let mut nested = Eip8130Constants::K1_AUTHENTICATOR.to_vec();
        nested.extend_from_slice(&[STUB_AUTH_FILL; 65]);
        let mut blob = Eip8130Contracts::DELEGATE_AUTHENTICATOR.to_vec();
        blob.extend_from_slice(delegate_account.as_slice());
        blob.extend_from_slice(&nested);
        let tx = sim_tx(json!({
            "sender": SENDER,
            "calls": [],
            "senderAuth": alloy_primitives::hex::encode_prefixed(&blob),
        }));
        let s = signed(&tx);
        assert_eq!(
            s.tx().sender,
            Some(SENDER),
            "a delegate-prefixed blob selects the configured-account path",
        );
        assert_eq!(s.sender_auth().as_ref(), blob.as_slice(), "priced verbatim");
    }

    #[test]
    fn delegate_prefixed_payer_auth_is_accepted() {
        // Mirrors the sender-side case: a delegate-authenticated payer is a
        // recognized prefix and must not be rejected as an unrecognized
        // authenticator selector.
        let payer = address!("0x00000000000000000000000000000000000000b2");
        let delegate_account = address!("0x00000000000000000000000000000000000000d4");
        let mut nested = Eip8130Contracts::P256_AUTHENTICATOR.to_vec();
        nested.extend_from_slice(&[STUB_AUTH_FILL; 128]);
        let mut blob = Eip8130Contracts::DELEGATE_AUTHENTICATOR.to_vec();
        blob.extend_from_slice(delegate_account.as_slice());
        blob.extend_from_slice(&nested);
        let tx = sim_tx(json!({
            "sender": SENDER,
            "calls": [],
            "payer": payer,
            "payerAuth": alloy_primitives::hex::encode_prefixed(&blob),
        }));
        let s = signed(&tx);
        assert_eq!(s.tx().payer, Some(payer));
        assert_eq!(s.payer_auth().as_ref(), blob.as_slice(), "priced verbatim");
    }

    #[test]
    fn from_and_sender_mismatch_is_rejected() {
        let req: BaseTransactionRequest = serde_json::from_value(json!({
            "from": FROM,
            "sender": SENDER,
            "calls": [],
        }))
        .expect("valid request");
        assert!(
            req.to_eip8130_simulation_tx(CHAIN_ID, GAS_CAP).is_none(),
            "a `from`/`sender` mismatch is rejected rather than guessing the account",
        );
    }

    #[test]
    fn from_and_matching_sender_is_configured() {
        // Both present and equal is valid; a declared `sender` means the absent
        // blob defaults to the configured k1 stub, not the bare EOA form.
        let tx = sim_tx(json!({ "from": SENDER, "sender": SENDER, "calls": [] }));
        let s = signed(&tx);
        assert_eq!(s.tx().sender, Some(SENDER), "matching `from`/`sender` resolves the account");
        assert_eq!(
            &s.sender_auth()[..20],
            Eip8130Constants::K1_AUTHENTICATOR.as_slice(),
            "a declared sender defaults the absent blob to configured k1",
        );
    }

    #[test]
    fn no_account_is_rejected() {
        // A request with 8130 fields but neither `from` nor `sender` is rejected
        // rather than defaulting the account to the zero address.
        let req: BaseTransactionRequest =
            serde_json::from_value(json!({ "calls": [] })).expect("valid request");
        assert!(
            req.to_eip8130_simulation_tx(CHAIN_ID, GAS_CAP).is_none(),
            "an 8130 request with no account is rejected",
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
    fn oversize_bare_sender_auth_is_rejected() {
        // A bare blob has no selector, so its whole length is capped at
        // `MAX_AUTH_SIZE` (no 20-byte headroom).
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
