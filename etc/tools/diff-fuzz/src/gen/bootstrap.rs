//! Synthetic genesis states for each fork era, built from repository artifacts.
//!
//! The starting point is the Base mainnet genesis allocation embedded in `base-common-chains`
//! (predeploy proxies and their implementations). Each later upgrade is then applied the way the
//! chain applied it: the Canyon create2 deployer and Denim `BaseTime` force-deployments through
//! the executor's own helpers, and the Ecotone, Fjord, Isthmus, and Jovian upgrade deposit
//! transactions from `base-consensus-upgrades`, executed in the Base EVM. From Beryl the
//! native-precompile features are activated by the chain-config activation admin, and from Cobalt
//! the registry's admin storage is pointed at the generator's operator key.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, OnceLock},
};

use alloy_eips::Decodable2718;
use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, B256, Bytes, LogData, TxKind, U256, keccak256};
use alloy_sol_types::{SolCall, SolEvent};
use base_common_chains::{ChainConfig, MAINNET_BERYL_ACTIVATION_ADMIN_ADDRESS};
use base_common_consensus::{BaseTxEnvelope, Predeploys, TxDeposit};
use base_common_evm::{
    BaseSpecId, BaseTime, BaseTransaction, Builder, DefaultBase, ensure_create2_deployer,
};
use base_common_precompiles::{
    ActivationFeature, ActivationRegistryStorage, B20FactoryStorage, B20PolicyType, B20Variant,
    IActivationRegistry, IB20, IPolicyRegistry, PolicyRegistryStorage,
};
use base_consensus_upgrades::{Upgrade, Upgrades};
use revm::{
    Context, ExecuteCommitEvm,
    context::{BlockEnv, CfgEnv, TxEnv},
    database::{CacheDB, EmptyDB},
    state::{AccountInfo, Bytecode},
};
use serde::Deserialize;

use crate::{
    Log,
    r#gen::{
        era::{Era, Schedule},
        keys::Keys,
        native::Native,
        registry::Registry,
        state::{Account, WorldState},
    },
};

/// The part of the genesis file the bootstrap reads.
#[derive(Deserialize)]
struct GenesisFile {
    alloc: BTreeMap<Address, GenesisAccount>,
}

/// The synthetic genesis of an era.
#[derive(Debug)]
pub struct Bootstrapped {
    /// State with every upgrade up to the era applied.
    pub state: WorldState,
    /// Logs of the setup calls, from which a registry learns the seeded tokens and policies.
    pub logs: Vec<Log>,
}

/// Builds and caches the synthetic genesis state of each era.
#[derive(Debug)]
pub struct Bootstrap;

impl Bootstrap {
    /// Highest predeploy proxy index kept from the genesis allocation. The genesis allocates 2048
    /// proxies; only those with a role on Base are kept to keep witnesses small.
    pub const MAX_PROXY_INDEX: u16 = 0x30;

    /// Number of keys, from key 0, holding the seeded tokens and allowed by the seeded allowlist.
    pub const SEEDED_HOLDERS: usize = 12;

    /// Key on the seeded blocklist, which is the seeded tokens' transfer-sender policy. It also
    /// holds seeded tokens, so `burnBlocked` against it can succeed.
    pub const SEEDED_BLOCKED: usize = 11;

    /// The synthetic state at the start of `era`: every upgrade activating at or before the era
    /// applied, with no user accounts.
    pub fn state(era: Era) -> Arc<Bootstrapped> {
        static CACHE: OnceLock<Mutex<HashMap<Era, Arc<Bootstrapped>>>> = OnceLock::new();
        let cache = CACHE.get_or_init(Default::default);
        if let Some(state) = cache.lock().expect("bootstrap cache").get(&era) {
            return Arc::clone(state);
        }
        let state = Arc::new(Self::build(era));
        cache.lock().expect("bootstrap cache").insert(era, Arc::clone(&state));
        state
    }

