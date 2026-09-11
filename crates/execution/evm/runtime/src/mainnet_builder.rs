use base_execution_evm_runtime::{
    Cfg, CfgEnv, Database, EmptyDB, EthInstructions, EthPrecompiles, EvmMachine, FrameStack,
    ReferenceContext, Transaction, TxEnv, hardfork::SpecId,
};

/// Type alias for a mainnet EVM instance with standard Ethereum components.
pub type MainnetEvm<CTX, INSP = ()> = EvmMachine<CTX, INSP, EthPrecompiles>;

/// Trait for building mainnet EVM instances from contexts.
pub trait MainBuilder: Sized {
    /// The context type that will be used in the EVM.
    type ReferenceContext;

    /// Builds a mainnet EVM instance without an inspector.
    fn build_mainnet(self) -> MainnetEvm<Self::ReferenceContext>;

    /// Builds a mainnet EVM instance with the provided inspector.
    fn build_mainnet_with_inspector<INSP>(
        self,
        inspector: INSP,
    ) -> MainnetEvm<Self::ReferenceContext, INSP>;
}

impl<TX, CFG, DB, CHAIN> MainBuilder for ReferenceContext<TX, CFG, DB, CHAIN>
where
    TX: Transaction,
    CFG: Cfg,
    DB: Database,
{
    type ReferenceContext = Self;

    fn build_mainnet(self) -> MainnetEvm<Self::ReferenceContext> {
        let spec = self.cfg.spec().into();
        EvmMachine {
            ctx: self,
            inspector: (),
            instruction: EthInstructions::new_mainnet_with_spec(spec),
            precompiles: EthPrecompiles::new(spec),
            frame_stack: FrameStack::new_prealloc(8),
        }
    }

    fn build_mainnet_with_inspector<INSP>(
        self,
        inspector: INSP,
    ) -> MainnetEvm<Self::ReferenceContext, INSP> {
        let spec = self.cfg.spec().into();
        EvmMachine {
            ctx: self,
            inspector,
            instruction: EthInstructions::new_mainnet_with_spec(spec),
            precompiles: EthPrecompiles::new(spec),
            frame_stack: FrameStack::new_prealloc(8),
        }
    }
}

/// Trait used to initialize ReferenceContext with default mainnet types.
pub trait MainContext {
    /// Creates a new mainnet context with default configuration.
    fn mainnet() -> Self;
}

impl MainContext for base_execution_evm_runtime::ReferenceContext<TxEnv, CfgEnv, EmptyDB, ()> {
    fn mainnet() -> Self {
        ReferenceContext::new(EmptyDB::new(), SpecId::default())
    }
}

#[cfg(test)]
mod test {
    use alloy_signer::{Either, SignerSync};
    use base_common_client_ethereum::PrivateKeySigner;
    use base_execution_evm_runtime::{
        Authorization, BenchmarkDB, Bytecode, EEADDRESS, ExecuteEvm, FFADDRESS, MainBuilder,
        MainContext, StorageKey, StorageValue, TxEnv, TxKind, U256,
        hardfork::SpecId,
        opcode::{PUSH1, SSTORE},
    };

    #[test]
    fn sanity_eip7702_tx() {
        let signer = PrivateKeySigner::random();
        let auth = Authorization { chain_id: U256::ZERO, nonce: 0, address: FFADDRESS };
        let signature = signer.sign_hash_sync(&auth.signature_hash()).unwrap();
        let auth = auth.into_signed(signature);

        let bytecode = Bytecode::new_legacy([PUSH1, 0x01, PUSH1, 0x01, SSTORE].into());

        let ctx = base_execution_evm_runtime::ReferenceContext::mainnet()
            .modify_cfg_chained(|cfg| cfg.set_spec_and_mainnet_gas_params(SpecId::PRAGUE))
            .with_db(BenchmarkDB::new_bytecode(bytecode));

        let mut evm = ctx.build_mainnet();

        let state = evm
            .transact(
                TxEnv::builder()
                    .gas_limit(100_000)
                    .authorization_list(vec![Either::Left(auth)])
                    .caller(EEADDRESS)
                    .kind(TxKind::Call(signer.address()))
                    .build()
                    .unwrap(),
            )
            .unwrap()
            .state;

        let auth_acc = state.get(&signer.address()).unwrap();
        assert_eq!(auth_acc.info.code, Some(Bytecode::new_eip7702(FFADDRESS)));
        assert_eq!(auth_acc.info.nonce, 1);
        assert_eq!(
            auth_acc.storage.get(&StorageKey::from(1)).unwrap().present_value,
            StorageValue::from(1)
        );
    }
}
