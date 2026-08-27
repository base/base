//! Base's EVM2 execution type family.

use alloy_primitives::U256;
use evm2::{BaseEvmConfigSelector, Evm, EvmTypesHost, SpecId};

use crate::{L1BlockInfo, transaction::BaseTransaction};

/// Transaction-result extension data for Base transactions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BaseTxResultExt {
    /// L1 data fee charged for this transaction (zero for deposits).
    pub l1_fee: U256,
}

/// EVM instance-wide extension state for Base transactions.
///
/// Carries the L1 data fee computed and charged in
/// [`BaseTxHandlerHooks::before_execution`](crate::BaseTxHandlerHooks) so
/// [`settle_transaction`](evm2::handler::TxHandlerHooks::settle_transaction) can
/// record the exact same value on [`BaseTxResultExt::l1_fee`] without recomputing
/// it — this keeps the charged and recorded fees in lockstep and avoids a second
/// pass over the transaction calldata.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BaseEvmExt {
    /// L1 data fee charged for the transaction currently executing.
    pub l1_fee: U256,
}

/// Base's EVM2 execution type family.
///
/// Wires Base's transaction envelope ([`BaseTransaction`], including deposits),
/// L1 fee inputs ([`L1BlockInfo`] as the block-env extension), and L1-fee
/// result data ([`BaseTxResultExt`]) into EVM2's [`EvmTypesHost`]. The spec
/// schedule still uses the stock Ethereum selector; the Base fork schedule is
/// layered on in follow-up work. Not wired into the node.
#[derive(Clone, Copy, Debug)]
pub struct BaseEvmTypes;

impl EvmTypesHost for BaseEvmTypes {
    type ConfigSelector = BaseEvmConfigSelector;
    type SpecId = SpecId;
    type Tx = BaseTransaction;
    type EvmExt = BaseEvmExt;
    type MessageExt = ();
    type MessageResultExt = ();
    type TxEnvExt = ();
    type TxResultExt = BaseTxResultExt;
    type BlockEnvExt = L1BlockInfo;
    type Host<'a> = Evm<'a, Self>;
}
