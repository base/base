//! System tests for gas-limit fee billing, the Everest rule that charges a transaction for its
//! reserved gas (`gas_limit`, or `gas_limit + payer_auth` for EIP-8130) instead of the gas it
//! consumed.
//!
//! The same over-provisioned transfer is sent before and after the Everest activation on a real
//! builder + validator stack. Before Everest the receipt and the sender's balance reflect the
//! 21k gas a transfer uses; after it they reflect the 100k gas limit, every fee recipient is paid
//! on the limit, `eth_estimateGas` still reports the metered requirement, and the validator agrees
//! with the builder on every block. A sponsored EIP-8130 transaction is then billed to its payer
//! for the reserved `gas_limit + payer_auth` budget, and one sent at `gas_limit = eth_estimateGas`
//! settles at the estimate plus payer authentication, which the estimate leaves out.

mod common;

use std::time::Duration;

use alloy_consensus::{SignableTransaction, Transaction, constants::ETH_TO_WEI};
use alloy_eips::{BlockId, BlockNumberOrTag, eip2718::Encodable2718};
use alloy_network::{EthereumWallet, ReceiptResponse, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, sol};
use base_common_consensus::{Call, Eip8130Constants, Eip8130Signed, Predeploys, TxEip8130};
use base_common_eip8130::IntrinsicGas;
use base_common_genesis::{BaseUpgrade, RollupConfig};
use base_common_network::Base;
use base_common_rpc_types::{BaseTransactionReceipt, BaseTransactionRequest};
use base_system_tests::{
    ANVIL_ACCOUNT_1, ANVIL_ACCOUNT_2, DEPLOYER, SystemTestProviderExt, SystemTestStack,
    SystemTestStackBuilder,
};
use eyre::{OptionExt, Result, WrapErr, ensure};
use tokio::time::{sleep, timeout};

use crate::common::{
    BASE_AZUL_ACTIVATION_BLOCK, BASE_BERYL_ACTIVATION_BLOCK, L1_CHAIN_ID, L2_CHAIN_ID,
    wait_for_block,
};

/// Cobalt activates where the other Everest system tests schedule it (`common/everest.rs`).
const COBALT_ACTIVATION_BLOCK: u64 = 5;
/// Everest, and with it gas-limit billing, activates late enough that the operator fee seeded on
/// L1 normally reaches the L2 (about 15 blocks) before activation. The pre-Everest transfer does
/// not wait for it: every settlement reads the operator fee live at its own block.
/// The other Everest system tests activate it at block 7 (see `common/everest.rs`).
const EVEREST_ACTIVATION_BLOCK: u64 = 40;
/// A plain transfer needs 21k gas; the limit is deliberately ~5x that.
const TRANSFER_GAS_USED: u64 = 21_000;
const TX_GAS_LIMIT: u64 = 100_000;
/// Declared limit of the sponsored EIP-8130 transaction, far above what its single empty call
/// needs so the limit, not the metered gas, is what settlement bills.
const EIP8130_GAS_LIMIT: u64 = 200_000;
const TX_VALUE: U256 = U256::from_limbs([1, 0, 0, 0]);
const RECIPIENT: Address = Address::repeat_byte(0xfe);
const MAX_FEE_PER_GAS: u128 = 2_000_000_000;
const MAX_PRIORITY_FEE_PER_GAS: u128 = 1_000_000;
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);
const FUNDING_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the L2 may take to reflect an L1 `SystemConfig` update in its `L1Block` predeploy.
const SYSTEM_CONFIG_PROPAGATION_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the validator may take to re-derive a block the builder has already produced.
const VALIDATOR_CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(30);
/// Operator fee parameters seeded through the L1 `SystemConfig`. The devnet genesis leaves both
/// at zero, which would make the operator-fee term of the post-Everest identities vacuous.
const OPERATOR_FEE_SCALAR: u32 = 1;
const OPERATOR_FEE_CONSTANT: u64 = 50;
/// Jovian scales the operator fee scalar by 100 (OP Stack Jovian operator fee), so the operator
/// fee on `gas` is `gas * scalar * 100 + constant` wei.
const OPERATOR_FEE_JOVIAN_MULTIPLIER: u64 = 100;

