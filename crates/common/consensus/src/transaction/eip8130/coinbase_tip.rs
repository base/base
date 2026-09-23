//! Static decode of a phase-0 value transfer to the Sequencer Fee Vault.

use alloy_primitives::U256;

use super::TxEip8130;
use crate::Predeploys;

/// Recovers a statically-analyzable phase-0 coinbase tip from an EIP-8130 body.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct CoinbaseTip;

impl CoinbaseTip {
    /// Statically decoded phase-0 coinbase tip, if one can be recovered without
    /// executing the transaction.
    ///
    /// EIP-8130 `calls` are grouped into phases. A revert discards that phase
    /// and skips later ones, so only a tip in **phase 0** is statically
    /// meaningful. The protocol dispatches each call from the sender and moves
    /// `call.value` to `call.to`, so the tip does not depend on the sender's
    /// wallet code.
    ///
    /// Returns [`Some`] when phase 0 contains exactly one call: a non-zero
    /// `value` sent to [`Predeploys::SEQUENCER_FEE_VAULT`] with empty calldata.
    #[must_use]
    pub fn decode(tx: &TxEip8130) -> Option<U256> {
        let [call] = tx.calls.first()?.as_slice() else {
            return None;
        };
        (call.to == Predeploys::SEQUENCER_FEE_VAULT
            && call.data.is_empty()
            && !call.value.is_zero())
        .then_some(call.value)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256, address, bytes};

    use super::CoinbaseTip;
    use crate::{
        Predeploys,
        transaction::eip8130::{Call, TxEip8130},
    };

    const COINBASE: Address = Predeploys::SEQUENCER_FEE_VAULT;
    const OTHER_RECIPIENT: Address = address!("0x00000000000000000000000000000000000000cc");
    const TIP_AMOUNT: u64 = 123;

    fn tip_call(to: Address) -> Call {
        Call { to, value: U256::from(TIP_AMOUNT), data: Default::default() }
    }

    fn with_phase0(calls: Vec<Call>) -> TxEip8130 {
        TxEip8130 { calls: vec![calls], ..Default::default() }
    }

    #[test]
    fn coinbase_tip_value_call_returns_amount() {
        assert_eq!(
            CoinbaseTip::decode(&with_phase0(vec![tip_call(COINBASE)])),
            Some(U256::from(TIP_AMOUNT))
        );
        let configured = TxEip8130 {
            sender: Some(address!("0x00000000000000000000000000000000000000bb")),
            ..with_phase0(vec![tip_call(COINBASE)])
        };
        assert_eq!(CoinbaseTip::decode(&configured), Some(U256::from(TIP_AMOUNT)));
    }

    #[test]
    fn coinbase_tip_rejects_other_recipient_calldata_or_zero_value() {
        assert_eq!(CoinbaseTip::decode(&with_phase0(vec![tip_call(OTHER_RECIPIENT)])), None);

        let with_calldata = Call { data: bytes!("01"), ..tip_call(COINBASE) };
        assert_eq!(CoinbaseTip::decode(&with_phase0(vec![with_calldata])), None);

        let zero_value = Call { value: U256::ZERO, ..tip_call(COINBASE) };
        assert_eq!(CoinbaseTip::decode(&with_phase0(vec![zero_value])), None);
    }

    #[test]
    fn coinbase_tip_requires_single_phase0_call() {
        let call = tip_call(COINBASE);
        assert_eq!(CoinbaseTip::decode(&with_phase0(vec![call.clone(), call.clone()])), None);
        assert_eq!(CoinbaseTip::decode(&with_phase0(vec![])), None);
        assert_eq!(CoinbaseTip::decode(&TxEip8130::default()), None);

        let later_phase = TxEip8130 { calls: vec![vec![], vec![call]], ..Default::default() };
        assert_eq!(CoinbaseTip::decode(&later_phase), None);
    }
}
