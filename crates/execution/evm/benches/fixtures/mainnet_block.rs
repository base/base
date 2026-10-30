//! Fixture for the block execution and assembly workloads: one block shaped like a Base mainnet
//! block, with a transaction mix chosen by each bench through [`BlockMix`]. Shared by
//! `block_execution_iai.rs` and `contract_block_execution_iai.rs` (which count executing a
//! block) and `block_assembly_iai.rs` (which executes a block in setup and counts assembling its
//! header), so all three measure blocks built the same way.
//!
//! The block runs at a Cobalt timestamp, so every upgrade through Cobalt is active (Isthmus,
//! Jovian, and Osaka included); Denim and Everest are not. It holds, in order:
//!
//! - the Jovian L1 attributes deposit, executed by the real Jovian `L1Block` implementation
//!   behind the genesis predeploy proxy;
//! - [`BlockMix::user_deposits`] user deposits that mint ETH to an existing account;
//! - signed EIP-1559 transactions in a fixed shuffled order: plain transfers carrying
//!   [`TRANSFER_CALLDATA_BYTES`] of incompressible calldata; calls to a minimal token whose
//!   `transfer` reads and writes two balance slots and emits a log; calls to minimal
//!   constant-product pools whose `swap` reads five slots, writes six, and emits a log; and calls
//!   to the genesis `L2ToL1MessagePasser`, which write its storage and so feed the Isthmus
//!   withdrawals root. Every [`BlockMix::failed_call_every`]th token or pool caller holds no
//!   balance, so its call reverts.
//!
//! Every user transaction has its own sender at nonce zero, as in a mainnet block. The EIP-4788
//! and EIP-2935 system contracts are deployed, so pre-execution changes run them as in
//! production. The token and pool are hand-written bytecode, so the fixture needs no compiler.

use std::sync::Arc;

use alloy_consensus::{Header, SignableTransaction, TxEip1559};
use alloy_eips::{
    eip2718::{Encodable2718, WithEncoded},
    eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
    eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE},
};
use alloy_evm::{Evm, block::BlockExecutor};
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256, b256, hex, keccak256};
use base_common_chains::{ChainConfig, MAINNET_BERYL_ACTIVATION_ADMIN_ADDRESS};
use base_common_consensus::{
    BaseReceipt, BaseTransactionSigned, Deployers, L1BlockInfoJovian, Predeploys, SystemAddresses,
    TxDeposit,
};
use base_common_evm::{BaseBlockExecutionCtx, BaseSpecId, BaseTransaction};
use base_consensus_upgrades::Jovian;
use base_execution_chainspec::BaseChainSpecBuilder;
use base_execution_evm::BaseEvmConfig;
use reth_evm::{ConfigureEvm, EvmEnv, execute::WithTxEnv};
use reth_execution_types::BlockExecutionResult;
use reth_primitives_traits::Recovered;
use reth_revm::State;
use revm::{bytecode::Bytecode, context::TxEnv, database::InMemoryDB, state::AccountInfo};

/// Base mainnet chain id.
pub(crate) const CHAIN_ID: u64 = 8_453;
/// Block timestamp, after the Cobalt activation on Base mainnet.
pub(crate) const TIMESTAMP: u64 = 1_791_000_000;
/// Block gas limit.
pub(crate) const BLOCK_GAS_LIMIT: u64 = 150_000_000;
/// Block base fee, in wei: 0.005 gwei, typical for Base mainnet.
pub(crate) const BASE_FEE: u64 = 5_000_000;
/// Calldata that brings a transfer close to the 459-byte median mainnet transaction size.
pub(crate) const TRANSFER_CALLDATA_BYTES: usize = 344;
/// Distinct recipients of the plain transfers, all existing accounts.
const TRANSFER_RECIPIENTS: usize = 64;
/// Balance of every funded account, in wei.
const FUNDED_BALANCE: u128 = 10u128.pow(18);
/// Token balance of every funded token caller and holder.
const TOKEN_BALANCE: u64 = 1_000_000_000;
/// Address of the token contract.
const TOKEN: Address = Address::repeat_byte(0x70);
/// Starting reserves of each token in every pool.
const POOL_RESERVE: u128 = 10u128.pow(24);
/// Input-token balance of every funded pool caller.
const SWAPPER_BALANCE: u128 = 10u128.pow(18);
/// ERC-1967 implementation slot of the predeploy proxies.
const PROXY_IMPLEMENTATION_SLOT: B256 =
    b256!("0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");
