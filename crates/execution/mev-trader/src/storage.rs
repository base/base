use std::collections::BTreeSet;

use alloy_primitives::U256;
use revm::{database_interface::Database, state::EvmState};

use crate::{AuditedWriteKey, PortError};

/// One immutable value materialized after victim execution commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaterializedWrite {
    /// Registry-audited key represented by this value.
    pub key: AuditedWriteKey,
    /// Post-victim value for the audited key.
    pub value: U256,
}

/// Provider-free immutable state passed to later analysis stages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedState {
    /// Canonically ordered post-victim values.
    pub writes: Vec<MaterializedWrite>,
}

/// Validates that execution changed only registry-audited keys.
#[derive(Debug, Default, Clone, Copy)]
pub struct DeltaGuard;

impl DeltaGuard {
    /// Returns whether every changed key is uniquely authorized and contract code is unchanged.
    pub fn permits(state: &EvmState, audited_writes: &[AuditedWriteKey]) -> bool {
        let unique: BTreeSet<_> = audited_writes.iter().copied().collect();
        if unique.len() != audited_writes.len()
            || audited_writes.iter().any(|key| key.evidence_digest().is_zero())
        {
            return false;
        }

        for (address, account) in state {
            let original = account.original_info();
            if account.info.code_hash != original.code_hash {
                return false;
            }
            if account.info.balance != original.balance
                && !unique.iter().any(|key| {
                    matches!(key, AuditedWriteKey::AccountBalance { address: allowed, .. } if allowed == address)
                })
            {
                return false
            }
            if account.info.nonce != original.nonce
                && !unique.iter().any(|key| {
                    matches!(key, AuditedWriteKey::AccountNonce { address: allowed, .. } if allowed == address)
                })
            {
                return false
            }
            for (slot, _) in account.changed_storage_slots() {
                if !unique.iter().any(|key| {
                    matches!(key, AuditedWriteKey::Storage { address: allowed, slot: allowed_slot, .. } if allowed == address && allowed_slot == slot)
                }) {
                    return false
                }
            }
        }
        true
    }
}

/// Materializes audited post-commit values into provider-free storage.
#[derive(Debug, Default, Clone, Copy)]
pub struct StateMaterializer;

impl StateMaterializer {
    /// Reads every audited key after the sole database commit and returns canonical values.
    pub fn materialize<DB: Database>(
        database: &mut DB,
        audited_writes: &[AuditedWriteKey],
    ) -> Result<MaterializedState, PortError> {
        let mut keys = audited_writes.to_vec();
        keys.sort_unstable();
        keys.dedup();
        if keys.len() != audited_writes.len() {
            return Err(PortError::Incoherent);
        }

        let mut writes = Vec::with_capacity(keys.len());
        for key in keys {
            let value = match key {
                AuditedWriteKey::AccountBalance { address, .. } => database
                    .basic(address)
                    .map_err(|_| PortError::ProviderUnavailable)?
                    .map_or(U256::ZERO, |account| account.balance),
                AuditedWriteKey::AccountNonce { address, .. } => database
                    .basic(address)
                    .map_err(|_| PortError::ProviderUnavailable)?
                    .map_or(U256::ZERO, |account| U256::from(account.nonce)),
                AuditedWriteKey::Storage { address, slot, .. } => {
                    database.storage(address, slot).map_err(|_| PortError::ProviderUnavailable)?
                }
            };
            writes.push(MaterializedWrite { key, value });
        }
        Ok(MaterializedState { writes })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256};
    use revm::{database_interface::DatabaseCommit, state::Account};
    use revm_database::InMemoryDB;

    use super::*;

    fn evidence() -> B256 {
        B256::with_last_byte(1)
    }

    #[test]
    fn delta_guard_rejects_write_outside_audited_subset() {
        let address = Address::with_last_byte(1);
        let mut account = Account::default();
        account.set_current_info_as_original();
        account.info.balance = U256::from(2);
        account.mark_touch();
        let state: EvmState = [(address, account)].into_iter().collect();

        assert!(!DeltaGuard::permits(&state, &[]));
        assert!(DeltaGuard::permits(
            &state,
            &[AuditedWriteKey::AccountBalance { address, evidence_digest: evidence() }]
        ));
    }

    #[test]
    fn materializer_reads_only_after_commit() {
        let address = Address::with_last_byte(2);
        let key = AuditedWriteKey::AccountBalance { address, evidence_digest: evidence() };
        let mut account = Account::default();
        account.info.balance = U256::from(7);
        account.mark_touch();
        let mut database = InMemoryDB::default();

        database.commit([(address, account)].into_iter().collect());
        let materialized =
            StateMaterializer::materialize(&mut database, &[key]).expect("materialize");

        assert_eq!(materialized.writes, vec![MaterializedWrite { key, value: U256::from(7) }]);
    }

    #[test]
    fn materialized_state_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MaterializedState>();
    }
}
