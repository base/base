//! Shared test environment for Base Beryl action tests.

use alloy_consensus::TxReceipt;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, hex, uint};
use alloy_sol_types::{SolCall, SolEvent, SolValue};
use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, L2Sequencer,
    SharedL1Chain, TEST_ACCOUNT_ADDRESS, TestAccount, TestRollupConfigBuilder, TestRollupNode,
    VerifierPipeline,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_common_consensus::{BaseBlock, BaseReceipt, BaseTxEnvelope};
use base_common_precompiles::{
    ActivationFeature, ActivationRegistryStorage, B20FactoryStorage, B20Variant,
    IActivationRegistry, IB20, IB20Factory, IPolicyRegistry,
};
use base_precompile_storage::StorageKey;
use base_test_utils::Account;

/// L2 timestamp where the Beryl fork activates in these tests.
pub(crate) const BERYL_ACTIVATION_TIMESTAMP: u64 = 4;

/// B-20 token storage slot for `total_supply`.
const B20_TOTAL_SUPPLY_SLOT: U256 =
    uint!(0xc78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434003_U256);

/// B-20 token storage slot for `balances`.
const B20_BALANCES_SLOT: U256 =
    uint!(0xc78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434004_U256);

/// B-20 token storage slot for `allowances`.
const B20_ALLOWANCES_SLOT: U256 =
    uint!(0xc78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434005_U256);

/// Storage slot where staticcall probes store the call success flag.
const PROBE_CALL_SUCCESS_SLOT: U256 = U256::ZERO;

/// Storage slot where staticcall probes store the first returned word.
const PROBE_RETURN_WORD_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);

/// Storage slot where staticcall probes store the returned byte length.
const PROBE_RETURN_SIZE_SLOT: U256 = U256::from_limbs([2, 0, 0, 0]);

/// Storage slot where staticcall probes store `keccak256(returndata)`.
const PROBE_RETURN_HASH_SLOT: U256 = U256::from_limbs([3, 0, 0, 0]);

/// Test environment preconfigured to cross Base Beryl at L2 block 2.
pub(crate) struct BerylTestEnv {
    /// Sequencer used to build Beryl precompile blocks.
    pub(crate) sequencer: L2Sequencer,
    harness: ActionTestHarness,
    batcher_cfg: BatcherConfig,
    node: TestRollupNode<VerifierPipeline>,
    chain: SharedL1Chain,
    chain_id: u64,
    bob_account: TestAccount,
}

impl BerylTestEnv {
    /// Gas limit used for B-20 precompile transactions.
    pub(crate) const B20_GAS_LIMIT: u64 = 10_000_000;

    /// Gas limit used for B-20 staticcall probe transactions.
    pub(crate) const B20_PROBE_GAS_LIMIT: u64 = 1_000_000;

    /// Fixed decimals for the default B-20 token variant.
    pub(crate) const B20_DECIMALS: u8 = 6;

    /// Name for the default B-20 token variant.
    pub(crate) const B20_NAME: &str = "Action B20";

    /// Symbol for the default B-20 token variant.
    pub(crate) const B20_SYMBOL: &str = "AB20";

    /// Fixed decimals for the stablecoin B-20 token variant.
    pub(crate) const B20_STABLECOIN_DECIMALS: u8 = 6;

    /// Name for the stablecoin B-20 token variant.
    pub(crate) const B20_STABLECOIN_NAME: &str = "Action USD";

    /// Symbol for the stablecoin B-20 token variant.
    pub(crate) const B20_STABLECOIN_SYMBOL: &str = "AUSD";

    /// ISO 4217 currency code for the stablecoin B-20 token variant.
    pub(crate) const B20_STABLECOIN_CURRENCY: &str = "USD";

    /// Fixed decimals for the security B-20 token variant.
    pub(crate) const B20_ASSET_DECIMALS: u8 = 6;

    /// Name for the security B-20 token variant.
    pub(crate) const B20_ASSET_NAME: &str = "Action Security";

    /// Symbol for the security B-20 token variant.
    pub(crate) const B20_ASSET_SYMBOL: &str = "ASEC";

    /// Initial B-20 supply minted to Alice.
    pub(crate) const B20_INITIAL_SUPPLY: u64 = 1_000_000;

    /// Amount transferred from Alice to Bob.
    pub(crate) const B20_BOB_TRANSFER: u64 = 100_000;

