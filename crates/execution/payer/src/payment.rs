//! Phase-0 token payments.

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::{SolCall, sol};
use base_common_consensus::Call;
use revm::{
    Context, Database, MainBuilder, MainContext, SystemCallEvm,
    context::{
        BlockEnv,
        result::{EVMError, ExecutionResult},
    },
};

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

/// Result of simulating a [`TokenPayment`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferOutcome {
    /// The transfer succeeded and returned `true` or no data.
    Transferred,
    /// The transfer reverted with this output.
    Reverted(Bytes),
    /// The transfer halted or returned `false`.
    Failed,
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

    /// Runs the transfer from `sender` against `db` without committing it.
    ///
    /// The call runs as a system call, so it skips fee payment and the caller
    /// checks a transaction would fail for a smart-account sender with code.
    pub fn simulate<DB: Database>(
        &self,
        db: DB,
        sender: Address,
        block: BlockEnv,
        chain_id: u64,
    ) -> Result<TransferOutcome, EVMError<DB::Error>> {
        let mut evm = Context::mainnet()
            .with_db(db)
            .with_block(block)
            .modify_cfg_chained(|cfg| cfg.chain_id = chain_id)
            .build_mainnet();
        let data = IERC20::transferCall { to: self.recipient, amount: self.amount }.abi_encode();
        Ok(match evm.system_call_one_with_caller(sender, self.token, data.into())? {
            ExecutionResult::Success { output, .. } => {
                let output = output.into_data();
                if output.is_empty()
                    || IERC20::transferCall::abi_decode_returns(&output).unwrap_or(false)
                {
                    TransferOutcome::Transferred
                } else {
                    TransferOutcome::Failed
                }
            }
            ExecutionResult::Revert { output, .. } => TransferOutcome::Reverted(output),
            ExecutionResult::Halt { .. } => TransferOutcome::Failed,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::hex;
    use revm::{bytecode::Bytecode, database::InMemoryDB, state::AccountInfo};

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

    /// Runs `payment` against a token whose runtime is `code`, from a sender
    /// that itself has code.
    fn simulate(code: &'static [u8]) -> TransferOutcome {
        let sender = Address::repeat_byte(0xaa);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            TOKEN,
            AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from_static(code))),
        );
        db.insert_account_info(
            sender,
            AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from_static(&[0x00]))),
        );
        let payment = TokenPayment { token: TOKEN, recipient: PAYER, amount: U256::from(1) };
        payment.simulate(&mut db, sender, BlockEnv::default(), 8453).unwrap()
    }

    #[test]
    fn simulates_transfer_outcome() {
        // PUSH1 1 PUSH1 0 MSTORE PUSH1 32 PUSH1 0 RETURN
        assert_eq!(simulate(&hex!("600160005260206000f3")), TransferOutcome::Transferred);
        // STOP, as a token that returns no data.
        assert_eq!(simulate(&hex!("00")), TransferOutcome::Transferred);
        // PUSH1 32 PUSH1 0 RETURN: returns `false`.
        assert_eq!(simulate(&hex!("60206000f3")), TransferOutcome::Failed);
        // PUSH1 0 PUSH1 0 REVERT
        assert_eq!(simulate(&hex!("60006000fd")), TransferOutcome::Reverted(Bytes::new()));
        // INVALID
        assert_eq!(simulate(&hex!("fe")), TransferOutcome::Failed);
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
