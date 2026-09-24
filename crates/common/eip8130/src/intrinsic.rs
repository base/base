//! EIP-8130 intrinsic gas: the total cost to include an AA transaction.

use alloy_primitives::Address;
use base_common_consensus::{AccountChange, Eip8130Constants, Eip8130Signed};

use crate::Eip8130GasSchedule;

/// Reason intrinsic gas cannot be computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IntrinsicGasError {
    /// A transaction authenticator has no execution-gas entry in the schedule.
    /// On the launch wire the only enshrined authenticator is the native
    /// secp256k1 sentinel, so this fires for any non-k1 authenticator selector
    /// naming a sender or payer — surfacing the rejection here (as an
    /// undercharge guard) exactly as dispatch rejects it upstream.
    #[error("no gas-schedule entry for authenticator {0}")]
    UnscheduledAuthenticator(Address),
    /// `payer_auth_cost` exceeds [`Eip8130GasSchedule::MAX_AUTHENTICATION_GAS`].
    #[error("payer authentication gas {0} exceeds MAX_AUTHENTICATION_GAS")]
    PayerAuthGasExceeded(u64),
}

/// Wire encoding of an authentication blob, selecting how it is parsed and
/// priced. This is the encoding shape, not the account type: an empty-`sender`
/// (default-EOA) owner is [`Self::BareSignature`] on the `sender_auth` path but
/// [`Self::Prefixed`] when it names itself as `K1_AUTHENTICATOR || sig`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthWireForm {
    /// A raw 65-byte secp256k1 signature with no authenticator prefix: the
    /// empty-`sender` (default-EOA) path, priced as a k1 authentication over a
    /// single account-state SLOAD.
    BareSignature,
    /// An `authenticator(20) || data` blob: a configured sender and every payer.
    Prefixed,
}

impl AuthWireForm {
    /// The wire form of a transaction's `sender_auth`: a bare signature on the
    /// empty-`sender` (EOA) path, otherwise an `authenticator || data` blob.
    #[must_use]
    pub const fn for_sender(sender: Option<Address>) -> Self {
        match sender {
            Some(_) => Self::Prefixed,
            None => Self::BareSignature,
        }
    }
}

/// State-derived inputs the transaction body alone cannot determine.
///
/// These flags come from the caller's state view (the nonce manager and the
/// resolved sender), supplied so this crate stays a pure function of the
/// transaction plus these hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct IntrinsicGasInput {
    /// The resolved sender. Calls back to the sender carry no
    /// `value_transfer_cost`.
    pub sender: Address,
    /// Whether this transaction's sequence nonce channel is being used for the
    /// first time (its current nonce is zero) — selects the SSTORE *set* cost
    /// over the *reset* cost. Ignored for nonce-free (`NONCE_KEY_MAX`)
    /// transactions.
    pub nonce_key_first_use: bool,
}

impl IntrinsicGasInput {
    /// Creates the intrinsic-gas state hints.
    #[must_use]
    pub const fn new(sender: Address, nonce_key_first_use: bool) -> Self {
        Self { sender, nonce_key_first_use }
    }

    /// Safe-ceiling input shared by the estimation (`eth_estimateGas` /
    /// `eth_call`) and mempool-admission paths.
    ///
    /// Intrinsic gas is now fully body-derivable (no state-dependent revoke
    /// discount), so this equals [`Self::new`]; it is retained as the shared
    /// entry point both estimation and admission call so the two cannot drift
    /// apart.
    #[must_use]
    pub const fn worst_case(sender: Address, nonce_key_first_use: bool) -> Self {
        Self::new(sender, nonce_key_first_use)
    }
}

