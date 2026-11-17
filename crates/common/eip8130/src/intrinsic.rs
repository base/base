//! EIP-8130 intrinsic gas: the total cost to include an AA transaction.

use alloy_primitives::Address;
use base_common_consensus::{AccountChange, Eip8130Constants, Eip8130Signed};

use crate::Eip8130GasSchedule;

/// The EIP-8130 intrinsic-gas breakdown, one field per spec component.
///
/// Intrinsic gas is a pure function of the signed transaction and its resolved
/// sender: it reads no state, so estimation, mempool admission and execution all
/// compute the same value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct IntrinsicGas {
    /// `AA_BASE_COST`.
    pub base: u64,
    /// `tx_payload_cost` — EIP-2028 data-availability cost (the *standard*
    /// per-token rate) over the RLP of `account_changes`, `calls` and
    /// `metadata`. The k1 signatures in `sender_auth` and `payer_auth` are not
    /// metered: the k1 authentication cost covers them, as EIP-2780's base
    /// covers an ECDSA signature. Subject to the EIP-7623 calldata floor; see
    /// [`Self::payload_floor`] and [`Self::sender_floor`].
    pub payload: u64,
    /// The EIP-7623 calldata *floor* over the same bytes [`Self::payload`]
    /// covers: `TX_TOTAL_COST_FLOOR_PER_TOKEN` (10) per payload token instead of
    /// the standard `TX_DATA_ZERO_BYTE` (4). Always `>= payload`. It only raises
    /// the metered gas via [`Self::sender_floor`] when a data-heavy
    /// transaction's execution is cheap.
    pub payload_floor: u64,
    /// `first_call_cost` — [`Eip8130GasSchedule::COLD_ACCOUNT_ACCESS`] for the
    /// first call's target, as EIP-2780 charges `tx.to`. `0` when `calls` is
    /// empty or the first call targets the sender. Dispatch then charges the
    /// first call only for its EIP-7702 delegate.
    pub first_call: u64,
    /// `sender_access_cost` — [`Eip8130GasSchedule::COLD_ACCOUNT_ACCESS`] for
    /// the sender's account when `payer` is set and differs from the sender.
    /// `AA_BASE_COST` covers one account access, which a sponsored transaction
    /// spends on the payer, and every transaction reads the sender's account
    /// nonce. `0` for self-pay (`payer` empty or equal to the sender). Open
    /// payer mode always pays it: its payer is only known from `payer_auth`.
    pub sender_account: u64,
    /// `nonce_key_cost`.
    pub nonce_key: u64,
    /// `value_transfer_cost` — [`Eip8130GasSchedule::TX_VALUE_COST`] per call
    /// with `value > 0` and `to != sender`.
    pub value_transfer: u64,
    /// `account_changes_cost` — the delegation entry's indicator deposit.
    pub account_changes: u64,
    /// `sender_auth_cost` — [`Eip8130GasSchedule::K1_AUTH_COST`].
    pub sender_auth: u64,
    /// `payer_auth_cost` — [`Eip8130GasSchedule::K1_AUTH_COST`], or `0` for
    /// self-pay. Billed to the payer on top of `gas_limit`.
    pub payer_auth_cost: u64,
}

