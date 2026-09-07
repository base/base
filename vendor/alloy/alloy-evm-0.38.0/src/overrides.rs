//! RPC utilities for working with EVM.
//!
//! This module provides helper functions for RPC implementations, including:
//! - Block and state overrides

use alloc::collections::BTreeMap;
use alloy_primitives::{keccak256, map::HashMap, Address, B256, U256};
use alloy_rpc_types_eth::{
    state::{AccountOverride, StateOverride},
    BlockOverrides,
};
use revm::{
    bytecode::BytecodeDecodeError,
    context::BlockEnv,
    context_interface::block::BlobExcessGasAndPrice,
    database::{CacheDB, State},
    state::{Account, AccountStatus, Bytecode, EvmStorageSlot, TransactionId},
    Database, DatabaseCommit,
};

/// Errors that can occur when applying state overrides.
#[derive(Debug, thiserror::Error)]
pub enum StateOverrideError<E> {
    /// Invalid bytecode provided in override.
    #[error(transparent)]
    InvalidBytecode(#[from] BytecodeDecodeError),
    /// Both state and state_diff were provided for an account.
    #[error("Both 'state' and 'stateDiff' fields are set for account {0}")]
    BothStateAndStateDiff(Address),
    /// Database error occurred.
    #[error(transparent)]
    Database(E),
}

/// Helper trait implemented for databases that support overriding block hashes.
///
/// Used for applying [`BlockOverrides::block_hash`]
pub trait OverrideBlockHashes {
    /// Overrides the given block hashes.
    fn override_block_hashes(&mut self, block_hashes: BTreeMap<u64, B256>);

    /// Applies the given block overrides to the env and updates overridden block hashes.
    fn apply_block_overrides(&mut self, overrides: BlockOverrides, env: &mut BlockEnv)
    where
        Self: Sized,
    {
        apply_block_overrides(overrides, self, env);
    }
}

impl<DB> OverrideBlockHashes for CacheDB<DB> {
    fn override_block_hashes(&mut self, block_hashes: BTreeMap<u64, B256>) {
        self.cache
            .block_hashes
            .extend(block_hashes.into_iter().map(|(num, hash)| (U256::from(num), hash)))
    }
}

impl<DB> OverrideBlockHashes for State<DB> {
    fn override_block_hashes(&mut self, block_hashes: BTreeMap<u64, B256>) {
        for (number, hash) in block_hashes {
            self.block_hashes.insert(number, hash);
        }
    }
}

/// Applies the given block overrides to the env and updates overridden block hashes in the db.
pub fn apply_block_overrides<DB>(overrides: BlockOverrides, db: &mut DB, env: &mut BlockEnv)
where
    DB: OverrideBlockHashes,
{
    #[allow(clippy::needless_update)]
    let BlockOverrides {
        number,
        difficulty,
        time,
        gas_limit,
        coinbase,
        random,
        base_fee,
        blob_base_fee,
        block_hash,
        ..
    } = BlockOverrides { ..overrides };

    if let Some(block_hashes) = block_hash {
        // override block hashes
        db.override_block_hashes(block_hashes);
    }

    if let Some(number) = number {
        env.number = number.saturating_to();
    }
    if let Some(difficulty) = difficulty {
        env.difficulty = difficulty;
    }
    if let Some(time) = time {
        env.timestamp = U256::from(time);
    }
    if let Some(gas_limit) = gas_limit {
        env.gas_limit = gas_limit;
    }
    if let Some(coinbase) = coinbase {
        env.beneficiary = coinbase;
    }
    if let Some(random) = random {
        env.prevrandao = Some(random);
    }
    if let Some(base_fee) = base_fee {
        env.basefee = base_fee.saturating_to();
    }
    if let Some(blob_base_fee) = blob_base_fee {
        let excess_blob_gas =
            env.blob_excess_gas_and_price.map(|blob| blob.excess_blob_gas).unwrap_or_default();
        env.blob_excess_gas_and_price = Some(BlobExcessGasAndPrice {
            excess_blob_gas,
            blob_gasprice: blob_base_fee.saturating_to(),
        });
    }
}

/// Applies the given state overrides (a set of [`AccountOverride`]) to the database.
pub fn apply_state_overrides<DB>(
    overrides: StateOverride,
    db: &mut DB,
) -> Result<(), StateOverrideError<DB::Error>>
where
    DB: Database + DatabaseCommit,
{
    for (account, account_overrides) in overrides {
        apply_account_override(account, account_overrides, db)?;
    }
    Ok(())
}

/// Applies a single [`AccountOverride`] to the database.
fn apply_account_override<DB>(
    account: Address,
    account_override: AccountOverride,
    db: &mut DB,
) -> Result<(), StateOverrideError<DB::Error>>
where
    DB: Database + DatabaseCommit,
{
    let mut info = db.basic(account).map_err(StateOverrideError::Database)?.unwrap_or_default();

    if let Some(nonce) = account_override.nonce {
        info.nonce = nonce;
    }
    if let Some(code) = account_override.code {
        // we need to set both the bytecode and the codehash
        info.code_hash = keccak256(&code);
        info.code = Some(Bytecode::new_raw_checked(code)?);
    }
    if let Some(balance) = account_override.balance {
        info.balance = balance;
    }

    // Create a new account marked as touched
    let mut acc = revm::state::Account::from(info);
    acc.status = AccountStatus::Touched;

    let storage_diff = match (account_override.state, account_override.state_diff) {
        (Some(_), Some(_)) => return Err(StateOverrideError::BothStateAndStateDiff(account)),
        (None, None) => None,
        // If we need to override the entire state, we firstly mark account as destroyed to clear
        // its storage, and then we mark it is "NewlyCreated" to make sure that old storage won't be
        // used.
        (Some(state), None) => {
            // Destroy the account to ensure that its storage is cleared
            let mut destroyed = Account::default();
            destroyed.status = AccountStatus::SelfDestructed | AccountStatus::Touched;
            db.commit(HashMap::from_iter([(account, destroyed)]));
            // Mark the account as created to ensure that old storage is not read
            acc.mark_created();
            Some(state)
        }
        (None, Some(state)) => {
            // If the account is empty/non-existent, mark it as Created so that
            // State::commit() preserves the storage via newly_created() instead
            // of discarding it via touch_empty_eip161() (EIP-161 state clear).
            // Without this, stateDiff storage for empty accounts is silently
            // dropped because the commit path sees an empty touched account and
            // treats it as a no-op state clear.
            if acc.info.is_empty() && !state.is_empty() {
                acc.mark_created();
            }
            Some(state)
        }
    };

    if let Some(state) = storage_diff {
        for (slot, value) in state {
            acc.storage.insert(
                slot.into(),
                EvmStorageSlot {
                    // we use inverted value here to ensure that storage is treated as changed
                    original_value: (!value).into(),
                    present_value: value.into(),
                    is_cold: false,
                    transaction_id: TransactionId::ZERO,
                },
            );
        }
    }

    db.commit(HashMap::from_iter([(account, acc)]));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, bytes};
    use revm::database::EmptyDB;