    /// Amount transferred from Bob to Carol.
    pub(crate) const B20_CAROL_TRANSFER: u64 = 25_000;

    /// Allowance Alice approves for Bob.
    pub(crate) const B20_BOB_ALLOWANCE: u64 = 50_000;

    /// Amount Bob transfers from Alice to Carol using allowance.
    pub(crate) const B20_TRANSFER_FROM_CAROL: u64 = 40_000;

    /// Creates an environment with all forks through Azul active at genesis
    /// and Base Beryl active at timestamp 4.
    pub(crate) fn new() -> Self {
        let batcher_cfg = BatcherConfig {
            encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
            ..Default::default()
        };

        let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
            .through_isthmus()
            .with_jovian_at(0)
            .with_azul_at(0)
            .with_beryl_at(BERYL_ACTIVATION_TIMESTAMP)
            .build();
        let chain_id = rollup_cfg.l2_chain_id.id();
        let harness = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

        let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
        let mut sequencer = harness.create_l2_sequencer(l1_chain);

        let (node, chain) = harness.create_test_rollup_node_from_sequencer(
            &mut sequencer,
            SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
        );

        let bob_account = TestAccount::new(Account::Bob.signer_b256());

        Self { sequencer, harness, batcher_cfg, node, chain, chain_id, bob_account }
    }

    /// Returns the funded test account that creates and holds the B-20 supply.
    pub(crate) const fn alice() -> Address {
        TEST_ACCOUNT_ADDRESS
    }

    /// Returns Bob's recipient address for B-20 transfer assertions.
    pub(crate) const fn bob() -> Address {
        Account::Bob.address()
    }

    /// Returns Carol's recipient address for B-20 transfer assertions.
    pub(crate) const fn carol() -> Address {
        Account::Charlie.address()
    }

    /// Returns the address created by the first test-account deployment.
    pub(crate) fn first_contract_address(&self) -> Address {
        TEST_ACCOUNT_ADDRESS.create(0)
    }

    /// Creates and signs a test-account transaction.
    pub(crate) fn create_tx(&self, to: TxKind, input: Bytes, gas_limit: u64) -> BaseTxEnvelope {
        let account = self.sequencer.test_account();
        let mut account = account.lock().expect("test account lock");
        account.create_tx(self.chain_id, to, input, U256::ZERO, gas_limit)
    }

    /// Creates and signs a transaction from Bob's account.
    pub(crate) fn create_bob_tx(
        &mut self,
        to: TxKind,
        input: Bytes,
        gas_limit: u64,
    ) -> BaseTxEnvelope {
        Self::create_account_tx(self.chain_id, &mut self.bob_account, to, input, gas_limit)
    }

    /// Returns the L2 chain ID used by the Beryl test environment.
    pub(crate) const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Activation registry feature ID for the B-20 asset precompile.
    pub(crate) const fn b20_asset_feature() -> B256 {
        ActivationFeature::B20Asset.id()
    }

    /// Activation registry feature ID for the B-20 stablecoin precompile.
    pub(crate) const fn b20_stablecoin_feature() -> B256 {
        ActivationFeature::B20Stablecoin.id()
    }

    /// Activation registry feature ID for the policy registry precompile.
    pub(crate) const fn policy_registry_feature() -> B256 {
        ActivationFeature::PolicyRegistry.id()
    }

    /// Computes the expected policy ID for a custom policy.
    ///
    /// IDs are encoded as `(type_discriminant << 56) | counter` where the counter is a
    /// global monotonic sequence. Counters 0 and 1 are reserved for the built-in policies,
    /// so the first custom policy always gets counter 2.
    pub(crate) const fn policy_id(policy_type: IPolicyRegistry::PolicyType, counter: u64) -> u64 {
        (policy_type as u64) << 56 | counter
    }

    /// Alternate salt for a second token creation used in deactivation/re-activation tests.
    pub(crate) const ALT_SALT: B256 = B256::repeat_byte(0x43);

    /// Returns the deterministic salt used to create the B-20 token.
    pub(crate) const fn b20_token_salt() -> B256 {
        B256::repeat_byte(0x42)
    }

    /// Returns the deterministic salt used to create the B-20 stablecoin token.
    pub(crate) const fn b20_stablecoin_salt() -> B256 {
        B256::repeat_byte(0x45)
    }