/// The EIP-8130 intrinsic-gas breakdown, one field per spec component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct IntrinsicGas {
    /// `AA_BASE_COST`.
    pub base: u64,
    /// `tx_payload_cost` — EIP-2028 data-availability cost over the encoding
    /// with an empty `payer_auth` (the *standard* per-token rate). Subject to
    /// the EIP-7623 calldata floor; see [`Self::payload_floor`] and
    /// [`Self::sender_floor`].
    pub payload: u64,
    /// The EIP-7623 calldata *floor* over the same bytes [`Self::payload`]
    /// covers: `TX_TOTAL_COST_FLOOR_PER_TOKEN` (10) per payload token instead of
    /// the standard `TX_DATA_ZERO_BYTE` (4). Always `>= payload`. Not part of
    /// [`Self::total`]; it only raises the sender's metered gas via
    /// [`Self::sender_floor`] when a data-heavy transaction's execution is cheap.
    pub payload_floor: u64,
    /// `nonce_key_cost`.
    pub nonce_key: u64,
    /// `value_transfer_cost` — [`Eip8130GasSchedule::TX_VALUE_COST`] per call
    /// with `value > 0` and `to != sender`.
    pub value_transfer: u64,
    /// `account_changes_cost` — delegation entries
    /// ([`Eip8130GasSchedule::DELEGATION_DEPOSIT_COST`] each).
    pub account_changes: u64,
    /// `sender_auth_cost` — sender authenticator execution + its account-state
    /// SLOAD (k1 = 5100).
    pub sender_auth: u64,
    /// `payer_auth_cost` — payer authenticator execution + its account-state
    /// SLOAD + the data cost of the `payer_auth` bytes, or `0` for self-pay.
    pub payer_auth: u64,
}