/// Genesis implementation of `L2ToL1MessagePasser`, behind its predeploy proxy.
const MESSAGE_PASSER_IMPLEMENTATION: Address =
    Address::new(hex!("c0d3c0d3c0d3c0d3c0d3c0d3c0d3c0d3c0d30016"));
/// Genesis predeploys copied into the pre-state with their code, storage, and balance.
/// The fee vaults are present because every user transaction credits them.
const GENESIS_ACCOUNTS: [Address; 7] = [
    Predeploys::L1_BLOCK_INFO,
    Predeploys::L2_TO_L1_MESSAGE_PASSER,
    MESSAGE_PASSER_IMPLEMENTATION,
    Predeploys::SEQUENCER_FEE_VAULT,
    Predeploys::BASE_FEE_VAULT,
    Predeploys::L1_FEE_VAULT,
    Predeploys::OPERATOR_FEE_VAULT,
];

/// The node and builder binaries run on jemalloc. Using it here also keeps allocator instruction
/// counts stable: glibc malloc's free lists depend on the per-process hash seeds that order setup's
/// allocations, which moved the count by up to 0.2% between runs.
#[cfg(unix)]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Address namespaces, so every role draws distinct deterministic addresses.
const NS_TRANSFER_SENDER: u64 = 1;
const NS_TRANSFER_RECIPIENT: u64 = 2;
const NS_TOKEN_CALLER: u64 = 3;
const NS_TOKEN_HOLDER: u64 = 4;
const NS_WITHDRAWAL_SENDER: u64 = 5;
const NS_DEPOSITOR: u64 = 6;
const NS_DEPOSIT_RECIPIENT: u64 = 7;
const NS_SWAPPER: u64 = 8;
const NS_POOL: u64 = 9;

/// The transaction mix of a [`MainnetBlock`], in counts.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BlockMix {
    /// User deposits after the L1 attributes deposit.
    pub(crate) user_deposits: usize,
    /// Plain EIP-1559 transfers.
    pub(crate) transfers: usize,
    /// Calls to the token's `transfer`.
    pub(crate) token_calls: usize,
    /// Distinct token holders that receive token transfers, all holding tokens already.
    pub(crate) token_holders: usize,
    /// Calls to a pool's `swap`.
    pub(crate) swaps: usize,
    /// Distinct pools the swaps spread over.
    pub(crate) pools: usize,
    /// Calls to `L2ToL1MessagePasser.initiateWithdrawal`.
    pub(crate) withdrawals: usize,
    /// Every this-many-th token or pool caller holds no balance, so its call reverts.
    pub(crate) failed_call_every: usize,
}

/// The mainnet-shaped mix: mostly plain transfers with calldata, some token transfers, and two
/// withdrawals. `block_execution_iai.rs` executes it and `block_assembly_iai.rs` assembles it.
pub(crate) const MAINNET_MIX: BlockMix = BlockMix {
    user_deposits: 4,
    transfers: 300,
    token_calls: 80,
    token_holders: 32,
    swaps: 0,
    pools: 0,
    withdrawals: 2,
    failed_call_every: 4,
};

impl BlockMix {
    /// Whether the `index`th token or pool caller is funded, so its call succeeds.
    const fn funded_caller(&self, index: usize) -> bool {
        index % self.failed_call_every != self.failed_call_every - 1
    }
}

/// A transaction as the engine hands it to the executor: decoded, sender recovered, and its
/// [`BaseTransaction`] environment already built by the payload processor's conversion workers.
pub(crate) type BlockTx =
    WithTxEnv<BaseTransaction<TxEnv>, WithEncoded<Recovered<BaseTransactionSigned>>>;

/// Deterministic address in a numbered namespace.
fn address(namespace: u64, index: usize) -> Address {
    Address::from_word(keccak256([namespace.to_be_bytes(), (index as u64).to_be_bytes()].concat()))
}

/// Deterministic calldata that compresses poorly, so its estimated DA size tracks its length.
fn incompressible_calldata(seed: usize, len: usize) -> Bytes {
    (0..len.div_ceil(32))
        .flat_map(|chunk| keccak256([seed.to_be_bytes(), chunk.to_be_bytes()].concat()).0)
        .take(len)
        .collect()
}