    /// Returns the deterministic salt used to create the B-20 security token.
    pub(crate) const fn b20_security_salt() -> B256 {
        B256::repeat_byte(0x46)
    }

    /// Returns the deterministic B-20 token address created by Alice.
    pub(crate) fn b20_token_address(&self) -> Address {
        B20Variant::Asset.compute_address(Self::alice(), Self::b20_token_salt()).0
    }

    /// Returns the deterministic B-20 stablecoin address created by Alice.
    pub(crate) fn b20_stablecoin_address(&self) -> Address {
        B20Variant::Stablecoin.compute_address(Self::alice(), Self::b20_stablecoin_salt()).0
    }

    /// Returns the deterministic B-20 security token address created by Alice.
    pub(crate) fn b20_security_address(&self) -> Address {
        B20Variant::Asset.compute_address(Self::alice(), Self::b20_security_salt()).0
    }

    /// Creates a transaction that calls the B-20 token factory with the default salt.
    pub(crate) fn create_b20_token_tx(&self) -> BaseTxEnvelope {
        self.create_b20_token_with_salt_tx(Self::b20_token_salt())
    }

    /// Creates a transaction that calls the B-20 token factory with the given `salt`.
    pub(crate) fn create_b20_token_with_salt_tx(&self, salt: B256) -> BaseTxEnvelope {
        self.create_tx(
            TxKind::Call(B20FactoryStorage::ADDRESS),
            Bytes::from(self.create_b20_token_call_with_salt(salt).abi_encode()),
            Self::B20_GAS_LIMIT,
        )
    }

    /// Creates a transaction that calls the B-20 token factory for a stablecoin.
    pub(crate) fn create_b20_stablecoin_tx(&self) -> BaseTxEnvelope {
        self.create_b20_stablecoin_with_salt_tx(Self::b20_stablecoin_salt())
    }

    /// Creates a stablecoin factory transaction with the given `salt`.
    pub(crate) fn create_b20_stablecoin_with_salt_tx(&self, salt: B256) -> BaseTxEnvelope {
        self.create_tx(
            TxKind::Call(B20FactoryStorage::ADDRESS),
            Bytes::from(self.create_b20_stablecoin_call_with_salt(salt).abi_encode()),
            Self::B20_GAS_LIMIT,
        )
    }

    /// Creates a transaction that calls the B-20 token factory for a security token.
    pub(crate) fn create_b20_security_tx(&self) -> BaseTxEnvelope {
        self.create_b20_security_with_salt_tx(Self::b20_security_salt())
    }

    /// Creates a security-token factory transaction with the given `salt`.
    pub(crate) fn create_b20_security_with_salt_tx(&self, salt: B256) -> BaseTxEnvelope {
        self.create_tx(
            TxKind::Call(B20FactoryStorage::ADDRESS),
            Bytes::from(self.create_b20_security_call_with_salt(salt).abi_encode()),
            Self::B20_GAS_LIMIT,
        )
    }

    /// Creates and signs a transaction that deploys a staticcall probe for `target`.
    pub(crate) fn deploy_staticcall_probe_tx(&self, target: Address) -> (Address, BaseTxEnvelope) {
        let account = self.sequencer.test_account();
        let mut account = account.lock().expect("test account lock");
        let address = account.address().create(account.nonce());
        let tx = account.create_tx(
            self.chain_id,
            TxKind::Create,
            Self::staticcall_probe_init_code(target),
            U256::ZERO,
            Self::B20_PROBE_GAS_LIMIT,
        );
        (address, tx)
    }

    /// Creates a transaction that calls a deployed staticcall probe with arbitrary calldata.
    pub(crate) fn call_staticcall_probe_tx(
        &self,
        probe: Address,
        input: Bytes,
        gas_limit: u64,
    ) -> BaseTxEnvelope {
        self.create_tx(TxKind::Call(probe), input, gas_limit)
    }

    /// Creates a transaction that transfers B-20 tokens from Alice to `to`.
    pub(crate) fn transfer_b20_tx(
        &self,
        token: Address,
        to: Address,
        amount: U256,
    ) -> BaseTxEnvelope {
        self.create_tx(
            TxKind::Call(token),
            Bytes::from(IB20::transferCall { to, amount }.abi_encode()),
            Self::B20_GAS_LIMIT,
        )
    }