sol! {
    /// The L1 `SystemConfig` owner call that updates the operator fee parameters.
    interface SystemConfig {
        function setOperatorFeeScalars(uint32 _operatorFeeScalar, uint64 _operatorFeeConstant) external;
    }
    /// The L2 `L1Block` predeploy getters the sequencer fills from the L1 `SystemConfig`.
    interface L1Block {
        function operatorFeeScalar() external view returns (uint32);
        function operatorFeeConstant() external view returns (uint64);
    }
}

/// The operator fee parameters the L2 `L1Block` predeploy reports at a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OperatorFee {
    scalar: u32,
    constant: u64,
}

impl OperatorFee {
    /// The parameters [`EverestSystem::start`] seeds on L1.
    const SEEDED: Self = Self { scalar: OPERATOR_FEE_SCALAR, constant: OPERATOR_FEE_CONSTANT };

    /// Reads the parameters live at `block` from the `L1Block` predeploy.
    async fn at(provider: &RootProvider<Base>, block: BlockId) -> Result<Self> {
        let l1_block_call = |call: Vec<u8>| {
            BaseTransactionRequest::default().to(Predeploys::L1_BLOCK_INFO).input(call.into())
        };
        let scalar = L1Block::operatorFeeScalarCall::abi_decode_returns(
            &provider
                .call(l1_block_call(L1Block::operatorFeeScalarCall.abi_encode()))
                .block(block)
                .await?,
        )?;
        let constant = L1Block::operatorFeeConstantCall::abi_decode_returns(
            &provider
                .call(l1_block_call(L1Block::operatorFeeConstantCall.abi_encode()))
                .block(block)
                .await?,
        )?;
        Ok(Self { scalar, constant })
    }

    /// The operator fee owed on `gas`, computed independently of the node's fee code.
    fn charge(self, gas: u64) -> U256 {
        U256::from(gas) * U256::from(self.scalar) * U256::from(OPERATOR_FEE_JOVIAN_MULTIPLIER)
            + U256::from(self.constant)
    }
}

/// A running stack crossing Everest, with its builder and validator providers.
struct EverestSystem {
    /// Keeps the containers alive for the duration of the test.
    _system: SystemTestStack,
    builder: RootProvider<Base>,
    client: RootProvider<Base>,
    /// L2 timestamp at which Everest, and with it gas-limit billing, activates.
    everest_timestamp: u64,
}

impl EverestSystem {
    /// Starts the stack with Everest at [`EVEREST_ACTIVATION_BLOCK`], waits past Cobalt, and
    /// submits the operator fee seed on L1. [`Self::wait_for_operator_fee`] waits for it to reach
    /// the L2.
    async fn start() -> Result<Self> {
        base_node_runner::test_utils::init_silenced_tracing();
        let system = SystemTestStackBuilder::new()
            .with_l1_chain_id(L1_CHAIN_ID)
            .with_l2_chain_id(L2_CHAIN_ID)
            .with_base_azul_activation_block(BASE_AZUL_ACTIVATION_BLOCK)
            .with_base_beryl_activation_block(BASE_BERYL_ACTIVATION_BLOCK)
            .with_base_cobalt_activation_block(COBALT_ACTIVATION_BLOCK)
            .with_base_everest_activation_block(EVEREST_ACTIVATION_BLOCK)
            .build()
            .await?;
        let builder = system.l2_builder_provider()?;
        wait_for_block(&builder, COBALT_ACTIVATION_BLOCK + 1).await?;
        let client = system.l2_client_provider()?;
        let rollup_config: RollupConfig =
            serde_json::from_slice(&std::fs::read(system.l2_deployment().rollup_config_path())?)?;
        let everest_timestamp = rollup_config
            .upgrade_activation_timestamp(BaseUpgrade::Everest)
            .ok_or_eyre("Everest is not scheduled in the rollup config")?;
        let everest = Self { _system: system, builder, client, everest_timestamp };
        everest.submit_operator_fee(rollup_config.l1_system_config_address).await?;
        Ok(everest)
    }

