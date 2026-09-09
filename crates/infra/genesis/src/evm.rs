use std::collections::BTreeMap;

use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address, keccak256};
use eyre::{Result, WrapErr, ensure};
use revm::{
    Context, ExecuteCommitEvm, MainBuilder, MainContext, SystemCallCommitEvm,
    context::{BlockEnv, CfgEnv, TxEnv},
    database::InMemoryDB,
    handler::MainnetEvm,
    primitives::hardfork::SpecId,
    state::Bytecode,
};
use serde_json::Value;

use crate::ContractArtifacts;

/// Ordinary in-memory Ethereum execution, without inspectors or cheatcodes.
pub type GenesisContext = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB>;

/// Native constructor and initializer execution for genesis.
#[derive(Debug)]
pub struct GenesisEvm<'a> {
    /// Already-built Base contracts.
    pub artifacts: &'a ContractArtifacts,
    /// The Ethereum EVM and its in-memory database.
    pub evm: MainnetEvm<GenesisContext>,
}

impl<'a> GenesisEvm<'a> {
    /// Temporary L1 deployment account, excluded from genesis.
    pub const DEPLOYER: Address = address!("0100000000000000000000000000000000000000");
    /// The deterministic deployment proxy used by Base contracts.
    pub const FACTORY: Address = address!("4e59b44847b379578588920cA78FbF26c0B4956C");
    /// Reference L2 constructor origin (first script address); removed from exported state.
    /// Keeping its nonce/address preserves EAS's cached EIP-712 immutables byte-for-byte.
    pub const L2_DEPLOYER: Address = address!("fdAf657f7146c1AB9db4d70cEd5FDEa7dC4906c0");

    /// Creates a pre-genesis execution context at block zero, timestamp zero.
    ///
    /// This permits seeding initial upgrade history without weakening live notice/freeze rules.
    pub fn new(artifacts: &'a ContractArtifacts, chain_id: u64) -> Result<Self> {
        let ctx = Context::mainnet()
            .with_db(InMemoryDB::default())
            .modify_cfg_chained(|cfg| {
                cfg.set_spec_and_mainnet_gas_params(SpecId::CANCUN);
                cfg.chain_id = chain_id;
            })
            .modify_block_chained(|block| {
                block.timestamp = U256::ZERO;
                block.basefee = 0;
                block.gas_limit = 1_000_000_000;
            });
        let mut result = Self { artifacts, evm: ctx.build_mainnet() };
        let code = artifacts
            .preinstall(Self::FACTORY)?
            .code
            .clone()
            .filter(|code| !code.is_empty())
            .ok_or_else(|| eyre::eyre!("missing deployment proxy bytecode at {}", Self::FACTORY))?;
        result.code(Self::FACTORY, code);
        Ok(result)
    }

    /// Installs runtime code at a protocol-defined predeploy address.
    pub fn code(&mut self, address: Address, code: Bytes) {
        let code = Bytecode::new_raw(code);
        let db = &mut self.evm.ctx.journaled_state.database;
        let mut info = db.cache.accounts.get(&address).map(|a| a.info.clone()).unwrap_or_default();
        info.code_hash = code.hash_slow();
        info.code = Some(code);
        db.insert_account_info(address, info);
    }

    /// Writes a genesis storage slot directly, without simulating a cheatcode.
    pub fn storage(&mut self, address: Address, slot: B256, value: B256) -> Result<()> {
        self.evm.ctx.journaled_state.database.insert_account_storage(
            address,
            slot.into(),
            value.into(),
        )?;
        Ok(())
    }

    /// Executes actual CREATE/CREATE2 initialization code.
    pub fn deploy(
        &mut self,
        name: &str,
        args: Value,
        sender: Address,
        salt: Option<B256>,
    ) -> Result<Address> {
        let code = self
            .artifacts
            .get(name)?
            .constructor(&args)
            .wrap_err_with(|| format!("{name} constructor"))?;
        let db = &self.evm.ctx.journaled_state.database.cache;
        let nonce = db.accounts.get(&sender).map_or(0, |a| a.info.nonce);
        let address = salt.map_or_else(
            || sender.create(nonce),
            |salt| Self::FACTORY.create2(salt, keccak256(&code)),
        );
        if db.accounts.get(&address).is_some_and(|a| {
            !a.info.code_hash.is_zero() && a.info.code_hash != alloy_primitives::KECCAK256_EMPTY
        }) {
            return Ok(address);
        }
        let (kind, data) = if let Some(salt) = salt {
            (TxKind::Call(Self::FACTORY), [salt.as_slice(), code.as_ref()].concat().into())
        } else {
            (TxKind::Create, code)
        };
        let result = self.evm.transact_commit(TxEnv {
            caller: sender,
            kind,
            data,
            nonce,
            chain_id: None,
            gas_limit: 100_000_000,
            gas_price: 0,
            ..Default::default()
        })?;
        ensure!(result.is_success(), "{name} deployment failed: {result:?}");
        Ok(address)
    }

    /// Executes an ordinary initializer/message call without charging a synthetic transaction nonce.
    pub fn call(
        &mut self,
        name: &str,
        address: Address,
        method: &str,
        args: Value,
        caller: Address,
    ) -> Result<Bytes> {
        let data = self
            .artifacts
            .get(name)?
            .encode(method, &args)
            .wrap_err_with(|| format!("{name}.{method}"))?;
        let result = self.evm.system_call_with_caller_commit(caller, address, data)?;
        ensure!(
            result.is_success(),
            "{name}.{method} at {address} failed: {}; execution: {result:?}",
            self.artifacts
                .get(name)?
                .revert_reason(result.output().map_or(&[], |data| data.as_ref()))
        );
        Ok(result.into_output().unwrap_or_default())
    }