impl IntrinsicGas {
    /// Total intrinsic gas (all components).
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.base
            .saturating_add(self.payload)
            .saturating_add(self.nonce_key)
            .saturating_add(self.value_transfer)
            .saturating_add(self.account_changes)
            .saturating_add(self.sender_auth)
            .saturating_add(self.payer_auth)
    }

    /// Sender-intrinsic gas: intrinsic gas excluding `payer_auth_cost`, which is
    /// the portion bounded by `gas_limit` (payer authentication is metered on
    /// top of `gas_limit`).
    #[must_use]
    pub const fn sender_intrinsic(&self) -> u64 {
        self.total().saturating_sub(self.payer_auth)
    }

    /// Gas available to `calls` after sender-intrinsic gas, or `None` when
    /// sender-intrinsic gas alone exceeds `gas_limit` (the transaction is
    /// underfunded and cannot be included).
    ///
    /// The EIP-7623 calldata floor ([`Self::sender_floor`]) does not reduce this
    /// budget — it is a post-execution minimum spend, not an intrinsic cost — so
    /// the gas available to `calls` is still `gas_limit - sender_intrinsic`. The
    /// floor is enforced separately as a lower bound on `gas_limit` and on the
    /// settled charge.
    #[must_use]
    pub const fn execution_gas_available(&self, gas_limit: u64) -> Option<u64> {
        gas_limit.checked_sub(self.sender_intrinsic())
    }

    /// The EIP-7623 sender-side floor: the minimum sender gas a transaction is
    /// metered, regardless of how little its `calls` execute. Mirrors EIP-7623's
    /// floor branch by swapping the standard `tx_payload_cost` for the higher
    /// per-token floor over the same bytes:
    ///
    /// ```text
    /// sender_floor = (sender_intrinsic - payload) + payload_floor
    /// ```
    ///
    /// Always `>= sender_intrinsic` (since `payload_floor >= payload`), so a
    /// transaction that clears the floor also clears sender-intrinsic gas. A
    /// `gas_limit` below this is invalid at mempool acceptance, and settlement
    /// charges at least this for the sender portion.
    #[must_use]
    pub const fn sender_floor(&self) -> u64 {
        self.sender_intrinsic().saturating_sub(self.payload).saturating_add(self.payload_floor)
    }

    /// Computes the intrinsic gas for a signed EIP-8130 transaction.
    ///
    /// `encoded` is the EIP-2718-serialized signed transaction
    /// (`type_byte || rlp([..fields.., sender_auth, payer_auth])`) — the same
    /// bytes used for networking and the transaction hash. It is taken as a
    /// parameter, rather than re-serialized here, because `compute` runs for
    /// every transaction on both the mempool-admission and block-building paths,
    /// where the caller already holds the serialized form; it feeds only the
    /// EIP-2028 `payload` cost. A sponsored transaction is priced as if
    /// `payer_auth` were empty, since the payer's bytes are billed in
    /// `payer_auth` rather than against the sender's `gas_limit`. That cost is
    /// folded from `encoded` rather than by re-serializing the body.
    ///
    /// Returns [`IntrinsicGasError::UnscheduledAuthenticator`] if any sender or
    /// payer authenticator lacks a gas-schedule entry, and
    /// [`IntrinsicGasError::PayerAuthGasExceeded`] if `payer_auth_cost` exceeds
    /// [`Eip8130GasSchedule::MAX_AUTHENTICATION_GAS`].
    #[must_use = "discarding the result silently skips the entire intrinsic-gas computation"]
    pub fn compute(
        signed: &Eip8130Signed,
        encoded: &[u8],
        input: &IntrinsicGasInput,
    ) -> Result<Self, IntrinsicGasError> {
        let tx = signed.tx();

        let nonce_key = if tx.nonce_key == Eip8130Constants::NONCE_KEY_MAX {
            Eip8130GasSchedule::NONCE_FREE_COST
        } else if input.nonce_key_first_use {
            Eip8130GasSchedule::NONCE_KEY_FIRST_USE_COST
        } else {
            Eip8130GasSchedule::NONCE_KEY_EXISTING_COST
        };

        let mut account_changes = 0u64;
        for change in &tx.account_changes {
            match change {
                AccountChange::Delegation(_) => {
                    account_changes =
                        account_changes.saturating_add(Eip8130GasSchedule::DELEGATION_DEPOSIT_COST);
                }
            }
        }

        // Only the empty-`sender` path (`sender == None`) is a bare 65-byte
        // signature parsed via native ecrecover; a configured sender (and every
        // payer) is a `K1_AUTHENTICATOR || sig` blob.
        let sender_auth =
            Self::auth_cost(signed.sender_auth().as_ref(), AuthWireForm::for_sender(tx.sender))?;
        let payer_auth = Self::max_payer_auth_cost(signed)?;

        let value_calls = tx
            .calls
            .iter()
            .flatten()
            .filter(|call| !call.value.is_zero() && call.to != input.sender)
            .count();
        let value_transfer = Eip8130GasSchedule::TX_VALUE_COST
            .saturating_mul(u64::try_from(value_calls).unwrap_or(u64::MAX));

        // `payload` and its EIP-7623 floor are priced over the sender-billed
        // bytes: the encoding with an empty `payer_auth`. `payer_auth` bytes are
        // metered separately (outside the sender's `gas_limit`). The fold
        // rewrites the list header in place instead of re-encoding the body.
        let (payload, payload_floor) =
            signed.fold_sender_billed_bytes(encoded, (0u64, 0u64), |costs, byte| {
                let (standard, floor) = Self::byte_payload_costs(byte);
                (costs.0.saturating_add(standard), costs.1.saturating_add(floor))
            });

        Ok(Self {
            base: Eip8130GasSchedule::AA_BASE_COST,
            payload,
            payload_floor,
            nonce_key,
            value_transfer,
            account_changes,
            sender_auth,
            payer_auth,
        })
    }

    /// Payer-authentication gas billed *on top of* `gas_limit` for a signed
    /// EIP-8130 transaction (`0` for self-pay).
    ///
    /// Block gas reservation uses this to budget the payer's authentication in
    /// addition to the sender-signed `gas_limit`: the payer reimburses its own
    /// authentication beyond that limit, so a block admitting a transaction on
    /// `gas_limit` alone could let true consumption push cumulative gas over the
    /// block limit. The charge is fully determined by the auth blob (authenticator
    /// execution gas, its cold account-state SLOAD, and the data cost of its
    /// bytes), so building and validation share this exact bound.
    #[must_use = "discarding the result skips the payer-authentication reservation"]
    pub fn max_payer_auth_cost(signed: &Eip8130Signed) -> Result<u64, IntrinsicGasError> {
        if signed.tx().payer.is_none() {
            return Ok(0);
        }
        let payer_auth = signed.payer_auth().as_ref();
        let form = if signed.tx().is_open_payer() {
            AuthWireForm::BareSignature
        } else {
            AuthWireForm::Prefixed
        };
        let cost = Self::auth_cost(payer_auth, form)?.saturating_add(Self::data_cost(payer_auth));
        if cost > Eip8130GasSchedule::MAX_AUTHENTICATION_GAS {
            return Err(IntrinsicGasError::PayerAuthGasExceeded(cost));
        }
        Ok(cost)
    }

    /// Standard EIP-2028 cost and EIP-7623 floor cost of one payload byte.
    ///
    /// A zero byte is one token (`TX_DATA_ZERO_BYTE` / `TX_TOTAL_COST_FLOOR_PER_TOKEN`).
    /// A non-zero byte is four tokens.
    const fn byte_payload_costs(byte: u8) -> (u64, u64) {
        if byte == 0 {
            (
                Eip8130GasSchedule::TX_DATA_ZERO_BYTE,
                Eip8130GasSchedule::TX_TOTAL_COST_FLOOR_PER_TOKEN,
            )
        } else {
            (
                Eip8130GasSchedule::TX_DATA_NONZERO_BYTE,
                Eip8130GasSchedule::TX_TOTAL_COST_FLOOR_PER_TOKEN.saturating_mul(4),
            )
        }
    }

    /// EIP-2028 data cost of `bytes` (the standard per-byte rate, no floor).
    fn data_cost(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0u64, |acc, &byte| acc.saturating_add(Self::byte_payload_costs(byte).0))
    }

    /// Cost of authenticating one auth blob: authenticator execution gas plus the
    /// cold account-state SLOAD the authentication reads.
    ///
    /// Even though the pure-secp256k1 flow no longer reads any account-config
    /// storage, the intrinsic schedule still charges one `COLD_SLOAD` (2100) for
    /// the enshrined account-state read per k1 authentication, so removing the
    /// Keystore does not change gas for any transaction. Combined with
    /// [`Eip8130GasSchedule::AUTH_EXEC_K1`] (3000, `ecrecover`) this fixes k1
    /// authentication at `K1_AUTH_COST = 5100`.
    ///
    /// `form` selects how the blob is parsed:
    /// [`AuthWireForm::BareSignature`] is a raw 65-byte secp256k1 signature with
    /// no authenticator prefix (the empty-`sender` path); [`AuthWireForm::Prefixed`]
    /// is a `K1_AUTHENTICATOR(20) || data` blob (a configured sender and every
    /// payer).
    fn auth_cost(auth: &[u8], form: AuthWireForm) -> Result<u64, IntrinsicGasError> {
        let exec = Self::auth_exec_cost(auth, form)?;
        let sloads = Self::auth_sloads(auth, form, exec);
        Ok(exec.saturating_add(Eip8130GasSchedule::COLD_SLOAD.saturating_mul(sloads)))
    }

    /// Number of cold account-state SLOADs charged for one authentication.
    ///
    /// - **Bare signature** (default-EOA wire form): one account-state SLOAD.
    /// - **A resolved `K1_AUTHENTICATOR`**: one cold SLOAD.
    /// - **A degenerate sub-20-byte prefixed blob** resolves no authenticator and
    ///   reads no slot, so it costs `0` rather than a phantom SLOAD. Such blobs
    ///   are unreachable here (admission rejects them upstream); guarding keeps the
    ///   SLOAD tied to a real read.
    fn auth_sloads(auth: &[u8], form: AuthWireForm, exec: u64) -> u64 {
        if matches!(form, AuthWireForm::BareSignature) {
            return 1;
        }
        match Self::authenticator_of(auth) {
            Some(_) if exec > 0 => 1,
            _ => 0,
        }
    }

    /// Authenticator *execution* gas for an auth blob. On the launch wire the
    /// only enshrined authenticator is the native secp256k1 sentinel.
    fn auth_exec_cost(auth: &[u8], form: AuthWireForm) -> Result<u64, IntrinsicGasError> {
        if matches!(form, AuthWireForm::BareSignature) {
            return Ok(Eip8130GasSchedule::AUTH_EXEC_K1);
        }
        let Some(authenticator) = Self::authenticator_of(auth) else {
            return Ok(0);
        };
        Eip8130GasSchedule::leaf_auth_exec_gas(authenticator)
            .ok_or(IntrinsicGasError::UnscheduledAuthenticator(authenticator))
    }

    /// The authenticator address at the head of a configured-actor auth blob, or
    /// `None` when the blob is too short to carry one.
    fn authenticator_of(auth: &[u8]) -> Option<Address> {
        (auth.len() >= 20).then(|| Address::from_slice(&auth[..20]))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes, U256, address};
    use base_common_consensus::{AccountChange, Call, Delegation, TxEip8130};

    use super::*;

    const ACCOUNT: Address = address!("0x1111111111111111111111111111111111111111");
    const K1: Address = Eip8130Constants::K1_AUTHENTICATOR;
    const EXISTING_KEY: IntrinsicGasInput = IntrinsicGasInput::new(ACCOUNT, false);

    fn signed(tx: TxEip8130, sender_auth: Vec<u8>, payer_auth: Vec<u8>) -> Eip8130Signed {
        Eip8130Signed::new(tx, Bytes::from(sender_auth), Bytes::from(payer_auth))
    }

    /// `authenticator(20) || dummy data`.
    fn configured_auth(authenticator: Address) -> Vec<u8> {
        let mut blob = authenticator.to_vec();
        blob.extend_from_slice(&[0xab; 65]);
        blob
    }

    fn encode(signed: &Eip8130Signed) -> Vec<u8> {
        let mut encoded = vec![Eip8130Constants::EIP8130_TX_TYPE];
        signed.rlp_encode_signed(&mut encoded);
        encoded
    }

    /// Serializes `signed` (EIP-2718) and computes intrinsic gas, mirroring a
    /// caller that already holds the transaction's network encoding.
    fn intrinsic(signed: &Eip8130Signed, input: &IntrinsicGasInput) -> IntrinsicGas {
        IntrinsicGas::compute(signed, &encode(signed), input)
            .expect("k1 authenticator is scheduled")
    }

    #[test]
    fn eoa_self_pay_minimal() {
        // sender == None (EOA), key 0 existing, no payer, no account changes.
        let tx = TxEip8130::default();
        let gas = intrinsic(&signed(tx, vec![0xcd; 65], vec![]), &EXISTING_KEY);

        assert_eq!(gas.base, Eip8130GasSchedule::AA_BASE_COST);
        assert_eq!(gas.nonce_key, Eip8130GasSchedule::NONCE_KEY_EXISTING_COST);
        assert_eq!(gas.account_changes, 0);
        // native k1 exec + 1 cold SLOAD == 5100.
        assert_eq!(
            gas.sender_auth,
            Eip8130GasSchedule::AUTH_EXEC_K1 + Eip8130GasSchedule::COLD_SLOAD
        );
        assert_eq!(gas.sender_auth, 5_100);
        assert_eq!(gas.payer_auth, 0);
        assert!(gas.payload > 0);
        // self-pay: sender-intrinsic equals total.
        assert_eq!(gas.sender_intrinsic(), gas.total());
    }

    #[test]
    fn k1_authentication_costs_a_single_sload() {
        // A bare signature and an explicit `K1_AUTHENTICATOR` blob both resolve in
        // one cold SLOAD, fixing k1 authentication at 5100.
        assert_eq!(
            IntrinsicGas::auth_cost(&[0u8; 65], AuthWireForm::BareSignature),
            Ok(Eip8130GasSchedule::AUTH_EXEC_K1 + Eip8130GasSchedule::COLD_SLOAD)
        );
        assert_eq!(
            IntrinsicGas::auth_cost(&configured_auth(K1), AuthWireForm::Prefixed),
            Ok(Eip8130GasSchedule::AUTH_EXEC_K1 + Eip8130GasSchedule::COLD_SLOAD)
        );
    }

    #[test]
    fn degenerate_short_auth_blob_charges_no_sload() {
        // A sub-20-byte prefixed (non-bare) blob resolves no authenticator, so it
        // reads no slot and must cost 0 (not a phantom cold SLOAD).
        assert_eq!(IntrinsicGas::auth_cost(&[0u8; 5], AuthWireForm::Prefixed), Ok(0));
        assert_eq!(
            IntrinsicGas::auth_cost(&[0u8; 65], AuthWireForm::BareSignature),
            Ok(Eip8130GasSchedule::AUTH_EXEC_K1 + Eip8130GasSchedule::COLD_SLOAD)
        );
    }

    #[test]
    fn non_k1_authenticator_is_unscheduled() {
        // Any non-k1 selector on the prefixed surface has no schedule entry.
        let bogus = address!("0x00000000000000000000000000000000deadbeef");
        assert_eq!(
            IntrinsicGas::auth_cost(&configured_auth(bogus), AuthWireForm::Prefixed),
            Err(IntrinsicGasError::UnscheduledAuthenticator(bogus))
        );
        assert_eq!(
            IntrinsicGas::auth_cost(&configured_auth(Address::ZERO), AuthWireForm::Prefixed),
            Err(IntrinsicGasError::UnscheduledAuthenticator(Address::ZERO))
        );
    }

    #[test]
    fn eip7623_calldata_floor_prices_payload_above_the_standard_rate() {
        let gas = intrinsic(&signed(TxEip8130::default(), vec![0xcd; 65], vec![]), &EXISTING_KEY);

        assert!(gas.payload > 0);
        assert_eq!(gas.payload % Eip8130GasSchedule::TX_DATA_ZERO_BYTE, 0);
        let tokens = gas.payload / Eip8130GasSchedule::TX_DATA_ZERO_BYTE;
        assert_eq!(gas.payload_floor, Eip8130GasSchedule::TX_TOTAL_COST_FLOOR_PER_TOKEN * tokens);
        assert!(gas.payload_floor > gas.payload);

        assert_eq!(gas.sender_floor(), gas.sender_intrinsic() - gas.payload + gas.payload_floor);
        assert!(gas.sender_floor() >= gas.sender_intrinsic());
    }

    #[test]
    fn nonce_free_and_first_use_costs() {
        let mut tx = TxEip8130 { nonce_key: Eip8130Constants::NONCE_KEY_MAX, ..Default::default() };
        let free = intrinsic(&signed(tx.clone(), vec![0; 65], vec![]), &EXISTING_KEY);
        assert_eq!(free.nonce_key, Eip8130GasSchedule::NONCE_FREE_COST);

        tx.nonce_key = U256::from(7u64);
        let first =
            intrinsic(&signed(tx, vec![0; 65], vec![]), &IntrinsicGasInput::new(ACCOUNT, true));
        assert_eq!(first.nonce_key, Eip8130GasSchedule::NONCE_KEY_FIRST_USE_COST);
    }

    #[test]
    fn delegation_entry_charges_deposit() {
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Delegation(Delegation { target: ACCOUNT })],
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]), &EXISTING_KEY);
        assert_eq!(gas.account_changes, Eip8130GasSchedule::DELEGATION_DEPOSIT_COST);
    }

    #[test]
    fn open_payer_auth_is_priced_as_a_bare_k1_signature() {
        let tx = TxEip8130 { payer: Some(Eip8130Constants::OPEN_PAYER), ..Default::default() };
        let payer_auth = vec![0xab; 65];
        let gas = intrinsic(&signed(tx, vec![0; 65], payer_auth.clone()), &EXISTING_KEY);
        assert_eq!(
            gas.payer_auth,
            Eip8130GasSchedule::AUTH_EXEC_K1
                + Eip8130GasSchedule::COLD_SLOAD
                + IntrinsicGas::data_cost(&payer_auth)
        );
    }

    #[test]
    fn sponsored_payer_is_excluded_from_sender_intrinsic() {
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            payer: Some(address!("0x2222222222222222222222222222222222222222")),
            ..Default::default()
        };
        let payer_auth = configured_auth(K1);
        let gas = intrinsic(&signed(tx, configured_auth(K1), payer_auth.clone()), &EXISTING_KEY);
        assert_eq!(
            gas.payer_auth,
            Eip8130GasSchedule::AUTH_EXEC_K1
                + Eip8130GasSchedule::COLD_SLOAD
                + IntrinsicGas::payload_costs(&payer_auth).1
        );
        assert_eq!(gas.sender_intrinsic(), gas.total() - gas.payer_auth);
        assert!(gas.payer_auth > 0);
    }

    #[test]
    fn payer_auth_bytes_do_not_change_sender_payload() {
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            payer: Some(address!("0x2222222222222222222222222222222222222222")),
            ..Default::default()
        };
        let short =
            intrinsic(&signed(tx.clone(), configured_auth(K1), configured_auth(K1)), &EXISTING_KEY);
        let mut long_auth = configured_auth(K1);
        long_auth.extend_from_slice(&[0xff; 200]);
        let long = intrinsic(&signed(tx, configured_auth(K1), long_auth), &EXISTING_KEY);

        assert_eq!(short.payload, long.payload);
        assert_eq!(short.sender_intrinsic(), long.sender_intrinsic());
        assert_eq!(
            long.payer_auth - short.payer_auth,
            200 * Eip8130GasSchedule::TX_DATA_NONZERO_BYTE
        );
    }

    #[test]
    fn sender_payload_cost_matches_empty_payer_auth_encoding() {
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            payer: Some(address!("0x2222222222222222222222222222222222222222")),
            calls: vec![vec![Call {
                to: ACCOUNT,
                value: U256::from(1u64),
                data: Bytes::from(vec![0u8; 64]),
            }]],
            ..Default::default()
        };
        // A long, mostly-zero `payer_auth` changes the list header's
        // length-of-length and the zero/nonzero mix of the suffix. The
        // sender-billed payload must still match a true empty-`payer_auth`
        // re-encoding.
        let mut payer_auth = configured_auth(K1);
        payer_auth.extend_from_slice(&[0x00; 4_000]);
        let signed_tx = signed(tx, configured_auth(K1), payer_auth);
        let gas = intrinsic(&signed_tx, &EXISTING_KEY);
        let (payload, payload_floor) = signed_tx.encoded_2718_without_payer_auth().iter().fold(
            (0u64, 0u64),
            |costs, &byte| {
                let (standard, floor) = IntrinsicGas::byte_payload_costs(byte);
                (costs.0.saturating_add(standard), costs.1.saturating_add(floor))
            },
        );
        assert_eq!(gas.payload, payload);
        assert_eq!(gas.payload_floor, payload_floor);
    }

    #[test]
    fn payer_auth_over_max_authentication_gas_is_an_error() {
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            payer: Some(address!("0x2222222222222222222222222222222222222222")),
            ..Default::default()
        };
        let mut payer_auth = configured_auth(K1);
        payer_auth.extend_from_slice(&[0xff; 7_000]);
        let s = signed(tx, configured_auth(K1), payer_auth);
        assert!(matches!(
            IntrinsicGas::compute(&s, &encode(&s), &EXISTING_KEY),
            Err(IntrinsicGasError::PayerAuthGasExceeded(cost))
                if cost > Eip8130GasSchedule::MAX_AUTHENTICATION_GAS
        ));
    }

    #[test]
    fn unscheduled_authenticator_is_an_error() {
        let bogus = address!("0x00000000000000000000000000000000deadbeef");
        let tx = TxEip8130 { sender: Some(ACCOUNT), ..Default::default() };
        let s = signed(tx, configured_auth(bogus), vec![]);
        assert_eq!(
            IntrinsicGas::compute(&s, &encode(&s), &EXISTING_KEY),
            Err(IntrinsicGasError::UnscheduledAuthenticator(bogus))
        );
    }

    #[test]
    fn value_calls_to_others_charge_tx_value_cost() {
        let other = address!("0x3333333333333333333333333333333333333333");
        let call = |to, value: u64| Call { to, value: U256::from(value), data: Bytes::new() };
        let tx = TxEip8130 {
            calls: vec![
                vec![call(other, 1), call(ACCOUNT, 1), call(other, 0)],
                vec![call(other, 5)],
            ],
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]), &EXISTING_KEY);
        assert_eq!(gas.value_transfer, 2 * Eip8130GasSchedule::TX_VALUE_COST);
    }

    #[test]
    fn execution_gas_available_subtracts_sender_intrinsic() {
        let tx = TxEip8130::default();
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]), &EXISTING_KEY);
        let si = gas.sender_intrinsic();
        assert_eq!(gas.execution_gas_available(si + 1_000), Some(1_000));
        assert_eq!(gas.execution_gas_available(si.saturating_sub(1)), None);
    }
}