/// Runtime code of a minimal token: `transfer(address to, uint256 amount)` without a selector
/// check. It reverts when the caller's balance (slot `caller`) is below `amount`; otherwise it
/// moves `amount` to slot `to` and emits `Transfer(caller, to, amount)`.
fn token_code() -> Bytes {
    [
        // balF = sload(caller); amt = calldataload(0x24); if amt > balF jump to 0x4b.
        &hex!("3354602435818111604b57")[..],
        // sstore(caller, balF - amt); sstore(to, sload(to) + amt); mstore(0, amt);
        // push to, caller, and the Transfer topic.
        &hex!("900333556004358054602435019055602435600052600435337f")[..],
        keccak256("Transfer(address,address,uint256)").as_slice(),
        // log3(0, 0x20, Transfer, caller, to); stop; 0x4b: revert(0, 0).
        &hex!("60206000a3005b600080fd")[..],
    ]
    .concat()
    .into()
}

/// Runtime code of a minimal constant-product pool: `swap(uint256 amountIn)` without a selector
/// check or fee. It reverts when the caller's input balance (slot `caller`) is below `amountIn`;
/// otherwise it computes `amountOut = reserve1 * amountIn / (reserve0 + amountIn)` from slots 0
/// and 1, debits the caller's input balance, updates both reserves, credits the caller's output
/// balance (slot `caller + 1`), increments the swap counter (slot 2), records the timestamp
/// (slot 3), and emits `Swap(caller, amountIn, amountOut)`.
fn pool_code() -> Bytes {
    [
        // in = calldataload(4); bal0 = sload(caller); if bal0 < in jump to 0x6f;
        // sstore(caller, bal0 - in); out = r1 * in / (r0 + in); sstore(1, r1 - out);
        // sstore(0, r0 + in); sstore(caller + 1, sload(caller + 1) + out);
        // sstore(2, sload(2) + 1); sstore(3, timestamp); mstore(0x20, out); mstore(0, in);
        // push caller and the Swap topic.
        &hex!(
            "6004353354818110606f57819003335560005460015482820181840204808203600155918301600055"
            "503360010180548201905560025460010160025542600355602052600052337f"
        )[..],
        keccak256("Swap(address,uint256,uint256)").as_slice(),
        // log2(0, 0x40, Swap, caller); stop; 0x6f: revert(0, 0).
        &hex!("60406000a2005b600080fd")[..],
    ]
    .concat()
    .into()
}

/// Signs an EIP-1559 call with the test signature; the executor does not check signatures, so
/// the sender is whatever the fixture assigns.
fn eip1559(
    index: usize,
    to: Address,
    gas_limit: u64,
    value: U256,
    input: Bytes,
) -> BaseTransactionSigned {
    TxEip1559 {
        chain_id: CHAIN_ID,
        nonce: 0,
        gas_limit,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 1_000 + index as u128,
        to: TxKind::Call(to),
        value,
        access_list: Default::default(),
        input,
    }
    .into_signed(Signature::test_signature())
    .into()
}

/// The kinds of signed user transaction in the block.
#[derive(Debug, Clone, Copy)]
enum UserTx {
    Transfer(usize),
    TokenCall(usize),
    Swap(usize),
    Withdrawal(usize),
}

/// A Base mainnet-shaped block, ready to execute over [`Self::fresh_state`].
pub(crate) struct MainnetBlock {
    /// Production EVM configuration for Base mainnet rules at [`TIMESTAMP`].
    pub(crate) evm_config: BaseEvmConfig,
    /// EVM environment derived from the block header.
    pub(crate) evm_env: EvmEnv<BaseSpecId>,
    /// Execution context derived from the block header.
    pub(crate) ctx: BaseBlockExecutionCtx,
    /// The block's transactions in order, with their encoded bytes and senders.
    pub(crate) transactions: Vec<WithEncoded<Recovered<BaseTransactionSigned>>>,
    /// State before the block.
    pub(crate) pre_state: InMemoryDB,
}

