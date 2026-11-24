//! Base's EVM2 execution type family.

use alloy_primitives::{Address, U256};
use base_common_l1_fees::L1FeeParams;
use evm2::{BaseEvmConfigSelector, Evm, EvmTypesHost};

use crate::{BaseSpecId, BaseTxEnvelope};

/// Base's EVM2 execution type family.
///
/// Wires Base's transaction envelope ([`BaseTxEnvelope`], including deposits),
/// L1 fee inputs (the engine-neutral [`L1FeeParams`] in the EVM extension), and L1-fee result data ([`BaseTxResultExt`]) into EVM2's
/// [`EvmTypesHost`]. The spec schedule is driven by the Base fork schedule
/// ([`BaseSpecId`]), which maps each Base upgrade to the governing EVM2 spec.
/// Not wired into the node.
#[derive(Clone, Copy, Debug)]
pub struct BaseEvmTypes;

impl EvmTypesHost for BaseEvmTypes {
    type ConfigSelector = BaseEvmConfigSelector;
    type SpecId = BaseSpecId;
    type Tx = BaseTxEnvelope;
    type EvmExt = BaseEvmExt;
    type MessageExt = ();
    type MessageResultExt = ();
    type TxEnvExt = ();
    type TxResultExt = BaseTxResultExt;
    type BlockEnvExt = ();
    type Host<'a> = Evm<'a, Self>;
}

/// Transaction-result extension data for Base transactions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BaseTxResultExt {
    /// L1 data fee charged for this transaction (zero for deposits).
    pub l1_fee: U256,
}

/// EVM instance-wide extension state for Base transactions.
///
/// Carries the L1 data fee and operator fee computed and charged in
/// [`BaseTxHandlerHooks::before_execution`](crate::BaseTxHandlerHooks) so
/// [`settle_transaction`](evm2::handler::TxHandlerHooks::settle_transaction) can
/// record/refund against the exact amounts charged without recomputing them —
/// keeping the charged and settled fees in lockstep and avoiding a second pass
/// over the transaction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BaseEvmExt {
    /// L1 fee parameters of the block, read from the `L1Block` predeploy before the block's first
    /// non-deposit transaction (see [`crate::L1BlockInfo`]).
    pub l1: L1FeeParams,
    /// `tx.origin` of the transaction currently executing, for native precompiles, which see
    /// only the EVM and the message. Set at the start of every transaction and system call.
    pub tx_origin: Address,
    /// L1 data fee charged for the transaction currently executing.
    pub l1_fee: U256,
    /// Operator fee charged upfront (on the gas limit) for the transaction currently executing;
    /// its unused portion is refunded at settlement.
    pub operator_fee: U256,
}