    #[test]
    fn test_block_override_blob_base_fee() {
        let mut db = CacheDB::new(EmptyDB::new());
        let mut env = BlockEnv {
            blob_excess_gas_and_price: Some(BlobExcessGasAndPrice {
                excess_blob_gas: 42,
                blob_gasprice: 1,
            }),
            ..Default::default()
        };

        let overrides = BlockOverrides::default().with_blob_base_fee(U256::from(12345));
        apply_block_overrides(overrides, &mut db, &mut env);

        let blob = env.blob_excess_gas_and_price.unwrap();
        assert_eq!(blob.excess_blob_gas, 42);
        assert_eq!(blob.blob_gasprice, 12345);
    }

    #[test]
    fn test_state_override_state() {
        let code = bytes!(
            "0x63d0e30db05f525f5f6004601c3473c02aaa39b223fe8d0a0e5c4f27ead9083c756cc25af15f5260205ff3"
        );
        let to = address!("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");

        let mut db = State::builder().with_database(CacheDB::new(EmptyDB::new())).build();

        let acc_override = AccountOverride::default().with_code(code.clone());
        apply_account_override(to, acc_override, &mut db).unwrap();

        let account = db.basic(to).unwrap().unwrap();
        assert!(account.code.is_some());
        assert_eq!(account.code_hash, keccak256(&code));
    }

    #[test]
    fn test_state_override_cache_db() {
        let code = bytes!(
            "0x63d0e30db05f525f5f6004601c3473c02aaa39b223fe8d0a0e5c4f27ead9083c756cc25af15f5260205ff3"
        );
        let to = address!("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");

        let mut db = CacheDB::new(EmptyDB::new());

        let acc_override = AccountOverride::default().with_code(code.clone());
        apply_account_override(to, acc_override, &mut db).unwrap();

        let account = db.basic(to).unwrap().unwrap();
        assert!(account.code.is_some());
        assert_eq!(account.code_hash, keccak256(&code));
    }

    #[test]
    fn test_state_override_storage() {
        let account = address!("0x1234567890123456789012345678901234567890");
        let slot1 = B256::from(U256::from(1));
        let slot2 = B256::from(U256::from(2));
        let value1 = B256::from(U256::from(100));
        let value2 = B256::from(U256::from(200));

        let mut db = CacheDB::new(EmptyDB::new());

        // Create storage overrides
        let mut storage = HashMap::<B256, B256>::default();
        storage.insert(slot1, value1);
        storage.insert(slot2, value2);

        let acc_override = AccountOverride::default().with_state_diff(storage);
        apply_account_override(account, acc_override, &mut db).unwrap();

        // Get the storage value using the database interface
        let storage1 = db.storage(account, U256::from(1)).unwrap();
        let storage2 = db.storage(account, U256::from(2)).unwrap();

        assert_eq!(storage1, U256::from(100));
        assert_eq!(storage2, U256::from(200));
    }