    /// Creates a transaction that approves `spender` to spend Alice's B-20 tokens.
    pub(crate) fn approve_b20_tx(
        &self,
        token: Address,
        spender: Address,
        amount: U256,
    ) -> BaseTxEnvelope {
        self.create_tx(
            TxKind::Call(token),
            Bytes::from(IB20::approveCall { spender, amount }.abi_encode()),
            Self::B20_GAS_LIMIT,
        )
    }

    /// Creates a transaction that transfers B-20 tokens from Bob to `to`.
    pub(crate) fn transfer_b20_from_bob_tx(
        &mut self,
        token: Address,
        to: Address,
        amount: U256,
    ) -> BaseTxEnvelope {
        let input = Bytes::from(IB20::transferCall { to, amount }.abi_encode());
        Self::create_account_tx(
            self.chain_id,
            &mut self.bob_account,
            TxKind::Call(token),
            input,
            Self::B20_GAS_LIMIT,
        )
    }

    /// Creates a transaction that transfers B-20 tokens from Alice using Bob's allowance.
    pub(crate) fn transfer_b20_from_alice_by_bob_tx(
        &mut self,
        token: Address,
        to: Address,
        amount: U256,
    ) -> BaseTxEnvelope {
        let input =
            Bytes::from(IB20::transferFromCall { from: Self::alice(), to, amount }.abi_encode());
        Self::create_account_tx(
            self.chain_id,
            &mut self.bob_account,
            TxKind::Call(token),
            input,
            Self::B20_GAS_LIMIT,
        )
    }

    /// Creates an activation registry `activate(feature)` transaction signed by the admin.
    ///
    /// The test rollup config sets `TEST_ACCOUNT_ADDRESS` as the activation admin.
    pub(crate) fn activate_feature_tx(&self, feature: B256) -> BaseTxEnvelope {
        let input = Bytes::from(IActivationRegistry::activateCall { feature }.abi_encode());
        self.create_tx(TxKind::Call(ActivationRegistryStorage::ADDRESS), input, Self::B20_GAS_LIMIT)
    }

    /// Creates an activation registry `deactivate(feature)` transaction signed by the admin.
    pub(crate) fn deactivate_feature_tx(&self, feature: B256) -> BaseTxEnvelope {
        let input = Bytes::from(IActivationRegistry::deactivateCall { feature }.abi_encode());
        self.create_tx(TxKind::Call(ActivationRegistryStorage::ADDRESS), input, Self::B20_GAS_LIMIT)
    }

    /// Creates a transaction that calls `totalSupply()` through `probe`.
    pub(crate) fn probe_b20_total_supply_tx(&self, probe: Address) -> BaseTxEnvelope {
        self.create_tx(
            TxKind::Call(probe),
            Bytes::from(IB20::totalSupplyCall {}.abi_encode()),
            Self::B20_PROBE_GAS_LIMIT,
        )
    }

    /// Creates a transaction that calls `balanceOf(account)` through `probe`.
    pub(crate) fn probe_b20_balance_tx(&self, probe: Address, account: Address) -> BaseTxEnvelope {
        self.create_tx(
            TxKind::Call(probe),
            Bytes::from(IB20::balanceOfCall { account }.abi_encode()),
            Self::B20_PROBE_GAS_LIMIT,
        )
    }

    /// Creates a transaction that calls `allowance(owner, spender)` through `probe`.
    pub(crate) fn probe_b20_allowance_tx(
        &self,
        probe: Address,
        owner: Address,
        spender: Address,
    ) -> BaseTxEnvelope {
        self.create_tx(
            TxKind::Call(probe),
            Bytes::from(IB20::allowanceCall { owner, spender }.abi_encode()),
            Self::B20_PROBE_GAS_LIMIT,
        )
    }

    /// Creates a transaction that calls `decimals()` through `probe`.
    pub(crate) fn probe_b20_decimals_tx(&self, probe: Address) -> BaseTxEnvelope {
        self.create_tx(
            TxKind::Call(probe),
            Bytes::from(IB20::decimalsCall {}.abi_encode()),
            Self::B20_PROBE_GAS_LIMIT,
        )
    }

    /// Reads the B-20 token's total supply from storage.
    pub(crate) fn b20_total_supply(&self, token: Address) -> U256 {
        self.sequencer.storage_at(token, B20_TOTAL_SUPPLY_SLOT)
    }