    /// Sets [`OPERATOR_FEE_SCALAR`] and [`OPERATOR_FEE_CONSTANT`] on the L1 `SystemConfig` as
    /// its owner. The L2 picks them up once the sequencer derives an L1 origin carrying the update.
    async fn submit_operator_fee(&self, system_config: Address) -> Result<()> {
        let owner = PrivateKeySigner::from_bytes(&DEPLOYER.private_key)?;
        let l1 = ProviderBuilder::new()
            .wallet(EthereumWallet::from(owner))
            .connect_http(self._system.l1_rpc_url().await?);
        let update = SystemConfig::setOperatorFeeScalarsCall {
            _operatorFeeScalar: OPERATOR_FEE_SCALAR,
            _operatorFeeConstant: OPERATOR_FEE_CONSTANT,
        };
        let receipt = l1
            .send_transaction(
                TransactionRequest::default().to(system_config).input(update.abi_encode().into()),
            )
            .await
            .wrap_err("setOperatorFeeScalars was not accepted")?
            .get_receipt()
            .await
            .wrap_err("setOperatorFeeScalars was not mined")?;
        ensure!(receipt.status(), "setOperatorFeeScalars reverted");
        Ok(())
    }

    /// Waits until the L2 `L1Block` predeploy reports the seeded operator fee parameters.
    async fn wait_for_operator_fee(&self) -> Result<()> {
        timeout(SYSTEM_CONFIG_PROPAGATION_TIMEOUT, async {
            loop {
                if OperatorFee::at(&self.builder, BlockId::latest()).await? == OperatorFee::SEEDED {
                    return Ok::<_, eyre::Error>(());
                }
                sleep(Duration::from_millis(500)).await;
            }
        })
        .await
        .wrap_err("L2 did not pick up the operator fee scalars from the L1 SystemConfig")?
    }