    /// Code produced by upgrades of eras after `era`, so that post-state diffs of activation
    /// blocks can be resolved.
    pub fn upgrade_codes() -> &'static BTreeMap<B256, Bytes> {
        static CODES: OnceLock<BTreeMap<B256, Bytes>> = OnceLock::new();
        CODES.get_or_init(|| Self::state(Era::Denim).state.codes.clone())
    }

    fn build(era: Era) -> Bootstrapped {
        let mut logs = Vec::new();
        let mut db = CacheDB::<EmptyDB>::default();
        let genesis: GenesisFile = serde_json::from_str(ChainConfig::mainnet().genesis_json)
            .expect("embedded mainnet genesis parses");
        for (address, account) in genesis.alloc {
            if Self::is_unused_proxy(&address) {
                continue;
            }
            let code = account.code.clone().unwrap_or_default();
            let bytecode = Bytecode::new_raw(code);
            db.insert_account_info(
                address,
                AccountInfo {
                    balance: account.balance,
                    nonce: account.nonce.unwrap_or_default(),
                    code_hash: bytecode.hash_slow(),
                    code: Some(bytecode),
                    ..Default::default()
                },
            );
            for (slot, value) in account.storage.unwrap_or_default() {
                db.insert_account_storage(address, slot.into(), value.into())
                    .expect("in-memory storage insert");
            }
        }

        let schedule = Schedule::mainnet();
        for step in Era::ALL.into_iter().skip(1).take_while(|step| *step <= era) {
            let timestamp = step.activation_timestamp();
            match step {
                Era::Canyon => {
                    ensure_create2_deployer(&schedule.rollup, timestamp, &mut db)
                        .expect("create2 deployer force-deploy");
                }
                Era::Ecotone => Self::run_upgrade(&mut db, step, Upgrades::ECOTONE.txs()),
                Era::Fjord => Self::run_upgrade(&mut db, step, Upgrades::FJORD.txs()),
                Era::Isthmus => Self::run_upgrade(&mut db, step, Upgrades::ISTHMUS.txs()),
                Era::Jovian => Self::run_upgrade(&mut db, step, Upgrades::JOVIAN.txs()),
                Era::Beryl => {
                    for feature in [
                        ActivationFeature::PolicyRegistry,
                        ActivationFeature::B20Stablecoin,
                        ActivationFeature::B20Asset,
                    ] {
                        let call = IActivationRegistry::activateCall { feature: feature.id() };
                        Self::run_call(
                            &mut db,
                            step,
                            MAINNET_BERYL_ACTIVATION_ADMIN_ADDRESS,
                            ActivationRegistryStorage::ADDRESS,
                            call.abi_encode().into(),
                        );
                    }
                    logs.extend(Self::seed_native(&mut db, step));
                }
                Era::Cobalt => {
                    let call =
                        IActivationRegistry::setAdminCall { newAdmin: Keys::get().operator() };
                    Self::run_call(
                        &mut db,
                        step,
                        MAINNET_BERYL_ACTIVATION_ADMIN_ADDRESS,
                        ActivationRegistryStorage::ADDRESS,
                        call.abi_encode().into(),
                    );
                    Self::seed_seize_exempt(&mut db, step, &logs);
                }
                Era::Denim => {
                    BaseTime::ensure_predeploy(&schedule.rollup, timestamp, &mut db)
                        .expect("BaseTime force-deploy");
                }
                _ => {}
            }
        }
        Bootstrapped { state: Self::extract(&db), logs }
    }

    /// Seeds the native precompiles so calls in short chains and single blocks have something to
    /// act on: three operator-administered policies (an allowlist of the first
    /// [`Self::SEEDED_HOLDERS`] keys, a blocklist of [`Self::SEEDED_BLOCKED`], and a seize-exempt
    /// allowlist of the operator), then an asset and a stablecoin minted to those keys, with every
    /// role granted to the operator and the blocklist as transfer-sender policy (so the blocked
    /// key can be burned from), and standing approvals between holders. Returns the emitted logs.
    fn seed_native(db: &mut CacheDB<EmptyDB>, era: Era) -> Vec<Log> {
        let keys = Keys::get();
        let operator = keys.operator();
        let holders: Vec<Address> = (0..Self::SEEDED_HOLDERS).map(|i| keys.address(i)).collect();
        let mut logs = Vec::new();
        let mut policy_ids = Vec::new();
        for (policy_type, accounts) in [
            (IPolicyRegistry::PolicyType::ALLOWLIST, holders.clone()),
            (IPolicyRegistry::PolicyType::BLOCKLIST, vec![keys.address(Self::SEEDED_BLOCKED)]),
            (IPolicyRegistry::PolicyType::ALLOWLIST, vec![operator]),
        ] {
            let call = IPolicyRegistry::createPolicyWithAccountsCall {
                admin: operator,
                policyType: policy_type,
                accounts,
            };
            let created = Self::run_call(
                db,
                era,
                operator,
                PolicyRegistryStorage::ADDRESS,
                call.abi_encode().into(),
            );
            let id = created
                .iter()
                .find_map(|log| {
                    let data = LogData::new_unchecked(log.topics.clone(), log.data.clone());
                    IPolicyRegistry::PolicyCreated::decode_log_data(&data).ok()
                })
                .expect("policy creation emits PolicyCreated")
                .policyId;
            policy_ids.push(id);
            logs.extend(created);
        }
        let extra = [IB20::updatePolicyCall {
            policyScope: B20PolicyType::TransferSender.id(),
            newPolicyId: policy_ids[1],
        }
        .abi_encode()
        .into()];
        for (salt, variant) in [(1u64, B20Variant::Asset), (2, B20Variant::Stablecoin)] {
            let input =
                Native::seeded_token_input(B256::from(U256::from(salt)), variant, &holders, &extra);
            logs.extend(Self::run_call(db, era, operator, B20FactoryStorage::ADDRESS, input));
        }
        // Standing unlimited approvals between holders, so `transferFrom` has allowances to use.
        let mut known = Registry::default();
        known.learn_logs(&logs);
        for token in &known.tokens {
            for owner in 1..Self::SEEDED_HOLDERS / 2 {
                let call =
                    IB20::approveCall { spender: keys.address(owner + 1), amount: U256::MAX };
                logs.extend(Self::run_call(
                    db,
                    era,
                    keys.address(owner),
                    token.address,
                    call.abi_encode().into(),
                ));
            }
        }
        logs
    }

    /// Points the seeded tokens' seize-exempt policy (a Cobalt policy scope) at the seeded
    /// operator-only allowlist, the last policy [`Self::seed_native`] created.
    fn seed_seize_exempt(db: &mut CacheDB<EmptyDB>, era: Era, seeded: &[Log]) {
        let mut registry = Registry::default();
        registry.learn_logs(seeded);
        let policy = registry.policies.last().expect("seeded policies").id;
        let call = IB20::updatePolicyCall {
            policyScope: B20PolicyType::SeizeExempt.id(),
            newPolicyId: policy,
        };
        for token in &registry.tokens {
            Self::run_call(
                db,
                era,
                Keys::get().operator(),
                token.address,
                call.abi_encode().into(),
            );
        }
    }

    fn is_unused_proxy(address: &Address) -> bool {
        let bytes = address.as_slice();
        bytes[..18] == Predeploys::L2_TO_L1_MESSAGE_PASSER.as_slice()[..18]
            && u16::from_be_bytes([bytes[18], bytes[19]]) > Self::MAX_PROXY_INDEX
    }

    fn evm_context(
        db: &mut CacheDB<EmptyDB>,
        era: Era,
    ) -> base_common_evm::BaseContext<&mut CacheDB<EmptyDB>> {
        let timestamp = era.activation_timestamp();
        let number = Schedule::mainnet().first_block_at(timestamp);
        Context::base()
            .with_cfg(
                CfgEnv::new_with_spec(BaseSpecId::new(era.upgrade()))
                    .with_chain_id(ChainConfig::mainnet().chain_id),
            )
            .with_block(BlockEnv {
                number: U256::from(number),
                timestamp: U256::from(timestamp),
                gas_limit: 1_000_000_000,
                ..Default::default()
            })
            .with_db(db)
    }

    /// Executes encoded upgrade deposits in order, as the activation block does.
    fn run_upgrade(db: &mut CacheDB<EmptyDB>, era: Era, txs: impl Iterator<Item = Bytes>) {
        for raw in txs {
            let BaseTxEnvelope::Deposit(deposit) =
                BaseTxEnvelope::decode_2718(&mut raw.as_ref()).expect("upgrade tx decodes")
            else {
                panic!("upgrade transaction is not a deposit");
            };
            Self::run_deposit(db, era, deposit.inner(), Some(raw));
        }
    }

    /// Executes a call from `from` as a deposit (no signature, nonce, or fee checks).
    fn run_call(
        db: &mut CacheDB<EmptyDB>,
        era: Era,
        from: Address,
        to: Address,
        input: Bytes,
    ) -> Vec<Log> {
        let deposit = TxDeposit {
            source_hash: keccak256(input.as_ref()),
            from,
            to: TxKind::Call(to),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 10_000_000,
            is_system_transaction: false,
            input,
        };
        Self::run_deposit(db, era, &deposit, None)
    }

    fn run_deposit(
        db: &mut CacheDB<EmptyDB>,
        era: Era,
        deposit: &TxDeposit,
        raw: Option<Bytes>,
    ) -> Vec<Log> {
        let tx = BaseTransaction::builder()
            .base(
                TxEnv::builder()
                    .caller(deposit.from)
                    .kind(deposit.to)
                    .value(deposit.value)
                    .data(deposit.input.clone())
                    .gas_limit(deposit.gas_limit)
                    .chain_id(Some(ChainConfig::mainnet().chain_id)),
            )
            .source_hash(deposit.source_hash)
            .mint(deposit.mint)
            .enveloped_tx(raw)
            .build_fill();
        let mut evm = Self::evm_context(db, era)
            .build_base_with_activation_admin_address(Some(MAINNET_BERYL_ACTIVATION_ADMIN_ADDRESS));
        let result = evm.transact_commit(tx).expect("bootstrap deposit executes");
        assert!(result.is_success(), "bootstrap deposit failed in {era:?}: {result:?}");
        result
            .logs()
            .iter()
            .map(|log| Log {
                address: log.address,
                topics: log.topics().to_vec(),
                data: log.data.data.clone(),
            })
            .collect()
    }

    fn extract(db: &CacheDB<EmptyDB>) -> WorldState {
        let mut state = WorldState::default();
        for (hash, code) in &db.cache.contracts {
            if !code.is_empty() {
                state.codes.insert(*hash, code.original_bytes());
            }
        }
        for (address, account) in &db.cache.accounts {
            if matches!(account.account_state, revm::database::AccountState::NotExisting) {
                continue;
            }
            let info = &account.info;
            let mut code_hash = info.code_hash;
            if let Some(code) = &info.code
                && !code.is_empty()
            {
                code_hash = state.register_code(code.original_bytes());
            }
            let storage = account
                .storage
                .iter()
                .filter(|(_, value)| !value.is_zero())
                .map(|(slot, value)| (B256::from(*slot), *value))
                .collect();
            state.accounts.insert(
                *address,
                Account { nonce: info.nonce, balance: info.balance, code_hash, storage },
            );
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{U256, address};
    use base_common_consensus::Predeploys;
    use base_common_evm::BaseTime;

    use super::Bootstrap;
    use crate::r#gen::{era::Era, registry::Registry};

    #[test]
    fn eras_accumulate_upgrades() {
        let regolith = Bootstrap::state(Era::Regolith).state.clone();
        assert!(regolith.code(&Predeploys::L1_BLOCK_INFO).is_some());
        let create2 = address!("0x13b0D85CcB8bf860b6b79AF3029fCA081AE9beF2");
        assert!(regolith.code(&create2).is_none());
        assert!(Bootstrap::state(Era::Canyon).state.code(&create2).is_some());

        let ecotone = Bootstrap::state(Era::Ecotone).state.clone();
        assert!(ecotone.code(&Predeploys::BEACON_BLOCK_ROOT).is_some());
        let history = address!("0x0000F90827F1C53a10cb7A02335B175320002935");
        assert!(ecotone.code(&history).is_none());
        assert!(Bootstrap::state(Era::Isthmus).state.code(&history).is_some());

        let denim = Bootstrap::state(Era::Denim).state.clone();
        assert!(denim.code(&BaseTime::IMPLEMENTATION_ADDRESS).is_some());
        let impl_slot = BaseTime::IMPLEMENTATION_SLOT.into();
        assert_ne!(denim.storage(&Predeploys::BASE_TIME, impl_slot), U256::ZERO);
        // The registry stores three activated features and, from Cobalt, the operator admin.
        let registry = address!("0x8453000000000000000000000000000000000001");
        let beryl = Bootstrap::state(Era::Beryl).state.clone();
        assert_eq!(beryl.account(&registry).map(|a| a.storage.len()), Some(3));
        assert_eq!(
            Bootstrap::state(Era::Cobalt).state.account(&registry).map(|a| a.storage.len()),
            Some(4)
        );

        // From Beryl the bootstrap seeds two tokens and three policies the registry learns.
        let mut known = Registry::default();
        known.learn_logs(&Bootstrap::state(Era::Beryl).logs);
        assert_eq!((known.tokens.len(), known.policies.len()), (2, 3));
        assert!(Bootstrap::state(Era::Isthmus).logs.is_empty());
    }
}
