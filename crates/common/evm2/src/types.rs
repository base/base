//! Base's EVM2 execution type family.

use evm2::{BaseEvmConfigSelector, Evm, EvmTypesHost, SpecId, ethereum::TxEnvelope};

/// Base's EVM2 execution type family.
///
/// Scaffold anchor for the Base EVM2 integration. It currently mirrors the
/// stock Ethereum configuration; Base-specific customization is layered on here
/// in follow-up work:
///
/// - deposit and EIP-8130 transactions via a `TxRegistry` (`Tx`),
/// - OP L1 fee settlement and intrinsic-gas adjustment via `TxHandlerHooks`,
/// - the Base spec schedule via `SpecId` / `ConfigSelector`,
/// - L1 block info via `BlockEnvExt`.
///
/// It is intentionally not wired into the node.
#[derive(Clone, Copy, Debug)]
pub struct BaseEvmTypes;

impl EvmTypesHost for BaseEvmTypes {
    type ConfigSelector = BaseEvmConfigSelector;
    type SpecId = SpecId;
    type Tx = TxEnvelope;
    type EvmExt = ();
    type MessageExt = ();
    type MessageResultExt = ();
    type TxEnvExt = ();
    type TxResultExt = ();
    type BlockEnvExt = ();
    type Host<'a> = Evm<'a, Self>;
}