impl MainnetBlock {
    /// Builds the chain spec, a block with the transactions of `mix`, and the pre-state it
    /// executes over.
    pub(crate) fn new(mix: BlockMix) -> Self {
        let chain_spec = Arc::new(
            BaseChainSpecBuilder::default()
                .chain(CHAIN_ID.into())
                .genesis(Genesis::default())
                .activation_admin_address(MAINNET_BERYL_ACTIVATION_ADMIN_ADDRESS)
                .cobalt_activated()
                .build(),
        );
        let evm_config = BaseEvmConfig::base(chain_spec);
        let header = Header {
            parent_hash: keccak256("parent"),
            beneficiary: Predeploys::SEQUENCER_FEE_VAULT,
            number: 40_000_000,
            gas_limit: BLOCK_GAS_LIMIT,
            timestamp: TIMESTAMP,
            mix_hash: keccak256("prevrandao"),
            base_fee_per_gas: Some(BASE_FEE),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(keccak256("beacon")),
            // Jovian extra data: version 1, EIP-1559 denominator 250 and elasticity 6, and the
            // minimum base fee.
            extra_data: [
                &[1u8][..],
                &250u32.to_be_bytes(),
                &6u32.to_be_bytes(),
                &BASE_FEE.to_be_bytes(),
            ]
            .concat()
            .into(),
            ..Default::default()
        };
        let evm_env = evm_config.evm_env(&header).expect("Base EVM env for the block header");
        let ctx = BaseBlockExecutionCtx {
            parent_hash: header.parent_hash,
            parent_beacon_block_root: header.parent_beacon_block_root,
            extra_data: header.extra_data.clone(),
        };
        let pre_state = Self::pre_state(&evm_config, &evm_env, mix);
        let transactions = Self::transactions(&header, mix);
        Self { evm_config, evm_env, ctx, transactions, pre_state }
    }

    /// The deposits, then the user transactions in a fixed shuffled order.
    fn transactions(
        header: &Header,
        mix: BlockMix,
    ) -> Vec<WithEncoded<Recovered<BaseTransactionSigned>>> {
        let l1_info = L1BlockInfoJovian::new(
            23_000_000,
            header.timestamp - 12,
            1_000_000_000,
            keccak256("l1 origin"),
            3,
            ChainConfig::mainnet().genesis_batcher_address,
            1,
            1_055_762,
            2_269,
            0,
            0,
            L1BlockInfoJovian::DEFAULT_DA_FOOTPRINT_GAS_SCALAR,
        );
        let mut txs: Vec<(BaseTransactionSigned, Address)> = vec![(
            TxDeposit {
                source_hash: keccak256("l1 info deposit"),
                from: SystemAddresses::DEPOSITOR_ACCOUNT,
                to: TxKind::Call(Predeploys::L1_BLOCK_INFO),
                mint: 0,
                value: U256::ZERO,
                gas_limit: 1_000_000,
                is_system_transaction: false,
                input: l1_info.encode_calldata(),
            }
            .into(),
            SystemAddresses::DEPOSITOR_ACCOUNT,
        )];
        txs.extend((0..mix.user_deposits).map(|i| {
            let from = address(NS_DEPOSITOR, i);
            let amount = 5 * 10u128.pow(16);
            let deposit = TxDeposit {
                source_hash: keccak256([b"user deposit".as_slice(), &i.to_be_bytes()].concat()),
                from,
                to: TxKind::Call(address(NS_DEPOSIT_RECIPIENT, i)),
                mint: amount,
                value: U256::from(amount),
                gas_limit: 100_000,
                is_system_transaction: false,
                input: Bytes::new(),
            };
            (deposit.into(), from)
        }));

        let mut user_txs: Vec<UserTx> = (0..mix.transfers)
            .map(UserTx::Transfer)
            .chain((0..mix.token_calls).map(UserTx::TokenCall))
            .chain((0..mix.swaps).map(UserTx::Swap))
            .chain((0..mix.withdrawals).map(UserTx::Withdrawal))
            .collect();
        user_txs.sort_by_key(|kind| keccak256(format!("{kind:?}")));
        txs.extend(user_txs.into_iter().enumerate().map(|(index, kind)| match kind {
            UserTx::Transfer(i) => (
                eip1559(
                    index,
                    address(NS_TRANSFER_RECIPIENT, i % TRANSFER_RECIPIENTS),
                    50_000,
                    U256::from(10u64.pow(12) + i as u64),
                    incompressible_calldata(i, TRANSFER_CALLDATA_BYTES),
                ),
                address(NS_TRANSFER_SENDER, i),
            ),
            UserTx::TokenCall(i) => {
                let to = address(NS_TOKEN_HOLDER, i % mix.token_holders);
                let input = [
                    &keccak256("transfer(address,uint256)")[..4],
                    to.into_word().as_slice(),
                    &U256::from(1_000 + i).to_be_bytes::<32>(),
                ]
                .concat();
                (
                    eip1559(index, TOKEN, 100_000, U256::ZERO, input.into()),
                    address(NS_TOKEN_CALLER, i),
                )
            }
            UserTx::Swap(i) => {
                let input = [
                    &keccak256("swap(uint256)")[..4],
                    &U256::from(10u64.pow(15) + i as u64).to_be_bytes::<32>(),
                ]
                .concat();
                (
                    eip1559(
                        index,
                        address(NS_POOL, i % mix.pools),
                        150_000,
                        U256::ZERO,
                        input.into(),
                    ),
                    address(NS_SWAPPER, i),
                )
            }
            UserTx::Withdrawal(i) => {
                let sender = address(NS_WITHDRAWAL_SENDER, i);
                // initiateWithdrawal(sender, 100_000, ""): target, L1 gas limit, offset and
                // length of the empty data.
                let input = [
                    &keccak256("initiateWithdrawal(address,uint256,bytes)")[..4],
                    sender.into_word().as_slice(),
                    &U256::from(100_000).to_be_bytes::<32>(),
                    &U256::from(0x60).to_be_bytes::<32>(),
                    &U256::ZERO.to_be_bytes::<32>(),
                ]
                .concat();
                (
                    eip1559(
                        index,
                        Predeploys::L2_TO_L1_MESSAGE_PASSER,
                        200_000,
                        U256::from(10u64.pow(16)),
                        input.into(),
                    ),
                    sender,
                )
            }
        }));

        txs.into_iter()
            .map(|(tx, sender)| {
                WithEncoded::new(tx.encoded_2718().into(), Recovered::new_unchecked(tx, sender))
            })
            .collect()
    }

