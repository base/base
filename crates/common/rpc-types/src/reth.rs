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

/// Upper bound (in bytes) on a caller-declared authentication-payload size
/// (`sender_auth_size` / `payer_auth_size`). Real authenticator payloads are at
/// most a few hundred bytes (e.g. a `WebAuthn` assertion with its client-data
/// JSON), so 8 `KiB` is generous. Without a cap an attacker could declare
/// `u32::MAX` (~4 `GiB`) and OOM the node via the `vec![STUB_AUTH_FILL; len]`
/// allocation before any gas estimation runs; an over-cap value is rejected
/// (surfaced as `INVALID_PARAMS`) rather than allocated.
const MAX_AUTH_SIZE: u32 = 8_192;

impl BaseTransactionRequest {
    /// Builds the unsigned simulation transaction for an EIP-8130
    /// `eth_estimateGas` / `eth_call` request, or `None` when the request
    /// carries no EIP-8130 fields or omits the required `from` sender.
    ///
    /// Estimation runs without a signature. The caller declares the
    /// authentication *scheme* (and, for sponsored transactions, the `payer`);
    /// this synthesizes a correctly-shaped stub authentication blob so the
    /// intrinsic-gas schedule prices that scheme's authentication gas (the
    /// authenticator's execution gas plus the EIP-2028 calldata cost of its
    /// payload). The blob is never recovered —
    /// [`base_common_evm::Eip8130Executor::simulate`] simulates from `from`
    /// without verification. `gas_limit_cap` bounds execution when the request
    /// omits `gas`.
    ///
    /// - A `secp256k1` (or absent) sender scheme prices the default-EOA
    ///   bare-signature path (`sender` unset).
    /// - A `p256` / `webauthn` sender scheme prices the configured-account
    ///   authenticator path (`sender` set to `from`, blob = `authenticator(20)
    ///   || data`).
    /// - A declared `payer` adds payer authentication, priced from the payer's
    ///   scheme.
    ///
    /// `from` is mandatory for EIP-8130: the sender identity drives actor
    /// resolution, policy lookup, and auto-delegation, so a missing `from`
    /// returns `None` (surfaced as `INVALID_PARAMS`) rather than silently
    /// falling back to the zero address.
    ///
    /// A declared `sender_auth_size` / `payer_auth_size` exceeding
    /// [`MAX_AUTH_SIZE`] also returns `None` rather than allocating a stub blob
    /// of attacker-controlled size.
    pub fn to_eip8130_simulation_tx(
        &self,
        chain_id: u64,
        gas_limit_cap: u64,
    ) -> Option<BaseRevm<TxEnv>> {
        let aa = self.as_eip8130()?;
        let req = self.as_ref();
        let from = req.from?;

        // Sender authentication. A declared P-256/WebAuthn scheme prices the
        // configured-account path (`sender` set, prefixed `authenticator || data`
        // blob); absent or secp256k1 prices the default-EOA bare-signature path.
        let (sender, sender_auth) = match aa.sender_auth_scheme {
            None | Some(Eip8130AuthScheme::Secp256k1) => {
                let len = Self::auth_data_len(aa.sender_auth_size, Eip8130AuthScheme::Secp256k1)?;
                (None, Bytes::from(vec![STUB_AUTH_FILL; len]))
            }
            Some(scheme) => {
                let len = Self::auth_data_len(aa.sender_auth_size, scheme)?;
                (Some(from), Self::stub_prefixed_auth(scheme, len))
            }
        };

        // Sponsored payer authentication, priced only when a payer is declared.
        // The payer auth is always a prefixed `authenticator || data` blob.
        let (payer, payer_auth) = match aa.payer {
            None => (None, Bytes::new()),
            Some(payer) => {
                let scheme = aa.payer_auth_scheme.unwrap_or(Eip8130AuthScheme::Secp256k1);
                let len = Self::auth_data_len(aa.payer_auth_size, scheme)?;
                (Some(payer), Self::stub_prefixed_auth(scheme, len))
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

    /// Resolves the authentication-data byte length: an explicit request size,
    /// else the scheme's representative default.
    ///
    /// Returns `None` (surfaced to the caller as `INVALID_PARAMS`) when an
    /// explicit size exceeds [`MAX_AUTH_SIZE`], so an attacker cannot drive a
    /// multi-gigabyte stub allocation by declaring `u32::MAX`.
    const fn auth_data_len(size: Option<u32>, scheme: Eip8130AuthScheme) -> Option<usize> {
        match size {
            None => Some(scheme.default_data_len()),
            Some(s) if s <= MAX_AUTH_SIZE => Some(s as usize),
            Some(_) => None,
        }
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

    #[test]
    fn default_scheme_builds_bare_secp256k1_sender_auth() {
        let tx = sim_tx(json!({ "from": FROM, "calls": [] }));
        let s = signed(&tx);
        assert!(s.tx().sender.is_none(), "the secp256k1 path uses the default-EOA bare form");
        assert_eq!(
            s.sender_auth().len(),
            Eip8130AuthScheme::Secp256k1.default_data_len(),
            "the bare secp256k1 stub is the scheme's default length",
        );
        assert!(s.payer_auth().is_empty(), "no declared payer means no payer auth");
    }

    #[test]
    fn p256_scheme_builds_prefixed_configured_sender_auth() {
        let tx = sim_tx(json!({ "from": FROM, "calls": [], "senderAuthScheme": "p256" }));
        let s = signed(&tx);
        assert_eq!(s.tx().sender, Some(FROM), "a configured scheme sets the sender");
        let auth = s.sender_auth();
        assert_eq!(
            &auth[..20],
            Eip8130Contracts::P256_AUTHENTICATOR.as_slice(),
            "the blob is prefixed with the P-256 authenticator selector",
        );
        assert_eq!(
            auth.len(),
            20 + Eip8130AuthScheme::P256.default_data_len(),
            "selector + the scheme's default data length",
        );
    }

    #[test]
    fn webauthn_scheme_honours_explicit_size() {
        let tx = sim_tx(json!({
            "from": FROM,
            "calls": [],
            "senderAuthScheme": "webAuthn",
            "senderAuthSize": 512,
        }));
        let s = signed(&tx);
        let auth = s.sender_auth();
        assert_eq!(&auth[..20], Eip8130Contracts::WEBAUTHN_AUTHENTICATOR.as_slice());
        assert_eq!(auth.len(), 20 + 512, "the WebAuthn stub honours the requested size");
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
    fn auth_size_at_the_cap_is_accepted() {
        let tx = sim_tx(json!({
            "from": FROM,
            "calls": [],
            "senderAuthScheme": "webAuthn",
            "senderAuthSize": MAX_AUTH_SIZE,
        }));
        let auth = signed(&tx).sender_auth();
        assert_eq!(
            auth.len(),
            20 + MAX_AUTH_SIZE as usize,
            "a size at the cap is honoured (selector + data)",
        );
    }

    #[test]
    fn oversize_sender_auth_size_is_rejected() {
        let req: BaseTransactionRequest = serde_json::from_value(json!({
            "from": FROM,
            "calls": [],
            "senderAuthScheme": "webAuthn",
            "senderAuthSize": MAX_AUTH_SIZE + 1,
        }))
        .expect("valid request");
        assert!(
            req.to_eip8130_simulation_tx(CHAIN_ID, GAS_CAP).is_none(),
            "an over-cap sender auth size is rejected rather than allocated",
        );
    }

    #[test]
    fn oversize_payer_auth_size_is_rejected() {
        let payer = address!("0x00000000000000000000000000000000000000b2");
        let req: BaseTransactionRequest = serde_json::from_value(json!({
            "from": FROM,
            "calls": [],
            "payer": payer,
            "payerAuthScheme": "p256",
            "payerAuthSize": u32::MAX,
        }))
        .expect("valid request");
        assert!(
            req.to_eip8130_simulation_tx(CHAIN_ID, GAS_CAP).is_none(),
            "an over-cap payer auth size is rejected rather than allocated",
        );
    }

    #[test]
    fn declared_payer_builds_prefixed_payer_auth() {
        let payer = address!("0x00000000000000000000000000000000000000b2");
        let tx = sim_tx(json!({
            "from": FROM,
            "calls": [],
            "payer": payer,
            "payerAuthScheme": "p256",
        }));
        let s = signed(&tx);
        assert_eq!(s.tx().payer, Some(payer), "the payer is set on the transaction");
        let auth = s.payer_auth();
        assert_eq!(&auth[..20], Eip8130Contracts::P256_AUTHENTICATOR.as_slice());
        assert_eq!(auth.len(), 20 + Eip8130AuthScheme::P256.default_data_len());
    }
}