    /// Polls the builder until its latest block carries a timestamp at or past Everest.
    async fn wait_for_everest(&self) -> Result<()> {
        timeout(Duration::from_secs(90), async {
            loop {
                let block = self
                    .builder
                    .get_block_by_number(BlockNumberOrTag::Latest)
                    .await?
                    .ok_or_eyre("latest block missing")?;
                if block.header.timestamp >= self.everest_timestamp {
                    return Ok::<_, eyre::Error>(());
                }
                sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .wrap_err_with(|| format!("timed out waiting for Everest at {}", self.everest_timestamp))?
    }

    /// Waits until the validator re-derived each of `block_numbers` to the builder's hash.
    async fn assert_validator_agrees(
        &self,
        block_numbers: impl IntoIterator<Item = u64>,
    ) -> Result<()> {
        for number in block_numbers {
            self.client
                .wait_for_convergence(&self.builder, number, VALIDATOR_CONVERGENCE_TIMEOUT)
                .await
                .wrap_err_with(|| format!("builder and validator disagree on block {number}"))?;
        }
        Ok(())
    }
}

/// What one transaction cost, read from its receipt and from balances at its block and parent.
struct Settlement {
    tx_hash: B256,
    block_number: u64,
    block_timestamp: u64,
    gas_used: u64,
    effective_gas_price: u128,
    /// The inclusion block's base fee, read from its header.
    base_fee_per_gas: u128,
    /// The L1 data fee the receipt reports.
    l1_fee: u128,
    /// The operator fee parameters live at the inclusion block.
    operator_fee: OperatorFee,
    /// Wei the fee payer transferred as value on top of the fees (zero when it only sponsors).
    value_paid: U256,
    /// How much the fee payer's balance dropped, including `value_paid`.
    payer_paid: U256,
    coinbase_received: U256,
    base_fee_vault_received: U256,
    operator_fee_vault_received: U256,
    l1_fee_vault_received: U256,
}

impl Settlement {
    /// Everything the payer paid beyond the value and the L1 data fee, which must have landed
    /// at the three execution-fee recipients: billing on the limit neither mints nor burns ETH.
    /// The L1 data fee itself is checked against the `L1FeeVault` credit in
    /// [`Self::assert_consistent`].
    fn execution_fee_paid(&self) -> U256 {
        self.payer_paid - self.value_paid - U256::from(self.l1_fee)
    }

    fn execution_fee_received(&self) -> U256 {
        self.coinbase_received + self.base_fee_vault_received + self.operator_fee_vault_received
    }

    /// The receipt-level fee identities that hold under both billing rules for `billed_gas`: every
    /// per-gas fee component is computed on `billed_gas` from independently known parameters.
    fn assert_consistent(&self, billed_gas: u64) {
        let gas = U256::from(billed_gas);
        let base_fee = U256::from(self.base_fee_per_gas);
        assert_eq!(
            self.effective_gas_price,
            self.base_fee_per_gas + MAX_PRIORITY_FEE_PER_GAS,
            "the fee cap must not truncate the priority fee in this fixture"
        );
        assert_eq!(self.gas_used, billed_gas, "receipt must report the billed gas");
        assert_eq!(
            self.coinbase_received,
            gas * U256::from(MAX_PRIORITY_FEE_PER_GAS),
            "coinbase tip must be the priority fee on the billed gas"
        );
        assert_eq!(
            self.base_fee_vault_received,
            gas * base_fee,
            "base fee burn must be the base fee on the billed gas"
        );
        assert_eq!(
            self.operator_fee_vault_received,
            self.operator_fee.charge(billed_gas),
            "operator fee must be the live scalar and constant applied to the billed gas"
        );
        assert_eq!(
            self.l1_fee_vault_received,
            U256::from(self.l1_fee),
            "the L1 data fee the receipt reports must reach the L1FeeVault"
        );
        assert_eq!(
            self.execution_fee_paid(),
            self.execution_fee_received(),
            "every wei the payer paid in execution fees must reach a fee recipient"
        );
        assert_eq!(
            self.payer_paid,
            self.value_paid
                + gas * U256::from(self.effective_gas_price)
                + U256::from(self.l1_fee)
                + self.operator_fee_vault_received,
            "payer must pay value + billed_gas * effective_gas_price + L1 fee + operator fee"
        );
    }
}

/// Before Everest an over-provisioned transfer is billed for the 21k gas it used; from Everest
/// the identical transfer is billed for its 100k limit, the block accounts for that gas,
/// estimation stays metered, and the validator re-derives the builder's blocks.
#[tokio::test]
async fn everest_bills_transactions_for_their_gas_limit() -> Result<()> {
    let system = EverestSystem::start().await?;
    let builder = &system.builder;
    let signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)?;

    // Before Everest: the transfer is charged for the 21k gas it used and the rest of its
    // limit is reimbursed. It is sent right away, so it has the whole pre-Everest runway; the
    // operator fee seed may or may not have reached the L2 yet, and the identities use whichever
    // parameters are live at its block.
    let before = send_transfer(builder, &signer).await.wrap_err("pre-Everest transfer")?;
    ensure!(
        before.block_timestamp < system.everest_timestamp,
        "pre-Everest transfer landed at block {} after the Everest activation",
        before.block_number
    );
    before.assert_consistent(TRANSFER_GAS_USED);

    // After Everest: the identical transfer is charged for its full 100k gas limit. The receipt,
    // the block and every fee recipient agree on that quantity.
    system.wait_for_operator_fee().await?;
    system.wait_for_everest().await?;
    let after = send_transfer(builder, &signer).await.wrap_err("post-Everest transfer")?;
    ensure!(
        after.block_timestamp >= system.everest_timestamp,
        "post-Everest transfer landed at block {} before the Everest activation",
        after.block_number
    );
    after.assert_consistent(TX_GAS_LIMIT);
    assert_eq!(
        after.operator_fee,
        OperatorFee::SEEDED,
        "the operator fee term must not be vacuous"
    );
    // The L1 data fee follows the L1 base fee, which moves between blocks, so it is only checked
    // to still be charged (the settlement identity above accounts for its exact amount).
    assert!(after.l1_fee > 0, "the L1 data fee must still be charged under gas-limit billing");
    assert!(
        after.execution_fee_paid() > before.execution_fee_paid(),
        "the over-provisioned transfer must cost more in execution fees under gas-limit billing"
    );
    assert_block_gas_matches_receipts(builder, after.block_number, TX_GAS_LIMIT).await?;

    // Gas estimation is a simulation that bills nothing, so it still converges on the metered
    // requirement rather than on the request's gas limit.
    let estimate = builder
        .estimate_gas(transfer_request(
            &signer,
            builder.get_transaction_count(signer.address()).await?,
        ))
        .await?;
    assert_eq!(estimate, TRANSFER_GAS_USED, "eth_estimateGas must keep reporting the metered need");

    // The validator re-executes and must accept the builder's post-Everest blocks: the fee
    // settlement and block gas accounting are consensus rules, so both nodes must agree.
    let last_block = after.block_number + 2;
    wait_for_block(builder, last_block).await?;
    system.assert_validator_agrees([before.block_number, after.block_number, last_block]).await?;
    let client_receipt = system
        .client
        .get_transaction_receipt(after.tx_hash)
        .await?
        .ok_or_eyre("validator has no receipt for the post-Everest transfer")?;
    assert_eq!(client_receipt.gas_used(), TX_GAS_LIMIT);

    Ok(())
}

/// From Everest a sponsored EIP-8130 transaction bills its payer for the reserved budget,
/// `gas_limit + payer_auth`, the sender pays nothing, and the validator agrees with the builder.
#[tokio::test]
async fn everest_bills_sponsored_eip8130_for_its_reserved_budget() -> Result<()> {
    let system = EverestSystem::start().await?;
    let builder = &system.builder;
    let sender = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)?;
    let payer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_2.private_key)?;
    builder.wait_for_balance(payer.address(), FUNDING_TIMEOUT).await?;
    system.wait_for_operator_fee().await?;
    system.wait_for_everest().await?;