    #[test]
    fn test_state_override_full_state() {
        let account = address!("0x1234567890123456789012345678901234567890");
        let slot1 = B256::from(U256::from(1));
        let slot2 = B256::from(U256::from(2));
        let value1 = B256::from(U256::from(100));
        let value2 = B256::from(U256::from(200));

        let mut db = State::builder().with_database(CacheDB::new(EmptyDB::new())).build();

        // Create storage overrides using state (not state_diff)
        let mut storage = HashMap::<B256, B256>::default();
        storage.insert(slot1, value1);
        storage.insert(slot2, value2);

        let acc_override = AccountOverride::default().with_state(storage);
        let mut state_overrides = StateOverride::default();
        state_overrides.insert(account, acc_override);
        apply_state_overrides(state_overrides, &mut db).unwrap();

        // Get the storage value using the database interface
        let storage1 = db.storage(account, U256::from(1)).unwrap();
        let storage2 = db.storage(account, U256::from(2)).unwrap();

        assert_eq!(storage1, U256::from(100));
        assert_eq!(storage2, U256::from(200));
    }

    /// Regression test: stateDiff on a non-existent account must preserve storage
    /// when using `State<DB>` (not just `CacheDB`).
    ///
    /// Before the fix, `State::commit()` would discard the storage for empty accounts
    /// via `touch_empty_eip161()` because the account info was empty and the EIP-161
    /// state clear path was triggered.
    #[test]
    fn test_state_diff_empty_account_state_db() {
        let account = address!("0x1234567890123456789012345678901234567890");
        let slot1 = B256::from(U256::from(1));
        let slot2 = B256::from(U256::from(2));
        let value1 = B256::from(U256::from(100));
        let value2 = B256::from(U256::from(200));

        let mut db = State::builder().with_database(CacheDB::new(EmptyDB::new())).build();

        // Apply stateDiff to a non-existent account
        let mut storage = HashMap::<B256, B256>::default();
        storage.insert(slot1, value1);
        storage.insert(slot2, value2);

        let acc_override = AccountOverride::default().with_state_diff(storage);
        apply_account_override(account, acc_override, &mut db).unwrap();

        // Storage must survive State::commit() — this was the bug
        let storage1 = db.storage(account, U256::from(1)).unwrap();
        let storage2 = db.storage(account, U256::from(2)).unwrap();

        assert_eq!(storage1, U256::from(100), "stateDiff slot 1 lost in State<DB> commit");
        assert_eq!(storage2, U256::from(200), "stateDiff slot 2 lost in State<DB> commit");
    }

