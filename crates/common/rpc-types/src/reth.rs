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
    /// - An absent `sender_auth` prices the default-EOA bare-signature path
    ///   (`sender` unset).
    /// - A `sender_auth` prefixed with a configured-account authenticator
    ///   (`authenticator(20) || data`) prices that configured-account path
    ///   (`sender` set to `from`).
    /// - A declared `payer` adds payer authentication, priced from `payer_auth`
    ///   (defaulting to a representative secp256k1 authorization).
    ///
    /// `from` is mandatory for EIP-8130: the sender identity drives actor
    /// resolution, policy lookup, and auto-delegation, so a missing `from`
    /// returns `None` (surfaced as `INVALID_PARAMS`) rather than silently
    /// falling back to the zero address.
    ///
    /// A `sender_auth` / `payer_auth` blob longer than the 20-byte authenticator
    /// selector plus [`MAX_AUTH_SIZE`] data bytes returns `None` (surfaced as
    /// `INVALID_PARAMS`) rather than pricing an unbounded payload.
    pub fn to_eip8130_simulation_tx(
        &self,
        chain_id: u64,
        gas_limit_cap: u64,
    ) -> Option<BaseRevm<TxEnv>> {
        let aa = self.as_eip8130()?;
        let req = self.as_ref();
        let from = req.from?;

        // Sender authentication, priced verbatim from the caller-supplied blob
        // (never verified). A blob prefixed with a configured-account
        // authenticator (P-256/WebAuthn) prices the configured-account path
        // (`sender` set); an absent or unprefixed blob prices the default-EOA
        // bare-signature path.
        let (sender, sender_auth) = match &aa.sender_auth {
            None => (None, Self::default_bare_auth()),
            Some(blob) => {
                Self::check_auth_len(blob)?;
                if Self::selects_configured_path(blob) {
                    (Some(from), blob.clone())
                } else {
                    (None, blob.clone())
                }
            }
        };

        // Sponsored payer authentication, priced only when a payer is declared.
        // The payer auth is always a prefixed `authenticator || data` blob.
        let (payer, payer_auth) = match aa.payer {
            None => (None, Bytes::new()),
            Some(payer) => {
                let blob = match &aa.payer_auth {
                    Some(blob) => {
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
    /// authentication blob longer than the 20-byte authenticator selector plus
    /// [`MAX_AUTH_SIZE`] data bytes, bounding the calldata the estimate prices.
    fn check_auth_len(blob: &Bytes) -> Option<()> {
        (blob.len() as u64 <= AUTHENTICATOR_SELECTOR_LEN as u64 + u64::from(MAX_AUTH_SIZE))
            .then_some(())
    }

    /// Whether a raw sender authentication blob selects the configured-account
    /// path (`sender` set): true when it is prefixed with a canonical
    /// configured-account authenticator (P-256 or `WebAuthn`). An absent or
    /// unprefixed blob is the default-EOA bare-signature path.
    ///
    /// A configured account authenticating with the k1 selector is priced on
    /// the (equivalent-cost) EOA path: the enshrined k1 execution gas and its
    /// cold account-state SLOAD are identical either way.
    fn selects_configured_path(blob: &Bytes) -> bool {
        if blob.len() < AUTHENTICATOR_SELECTOR_LEN {
            return false;
        }
        let prefix = Address::from_slice(&blob[..AUTHENTICATOR_SELECTOR_LEN]);
        prefix == Eip8130AuthScheme::P256.authenticator()
            || prefix == Eip8130AuthScheme::WebAuthn.authenticator()
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
    fn absent_sender_auth_builds_bare_secp256k1_stub() {
        let tx = sim_tx(json!({ "from": FROM, "calls": [] }));
        let s = signed(&tx);
        assert!(s.tx().sender.is_none(), "an absent sender auth uses the default-EOA bare form");
        assert_eq!(
            s.sender_auth().len(),
            Eip8130AuthScheme::Secp256k1.default_data_len(),
            "the bare secp256k1 stub is the scheme's default length",
        );
        assert!(s.payer_auth().is_empty(), "no declared payer means no payer auth");
    }

    #[test]
    fn p256_prefixed_sender_auth_selects_the_configured_path() {
        let tx = sim_tx(json!({
            "from": FROM,
            "calls": [],
            "senderAuth": blob(Some(Eip8130Contracts::P256_AUTHENTICATOR), 128),
        }));
        let s = signed(&tx);
        assert_eq!(s.tx().sender, Some(FROM), "a configured-authenticator blob sets the sender");
        let auth = s.sender_auth();
        assert_eq!(
            &auth[..20],
            Eip8130Contracts::P256_AUTHENTICATOR.as_slice(),
            "the caller's blob is priced verbatim, prefix intact",
        );
        assert_eq!(auth.len(), 20 + 128, "selector + supplied data length");
    }

    #[test]
    fn webauthn_prefixed_sender_auth_is_priced_verbatim() {
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
    fn k1_prefixed_sender_auth_stays_on_the_eoa_path() {
        // The k1 selector is not a configured-account authenticator, so the blob
        // prices the (equivalent-cost) default-EOA path with `sender` unset.
        let tx = sim_tx(json!({
            "from": FROM,
            "calls": [],
            "senderAuth": blob(Some(Eip8130Constants::K1_AUTHENTICATOR), 65),
        }));
        let s = signed(&tx);
        assert!(s.tx().sender.is_none(), "a k1-prefixed blob stays on the EOA path");
        assert_eq!(s.sender_auth().len(), 20 + 65, "the blob is still priced verbatim");
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
    fn sender_auth_at_the_cap_is_accepted() {
        let tx = sim_tx(json!({
            "from": FROM,
            "calls": [],
            "senderAuth": blob(Some(Eip8130Contracts::WEBAUTHN_AUTHENTICATOR), MAX_AUTH_SIZE as usize),
        }));
        let auth = signed(&tx).sender_auth();
        assert_eq!(
            auth.len(),
            20 + MAX_AUTH_SIZE as usize,
            "a blob at the cap is honoured (selector + data)",
        );
    }

    #[test]
    fn oversize_sender_auth_is_rejected() {
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
