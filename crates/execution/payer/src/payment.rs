//! Phase-0 token payments.

use alloy_primitives::{Address, U256};
use alloy_sol_types::{SolCall, sol};
use base_common_consensus::Call;

use crate::{PayerErrorCode, PayerRejection};

sol! {
    /// ERC-20 calls the payer inspects or issues.
    interface IERC20 {
        /// Transfers `amount` tokens from the caller to `to`.
        function transfer(address to, uint256 amount) external returns (bool);

        /// Returns the token balance of `account`.
        function balanceOf(address account) external view returns (uint256);
    }
}

/// A phase-0 `IERC20.transfer`, the only payment shape the payer accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenPayment {
    /// Token transferred.
    pub token: Address,
    /// Account credited.
    pub recipient: Address,
    /// Amount credited, in token atomic units.
    pub amount: U256,
}

impl TokenPayment {
    /// Decodes the payment from a transaction's call phases.
    ///
    /// Phase 0 must be exactly one canonical `transfer` call that sends no ETH.
    pub fn from_phases(phases: &[Vec<Call>]) -> Result<Self, PayerRejection> {
        let Some([call]) = phases.first().map(Vec::as_slice) else {
            return Err(PayerRejection::new(
                PayerErrorCode::InvalidTransaction,
                "phase 0 must be a single token transfer",
            ));
        };
        if !call.value.is_zero() {
            return Err(PayerRejection::new(
                PayerErrorCode::InvalidTransaction,
                "phase 0 must not transfer ETH",
            ));
        }
        let non_canonical = || {
            PayerRejection::new(
                PayerErrorCode::InvalidTransaction,
                "phase 0 must be a canonical IERC20.transfer call",
            )
        };
        let transfer =
            IERC20::transferCall::abi_decode_validate(&call.data).map_err(|_| non_canonical())?;
        if transfer.abi_encode() != call.data.as_ref() {
            return Err(non_canonical());
        }
        Ok(Self { token: call.to, recipient: transfer.to, amount: transfer.amount })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;

    use super::*;

    const TOKEN: Address = Address::repeat_byte(0x83);
    const PAYER: Address = Address::repeat_byte(0xcc);

    fn transfer(amount: u64) -> Call {
        Call {
            to: TOKEN,
            value: U256::ZERO,
            data: IERC20::transferCall { to: PAYER, amount: U256::from(amount) }
                .abi_encode()
                .into(),
        }
    }

    fn rejection_code(phases: &[Vec<Call>]) -> PayerErrorCode {
        TokenPayment::from_phases(phases).unwrap_err().code
    }

    #[test]
    fn decodes_single_transfer() {
        let user_call = Call { to: Address::repeat_byte(1), value: U256::ZERO, data: Bytes::new() };

        let payment = TokenPayment::from_phases(&[vec![transfer(210_000)], vec![user_call]]);

        assert_eq!(
            payment.unwrap(),
            TokenPayment { token: TOKEN, recipient: PAYER, amount: U256::from(210_000) }
        );
    }

    #[test]
    fn rejects_other_phase_zero_shapes() {
        let mut with_value = transfer(1);
        with_value.value = U256::from(1);
        let mut trailing = transfer(1);
        trailing.data = [trailing.data.as_ref(), &[0u8; 32]].concat().into();
        let approve = Call {
            to: TOKEN,
            value: U256::ZERO,
            data: Bytes::from(IERC20::balanceOfCall { account: PAYER }.abi_encode()),
        };

        for phases in [
            vec![],
            vec![vec![]],
            vec![vec![transfer(1), transfer(1)]],
            vec![vec![with_value]],
            vec![vec![trailing]],
            vec![vec![approve]],
        ] {
            assert_eq!(rejection_code(&phases), PayerErrorCode::InvalidTransaction);
        }
    }
}