    /// Regression test for reth issue #22622: `debug_traceCall` with `stateOverrides` produces
    /// different execution results than `eth_call`.
    ///
    /// Reproduces the scenario where a `stateDiff` override pre-populates storage for an
    /// address that is later deployed to via CREATE2. The initcode SSTOREs a value, and a
    /// subsequent STATICCALL SLOADs it back.
    ///
    /// Tests multiple configurations to isolate the bug:
    /// - Basic `State<CacheDB<EmptyDB>>` (no bundle update)
    /// - `State` with `with_bundle_update()` (as reth uses)
    /// - Inspector that loads only the caller (mimicking TracingInspector)
    /// - Inspector that loads the created address (testing journal preloading)
    /// - Using `EthEvmFactory` (matching reth's actual EVM construction)
    #[test]
    fn test_create2_state_diff_inspect_bug() {
        use alloy_primitives::{Bytes, TxKind};
        use revm::{
            context::{Context, Journal, TxEnv},
            context_interface::{ContextTr, JournalTr},
            database_interface::EmptyDB,
            inspector::Inspector,
            interpreter::{CreateInputs, CreateOutcome},
            primitives::hardfork::SpecId,
            state::AccountInfo,
            ExecuteEvm, InspectEvm, MainBuilder,
        };

        type TestDb = State<CacheDB<EmptyDB>>;
        type TestCtx =
            Context<revm::context::BlockEnv, TxEnv, revm::context::CfgEnv, TestDb, Journal<TestDb>>;

        // --- Bytecode construction ---

        // Runtime code: SLOAD(0), MSTORE(0), RETURN(0, 32)
        // Reads storage slot 0 and returns it as 32 bytes
        let runtime_code: &[u8] = &[
            0x60, 0x00, // PUSH1 0x00 (slot)
            0x54, //       SLOAD
            0x60, 0x00, // PUSH1 0x00 (mem offset)
            0x52, //       MSTORE
            0x60, 0x20, // PUSH1 0x20 (size)
            0x60, 0x00, // PUSH1 0x00 (offset)
            0xf3, //       RETURN
        ]; // 11 bytes

        // Initcode: SSTORE(0, 0xBEEF), CODECOPY runtime, RETURN runtime
        let initcode_instrs: &[u8] = &[
            0x61, 0xBE, 0xEF, // PUSH2 0xBEEF
            0x60, 0x00, //       PUSH1 0x00 (slot)
            0x55, //             SSTORE
            0x60, 0x0b, //      PUSH1 11 (runtime size)
            0x60, 0x12, //      PUSH1 18 (offset of runtime in initcode)
            0x60, 0x00, //      PUSH1 0x00 (memory dest)
            0x39, //             CODECOPY
            0x60, 0x0b, //      PUSH1 11 (return size)
            0x60, 0x00, //      PUSH1 0x00 (return offset)
            0xf3, //             RETURN
        ]; // 18 bytes
        assert_eq!(initcode_instrs.len(), 18);
        let initcode: Bytes = [initcode_instrs, runtime_code].concat().into(); // 29 bytes

        let deployer_addr = address!("0x1000000000000000000000000000000000000001");
        let create2_addr = deployer_addr.create2_from_code(B256::ZERO, &initcode);

        // Deployer code: CODECOPY initcode → CREATE2 → STATICCALL created addr → RETURN
        let deployer_instrs: Vec<u8> = vec![
            // CODECOPY(destOffset=0, offset=34, size=29)
            0x60, 0x1d, // PUSH1 29  (initcode size)
            0x60, 0x22, // PUSH1 34  (offset = deployer instr len)
            0x60, 0x00, // PUSH1 0   (dest)
            0x39, //       CODECOPY
            // CREATE2(value=0, offset=0, size=29, salt=0)
            0x60, 0x00, // PUSH1 0   (salt)
            0x60, 0x1d, // PUSH1 29  (size)
            0x60, 0x00, // PUSH1 0   (offset)
            0x60, 0x00, // PUSH1 0   (value)
            0xf5, //       CREATE2
            // Stack: [created_addr]
            // STATICCALL(gas, addr, argsOffset=0, argsLen=0, retOffset=0, retLen=32)
            0x60, 0x20, // PUSH1 32  (retLength)
            0x60, 0x00, // PUSH1 0   (retOffset)
            0x60, 0x00, // PUSH1 0   (argsLength)
            0x60, 0x00, // PUSH1 0   (argsOffset)
            0x84, //       DUP5      (copy created_addr)
            0x5a, //       GAS
            0xfa, //       STATICCALL
            // Return memory[0..32]
            0x50, //       POP (success)
            0x50, //       POP (created_addr)
            0x60, 0x20, // PUSH1 32
            0x60, 0x00, // PUSH1 0
            0xf3, //       RETURN
        ]; // 34 bytes
        assert_eq!(deployer_instrs.len(), 34);
        let deployer_code: Bytes = [deployer_instrs.as_slice(), &initcode].concat().into();

        let caller = address!("0x0000000000000000000000000000000000000099");

        // --- Helper: build a fresh database with deployer + stateDiff override ---
        let build_db = |with_bundle: bool| {
            let mut cache_db = CacheDB::new(EmptyDB::new());

            // Insert deployer contract
            cache_db.insert_account_info(
                deployer_addr,
                AccountInfo {
                    code_hash: keccak256(&deployer_code),
                    code: Some(
                        revm::state::Bytecode::new_raw_checked(deployer_code.clone()).unwrap(),
                    ),
                    nonce: 1,
                    ..Default::default()
                },
            );

            // Insert caller with balance
            cache_db.insert_account_info(
                caller,
                AccountInfo {
                    balance: U256::from(1_000_000_000_000_000_000u128),
                    ..Default::default()
                },
            );

            let mut builder = State::builder().with_database(cache_db);
            if with_bundle {
                builder = builder.with_bundle_update();
            }
            let mut db = builder.build();

            // Apply stateDiff override on the CREATE2 target address, pre-populating
            // storage. This is the key setup: the address will have AccountStatus::Touched
            // from this commit, and then CREATE2 will deploy to it.
            let mut storage = HashMap::<B256, B256>::default();
            storage.insert(B256::from(U256::from(42)), B256::from(U256::from(999)));
            let acc_override = AccountOverride::default().with_state_diff(storage);
            apply_account_override(create2_addr, acc_override, &mut db).unwrap();

            db
        };

        let build_tx = || {
            TxEnv::builder()
                .gas_limit(1_000_000)
                .gas_price(0)
                .caller(caller)
                .kind(TxKind::Call(deployer_addr))
                .build()
                .unwrap()
        };

        /// Inspector that calls `journal.load_account(caller)` during create(),
        /// mimicking `TracingInspector`'s behavior.
        struct LoadCallerOnCreate;

        impl<CTX: ContextTr> Inspector<CTX> for LoadCallerOnCreate {
            fn create(
                &mut self,
                context: &mut CTX,
                inputs: &mut CreateInputs,
            ) -> Option<CreateOutcome> {
                let _ = context.journal_mut().load_account(inputs.caller());
                None
            }
        }

        /// Inspector that loads the CREATED address during create(),
        /// testing if pre-loading the target into the journal triggers the bug.
        struct LoadCreatedAddrOnCreate {
            create2_addr: Address,
        }

        impl<CTX: ContextTr> Inspector<CTX> for LoadCreatedAddrOnCreate {
            fn create(
                &mut self,
                context: &mut CTX,
                _inputs: &mut CreateInputs,
            ) -> Option<CreateOutcome> {
                // Load the CREATE2 target address into the journal BEFORE
                // create_account_checkpoint runs — this pre-warms the target.
                let _ = context.journal_mut().load_account(self.create2_addr);
                None
            }
        }

        /// Inspector that loads BOTH the caller and the created address,
        /// testing the combined effect.
        struct LoadBothOnCreate {
            create2_addr: Address,
        }

        impl<CTX: ContextTr> Inspector<CTX> for LoadBothOnCreate {
            fn create(
                &mut self,
                context: &mut CTX,
                inputs: &mut CreateInputs,
            ) -> Option<CreateOutcome> {
                let _ = context.journal_mut().load_account(inputs.caller());
                let _ = context.journal_mut().load_account(self.create2_addr);
                None
            }
        }

        // Helper to run transact (no inspector) and return the output value
        let run_transact = |with_bundle: bool| -> U256 {
            let db = build_db(with_bundle);
            let mut evm = TestCtx::new(db, SpecId::CANCUN).build_mainnet();
            let result = evm.transact(build_tx()).unwrap();
            let output = result.result.output().cloned().unwrap_or_default();
            assert!(
                output.len() >= 32,
                "transact (bundle={with_bundle}): expected 32 bytes, got {}: {output:?}",
                output.len()
            );
            U256::from_be_slice(&output[..32])
        };

        // Helper to run inspect_tx with a given inspector and return the output value
        fn run_inspect_with<I: Inspector<TestCtx>>(db: TestDb, tx: TxEnv, inspector: I) -> U256 {
            let mut evm = TestCtx::new(db, SpecId::CANCUN).build_mainnet_with_inspector(inspector);
            let result = evm.inspect_tx(tx).unwrap();
            let output = result.result.output().cloned().unwrap_or_default();
            assert!(
                output.len() >= 32,
                "inspect_tx: expected 32 bytes, got {}: {output:?}",
                output.len()
            );
            U256::from_be_slice(&output[..32])
        }

        // --- Variant 1: Basic State (no bundle update) ---
        let transact_basic = run_transact(false);
        assert_eq!(transact_basic, U256::from(0xBEEF), "transact (no bundle): wrong value");

        let inspect_caller = run_inspect_with(build_db(false), build_tx(), LoadCallerOnCreate);
        assert_eq!(
            inspect_caller, transact_basic,
            "inspect_tx (load caller, no bundle): SLOAD returned {inspect_caller:#x}, \
             expected {transact_basic:#x}"
        );

        let inspect_target =
            run_inspect_with(build_db(false), build_tx(), LoadCreatedAddrOnCreate { create2_addr });
        assert_eq!(
            inspect_target, transact_basic,
            "inspect_tx (load target, no bundle): SLOAD returned {inspect_target:#x}, \
             expected {transact_basic:#x}"
        );

        let inspect_both =
            run_inspect_with(build_db(false), build_tx(), LoadBothOnCreate { create2_addr });
        assert_eq!(
            inspect_both, transact_basic,
            "inspect_tx (load both, no bundle): SLOAD returned {inspect_both:#x}, \
             expected {transact_basic:#x}"
        );

        // --- Variant 2: State with bundle update (as reth uses) ---
        let transact_bundle = run_transact(true);
        assert_eq!(transact_bundle, U256::from(0xBEEF), "transact (with bundle): wrong value");

        let inspect_caller_bundle =
            run_inspect_with(build_db(true), build_tx(), LoadCallerOnCreate);
        assert_eq!(
            inspect_caller_bundle, transact_bundle,
            "inspect_tx (load caller, with bundle): SLOAD returned {inspect_caller_bundle:#x}, \
             expected {transact_bundle:#x}"
        );

        let inspect_target_bundle =
            run_inspect_with(build_db(true), build_tx(), LoadCreatedAddrOnCreate { create2_addr });
        assert_eq!(
            inspect_target_bundle, transact_bundle,
            "inspect_tx (load target, with bundle): SLOAD returned {inspect_target_bundle:#x}, \
             expected {transact_bundle:#x}"
        );

        let inspect_both_bundle =
            run_inspect_with(build_db(true), build_tx(), LoadBothOnCreate { create2_addr });
        assert_eq!(
            inspect_both_bundle, transact_bundle,
            "inspect_tx (load both, with bundle): SLOAD returned {inspect_both_bundle:#x}, \
             expected {transact_bundle:#x}"
        );
    }