    /// Installs Base's preinstalls, specializing Permit2 for this chain.
    pub fn preinstalls(&mut self, chain_id: u64) -> Result<()> {
        let permit2 = address!("000000000022D473030F116dDEE9F6B43aC78BA3");
        ensure!(
            self.artifacts.preinstall(permit2)?.code.as_ref().is_some_and(|code| !code.is_empty()),
            "missing Permit2 template bytecode"
        );
        for (address, account) in &self.artifacts.preinstalls {
            if let Some(code) = &account.code {
                let mut code = code.to_vec();
                if *address == permit2 {
                    // Base Preinstalls.getPermit2Code: the two cached EIP-712 immutables.
                    ensure!(
                        code.len() >= 7015,
                        "Permit2 template is too short for its cached immutables"
                    );
                    let mut domain = [
                        keccak256(
                            "EIP712Domain(string name,uint256 chainId,address verifyingContract)",
                        ),
                        keccak256("Permit2"),
                        B256::from(U256::from(1)),
                        permit2.into_word(),
                    ];
                    let domain_hash = |domain: &[B256; 4]| {
                        keccak256(
                            domain
                                .iter()
                                .flat_map(|word| word.as_slice())
                                .copied()
                                .collect::<Vec<_>>(),
                        )
                    };
                    ensure!(
                        code[6944] == 0x7f
                            && code[6982] == 0x7f
                            && code[6945..6977] == domain[2][..]
                            && code[6983..7015] == domain_hash(&domain)[..],
                        "Permit2 cached immutable layout changed; update specialization for the pinned contracts"
                    );
                    domain[2] = B256::from(U256::from(chain_id));
                    code[6945..6977].copy_from_slice(domain[2].as_slice());
                    code[6983..7015].copy_from_slice(domain_hash(&domain).as_slice());
                }
                self.code(*address, code.into());
            }
            let info = &mut self
                .evm
                .ctx
                .journaled_state
                .database
                .cache
                .accounts
                .entry(*address)
                .or_default()
                .info;
            info.nonce = info.nonce.max(account.nonce.unwrap_or_default());
        }
        Ok(())
    }

    /// Exports protocol accounts, removing only empty and temporary deployment accounts.
    pub fn allocs(&self) -> BTreeMap<Address, GenesisAccount> {
        let cache = &self.evm.ctx.journaled_state.database.cache;
        cache
            .accounts
            .iter()
            .filter_map(|(address, account)| {
                if [Self::DEPLOYER, Self::L2_DEPLOYER].contains(address) || account.info.is_empty()
                {
                    return None;
                }
                Some((
                    *address,
                    GenesisAccount {
                        nonce: Some(account.info.nonce),
                        balance: account.info.balance,
                        code: cache
                            .contracts
                            .get(&account.info.code_hash)
                            .map(Bytecode::original_bytes),
                        storage: Some(
                            account
                                .storage
                                .iter()
                                .filter(|(_, v)| !v.is_zero())
                                .map(|(k, v)| (B256::from(*k), B256::from(*v)))
                                .collect(),
                        ),
                        ..Default::default()
                    },
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::{env, path::Path};

    use base_common_genesis::RollupConfig;

    use super::*;
    use crate::{Deployment, GenesisConfig};

    #[test]
    #[ignore = "requires pinned contract artifacts; run just genesis-test"]
    fn malformed_pinned_constants_return_errors_instead_of_panicking() -> Result<()> {
        let mut artifacts =
            ContractArtifacts::load(Path::new(&env::var("BASE_GENESIS_ARTIFACTS")?))?;
        let factory = artifacts.preinstalls.remove(&GenesisEvm::FACTORY).unwrap();
        assert!(GenesisEvm::new(&artifacts, 1337).unwrap_err().to_string().contains("preinstall"));
        artifacts.preinstalls.insert(GenesisEvm::FACTORY, GenesisAccount::default());
        assert!(GenesisEvm::new(&artifacts, 1337).unwrap_err().to_string().contains("bytecode"));
        artifacts.preinstalls.insert(GenesisEvm::FACTORY, factory);
        let permit2 = address!("000000000022D473030F116dDEE9F6B43aC78BA3");
        let original = artifacts.preinstalls.remove(&permit2).unwrap();
        assert!(
            GenesisEvm::new(&artifacts, 1337)?
                .preinstalls(1337)
                .unwrap_err()
                .to_string()
                .contains("preinstall")
        );
        artifacts.preinstalls.insert(
            permit2,
            GenesisAccount { code: Some(vec![0u8; 10].into()), ..Default::default() },
        );
        assert!(
            GenesisEvm::new(&artifacts, 1337)?
                .preinstalls(1337)
                .unwrap_err()
                .to_string()
                .contains("too short")
        );
        let mut changed = original.clone();
        let mut code = changed.code.unwrap().to_vec();
        code[6944] = 0;
        changed.code = Some(code.into());
        artifacts.preinstalls.insert(permit2, changed);
        assert!(
            GenesisEvm::new(&artifacts, 1337)?
                .preinstalls(1337)
                .unwrap_err()
                .to_string()
                .contains("layout changed")
        );
        artifacts.preinstalls.insert(permit2, original);
        GenesisEvm::new(&artifacts, 1337)?.preinstalls(1337)?;
        artifacts.predeploys.remove("WETH");
        let error = Deployment::l2(
            &GenesisConfig::default(),
            &artifacts,
            &BTreeMap::new(),
            &RollupConfig::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("missing Base predeploy WETH"));
        Ok(())
    }
}