impl IntrinsicGas {
    /// Sender-intrinsic gas: every component except payer authentication, which
    /// is metered on top of `gas_limit`. This is the portion bounded by
    /// `gas_limit`.
    #[must_use]
    pub const fn sender_intrinsic(&self) -> u64 {
        self.base
            .saturating_add(self.payload)
            .saturating_add(self.nonce_key)
            .saturating_add(self.value_transfer)
            .saturating_add(self.account_changes)
            .saturating_add(self.sender_auth)
            .saturating_add(self.first_call)
            .saturating_add(self.sender_account)
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

    /// The EIP-7623 sender-side floor: the minimum gas a transaction is metered
    /// against its `gas_limit`, regardless of how little its `calls` execute.
    /// As in EIP-2780, the floor base keeps the base, authentication, account
    /// access and value charges and drops the state-dependent ones
    /// (`nonce_key`, `account_changes`):
    ///
    /// ```text
    /// sender_floor = base + sender_auth + first_call + sender_account
    ///              + value_transfer + payload_floor
    /// ```
    ///
    /// A `gas_limit` below this is invalid, and settlement charges at least
    /// `sender_floor + payer_auth_cost`.
    #[must_use]
    pub const fn sender_floor(&self) -> u64 {
        self.base
            .saturating_add(self.sender_auth)
            .saturating_add(self.first_call)
            .saturating_add(self.sender_account)
            .saturating_add(self.value_transfer)
            .saturating_add(self.payload_floor)
    }

    /// Computes the intrinsic gas for a signed EIP-8130 transaction.
    ///
    /// `sender` is the resolved sender: calls to it carry no recipient or value
    /// charge. Every sender and payer authenticates with a native secp256k1
    /// signature, so authentication is priced at
    /// [`Eip8130GasSchedule::K1_AUTH_COST`]; a transaction naming any other
    /// authenticator is rejected by authorization.
    #[must_use]
    pub fn compute(signed: &Eip8130Signed, sender: Address) -> Self {
        let tx = signed.tx();

        // A sequenced key's current nonce must equal `nonce_sequence`, so a zero
        // sequence is the key's first use.
        let nonce_key = if tx.nonce_key.is_zero() {
            Eip8130GasSchedule::PROTOCOL_NONCE_COST
        } else if tx.nonce_key == Eip8130Constants::NONCE_KEY_MAX {
            Eip8130GasSchedule::NONCE_FREE_COST
        } else if tx.nonce_sequence == 0 {
            Eip8130GasSchedule::NONCE_KEY_FIRST_USE_COST
        } else {
            Eip8130GasSchedule::NONCE_KEY_EXISTING_COST
        };

        let account_changes = tx.account_changes.iter().fold(0u64, |cost, change| match change {
            AccountChange::Delegation(_) => {
                cost.saturating_add(Eip8130GasSchedule::DELEGATION_DEPOSIT_COST)
            }
        });

        let first_call = match tx.calls.iter().flatten().next() {
            Some(call) if call.to != sender => Eip8130GasSchedule::COLD_ACCOUNT_ACCESS,
            _ => 0,
        };
        let value_calls = tx
            .calls
            .iter()
            .flatten()
            .filter(|call| !call.value.is_zero() && call.to != sender)
            .count();
        let value_transfer = Eip8130GasSchedule::TX_VALUE_COST
            .saturating_mul(u64::try_from(value_calls).unwrap_or(u64::MAX));
        let sender_account = if matches!(tx.payer, Some(payer) if payer != sender) {
            Eip8130GasSchedule::COLD_ACCOUNT_ACCESS
        } else {
            0
        };

        let (payload, payload_floor) = Self::data_costs_of_counts(
            signed.metered_payload_zero_bytes(),
            signed.metered_payload_len() - signed.metered_payload_zero_bytes(),
        );

        Self {
            base: Eip8130GasSchedule::AA_BASE_COST,
            payload,
            payload_floor,
            first_call,
            sender_account,
            nonce_key,
            value_transfer,
            account_changes,
            sender_auth: Eip8130GasSchedule::K1_AUTH_COST,
            payer_auth_cost: Self::payer_auth_cost_of(signed),
        }
    }

    /// The payer-authentication gas ([`Self::payer_auth_cost`]) billed to the
    /// payer on top of `gas_limit` for a signed EIP-8130 transaction (`0` for
    /// self-pay).
    ///
    /// Block gas reservation uses this to budget the payer's authentication in
    /// addition to the sender-signed `gas_limit`: the payer reimburses its own
    /// authentication beyond that limit, so a block admitting a transaction on
    /// `gas_limit` alone could let true consumption push cumulative gas over the
    /// block limit. It depends only on the payer field, so building,
    /// validation and settlement share this exact value.
    #[must_use]
    pub const fn payer_auth_cost_of(signed: &Eip8130Signed) -> u64 {
        if signed.tx().payer.is_some() { Eip8130GasSchedule::K1_AUTH_COST } else { 0 }
    }

    /// Standard EIP-2028 and EIP-7623 floor data costs of a payload with the
    /// given byte counts. A zero byte is one token and a non-zero byte is four.
    fn data_costs_of_counts(zero_bytes: usize, nonzero_bytes: usize) -> (u64, u64) {
        let zero = u64::try_from(zero_bytes).unwrap_or(u64::MAX);
        let nonzero = u64::try_from(nonzero_bytes).unwrap_or(u64::MAX);
        let standard = zero
            .saturating_mul(Eip8130GasSchedule::TX_DATA_ZERO_BYTE)
            .saturating_add(nonzero.saturating_mul(Eip8130GasSchedule::TX_DATA_NONZERO_BYTE));
        let tokens = zero.saturating_add(nonzero.saturating_mul(4));
        (standard, tokens.saturating_mul(Eip8130GasSchedule::TX_TOTAL_COST_FLOOR_PER_TOKEN))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes, U256, address};
    use base_common_consensus::{Call, Delegation, TxEip8130};

    use super::*;

    const ACCOUNT: Address = address!("0x1111111111111111111111111111111111111111");
    const K1: Address = Eip8130Constants::K1_AUTHENTICATOR;
    /// A k1 authentication: ecrecover and one cold SLOAD.
    const K1_AUTH: u64 = 5_100;

    fn signed(tx: TxEip8130, sender_auth: Vec<u8>, payer_auth: Vec<u8>) -> Eip8130Signed {
        Eip8130Signed::new(tx, Bytes::from(sender_auth), Bytes::from(payer_auth))
    }

    /// `K1_AUTHENTICATOR || dummy signature`.
    fn k1_auth() -> Vec<u8> {
        let mut blob = K1.to_vec();
        blob.extend_from_slice(&[0xab; 65]);
        blob
    }

    fn intrinsic(signed: &Eip8130Signed) -> IntrinsicGas {
        IntrinsicGas::compute(signed, ACCOUNT)
    }

    #[test]
    fn eoa_self_pay_minimal() {
        // sender == None (EOA), protocol nonce (key 0), no payer, no account changes.
        let tx = TxEip8130::default();
        let gas = intrinsic(&signed(tx, vec![0xcd; 65], vec![]));

        assert_eq!(gas.base, Eip8130GasSchedule::AA_BASE_COST);
        assert_eq!(gas.nonce_key, 0);
        assert_eq!(gas.account_changes, 0);
        assert_eq!(gas.sender_auth, K1_AUTH);
        assert_eq!(gas.first_call, 0);
        assert_eq!(gas.payer_auth_cost, 0);
        assert!(gas.payload > 0);
    }

    #[test]
    fn eth_transfer_costs_more_than_eip2780() {
        // A self-paid k1 value transfer costs more than the same transfer under
        // EIP-2780 before its payload bytes: the k1 authentication is charged
        // on top of EIP-2780's base.
        const EIP2780_TX_BASE_COST: u64 = 12_000;
        let other = address!("0x3333333333333333333333333333333333333333");
        let tx = TxEip8130 {
            calls: vec![vec![Call { to: other, value: U256::from(1u64), data: Bytes::new() }]],
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, vec![0xcd; 65], vec![]));
        let eip2780 = EIP2780_TX_BASE_COST
            + Eip8130GasSchedule::COLD_ACCOUNT_ACCESS
            + Eip8130GasSchedule::TX_VALUE_COST;
        assert!(gas.sender_intrinsic() - gas.payload > eip2780);
    }