    /// Extended test: verifies the bug doesn't manifest when using `EthEvmFactory` from this
    /// crate, which is how reth actually constructs the EVM. Tests that the wrapper layer
    /// doesn't introduce the divergence.
    #[test]
    fn test_create2_state_diff_eth_evm_factory() {
        use crate::{env::EvmEnv, eth::EthEvmFactory, evm::EvmFactory, Evm};
        use alloy_primitives::{Bytes, TxKind};
        use revm::{
            context::{CfgEnv, TxEnv},
            context_interface::{ContextTr, JournalTr},
            database_interface::EmptyDB,
            inspector::Inspector,
            interpreter::{CreateInputs, CreateOutcome},
            primitives::hardfork::SpecId,
            state::AccountInfo,
        };

        type TestDb = State<CacheDB<EmptyDB>>;

        // --- Bytecode construction (same as above) ---
        let runtime_code: &[u8] =
            &[0x60, 0x00, 0x54, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3];
        let initcode_instrs: &[u8] = &[
            0x61, 0xBE, 0xEF, 0x60, 0x00, 0x55, 0x60, 0x0b, 0x60, 0x12, 0x60, 0x00, 0x39, 0x60,
            0x0b, 0x60, 0x00, 0xf3,
        ];
        let initcode: Bytes = [initcode_instrs, runtime_code].concat().into();

        let deployer_addr = address!("0x1000000000000000000000000000000000000001");
        let create2_addr = deployer_addr.create2_from_code(B256::ZERO, &initcode);

        let deployer_instrs: Vec<u8> = vec![
            0x60, 0x1d, 0x60, 0x22, 0x60, 0x00, 0x39, 0x60, 0x00, 0x60, 0x1d, 0x60, 0x00, 0x60,
            0x00, 0xf5, 0x60, 0x20, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x84, 0x5a, 0xfa, 0x50,
            0x50, 0x60, 0x20, 0x60, 0x00, 0xf3,
        ];
        let deployer_code: Bytes = [deployer_instrs.as_slice(), &initcode].concat().into();

        let caller = address!("0x0000000000000000000000000000000000000099");

        let build_db = || -> TestDb {
            let mut cache_db = CacheDB::new(EmptyDB::new());
            cache_db.insert_account_info(
                deployer_addr,
                AccountInfo {
                    code_hash: keccak256(&deployer_code),
                    code: Some(
                        revm::state::Bytecode::new_raw_checked(deployer_code.clone()).unwrap(),
                    ),
                    nonce: 1,
                    ..Default::default()
                },
            );
            cache_db.insert_account_info(
                caller,
                AccountInfo {
                    balance: U256::from(1_000_000_000_000_000_000u128),
                    ..Default::default()
                },
            );

            let mut db = State::builder().with_database(cache_db).with_bundle_update().build();

            let mut storage = HashMap::<B256, B256>::default();
            storage.insert(B256::from(U256::from(42)), B256::from(U256::from(999)));
            let acc_override = AccountOverride::default().with_state_diff(storage);
            apply_account_override(create2_addr, acc_override, &mut db).unwrap();

            db
        };

        let build_tx = || {
            TxEnv::builder()
                .gas_limit(1_000_000)
                .gas_price(0)
                .caller(caller)
                .kind(TxKind::Call(deployer_addr))
                .build()
                .unwrap()
        };

        let mut cfg = CfgEnv::default();
        cfg.spec = SpecId::CANCUN;
        cfg.chain_id = 1;
        let env = EvmEnv { block_env: revm::context::BlockEnv::default(), cfg_env: cfg };

        let factory = EthEvmFactory;

        // Test 1: transact without inspector via EthEvmFactory
        let transact_value = {
            let db = build_db();
            let mut evm = factory.create_evm(db, env.clone());
            let result = evm.transact(build_tx()).unwrap();
            let output = result.result.output().cloned().unwrap_or_default();
            assert!(output.len() >= 32, "EthEvmFactory transact: short output");
            U256::from_be_slice(&output[..32])
        };
        assert_eq!(transact_value, U256::from(0xBEEF), "EthEvmFactory transact: wrong value");

        // Test 2: inspect_tx with inspector via EthEvmFactory
        struct LoadCallerOnCreate;
        impl<CTX: ContextTr> Inspector<CTX> for LoadCallerOnCreate {
            fn create(
                &mut self,
                context: &mut CTX,
                inputs: &mut CreateInputs,
            ) -> Option<CreateOutcome> {
                let _ = context.journal_mut().load_account(inputs.caller());
                None
            }
        }

        let inspect_value = {
            let db = build_db();
            let mut evm = factory.create_evm_with_inspector(db, env, LoadCallerOnCreate);
            let result = evm.transact(build_tx()).unwrap();
            let output = result.result.output().cloned().unwrap_or_default();
            assert!(output.len() >= 32, "EthEvmFactory inspect: short output");
            U256::from_be_slice(&output[..32])
        };
        assert_eq!(
            inspect_value, transact_value,
            "EthEvmFactory: inspect returned {inspect_value:#x} but transact returned \
             {transact_value:#x}. Bug manifests at the EthEvmFactory level."
        );
    }

