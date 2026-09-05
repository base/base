//! Zenith EIP-8130 system-account stub transition.

use alloy_primitives::{Address, Bytes, KECCAK256_EMPTY, address};
use base_common_genesis::BaseUpgrade;
use evm2::{
    AccountInfo, Evm,
    bytecode::Bytecode,
    evm::BlockStateAccumulator,
    registry::{HandlerError, HandlerResult},
};

use crate::{BaseEvmTypes, BaseForkActivations, IrregularStateChange};

/// Single-byte code stub planted on otherwise code-less EIP-8130 system accounts.
///
/// `0xEF` is the EIP-3541 reserved prefix: it can never be produced by a normal `CREATE`/`CREATE2`
/// deployment, so it is an unambiguous "this is a protocol system account" sentinel. Its only job
/// is to give the account a non-empty code hash; the address is still serviced by the native
/// precompile, not by executing this byte.
const SYSTEM_ACCOUNT_STUB: [u8; 1] = [0xEF];

/// Code-less EIP-8130 system accounts that hold persistent storage but carry no code, and
/// therefore must be made non-empty so EIP-161 end-of-block state clearing does not reap them
/// together with their storage.
///
/// Only the `NonceManager` storage account qualifies (mirrors `base_common_precompiles`'
/// `NonceManagerStorage::ADDRESS`; hardcoded here to keep this crate revm-free): it persists the
/// 2D nonce channels in the state trie while never being a deployed contract.
const CODELESS_SYSTEM_ACCOUNTS: [Address; 1] =
    [address!("0x813000000000000000000000000000000000aa01")];

/// The Zenith EIP-8130 system-account transition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Zenith;

impl Zenith {
    /// The Zenith upgrade enables EIP-8130, whose enshrined execution path writes persistent state
    /// to system accounts that hold storage but carry no code, leaving them EIP-161-"empty" and
    /// liable to be reaped by end-of-block state clearing.
    ///
    /// This force-deploys a one-byte code stub onto those accounts so they are no longer empty. It
    /// is planted only on an account that has no code yet, so it never overwrites a real
    /// deployment, and it is idempotent: it fires on the first Zenith block and is a no-op after.
    pub fn ensure_eip8130_system_accounts(
        chain_spec: &impl BaseForkActivations,
        timestamp: u64,
        evm: &mut Evm<'_, BaseEvmTypes>,
        block_state: &mut BlockStateAccumulator,
    ) -> HandlerResult<()> {
        if !chain_spec.is_active_at_timestamp(BaseUpgrade::Zenith, timestamp) {
            return Ok(());
        }

        let stub = Bytecode::new_legacy(Bytes::from_static(&SYSTEM_ACCOUNT_STUB));
        let stub_hash = stub.hash_slow();

        for address in CODELESS_SYSTEM_ACCOUNTS {
            let original =
                evm.state_mut().account_info_untracked(&address).map_err(HandlerError::Fatal)?;

            // Skip if the account already carries code (real deployment, or the stub planted on a
            // previous block); only an empty-code account needs it.
            if original.as_ref().is_some_and(|info| info.code_hash != KECCAK256_EMPTY) {
                continue;
            }

            let base = original.clone().unwrap_or_default();
            let current = AccountInfo::new(base.balance, base.nonce, stub_hash, stub.clone());
            IrregularStateChange::new(address, original, Some(current)).apply(evm, block_state);
        }

        Ok(())
    }
}