    #[test]
    fn first_call_target_is_charged_cold_unless_it_is_the_sender() {
        let other = address!("0x3333333333333333333333333333333333333333");
        let call = |to| Call { to, value: U256::ZERO, data: Bytes::new() };
        let first_call = |calls| {
            let tx = TxEip8130 { calls, ..Default::default() };
            intrinsic(&signed(tx, vec![0xcd; 65], vec![])).first_call
        };

        assert_eq!(first_call(vec![]), 0);
        assert_eq!(first_call(vec![vec![call(ACCOUNT)], vec![call(other)]]), 0);
        // Only the first call is charged, and empty phases are skipped.
        assert_eq!(
            first_call(vec![vec![], vec![call(other), call(other)]]),
            Eip8130GasSchedule::COLD_ACCOUNT_ACCESS
        );
    }

    #[test]
    fn sponsored_transaction_pays_for_the_sender_account() {
        let other = address!("0x3333333333333333333333333333333333333333");
        let call =
            |to, value: u64| vec![vec![Call { to, value: U256::from(value), data: Bytes::new() }]];
        let sender_account = |tx: TxEip8130| {
            let payer_auth = if tx.payer.is_some() { k1_auth() } else { vec![] };
            intrinsic(&signed(tx, vec![0xcd; 65], payer_auth)).sender_account
        };

        // A self-paid transaction touches only the sender's account, whether it
        // leaves `payer` empty or names the sender.
        assert_eq!(sender_account(TxEip8130 { calls: call(other, 1), ..Default::default() }), 0);
        let explicit_self_pay =
            TxEip8130 { payer: Some(ACCOUNT), calls: call(other, 1), ..Default::default() };
        assert_eq!(sender_account(explicit_self_pay), 0);

        // Execution reads the sender's account nonce whatever the nonce key, so a
        // sponsored transaction touches a second account even when it only calls
        // another address with no value.
        let payer = Some(Address::repeat_byte(0x22));
        for nonce_key in [U256::ZERO, U256::from(7u64), Eip8130Constants::NONCE_KEY_MAX] {
            let tx = TxEip8130 { payer, nonce_key, calls: call(other, 0), ..Default::default() };
            assert_eq!(sender_account(tx), Eip8130GasSchedule::COLD_ACCOUNT_ACCESS);
        }
    }