    let nonce_sequence = builder.get_transaction_count(sender.address()).await?;
    let signed = sponsored_eip8130(&sender, &payer, nonce_sequence, EIP8130_GAS_LIMIT)?;
    let reserved_gas = EIP8130_GAS_LIMIT + IntrinsicGas::payer_auth_cost_of(&signed)?;
    assert!(reserved_gas > EIP8130_GAS_LIMIT, "payer authentication must reserve gas");
    let raw: Bytes = signed.encoded_2718().into();

    let sender_before = builder.get_balance(sender.address()).await?;
    let (settlement, receipt) =
        settle(builder, &raw, payer.address(), U256::ZERO).await.wrap_err("sponsored EIP-8130")?;
    ensure!(
        settlement.block_timestamp >= system.everest_timestamp,
        "sponsored transaction landed at block {} before the Everest activation",
        settlement.block_number
    );
    assert!(receipt.status(), "sponsored EIP-8130 transaction must succeed");
    assert_eq!(receipt.payer, Some(payer.address()), "receipt must name the sponsor as payer");
    settlement.assert_consistent(reserved_gas);
    assert_eq!(
        settlement.operator_fee,
        OperatorFee::SEEDED,
        "the operator fee term must not be vacuous"
    );
    assert_block_gas_matches_receipts(builder, settlement.block_number, reserved_gas).await?;
    let sender_after = builder
        .get_balance(sender.address())
        .block_id(BlockId::number(settlement.block_number))
        .await?;
    assert_eq!(sender_before, sender_after, "the sponsored sender must not pay anything");

    system.assert_validator_agrees([settlement.block_number]).await?;
    let client_receipt = system
        .client
        .get_transaction_receipt(settlement.tx_hash)
        .await?
        .ok_or_eyre("validator has no receipt for the sponsored transaction")?;
    assert_eq!(client_receipt.gas_used(), reserved_gas);

