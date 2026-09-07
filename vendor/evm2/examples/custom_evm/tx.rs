//! Custom transaction envelope and registry handlers.

use alloy_eips::eip2718::Typed2718;
use alloy_primitives::{Address, Bytes};
use evm2::{
    bytecode::Bytecode,
    env::TxEnvExt,
    interpreter::{Host, MessageExt},
    registry::{HandlerResult, TxRegistry, TxRequest},
};

use crate::config::{
    CustomMessageExt, CustomMessageResultExt, CustomTxEnvExt, CustomTxResultExt, CustomTypes,
};

pub const EXECUTE_CODE_TX_TYPE: u8 = 0x7f;

#[derive(Debug)]
pub enum CustomEnvelope {
    ExecuteCode(ExecuteCodeTx),
}

impl Typed2718 for CustomEnvelope {
    fn ty(&self) -> u8 {
        match self {
            Self::ExecuteCode(tx) => tx.ty(),
        }
    }
}

impl CustomEnvelope {
    pub const fn as_execute_code(&self) -> Option<&ExecuteCodeTx> {
        match self {
            Self::ExecuteCode(tx) => Some(tx),
        }
    }
}

#[derive(Debug)]
pub struct ExecuteCodeTx {
    pub target: Address,
    pub code: Bytes,
    pub gas_limit: u64,
}

impl ExecuteCodeTx {
    pub const fn ty(&self) -> u8 {
        EXECUTE_CODE_TX_TYPE
    }
}

pub fn execute_code(
    req: TxRequest<'_, '_, CustomTypes, ExecuteCodeTx>,
) -> HandlerResult<evm2::TxResult<CustomTypes>> {
    // The transaction handler owns policy; the interpreter still executes a normal message.
    let mut message = MessageExt {
        gas_limit: req.tx.gas_limit,
        destination: req.tx.target,
        code: Bytecode::new_legacy(req.tx.code.clone()),
        code_address: req.tx.target,
        ext: CustomMessageExt { is_system: false },
        ..MessageExt::default()
    };
    let tx_env = TxEnvExt { ext: CustomTxEnvExt { label: "execute-code" }, ..TxEnvExt::default() };
    let mut result = req.host.execute_message(&tx_env, &mut message);
    result.ext = CustomMessageResultExt { handled_custom_message: true };
    Ok(evm2::TxResult::<CustomTypes> {
        status: result.stop.is_success(),
        total_gas_spent: req.tx.gas_limit - result.gas.remaining(),
        stop: result.stop,
        output: result.output,
        ext: CustomTxResultExt { handled_custom_tx: result.ext.handled_custom_message },
        ..Default::default()
    })
}

pub fn custom_registry() -> TxRegistry<CustomTypes, evm2::TxResult<CustomTypes>> {
    // The EIP-2718 type byte selects the typed extractor and handler.
    TxRegistry::new().with_handler(
        EXECUTE_CODE_TX_TYPE,
        CustomEnvelope::as_execute_code,
        execute_code,
    )
}