    /// Reads a B-20 account balance from storage.
    pub(crate) fn b20_balance(&self, token: Address, account: Address) -> U256 {
        self.sequencer.storage_at(token, Self::b20_balance_slot(account))
    }

    /// Reads a B-20 allowance from storage.
    pub(crate) fn b20_allowance(&self, token: Address, owner: Address, spender: Address) -> U256 {
        self.sequencer.storage_at(token, Self::b20_allowance_slot(owner, spender))
    }

    /// Reads whether a staticcall probe's most recent call succeeded.
    pub(crate) fn probe_call_succeeded(&self, probe: Address) -> bool {
        self.sequencer.storage_at(probe, PROBE_CALL_SUCCESS_SLOT) == U256::ONE
    }

    /// Reads the first returned word from a staticcall probe's most recent call.
    pub(crate) fn probe_return_word(&self, probe: Address) -> U256 {
        self.sequencer.storage_at(probe, PROBE_RETURN_WORD_SLOT)
    }

    /// Reads the returned byte length from a staticcall probe's most recent call.
    pub(crate) fn probe_return_size(&self, probe: Address) -> U256 {
        self.sequencer.storage_at(probe, PROBE_RETURN_SIZE_SLOT)
    }

    /// Reads `keccak256(returndata)` from a staticcall probe's most recent call.
    pub(crate) fn probe_return_hash(&self, probe: Address) -> B256 {
        B256::from(self.sequencer.storage_at(probe, PROBE_RETURN_HASH_SLOT).to_be_bytes::<32>())
    }

    /// Returns whether a user transaction in `block` succeeded.
    pub(crate) fn user_tx_succeeded(&self, block: &BaseBlock, user_tx_index: usize) -> bool {
        self.user_tx_receipt(block, user_tx_index).status()
    }

    /// Returns the receipt for a non-deposit transaction in `block`.
    pub(crate) fn user_tx_receipt(&self, block: &BaseBlock, user_tx_index: usize) -> BaseReceipt {
        let deposit_count = block
            .body
            .transactions
            .iter()
            .take_while(|tx| matches!(tx, BaseTxEnvelope::Deposit(_)))
            .count();
        let receipts = self
            .sequencer
            .receipts_at(block.header.number)
            .unwrap_or_else(|| panic!("receipts must exist for L2 block {}", block.header.number));
        receipts
            .into_iter()
            .nth(deposit_count + user_tx_index)
            .unwrap_or_else(|| panic!("user tx receipt {user_tx_index} must exist"))
    }

    /// Returns whether a user transaction emitted the expected B-20 `Transfer` event.
    pub(crate) fn b20_transfer_log_emitted(
        &self,
        block: &BaseBlock,
        user_tx_index: usize,
        token: Address,
        from: Address,
        to: Address,
        amount: U256,
    ) -> bool {
        let expected = IB20::Transfer { from, to, amount }.encode_log_data();
        self.user_tx_receipt(block, user_tx_index)
            .logs()
            .iter()
            .any(|log| log.address == token && log.data == expected)
    }

    /// Returns whether a user transaction emitted the expected B-20 `Approval` event.
    pub(crate) fn b20_approval_log_emitted(
        &self,
        block: &BaseBlock,
        user_tx_index: usize,
        token: Address,
        owner: Address,
        spender: Address,
        amount: U256,
    ) -> bool {
        let expected = IB20::Approval { owner, spender, amount }.encode_log_data();
        self.user_tx_receipt(block, user_tx_index)
            .logs()
            .iter()
            .any(|log| log.address == token && log.data == expected)
    }

    /// Batches the supplied L2 blocks, derives each one, and asserts the final safe head.
    pub(crate) async fn derive_blocks(
        &mut self,
        blocks: impl IntoIterator<Item = (BaseBlock, u64)>,
        expected_safe_head: u64,
    ) {
        let mut batcher = Batcher::new(
            ActionL2Source::new(),
            &self.harness.rollup_config,
            self.batcher_cfg.clone(),
        );
        self.node.initialize().await;

        for (block, i) in blocks {
            batcher.push_block(block);
            batcher.advance(&mut self.harness.l1).await;
            self.chain.push(self.harness.l1.tip().clone());
            let derived = self.node.run_until_idle().await;
            assert_eq!(derived, 1, "L1 block {i} should derive exactly one L2 block");
        }

        assert_eq!(
            self.node.l2_safe_number(),
            expected_safe_head,
            "all {expected_safe_head} L2 blocks must derive through the Beryl boundary"
        );
    }