    /// Tests the DELEGATECALL pattern used by ERC-4337 smart accounts: the initcode
    /// DELEGATECALLs to an implementation contract to write storage, then runtime code
    /// SLOADs from that storage. This mimics the real-world scenario more closely.
    #[test]
    fn test_create2_delegatecall_state_diff_inspect() {
        use alloy_primitives::{Bytes, TxKind};
        use revm::{
            context::{Context, Journal, TxEnv},
            context_interface::{ContextTr, JournalTr},
            database_interface::EmptyDB,
            inspector::Inspector,
            interpreter::{CreateInputs, CreateOutcome},
            primitives::hardfork::SpecId,
            state::AccountInfo,
            ExecuteEvm, InspectEvm, MainBuilder,
        };

        type TestDb = State<CacheDB<EmptyDB>>;
        type TestCtx =
            Context<revm::context::BlockEnv, TxEnv, revm::context::CfgEnv, TestDb, Journal<TestDb>>;

        // Implementation contract code: SSTORE(0, 0xBEEF), STOP
        // When DELEGATECALLed, this writes 0xBEEF to slot 0 of the caller's storage.
        let impl_code: &[u8] = &[
            0x61, 0xBE, 0xEF, // PUSH2 0xBEEF
            0x60, 0x00, // PUSH1 0x00 (slot)
            0x55, // SSTORE
            0x00, // STOP
        ]; // 7 bytes

        let impl_addr = address!("0x2000000000000000000000000000000000000002");

        // Runtime code: SLOAD(0), MSTORE(0), RETURN(0, 32)
        let runtime_code: &[u8] =
            &[0x60, 0x00, 0x54, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3]; // 11 bytes

        // Initcode: DELEGATECALL to impl_addr, then CODECOPY + RETURN runtime code.
        //
        // DELEGATECALL(gas, addr, argsOffset, argsLen, retOffset, retLen)
        // Then CODECOPY runtime, RETURN runtime.
        //
        // impl_addr is 20 bytes, embedded as PUSH20.
        let mut initcode_v: Vec<u8> = Vec::new();
        // DELEGATECALL(gas, impl_addr, 0, 0, 0, 0)
        initcode_v.extend_from_slice(&[0x60, 0x00]); // retLen = 0
        initcode_v.extend_from_slice(&[0x60, 0x00]); // retOff = 0
        initcode_v.extend_from_slice(&[0x60, 0x00]); // argsLen = 0
        initcode_v.extend_from_slice(&[0x60, 0x00]); // argsOff = 0
        initcode_v.push(0x73); // PUSH20
        initcode_v.extend_from_slice(impl_addr.as_slice());
        initcode_v.push(0x5a); // GAS
        initcode_v.push(0xf4); // DELEGATECALL
        initcode_v.push(0x50); // POP result
                               // Now: CODECOPY runtime, RETURN runtime
                               // Remaining instrs = 12 bytes (see below), runtime goes after
        let codecopy_start = initcode_v.len();
        // We need 12 more instruction bytes before runtime_code:
        //   PUSH1 size (2) + PUSH1 offset (2) + PUSH1 0 (2) + CODECOPY (1) = 7
        //   PUSH1 size (2) + PUSH1 0 (2) + RETURN (1) = 5
        // Total = 12
        let runtime_off = codecopy_start + 12;
        initcode_v.extend_from_slice(&[
            0x60,
            runtime_code.len() as u8, // PUSH1 runtime_size
            0x60,
            runtime_off as u8, // PUSH1 runtime_offset_in_initcode
            0x60,
            0x00, // PUSH1 0 (memory dest)
            0x39, // CODECOPY
            0x60,
            runtime_code.len() as u8, // PUSH1 runtime_size
            0x60,
            0x00, // PUSH1 0 (memory offset)
            0xf3, // RETURN
        ]);
        assert_eq!(initcode_v.len(), runtime_off);
        initcode_v.extend_from_slice(runtime_code);

        let initcode: Bytes = initcode_v.clone().into();

        let deployer_addr = address!("0x1000000000000000000000000000000000000001");
        let create2_addr = deployer_addr.create2_from_code(B256::ZERO, &initcode);

        // Deployer: CODECOPY initcode → CREATE2 → STATICCALL → RETURN
        let initcode_size = initcode.len();
        let deployer_len: u8 = 34;
        let deployer_instrs: Vec<u8> = vec![
            0x60,
            initcode_size as u8, // PUSH1 initcode_size
            0x60,
            deployer_len, // PUSH1 deployer_len (offset of initcode)
            0x60,
            0x00, // PUSH1 0 (dest)
            0x39, // CODECOPY
            0x60,
            0x00, // PUSH1 0 (salt)
            0x60,
            initcode_size as u8, // PUSH1 initcode_size
            0x60,
            0x00, // PUSH1 0 (offset)
            0x60,
            0x00, // PUSH1 0 (value)
            0xf5, // CREATE2
            0x60,
            0x20, // PUSH1 32 (retLen)
            0x60,
            0x00, // PUSH1 0 (retOff)
            0x60,
            0x00, // PUSH1 0 (argsLen)
            0x60,
            0x00, // PUSH1 0 (argsOff)
            0x84, // DUP5 (created addr)
            0x5a, // GAS
            0xfa, // STATICCALL
            0x50, // POP (success)
            0x50, // POP (created_addr)
            0x60,
            0x20, // PUSH1 32
            0x60,
            0x00, // PUSH1 0
            0xf3, // RETURN
        ];
        assert_eq!(deployer_instrs.len(), deployer_len as usize);
        let deployer_code: Bytes = [deployer_instrs.as_slice(), &initcode].concat().into();

        let caller = address!("0x0000000000000000000000000000000000000099");

        let build_db = || {
            let mut cache_db = CacheDB::new(EmptyDB::new());

            // Insert deployer contract
            cache_db.insert_account_info(
                deployer_addr,
                AccountInfo {
                    code_hash: keccak256(&deployer_code),
                    code: Some(
                        revm::state::Bytecode::new_raw_checked(deployer_code.clone()).unwrap(),
                    ),
                    nonce: 1,
                    ..Default::default()
                },
            );

            // Insert implementation contract
            let impl_bytecode: Bytes = impl_code.to_vec().into();
            cache_db.insert_account_info(
                impl_addr,
                AccountInfo {
                    code_hash: keccak256(&impl_bytecode),
                    code: Some(revm::state::Bytecode::new_raw_checked(impl_bytecode).unwrap()),
                    nonce: 1,
                    ..Default::default()
                },
            );

            // Insert caller with balance
            cache_db.insert_account_info(
                caller,
                AccountInfo {
                    balance: U256::from(1_000_000_000_000_000_000u128),
                    ..Default::default()
                },
            );

            let mut db = State::builder().with_database(cache_db).with_bundle_update().build();

            // Apply stateDiff override on the CREATE2 target address
            let mut storage = HashMap::<B256, B256>::default();
            storage.insert(B256::from(U256::from(42)), B256::from(U256::from(999)));
            let acc_override = AccountOverride::default().with_state_diff(storage);
            apply_account_override(create2_addr, acc_override, &mut db).unwrap();

            db
        };

        let build_tx = || {
            TxEnv::builder()
                .gas_limit(1_000_000)
                .gas_price(0)
                .caller(caller)
                .kind(TxKind::Call(deployer_addr))
                .build()
                .unwrap()
        };

        // transact (no inspector)
        let transact_value = {
            let db = build_db();
            let mut evm = TestCtx::new(db, SpecId::CANCUN).build_mainnet();
            let result = evm.transact(build_tx()).unwrap();
            let output = result.result.output().cloned().unwrap_or_default();
            assert!(output.len() >= 32, "delegatecall transact: short output: {output:?}");
            U256::from_be_slice(&output[..32])
        };
        assert_eq!(
            transact_value,
            U256::from(0xBEEF),
            "delegatecall transact: SLOAD should return DELEGATECALL's SSTORE'd value"
        );

        // inspect_tx with LoadCallerOnCreate
        struct LoadCallerOnCreate;
        impl<CTX: ContextTr> Inspector<CTX> for LoadCallerOnCreate {
            fn create(
                &mut self,
                context: &mut CTX,
                inputs: &mut CreateInputs,
            ) -> Option<CreateOutcome> {
                let _ = context.journal_mut().load_account(inputs.caller());
                None
            }
        }

        let inspect_value = {
            let db = build_db();
            let mut evm =
                TestCtx::new(db, SpecId::CANCUN).build_mainnet_with_inspector(LoadCallerOnCreate);
            let result = evm.inspect_tx(build_tx()).unwrap();
            let output = result.result.output().cloned().unwrap_or_default();
            assert!(output.len() >= 32, "delegatecall inspect_tx: short output: {output:?}");
            U256::from_be_slice(&output[..32])
        };

        assert_eq!(
            inspect_value, transact_value,
            "DELEGATECALL variant: inspect_tx SLOAD returned {inspect_value:#x} but transact \
             returned {transact_value:#x}. Bug manifests with DELEGATECALL-based initcode."
        );
    }
}
