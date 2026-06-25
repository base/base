use alloy_evm::Database;
use alloy_primitives::{Address, Bytes};
use base_common_chains::Upgrades;
use base_common_precompiles::NonceManagerStorage;
use revm::{DatabaseCommit, primitives::HashMap, state::Bytecode};

/// Single-byte code stub planted on otherwise code-less EIP-8130 system accounts.
///
/// `0xEF` is the EIP-3541 reserved prefix: it can never be produced by a normal
/// `CREATE`/`CREATE2` deployment, so it is an unambiguous "this is a protocol
/// system account" sentinel. Its only job is to give the account a non-empty
/// code hash; the address is still serviced by the native precompile, not by
/// executing this byte.
const SYSTEM_ACCOUNT_STUB: [u8; 1] = [0xEF];

/// Code-less EIP-8130 system accounts that hold persistent storage but carry no
/// code on any chain, and therefore must be made non-empty so EIP-161
/// end-of-block state clearing does not reap them together with their storage.
///
/// Only the [`NonceManager`](NonceManagerStorage) qualifies: it persists the 2D
/// nonce channels in the state trie while never being a deployed contract. The
/// transaction-context precompile (`0x8130…aa02`) uses transient storage only
/// (cleared every transaction, never trie-resident) so it needs no stub, and
/// `AccountConfiguration` is a genuinely deployed contract (it carries code) on
/// every chain where EIP-8130 is enabled.
const CODELESS_SYSTEM_ACCOUNTS: [Address; 1] = [NonceManagerStorage::ADDRESS];

/// The Cobalt upgrade enables EIP-8130. The enshrined execution path writes
/// persistent state (e.g. 2D nonce channels) to system accounts that hold
/// storage but carry no code, leaving them EIP-161-"empty" and liable to be
/// reaped — discarding that storage — by end-of-block state clearing.
///
/// This issues an irregular state transition at the Cobalt activation that
/// force-deploys a one-byte code stub onto those accounts, mirroring the Canyon
/// create2-deployer transition in [`ensure_create2_deployer`]. Once an account
/// has code it is no longer EIP-161-empty and survives clearing.
///
/// The stub is only planted on an account that has no code yet, so it never
/// overwrites a real deployment, and it is idempotent: it fires on the first
/// Cobalt block and is a no-op thereafter.
///
/// [`ensure_create2_deployer`]: crate::ensure_create2_deployer
pub fn ensure_eip8130_system_accounts<DB>(
    chain_spec: impl Upgrades,
    timestamp: u64,
    db: &mut DB,
) -> Result<(), DB::Error>
where
    DB: Database + DatabaseCommit,
{
    if !chain_spec.is_cobalt_active_at_timestamp(timestamp) {
        return Ok(());
    }

    let stub = Bytecode::new_legacy(Bytes::from_static(&SYSTEM_ACCOUNT_STUB));
    let stub_hash = stub.hash_slow();

    let mut updates = HashMap::default();
    for address in CODELESS_SYSTEM_ACCOUNTS {
        let mut acc_info = db.basic(address)?.unwrap_or_default();

        // Skip if the account already carries code (real deployment, or the stub
        // planted on a previous block); only an empty-code account needs it.
        if !acc_info.is_empty_code_hash() {
            continue;
        }

        acc_info.code_hash = stub_hash;
        acc_info.code = Some(stub.clone());

        let mut revm_acc: revm::state::Account = acc_info.into();
        revm_acc.mark_touch();
        updates.insert(address, revm_acc);
    }

    if !updates.is_empty() {
        db.commit(updates);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy_hardforks::ForkCondition;
    use base_common_chains::{BaseUpgradeExt, ChainUpgrades};
    use base_common_genesis::BaseUpgrade;
    use revm::{Database as _, database::InMemoryDB, state::AccountInfo};

    use super::*;

    const ADDR: Address = NonceManagerStorage::ADDRESS;

    /// Cobalt active: the code-less nonce manager is given the `0xEF` stub so it
    /// is no longer EIP-161-empty.
    #[test]
    fn cobalt_active_plants_stub_on_codeless_system_account() {
        let mut db = InMemoryDB::default();

        ensure_eip8130_system_accounts(cobalt(ForkCondition::Timestamp(0)), 100, &mut db).unwrap();

        let acc = db.basic(ADDR).unwrap().expect("system account must exist");
        assert!(!acc.is_empty_code_hash(), "the stub must give the account a non-empty code hash");
        assert_eq!(
            acc.code.as_ref().map(Bytecode::original_bytes),
            Some(Bytes::from_static(&SYSTEM_ACCOUNT_STUB)),
        );
    }

    /// Cobalt inactive: nothing is planted.
    #[test]
    fn cobalt_inactive_is_a_noop() {
        let mut db = InMemoryDB::default();

        ensure_eip8130_system_accounts(cobalt(ForkCondition::Never), 100, &mut db).unwrap();

        assert!(db.basic(ADDR).unwrap().is_none(), "no system account should be materialized");
    }

    /// An account that already has code (a real deployment) is left untouched.
    #[test]
    fn existing_code_is_not_overwritten() {
        let mut db = InMemoryDB::default();
        let real = Bytecode::new_raw(Bytes::from_static(&[0x60, 0x00]));
        db.insert_account_info(
            ADDR,
            AccountInfo {
                code_hash: real.hash_slow(),
                code: Some(real.clone()),
                ..Default::default()
            },
        );

        ensure_eip8130_system_accounts(cobalt(ForkCondition::Timestamp(0)), 100, &mut db).unwrap();

        let acc = db.basic(ADDR).unwrap().unwrap();
        assert_eq!(acc.code_hash, real.hash_slow(), "a real deployment must not be overwritten");
    }

    fn cobalt(condition: ForkCondition) -> ChainUpgrades {
        ChainUpgrades::new(BaseUpgrade::devnet().into_iter().map(move |(fork, cond)| {
            if fork == BaseUpgrade::Cobalt { (fork, condition) } else { (fork, cond) }
        }))
    }
}