    /// Mainnet genesis predeploys, the Jovian `L1Block` implementation, the system contracts, the
    /// token, and every account the block reads, funded.
    fn pre_state(
        evm_config: &BaseEvmConfig,
        evm_env: &EvmEnv<BaseSpecId>,
        mix: BlockMix,
    ) -> InMemoryDB {
        let mut db = InMemoryDB::default();

        let genesis: Genesis = serde_json::from_str(ChainConfig::mainnet().genesis_json)
            .expect("Base mainnet genesis parses");
        for address in GENESIS_ACCOUNTS {
            let account = &genesis.alloc[&address];
            let mut info = AccountInfo::from_balance(account.balance);
            if let Some(code) = &account.code {
                info = info.with_code(Bytecode::new_raw(code.clone()));
            }
            db.insert_account_info(address, info);
            for (slot, value) in account.storage.iter().flatten() {
                db.insert_account_storage(address, (*slot).into(), (*value).into())
                    .expect("in-memory storage insert");
            }
        }

        // Point the L1Block proxy at the Jovian implementation the Jovian upgrade deploys.
        let jovian_l1_block = Self::deploy_jovian_l1_block(evm_config, evm_env);
        db.insert_account_info(Jovian::l1_block_address(), jovian_l1_block);
        db.insert_account_storage(
            Predeploys::L1_BLOCK_INFO,
            PROXY_IMPLEMENTATION_SLOT.into(),
            Jovian::l1_block_address().into_word().into(),
        )
        .expect("in-memory storage insert");

        let (token_code, pool_code) = (token_code(), pool_code());
        let contracts = [
            (BEACON_ROOTS_ADDRESS, &BEACON_ROOTS_CODE),
            (HISTORY_STORAGE_ADDRESS, &HISTORY_STORAGE_CODE),
            (TOKEN, &token_code),
        ]
        .into_iter()
        .chain((0..mix.pools).map(|pool| (address(NS_POOL, pool), &pool_code)));
        for (address, code) in contracts {
            db.insert_account_info(
                address,
                AccountInfo { nonce: 1, ..Default::default() }
                    .with_code(Bytecode::new_raw(code.clone())),
            );
        }

        let funded = AccountInfo::from_balance(U256::from(FUNDED_BALANCE));
        let funded_addresses = (0..mix.transfers)
            .map(|i| address(NS_TRANSFER_SENDER, i))
            .chain((0..TRANSFER_RECIPIENTS).map(|i| address(NS_TRANSFER_RECIPIENT, i)))
            .chain((0..mix.token_calls).map(|i| address(NS_TOKEN_CALLER, i)))
            .chain((0..mix.swaps).map(|i| address(NS_SWAPPER, i)))
            .chain((0..mix.withdrawals).map(|i| address(NS_WITHDRAWAL_SENDER, i)))
            .chain((0..mix.user_deposits).map(|i| address(NS_DEPOSIT_RECIPIENT, i)));
        for address in funded_addresses {
            db.insert_account_info(address, funded.clone());
        }

        let token_holders = (0..mix.token_holders).map(|i| address(NS_TOKEN_HOLDER, i)).chain(
            (0..mix.token_calls)
                .filter(|&i| mix.funded_caller(i))
                .map(|i| address(NS_TOKEN_CALLER, i)),
        );
        for holder in token_holders {
            db.insert_account_storage(TOKEN, holder.into_word().into(), U256::from(TOKEN_BALANCE))
                .expect("in-memory storage insert");
        }

        // Pools start with equal reserves, a nonzero swap counter, and an earlier timestamp.
        for pool in 0..mix.pools {
            for (slot, value) in [
                (0, U256::from(POOL_RESERVE)),
                (1, U256::from(POOL_RESERVE)),
                (2, U256::from(1_000)),
                (3, U256::from(TIMESTAMP - 2)),
            ] {
                db.insert_account_storage(address(NS_POOL, pool), U256::from(slot), value)
                    .expect("in-memory storage insert");
            }
        }
        for swapper in (0..mix.swaps).filter(|&i| mix.funded_caller(i)) {
            db.insert_account_storage(
                address(NS_POOL, swapper % mix.pools),
                address(NS_SWAPPER, swapper).into_word().into(),
                U256::from(SWAPPER_BALANCE),
            )
            .expect("in-memory storage insert");
        }

        db
    }

