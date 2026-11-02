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

/// The Everest upgrade enables EIP-8130. The enshrined execution path writes
/// persistent state (e.g. 2D nonce channels) to system accounts that hold
/// storage but carry no code, leaving them EIP-161-"empty" and liable to be
/// reaped — discarding that storage — by end-of-block state clearing.
///
/// This issues an irregular state transition at the Everest activation that
/// force-deploys a one-byte code stub onto those accounts, mirroring the Canyon
/// create2-deployer transition in [`ensure_create2_deployer`]. Once an account
/// has code it is no longer EIP-161-empty and survives clearing.
///
/// The stub is only planted on an account that has no code yet, so it never
/// overwrites a real deployment, and it is idempotent: it fires on the first
/// Everest block and is a no-op thereafter.
///
/// The account is converted from its untouched database info before the stub is
/// written, so the install is described as a code change in the block access
/// list (EIP-7928) rather than hidden as the account's own original state.
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
    if !chain_spec.is_everest_active_at_timestamp(timestamp) {
        return Ok(());
    }

    let stub = Bytecode::new_legacy(Bytes::from_static(&SYSTEM_ACCOUNT_STUB));
    let stub_hash = stub.hash_slow();

    let mut updates = HashMap::default();
    for address in CODELESS_SYSTEM_ACCOUNTS {
        let acc_info = db.basic(address)?.unwrap_or_default();

        // Skip if the account already carries code (real deployment, or the stub
        // planted on a previous block); only an empty-code account needs it.
        if !acc_info.is_empty_code_hash() {
            continue;
        }

        // Convert the *untouched* database account first: `Account::from` snapshots the
        // `AccountInfo` it is given as the account's `original_info`, so mutating `info`
        // afterwards is what registers the stub as a code change in the block access list.
        // Mutating before the conversion would make the stub its own "original" and hide
        // the transition from the BAL builder.
        let mut revm_acc: revm::state::Account = acc_info.into();
        revm_acc.info.code_hash = stub_hash;
        revm_acc.info.code = Some(stub.clone());
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
    use base_common_genesis::{BaseUpgrade, RollupConfig};
    use revm::{
        Database as _,
        database::{InMemoryDB, State},
        state::AccountInfo,
    };

    use super::*;

    const ADDR: Address = NonceManagerStorage::ADDRESS;

    /// Everest active: the code-less nonce manager is given the `0xEF` stub so it
    /// is no longer EIP-161-empty.
    #[test]
    fn everest_active_plants_stub_on_codeless_system_account() {
        let mut db = InMemoryDB::default();

        ensure_eip8130_system_accounts(everest(Some(0)), 100, &mut db).unwrap();

        let acc = db.basic(ADDR).unwrap().expect("system account must exist");
        assert!(!acc.is_empty_code_hash(), "the stub must give the account a non-empty code hash");
        assert_eq!(
            acc.code.as_ref().map(Bytecode::original_bytes),
            Some(Bytes::from_static(&SYSTEM_ACCOUNT_STUB)),
        );
    }

    /// Everest inactive: nothing is planted.
    #[test]
    fn everest_inactive_is_a_noop() {
        let mut db = InMemoryDB::default();

        ensure_eip8130_system_accounts(everest(None), 100, &mut db).unwrap();

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

        ensure_eip8130_system_accounts(everest(Some(0)), 100, &mut db).unwrap();

        let acc = db.basic(ADDR).unwrap().unwrap();
        assert_eq!(acc.code_hash, real.hash_slow(), "a real deployment must not be overwritten");
    }

    /// Everest activation must describe the stub install in the EIP-7928 block
    /// access list: the code-less nonce manager is converted from its untouched
    /// database info, so the `0xEF` stub is recorded as a code change instead of
    /// being snapshotted as the account's own original state (which would leave
    /// the transition invisible to the BAL builder).
    #[test]
    fn everest_activation_records_stub_code_change_in_bal() {
        let mut state =
            State::builder().with_database(InMemoryDB::default()).with_bal_builder().build();

        ensure_eip8130_system_accounts(everest(Some(0)), 100, &mut state).unwrap();

        let stub = Bytecode::new_legacy(Bytes::from_static(&SYSTEM_ACCOUNT_STUB));
        let bal = state.take_built_alloy_bal().expect("the BAL builder must produce a list");
        assert_eq!(bal.len(), 1, "exactly one account must be described by the activation BAL");

        let account = &bal[0];
        assert_eq!(account.address, ADDR, "the nonce manager must be the described account");
        assert_eq!(
            account.code_changes.len(),
            1,
            "the stub install must be recorded as exactly one code change",
        );
        assert_eq!(
            account.code_changes[0].new_code,
            Bytes::from_static(&SYSTEM_ACCOUNT_STUB),
            "the recorded code change must carry the 0xEF stub bytes",
        );
        assert_eq!(
            account.code_changes[0].block_access_index,
            revm::state::bal::BlockAccessIndex::PRE_EXECUTION,
            "the stub install must be recorded at the block's initial access index",
        );

        let installed = state.basic(ADDR).unwrap().expect("system account must exist");
        assert_eq!(
            installed.code_hash,
            stub.hash_slow(),
            "the installed code hash must be the stub's hash",
        );
    }

    /// The stub install is idempotent: a later Everest activation block, where the
    /// stub is already installed, must add no code change for the account.
    #[test]
    fn everest_stub_install_is_idempotent_in_bal() {
        let mut state =
            State::builder().with_database(InMemoryDB::default()).with_bal_builder().build();

        ensure_eip8130_system_accounts(everest(Some(0)), 100, &mut state).unwrap();
        let first = state.take_built_alloy_bal().expect("the first activation builds a list");
        assert_eq!(first.len(), 1, "the first activation must describe the stub install");
        assert_eq!(
            first[0].code_changes.len(),
            1,
            "the first activation must record exactly one code change",
        );

        // Re-arm the builder so the second activation's writes are observed in isolation.
        state.bal_state.bal_builder = Some(revm::state::bal::Bal::new());

        ensure_eip8130_system_accounts(everest(Some(0)), 200, &mut state).unwrap();
        let second = state.take_built_alloy_bal().expect("the BAL builder must produce a list");
        assert!(
            second.is_empty(),
            "a block whose stub is already installed must record no account changes",
        );
    }

    fn everest(timestamp: Option<u64>) -> RollupConfig {
        let mut config = RollupConfig::default();
        if let Some(timestamp) = timestamp {
            config.set_upgrade_activation_timestamp(BaseUpgrade::Everest, timestamp);
        }
        config
    }
}