    // `eth_estimateGas` for a sponsored request returns the sender's budget only. The same call
    // estimated as self-pay has the same sender budget: naming a payer adds only its 20 address
    // bytes to the sender payload, far less than payer authentication, so an estimate that folded
    // payer authentication in would exceed the self-pay estimate by at least `payer_auth`. Sent at
    // `gas_limit = estimate`, the sponsored transaction succeeds and settles at
    // `estimate + payer_auth`. The EIP-8130 RPC module is installed on the validator node, not the
    // builder, so the estimates come from the client.
    let estimate_request = |payer: Option<Address>| -> Result<BaseTransactionRequest> {
        let mut request = serde_json::json!({
            "from": sender.address(),
            "calls": [[Call { to: RECIPIENT, value: U256::ZERO, data: Bytes::new() }]],
            "maxFeePerGas": U256::from(MAX_FEE_PER_GAS),
            "maxPriorityFeePerGas": U256::from(MAX_PRIORITY_FEE_PER_GAS),
        });
        if let Some(payer) = payer {
            request["payer"] = serde_json::json!(payer);
        }
        Ok(serde_json::from_value(request)?)
    };
    let estimate = system
        .client
        .estimate_gas(estimate_request(Some(payer.address()))?)
        .await
        .wrap_err("sponsored eth_estimateGas")?;
    let self_pay_estimate = system
        .client
        .estimate_gas(estimate_request(None)?)
        .await
        .wrap_err("self-pay eth_estimateGas")?;
    ensure!(
        estimate < EIP8130_GAS_LIMIT,
        "estimate {estimate} must be below the over-provisioned limit"
    );
    let signed = sponsored_eip8130(&sender, &payer, nonce_sequence + 1, estimate)?;
    let payer_auth = IntrinsicGas::payer_auth_cost_of(&signed)?;
    assert!(
        estimate < self_pay_estimate + payer_auth,
        "the sponsored estimate ({estimate}) must exclude payer authentication \
         (self-pay estimate {self_pay_estimate}, payer authentication {payer_auth})"
    );
    let raw: Bytes = signed.encoded_2718().into();
    let (settlement, receipt) = settle(builder, &raw, payer.address(), U256::ZERO)
        .await
        .wrap_err("sponsored EIP-8130 at the estimate")?;
    assert!(receipt.status(), "a sponsored transaction sent at the estimate must succeed");
    assert_eq!(
        receipt.gas_used(),
        estimate + payer_auth,
        "settlement must add payer authentication to the estimate exactly once"
    );
    settlement.assert_consistent(estimate + payer_auth);

    Ok(())
}

/// The block header's `gas_used` is the receipts' last `cumulative_gas_used`, and that cumulative
/// sum is made of per-receipt `gas_used` values, so a receipt billed on reserved gas shows up in
/// the header: a block's gas fullness is known from reserved gas alone.
async fn assert_block_gas_matches_receipts(
    provider: &RootProvider<Base>,
    block_number: u64,
    billed_gas: u64,
) -> Result<()> {
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(block_number))
        .await?
        .ok_or_eyre("block missing")?;
    let receipts = provider
        .get_block_receipts(BlockId::number(block_number))
        .await?
        .ok_or_eyre("block receipts missing")?;
    let summed: u64 = receipts.iter().map(|receipt| receipt.gas_used()).sum();
    assert_eq!(block.header.gas_used, summed, "header gas_used must be the sum of receipts");
    assert!(
        receipts.iter().any(|receipt| receipt.gas_used() == billed_gas),
        "the post-Everest block must carry a receipt billed at {billed_gas} gas"
    );
    assert!(block.header.gas_used >= billed_gas);
    Ok(())
}

fn transfer_request(signer: &PrivateKeySigner, nonce: u64) -> BaseTransactionRequest {
    BaseTransactionRequest::default()
        .from(signer.address())
        .to(RECIPIENT)
        .value(TX_VALUE)
        .transaction_type(2)
        .with_gas_limit(TX_GAS_LIMIT)
        .with_max_fee_per_gas(MAX_FEE_PER_GAS)
        .with_max_priority_fee_per_gas(MAX_PRIORITY_FEE_PER_GAS)
        .with_chain_id(L2_CHAIN_ID)
        .with_nonce(nonce)
}

/// Sends the over-provisioned self-paid transfer and reads back how it was settled.
async fn send_transfer(
    provider: &RootProvider<Base>,
    signer: &PrivateKeySigner,
) -> Result<Settlement> {
    let nonce = provider.get_transaction_count(signer.address()).await?;
    let transaction = transfer_request(signer, nonce)
        .build_typed_tx()
        .map_err(|error| eyre::eyre!("invalid transaction: {error:?}"))?;
    ensure!(transaction.gas_limit() == TX_GAS_LIMIT);
    let signature = signer.sign_hash_sync(&transaction.signature_hash())?;
    let raw: Bytes = transaction.into_signed(signature).encoded_2718().into();
    let (settlement, receipt) = settle(provider, &raw, signer.address(), TX_VALUE).await?;
    assert!(receipt.status(), "transfer must succeed");
    Ok(settlement)
}