    /// Runs the Jovian upgrade's `L1Block` deployment and returns the deployed account.
    fn deploy_jovian_l1_block(
        evm_config: &BaseEvmConfig,
        evm_env: &EvmEnv<BaseSpecId>,
    ) -> AccountInfo {
        let deployment: BaseTransactionSigned = TxDeposit {
            source_hash: Jovian::deploy_l1_block_source(),
            from: Deployers::JOVIAN_L1_BLOCK,
            to: TxKind::Create,
            mint: 0,
            value: U256::ZERO,
            gas_limit: 447_315,
            is_system_transaction: false,
            input: Jovian::l1_block_deployment_bytecode(),
        }
        .into();
        let mut db = State::builder().with_database(InMemoryDB::default()).build();
        let mut evm = evm_config.evm_with_env(&mut db, evm_env.clone());
        let mut outcome = evm
            .transact(Recovered::new_unchecked(deployment, Deployers::JOVIAN_L1_BLOCK))
            .expect("Jovian L1Block deployment executes");
        assert!(outcome.result.is_success(), "Jovian L1Block deployment: {:?}", outcome.result);
        outcome
            .state
            .remove(&Jovian::l1_block_address())
            .expect("Jovian L1Block is deployed at its documented address")
            .info
    }

    /// A fresh copy of the pre-state, configured as block import configures it.
    pub(crate) fn fresh_state(&self) -> State<InMemoryDB> {
        State::builder().with_database(self.pre_state.clone()).with_bundle_update().build()
    }

    /// The block's transactions with their EVM environments built, as the engine's payload
    /// processor converts them before execution.
    pub(crate) fn block_txs(&self) -> Vec<BlockTx> {
        self.transactions.iter().cloned().map(WithTxEnv::new).collect()
    }

    /// Executes the block over `state` the way block import does: build the EVM and the
    /// production Base block executor, apply pre-execution changes, execute every transaction,
    /// and finish.
    pub(crate) fn execute(
        &self,
        state: &mut State<InMemoryDB>,
        txs: Vec<BlockTx>,
    ) -> BlockExecutionResult<BaseReceipt> {
        let evm = self.evm_config.evm_with_env(state, self.evm_env.clone());
        let mut executor = self.evm_config.create_executor(evm, self.ctx.clone());
        executor.apply_pre_execution_changes().expect("pre-execution changes apply");
        for tx in txs {
            executor.execute_transaction(tx).expect("every block transaction is valid");
        }
        executor.finish().expect("block finishes").1
    }
}