    #[test]
    fn payload_meters_the_data_fields_but_not_the_envelope() {
        let base = TxEip8130 { sender: Some(ACCOUNT), ..Default::default() };
        let payload =
            |tx: TxEip8130, sender_auth| intrinsic(&signed(tx, sender_auth, vec![])).payload;
        let k1 = payload(base.clone(), k1_auth());

        let envelope = TxEip8130 {
            gas_limit: u64::MAX,
            max_fee_per_gas: u128::MAX,
            nonce_sequence: u64::MAX,
            ..base.clone()
        };
        assert_eq!(payload(envelope, k1_auth()), k1);

        let with_metadata = TxEip8130 { metadata: Bytes::from(vec![0xff; 10]), ..base };
        assert_eq!(
            payload(with_metadata, k1_auth()) - k1,
            10 * Eip8130GasSchedule::TX_DATA_NONZERO_BYTE
        );
    }

    #[test]
    fn sender_floor_excludes_state_dependent_costs() {
        // Nonce-free replay state and a delegation raise sender-intrinsic gas
        // but move the floor only by the bytes they add to the payload.
        let plain = intrinsic(&signed(TxEip8130::default(), vec![0xcd; 65], vec![]));
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            account_changes: vec![AccountChange::Delegation(Delegation { target: ACCOUNT })],
            ..Default::default()
        };
        let stateful = intrinsic(&signed(tx, vec![0xcd; 65], vec![]));
        assert_eq!(
            stateful.sender_floor() - plain.sender_floor(),
            stateful.payload_floor - plain.payload_floor
        );
        assert!(stateful.sender_floor() < stateful.sender_intrinsic());
    }

    #[test]
    fn eip7623_calldata_floor_prices_payload_above_the_standard_rate() {
        // The floor swaps the standard EIP-2028 4/16-per-byte rate for EIP-7623's
        // 10/40-per-token rate over the same serialized bytes, so `payload_floor`
        // is exactly `TX_TOTAL_COST_FLOOR_PER_TOKEN` per token and the sender floor
        // sits `(10 - 4)` gas per token above sender-intrinsic gas.
        let gas = intrinsic(&signed(TxEip8130::default(), vec![0xcd; 65], vec![]));

        assert!(gas.payload > 0);
        // Standard payload == TX_DATA_ZERO_BYTE (4) per token; recover the token
        // count from it, then check the floor is the per-token floor rate.
        assert_eq!(gas.payload % Eip8130GasSchedule::TX_DATA_ZERO_BYTE, 0);
        let tokens = gas.payload / Eip8130GasSchedule::TX_DATA_ZERO_BYTE;
        assert_eq!(gas.payload_floor, Eip8130GasSchedule::TX_TOTAL_COST_FLOOR_PER_TOKEN * tokens);
        assert!(gas.payload_floor > gas.payload);

        // With no state-dependent costs, the floor exceeds sender-intrinsic gas
        // by exactly (10 - 4) gas per token.
        let per_token_premium = Eip8130GasSchedule::TX_TOTAL_COST_FLOOR_PER_TOKEN
            - Eip8130GasSchedule::TX_DATA_ZERO_BYTE;
        assert_eq!(gas.sender_floor() - gas.sender_intrinsic(), per_token_premium * tokens);
        assert!(gas.sender_floor() >= gas.sender_intrinsic());
    }

    #[test]
    fn eip7623_floor_grows_with_payload_size() {
        // A data-heavy transaction pays a strictly larger floor premium than a
        // minimal one: the floor is what stops an 8130 transaction from posting
        // data availability more cheaply than a standard EIP-7623 transaction.
        let minimal = intrinsic(&signed(TxEip8130::default(), vec![0xcd; 65], vec![]));
        let heavy_tx =
            TxEip8130 { metadata: Bytes::from(vec![0x11u8; 20_000]), ..Default::default() };
        let heavy = intrinsic(&signed(heavy_tx, vec![0xcd; 65], vec![]));

        assert!(heavy.payload > minimal.payload);
        assert!(
            heavy.sender_floor() - heavy.sender_intrinsic()
                > minimal.sender_floor() - minimal.sender_intrinsic()
        );
    }

    #[test]
    fn nonce_key_cost_follows_the_signed_nonce_fields() {
        let nonce_key = |nonce_key, nonce_sequence| {
            let tx = TxEip8130 { nonce_key, nonce_sequence, ..Default::default() };
            intrinsic(&signed(tx, vec![0; 65], vec![])).nonce_key
        };

        assert_eq!(
            nonce_key(Eip8130Constants::NONCE_KEY_MAX, 0),
            Eip8130GasSchedule::NONCE_FREE_COST
        );
        // A sequenced key's nonce must equal `nonce_sequence`, so sequence 0 is
        // its first use.
        assert_eq!(nonce_key(U256::from(7u64), 0), Eip8130GasSchedule::NONCE_KEY_FIRST_USE_COST);
        assert_eq!(nonce_key(U256::from(7u64), 3), Eip8130GasSchedule::NONCE_KEY_EXISTING_COST);
        // Key 0 is the account nonce: execution bumps it without touching the
        // nonce manager, so neither a fresh nor a used account pays for a slot.
        assert_eq!(nonce_key(U256::ZERO, 0), Eip8130GasSchedule::PROTOCOL_NONCE_COST);
        assert_eq!(nonce_key(U256::ZERO, 3), Eip8130GasSchedule::PROTOCOL_NONCE_COST);
    }

    #[test]
    fn delegation_entry_charges_deposit() {
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Delegation(Delegation { target: ACCOUNT })],
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]));
        assert_eq!(gas.account_changes, Eip8130GasSchedule::DELEGATION_DEPOSIT_COST);
    }

    /// Every authentication is a native secp256k1 recovery plus one cold
    /// SLOAD, 5,100 gas, whatever its wire form: a bare EOA signature, a named
    /// sender or payer, or an open payer.
    #[test]
    fn k1_authentication_costs_5100() {
        assert_eq!(Eip8130GasSchedule::K1_AUTH_COST, K1_AUTH);

        let eoa = intrinsic(&signed(TxEip8130::default(), vec![0xcd; 65], vec![]));
        assert_eq!(eoa.sender_auth, K1_AUTH);
        assert_eq!(eoa.payer_auth_cost, 0);

        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            payer: Some(Address::repeat_byte(0x22)),
            ..Default::default()
        };
        let named = intrinsic(&signed(tx, k1_auth(), k1_auth()));
        assert_eq!(named.sender_auth, K1_AUTH);
        assert_eq!(named.payer_auth_cost, K1_AUTH);

        let tx = TxEip8130 { payer: Some(Eip8130Constants::OPEN_PAYER), ..Default::default() };
        let open = signed(tx, vec![0; 65], vec![0xab; 65]);
        assert_eq!(intrinsic(&open).payer_auth_cost, K1_AUTH);
        assert_eq!(IntrinsicGas::payer_auth_cost_of(&open), K1_AUTH);
    }

    /// Auth bytes are covered by the k1 authentication cost: they change
    /// neither the payload nor the payer's charge.
    #[test]
    fn auth_bytes_are_not_metered() {
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            payer: Some(address!("0x2222222222222222222222222222222222222222")),
            ..Default::default()
        };
        let short = intrinsic(&signed(tx.clone(), k1_auth(), k1_auth()));
        let mut long_auth = k1_auth();
        long_auth.extend_from_slice(&[0xff; 200]);
        let long = intrinsic(&signed(tx, long_auth.clone(), long_auth));

        assert_eq!(short, long);
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
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]));
        assert_eq!(gas.value_transfer, 2 * Eip8130GasSchedule::TX_VALUE_COST);
    }

    #[test]
    fn execution_gas_available_subtracts_sender_intrinsic() {
        let tx = TxEip8130::default();
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]));
        let si = gas.sender_intrinsic();
        assert_eq!(gas.execution_gas_available(si + 1_000), Some(1_000));
        assert_eq!(gas.execution_gas_available(si.saturating_sub(1)), None);
    }
}
