//! Custom opcode definitions.

use evm2::{
    interpreter::{Host, Word},
    version::{GasId, GasParams},
};
use evm2_macros::instruction;

use crate::config::CustomTypes;

pub const CUSTOM_OPCODE: u8 = 0x0c;
pub const CUSTOM_OPCODE_GAS: u16 = 7;
pub const CUSTOM_OPCODE_DYNAMIC_GAS_ID: GasId = GasId::Custom0;
pub const CUSTOM_OPCODE_DYNAMIC_GAS: u32 = 3;
pub const L1_BLOCKNUMBER_OPCODE: u8 = 0x0d;
pub const L1_BLOCKNUMBER_GAS: u16 = 2;

pub const fn install_gas_params(gas_params: &mut GasParams) {
    gas_params.set(CUSTOM_OPCODE_DYNAMIC_GAS_ID, CUSTOM_OPCODE_DYNAMIC_GAS);
}

// Static gas comes from the instruction table; the dynamic part comes from the active version.
#[instruction(dynamic_gas)]
pub fn custom(cx: _) -> Result<out> {
    cx.gas.spend(cx.state.gas_params().get(CUSTOM_OPCODE_DYNAMIC_GAS_ID).into())?;
    *out = Word::from(0xdead_u64);
}

#[instruction(EvmTypes = CustomTypes)]
pub fn l1_blocknumber(cx: _) -> out {
    *out = Word::from(cx.state.host().block_env().ext.l1_block_number);
}
