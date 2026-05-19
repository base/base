use alloy_evm::Database;
use alloy_primitives::{Address, Bytes};
use base_common_chains::Upgrades;
use base_common_consensus::{
    ACCOUNT_CONFIG_ADDRESS, NONCE_MANAGER_ADDRESS, TX_CONTEXT_ADDRESS,
    is_account_config_known_deployed, mark_account_config_deployed,
};
use revm::{DatabaseCommit, primitives::HashMap, state::Bytecode};

/// Precompile addresses that need stub bytecode to prevent EIP-161 cleanup.
///
/// The protocol writes storage directly to these addresses (nonces, tx
/// context). Without code, EIP-161 state clearing would remove the accounts
/// and their storage after each transaction.
///
/// `AccountConfiguration` is NOT included — it is a real Solidity contract
/// deployed via CREATE2 by `deploy-8130.sh` (devnet) or upgrade deposit
/// transactions (mainnet). The node gates AA validation on its presence:
/// before deployment, only the implicit EOA rule applies.
const AA_PRECOMPILE_ADDRESSES: [Address; 2] = [NONCE_MANAGER_ADDRESS, TX_CONTEXT_ADDRESS];

/// Stub bytecode deployed to precompile addresses.
///
/// `0xFE` is the `INVALID` opcode -- any direct call reverts immediately.
/// The real logic is handled by the node as native precompiles in the EVM
/// handler. This stub ensures the accounts are non-empty under EIP-161,
/// preventing state cleanup from deleting their storage.
const AA_STUB_BYTECODE: &[u8] = &[0xFE];

/// The Base V1 hardfork issues an irregular state transition that force-deploys
/// stub bytecode to the EIP-8130 precompile addresses.
///
/// This mirrors `ensure_create2_deployer` for Canyon: code is set directly
/// via `DatabaseCommit` on the first block where the fork is active.
pub fn ensure_aa_predeploys<DB>(
    chain_spec: impl Upgrades,
    timestamp: u64,
    db: &mut DB,
) -> Result<(), DB::Error>
where
    DB: Database + DatabaseCommit,
{
    if !chain_spec.is_azul_active_at_timestamp(timestamp) && !is_account_config_deployed(db)? {
        return Ok(());
    }

    // Only deploy on the first BASE_V1 block, or if the sentinel
    // (NonceManager) still has no code. The second check handles
    // genesis-activated devnets where the first-block heuristic
    // (`timestamp - 2`) can't distinguish block 0 from block 1.
    let sentinel = db.basic(NONCE_MANAGER_ADDRESS)?;
    let already_deployed =
        sentinel.as_ref().is_some_and(|info| info.code_hash != revm::primitives::KECCAK_EMPTY);

    if already_deployed {
        return Ok(());
    }

    let code = Bytecode::new_raw(Bytes::from_static(AA_STUB_BYTECODE));
    let code_hash = code.hash_slow();

    let mut accounts = HashMap::default();
    for addr in AA_PRECOMPILE_ADDRESSES {
        let mut acc_info = db.basic(addr)?.unwrap_or_default();
        acc_info.code_hash = code_hash;
        acc_info.code = Some(code.clone());

        let mut revm_acc: revm::state::Account = acc_info.into();
        revm_acc.mark_touch();
        accounts.insert(addr, revm_acc);
    }

    db.commit(accounts);
    Ok(())
}

fn is_account_config_deployed<DB>(db: &mut DB) -> Result<bool, DB::Error>
where
    DB: Database,
{
    if is_account_config_known_deployed() {
        return Ok(true);
    }

    let deployed = db
        .basic(ACCOUNT_CONFIG_ADDRESS)?
        .is_some_and(|info| info.code_hash != revm::primitives::KECCAK_EMPTY);

    if deployed {
        mark_account_config_deployed();
    }

    Ok(deployed)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;
    use revm::{Database, database::InMemoryDB};

    use super::*;

    fn devnet_spec() -> base_common_chains::ChainUpgrades {
        base_common_chains::ChainUpgrades::devnet()
    }

    fn make_db() -> revm::database::State<InMemoryDB> {
        revm::database::State::builder().with_database(InMemoryDB::default()).build()
    }

    #[test]
    fn deploys_precompile_stubs_on_activation() {
        let mut db = make_db();
        let spec = devnet_spec();

        ensure_aa_predeploys(&spec, 0, &mut db).unwrap();

        for addr in AA_PRECOMPILE_ADDRESSES {
            let info = db.basic(addr).unwrap().expect("account should exist");
            assert!(info.code.is_some(), "code missing for {addr}");
            assert_eq!(&info.code.unwrap().original_bytes()[..], &[0xFE]);
        }
    }

    #[test]
    fn idempotent_when_already_deployed() {
        let mut db = make_db();
        let spec = devnet_spec();

        ensure_aa_predeploys(&spec, 0, &mut db).unwrap();

        let mut info = db.basic(NONCE_MANAGER_ADDRESS).unwrap().unwrap_or_default();
        info.balance = U256::from(42);
        db.insert_account(NONCE_MANAGER_ADDRESS, info);

        ensure_aa_predeploys(&spec, 2, &mut db).unwrap();
        let info = db.basic(NONCE_MANAGER_ADDRESS).unwrap().expect("account should exist");
        assert_eq!(info.balance, U256::from(42));
    }

    #[test]
    fn no_op_when_fork_inactive() {
        let mut db = make_db();
        let spec = base_common_chains::ChainUpgrades::mainnet();

        ensure_aa_predeploys(&spec, 0, &mut db).unwrap();

        let info = db.basic(NONCE_MANAGER_ADDRESS).unwrap();
        assert!(info.is_none());
    }
}
