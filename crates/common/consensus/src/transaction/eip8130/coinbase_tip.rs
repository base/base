//! Static decode of a phase-0 `DefaultAccount` transfer to the Sequencer Fee Vault.

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;

use super::{
    IDefaultAccount, TxEip8130, account_changes::AccountChange, addresses::Eip8130Contracts,
};
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
    /// meaningful. Protocol calls carry no value (`call = rlp([to, data])`);
    /// ETH moves only when wallet bytecode issues a `CALL`.
    ///
    /// Returns [`Some`] when the sender uses
    /// [`Eip8130Contracts::DEFAULT_ACCOUNT`] (EOA auto-delegation, or an
    /// explicit delegation to that implementation), phase 0 contains exactly
    /// one call invoked on `resolved_sender`, and the calldata is
    /// `DefaultAccount.execute` (or a one-element `executeBatch`) transferring
    /// ETH to [`Predeploys::SEQUENCER_FEE_VAULT`] with empty calldata.
    ///
    /// `resolved_sender` is the account that will dispatch the call: the
    /// explicit [`TxEip8130::sender`] when present, otherwise the recovered EOA.
    /// The protocol call's `to` must equal that address, matching execution
    /// (`from` is the sender; `execute` must run on the sender itself).
    #[must_use]
    pub fn decode(tx: &TxEip8130, resolved_sender: Address) -> Option<U256> {
        if !Self::statically_uses_default_account(tx) {
            return None;
        }
        if tx.sender.is_some_and(|sender| sender != resolved_sender) {
            return None;
        }
        let [call] = tx.calls.first()?.as_slice() else {
            return None;
        };
        if call.to != resolved_sender {
            return None;
        }
        let (recipient, amount) = Self::default_account_eth_transfer(&call.data)?;
        (recipient == Predeploys::SEQUENCER_FEE_VAULT).then_some(amount)
    }

    /// Whether the unsigned body statically implies
    /// [`Eip8130Contracts::DEFAULT_ACCOUNT`] as the sender's wallet.
    ///
    /// A `Create` installs caller-supplied bytecode, so it is never the
    /// default account. A `Delegation` is default only when its target is
    /// [`Eip8130Contracts::DEFAULT_ACCOUNT`] (multiple delegations are invalid
    /// and treated as not default). With neither entry, only the EOA path
    /// (`sender == None`) auto-delegates; a configured sender's existing code
    /// is not visible here.
    fn statically_uses_default_account(tx: &TxEip8130) -> bool {
        let mut explicit_target = None;
        for change in &tx.account_changes {
            match change {
                AccountChange::Create(_) => return false,
                AccountChange::Delegation(delegation) => {
                    if explicit_target.is_some() {
                        return false;
                    }
                    explicit_target = Some(delegation.target);
                }
                AccountChange::ConfigChange(_) => {}
            }
        }
        explicit_target.map_or_else(
            || tx.sender.is_none(),
            |target| target == Eip8130Contracts::DEFAULT_ACCOUNT,
        )
    }

    /// Recipient and amount of a pure ETH transfer encoded as
    /// `execute(target, value, "")` or a one-element `executeBatch` of the same
    /// shape.
    fn default_account_eth_transfer(calldata: &[u8]) -> Option<(Address, U256)> {
        if let Ok(call) = IDefaultAccount::executeCall::abi_decode_validate(calldata) {
            return call.data.is_empty().then_some((call.target, call.value));
        }
        let batch = IDefaultAccount::executeBatchCall::abi_decode_validate(calldata).ok()?;
        let [inner] = batch.calls.as_slice() else {
            return None;
        };
        inner.data.is_empty().then_some((inner.target, inner.value))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes, U256, address, bytes};
    use alloy_sol_types::SolCall;

    use super::CoinbaseTip;
    use crate::{
        Predeploys,
        transaction::eip8130::{
            Call, IDefaultAccount, TxEip8130,
            account_changes::{
                AccountChange, AccountChangeChannel, CreateEntry, Delegation, SignedAccountChanges,
            },
            addresses::Eip8130Contracts,
        },
    };

    const SENDER: Address = address!("0x00000000000000000000000000000000000000bb");
    const COINBASE: Address = Predeploys::SEQUENCER_FEE_VAULT;
    const OTHER_RECIPIENT: Address = address!("0x00000000000000000000000000000000000000cc");
    const TIP_AMOUNT: u64 = 123;

    fn encode_execute(target: Address, value: U256, data: &[u8]) -> Bytes {
        Bytes::from(
            IDefaultAccount::executeCall { target, value, data: data.to_vec().into() }.abi_encode(),
        )
    }

    fn encode_execute_batch_one(target: Address, value: U256) -> Bytes {
        Bytes::from(
            IDefaultAccount::executeBatchCall {
                calls: vec![IDefaultAccount::Call { target, value, data: Default::default() }],
            }
            .abi_encode(),
        )
    }

    fn execute_call(to: Address) -> Call {
        Call { to, data: encode_execute(COINBASE, U256::from(TIP_AMOUNT), &[]) }
    }

    fn eoa_with_phase0(calls: Vec<Call>) -> TxEip8130 {
        TxEip8130 { sender: None, calls: vec![calls], ..Default::default() }
    }

    #[test]
    fn coinbase_tip_execute_encoding_matches_canonical_abi() {
        assert_eq!(
            encode_execute(COINBASE, U256::from(TIP_AMOUNT), &[]).as_ref(),
            bytes!("b61d27f60000000000000000000000004200000000000000000000000000000000000011000000000000000000000000000000000000000000000000000000000000007b00000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000000")
                .as_ref(),
        );
        assert_eq!(
            encode_execute_batch_one(COINBASE, U256::from(TIP_AMOUNT)).as_ref(),
            bytes!("34fcd5be0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000200000000000000000000000004200000000000000000000000000000000000011000000000000000000000000000000000000000000000000000000000000007b00000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000000")
                .as_ref(),
        );
    }

    #[test]
    fn coinbase_tip_eoa_execute_returns_amount() {
        let tx = eoa_with_phase0(vec![execute_call(SENDER)]);
        assert_eq!(CoinbaseTip::decode(&tx, SENDER), Some(U256::from(TIP_AMOUNT)));
        let other = eoa_with_phase0(vec![Call {
            to: SENDER,
            data: encode_execute(OTHER_RECIPIENT, U256::from(TIP_AMOUNT), &[]),
        }]);
        assert_eq!(CoinbaseTip::decode(&other, SENDER), None);
        let not_self = eoa_with_phase0(vec![execute_call(COINBASE)]);
        assert_eq!(CoinbaseTip::decode(&not_self, SENDER), None);
    }

    #[test]
    fn coinbase_tip_eoa_execute_batch_single_send_returns_amount() {
        let tx = eoa_with_phase0(vec![Call {
            to: SENDER,
            data: encode_execute_batch_one(COINBASE, U256::from(TIP_AMOUNT)),
        }]);
        assert_eq!(CoinbaseTip::decode(&tx, SENDER), Some(U256::from(TIP_AMOUNT)));
        let other = eoa_with_phase0(vec![Call {
            to: SENDER,
            data: encode_execute_batch_one(OTHER_RECIPIENT, U256::from(TIP_AMOUNT)),
        }]);
        assert_eq!(CoinbaseTip::decode(&other, SENDER), None);
    }

    #[test]
    fn coinbase_tip_explicit_sender_requires_self_call_and_default_delegation() {
        let delegated = TxEip8130 {
            sender: Some(SENDER),
            account_changes: vec![AccountChange::Delegation(Delegation {
                target: Eip8130Contracts::DEFAULT_ACCOUNT,
            })],
            calls: vec![vec![execute_call(SENDER)]],
            ..Default::default()
        };
        assert_eq!(CoinbaseTip::decode(&delegated, SENDER), Some(U256::from(TIP_AMOUNT)));

        let not_self = TxEip8130 { calls: vec![vec![execute_call(COINBASE)]], ..delegated.clone() };
        assert_eq!(CoinbaseTip::decode(&not_self, SENDER), None);
        assert_eq!(CoinbaseTip::decode(&delegated, OTHER_RECIPIENT), None);

        let no_delegation = TxEip8130 { account_changes: vec![], ..delegated };
        assert_eq!(CoinbaseTip::decode(&no_delegation, SENDER), None);
    }

    #[test]
    fn coinbase_tip_rejects_non_default_account() {
        let other_impl = TxEip8130 {
            sender: Some(SENDER),
            account_changes: vec![AccountChange::Delegation(Delegation {
                target: Eip8130Contracts::CANONICAL_HIGH_RATE_PAYER_ACCOUNT,
            })],
            calls: vec![vec![execute_call(SENDER)]],
            ..Default::default()
        };
        assert_eq!(CoinbaseTip::decode(&other_impl, SENDER), None);

        let cleared = TxEip8130 {
            account_changes: vec![AccountChange::Delegation(Delegation { target: Address::ZERO })],
            ..other_impl
        };
        assert_eq!(CoinbaseTip::decode(&cleared, SENDER), None);

        let created = TxEip8130 {
            sender: None,
            account_changes: vec![AccountChange::Create(CreateEntry {
                user_salt: B256::ZERO,
                code: bytes!("01"),
                initial_actors: vec![],
            })],
            calls: vec![vec![execute_call(SENDER)]],
            ..Default::default()
        };
        assert_eq!(CoinbaseTip::decode(&created, SENDER), None);
    }

    #[test]
    fn coinbase_tip_requires_single_phase0_eth_transfer() {
        let call = execute_call(SENDER);

        assert_eq!(
            CoinbaseTip::decode(&eoa_with_phase0(vec![call.clone(), call.clone()]), SENDER),
            None
        );
        assert_eq!(CoinbaseTip::decode(&eoa_with_phase0(vec![]), SENDER), None);

        let later_phase =
            TxEip8130 { sender: None, calls: vec![vec![], vec![call]], ..Default::default() };
        assert_eq!(CoinbaseTip::decode(&later_phase, SENDER), None);

        let with_calldata = eoa_with_phase0(vec![Call {
            to: SENDER,
            data: encode_execute(COINBASE, U256::from(TIP_AMOUNT), &[0x01]),
        }]);
        assert_eq!(CoinbaseTip::decode(&with_calldata, SENDER), None);

        let wrong_selector = eoa_with_phase0(vec![Call { to: SENDER, data: bytes!("deadbeef") }]);
        assert_eq!(CoinbaseTip::decode(&wrong_selector, SENDER), None);

        let configured = TxEip8130 { sender: Some(SENDER), ..Default::default() };
        assert_eq!(CoinbaseTip::decode(&configured, SENDER), None);
    }

    #[test]
    fn coinbase_tip_config_change_does_not_block_eoa_auto_delegation() {
        let tx = TxEip8130 {
            sender: None,
            account_changes: vec![AccountChange::ConfigChange(SignedAccountChanges {
                channel: AccountChangeChannel::Local,
                sequence: 0,
                changes: vec![],
                signature: Bytes::new(),
            })],
            calls: vec![vec![execute_call(SENDER)]],
            ..Default::default()
        };
        assert_eq!(CoinbaseTip::decode(&tx, SENDER), Some(U256::from(TIP_AMOUNT)));
    }

    #[test]
    fn coinbase_tip_rejects_truncated_or_dirty_calldata() {
        let mut truncated = encode_execute(COINBASE, U256::from(TIP_AMOUNT), &[]).to_vec();
        truncated.truncate(truncated.len().saturating_sub(32));
        let truncated_tx = eoa_with_phase0(vec![Call { to: SENDER, data: Bytes::from(truncated) }]);
        assert_eq!(CoinbaseTip::decode(&truncated_tx, SENDER), None);

        // High bytes of the address word must be zero; `abi_decode_validate` rejects
        // that dirty padding even though a lenient decoder would still yield a tip.
        let mut dirty = encode_execute(COINBASE, U256::from(TIP_AMOUNT), &[]).to_vec();
        dirty[4] = 0xff;
        let dirty_tx = eoa_with_phase0(vec![Call { to: SENDER, data: Bytes::from(dirty) }]);
        assert_eq!(CoinbaseTip::decode(&dirty_tx, SENDER), None);
    }
}