/// A sponsored EIP-8130 transaction: `sender` dispatches one empty call and `payer` covers the
/// fees, authenticating over the payer digest with the secp256k1 (`K1`) authenticator. The
/// transaction declares `gas_limit`.
fn sponsored_eip8130(
    sender: &PrivateKeySigner,
    payer: &PrivateKeySigner,
    nonce_sequence: u64,
    gas_limit: u64,
) -> Result<Eip8130Signed> {
    let tx = TxEip8130 {
        chain_id: L2_CHAIN_ID,
        sender: None,
        nonce_key: U256::ZERO,
        nonce_sequence,
        valid_after: 0,
        valid_before: 0,
        max_priority_fee_per_gas: MAX_PRIORITY_FEE_PER_GAS,
        max_fee_per_gas: MAX_FEE_PER_GAS,
        gas_limit,
        account_changes: Vec::new(),
        calls: vec![vec![Call { to: RECIPIENT, value: U256::ZERO, data: Bytes::new() }]],
        metadata: Bytes::new(),
        payer: Some(payer.address()),
    };
    let sender_auth: Bytes = sender.sign_hash_sync(&tx.sender_signature_hash())?.as_bytes().into();
    // Explicit payer auth is `authenticator(20) || data`; for K1 the data is the payer's 65-byte
    // signature over the payer digest, which binds to the resolved sender.
    let payer_signature = payer.sign_hash_sync(&tx.payer_signature_hash(sender.address()))?;
    let mut payer_auth = Eip8130Constants::K1_AUTHENTICATOR.to_vec();
    payer_auth.extend_from_slice(&payer_signature.as_bytes());
    Ok(Eip8130Signed::new(tx, sender_auth, payer_auth.into()))
}

/// Submits `raw`, waits for its receipt, and reads how `payer` and the fee recipients settled it.
async fn settle(
    provider: &RootProvider<Base>,
    raw: &Bytes,
    payer: Address,
    value_paid: U256,
) -> Result<(Settlement, BaseTransactionReceipt)> {
    let pending = provider.send_raw_transaction(raw).await?;
    let tx_hash = *pending.tx_hash();
    drop(pending);

    let receipt = provider.wait_for_receipt(tx_hash, RECEIPT_TIMEOUT).await?;
    let block_number = receipt.block_number().ok_or_eyre("receipt missing block number")?;
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(block_number))
        .await?
        .ok_or_eyre("receipt block missing")?;
    ensure!(
        block.transactions.len() == 2,
        "expected only the L1 attributes deposit and the transaction in block {block_number}"
    );

    // Deltas are signed: the payer must be debited and every recipient credited, so a settlement
    // that moved ETH the wrong way fails here instead of passing the identities by magnitude.
    let balances = async |address: Address| -> Result<(U256, U256)> {
        let before =
            provider.get_balance(address).block_id(BlockId::number(block_number - 1)).await?;
        let after = provider.get_balance(address).block_id(BlockId::number(block_number)).await?;
        Ok((before, after))
    };
    let credited = async |address: Address| -> Result<U256> {
        let (before, after) = balances(address).await?;
        after.checked_sub(before).ok_or_eyre("fee recipient was debited")
    };
    let (payer_before, payer_after) = balances(payer).await?;
    let payer_paid = payer_before.checked_sub(payer_after).ok_or_eyre("payer was credited")?;
    ensure!(payer_paid < U256::from(ETH_TO_WEI), "payer paid an implausible amount");

    let settlement = Settlement {
        tx_hash,
        block_number,
        block_timestamp: block.header.timestamp,
        gas_used: receipt.gas_used(),
        effective_gas_price: receipt.effective_gas_price(),
        base_fee_per_gas: block
            .header
            .base_fee_per_gas
            .ok_or_eyre("post-London block missing base fee")?
            .into(),
        l1_fee: receipt.l1_block_info.l1_fee.ok_or_eyre("receipt missing l1Fee")?,
        operator_fee: OperatorFee::at(provider, BlockId::number(block_number)).await?,
        value_paid,
        payer_paid,
        coinbase_received: credited(block.header.beneficiary).await?,
        base_fee_vault_received: credited(Predeploys::BASE_FEE_VAULT).await?,
        operator_fee_vault_received: credited(Predeploys::OPERATOR_FEE_VAULT).await?,
        l1_fee_vault_received: credited(Predeploys::L1_FEE_VAULT).await?,
    };
    Ok((settlement, receipt))
}