    fn create_b20_token_call_with_salt(&self, salt: B256) -> IB20Factory::createB20Call {
        IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::ASSET,
            salt,
            params: self.b20_token_params().abi_encode().into(),
            initCalls: vec![
                IB20::mintCall { to: Self::alice(), amount: U256::from(Self::B20_INITIAL_SUPPLY) }
                    .abi_encode()
                    .into(),
            ],
        }
    }

    fn create_b20_stablecoin_call_with_salt(&self, salt: B256) -> IB20Factory::createB20Call {
        IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::STABLECOIN,
            salt,
            params: self.b20_stablecoin_params().abi_encode().into(),
            initCalls: vec![
                IB20::mintCall { to: Self::alice(), amount: U256::from(Self::B20_INITIAL_SUPPLY) }
                    .abi_encode()
                    .into(),
            ],
        }
    }

    fn create_b20_security_call_with_salt(&self, salt: B256) -> IB20Factory::createB20Call {
        IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::ASSET,
            salt,
            params: self.b20_asset_params().abi_encode().into(),
            initCalls: vec![
                IB20::mintCall { to: Self::alice(), amount: U256::from(Self::B20_INITIAL_SUPPLY) }
                    .abi_encode()
                    .into(),
            ],
        }
    }

    fn create_account_tx(
        chain_id: u64,
        account: &mut TestAccount,
        to: TxKind,
        input: Bytes,
        gas_limit: u64,
    ) -> BaseTxEnvelope {
        account.create_tx(chain_id, to, input, U256::ZERO, gas_limit)
    }

    fn staticcall_probe_init_code(target: Address) -> Bytes {
        let mut runtime = Vec::with_capacity(65);
        runtime.extend_from_slice(&hex!("3660006000376000600036600073"));
        runtime.extend_from_slice(target.as_slice());
        runtime.extend_from_slice(&hex!(
            "5afa" // staticcall(gas(), target, 0, calldatasize(), 0, 0)
            "8060005550" // store success in slot 0
            "3d80600255" // store returndatasize in slot 2
            "80600060003e" // copy returndata to memory
            "600051600155" // store first returned word in slot 1
            "600020600355" // store keccak256(returndata) in slot 3
            "00"
        ));

        let mut init_code = Vec::with_capacity(12 + runtime.len());
        init_code.extend_from_slice(&hex!("6041600c60003960416000f3"));
        init_code.extend_from_slice(&runtime);
        Bytes::from(init_code)
    }

    fn b20_token_params(&self) -> IB20Factory::B20AssetCreateParams {
        IB20Factory::B20AssetCreateParams {
            version: B20Variant::Asset.supported_version(),
            name: Self::B20_NAME.to_string(),
            symbol: Self::B20_SYMBOL.to_string(),
            initialAdmin: Self::alice(),
            decimals: 6,
        }
    }

    fn b20_stablecoin_params(&self) -> IB20Factory::B20StablecoinCreateParams {
        IB20Factory::B20StablecoinCreateParams {
            version: B20Variant::Stablecoin.supported_version(),
            name: Self::B20_STABLECOIN_NAME.to_string(),
            symbol: Self::B20_STABLECOIN_SYMBOL.to_string(),
            initialAdmin: Self::alice(),
            currency: Self::B20_STABLECOIN_CURRENCY.to_string(),
        }
    }

    fn b20_asset_params(&self) -> IB20Factory::B20AssetCreateParams {
        IB20Factory::B20AssetCreateParams {
            version: B20Variant::Asset.supported_version(),
            name: Self::B20_ASSET_NAME.to_string(),
            symbol: Self::B20_ASSET_SYMBOL.to_string(),
            initialAdmin: Self::alice(),
            decimals: Self::B20_ASSET_DECIMALS,
        }
    }

    fn b20_balance_slot(account: Address) -> U256 {
        account.mapping_slot(B20_BALANCES_SLOT)
    }

    fn b20_allowance_slot(owner: Address, spender: Address) -> U256 {
        spender.mapping_slot(owner.mapping_slot(B20_ALLOWANCES_SLOT))
    }
}

impl Default for BerylTestEnv {
    fn default() -> Self {
        Self::new()
    }
}
