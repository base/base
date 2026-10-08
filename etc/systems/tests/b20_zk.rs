//! System test that dry-run proves a block containing a B-20 transaction.

#[path = "common/balance.rs"]
mod balance;
#[path = "common/cobalt.rs"]
mod cobalt;
mod common;
#[path = "common/zk_dry_run.rs"]
mod zk_dry_run;

use alloy_network::ReceiptResponse;
use alloy_primitives::{B256, U256};
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use base_common_network::Base;
use base_common_precompiles::{ActivationFeature, B20Variant, IB20};
use base_common_rpc_types::BaseTransactionReceipt;
use base_optimism_rpc::OptimismRollupProviderExt;
use base_system_tests::{
    ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, B20PrecompileClient, InProcessProverService, InProcessZkHost,
    SystemTestStackBuilder,
};
use eyre::{Result, WrapErr, ensure};

const INITIAL_SUPPLY: u64 = 1_000_000_000;
const TRANSFER_AMOUNT: u64 = 100_000_000;

/// Dry-run SP1-executes a block that called the B-20 precompile.
///
/// Confirms native precompile dispatch proves in the `no_std` Cobalt guest.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "SP1 dry-run execute is too slow for merge-queue; run with --release"]
async fn b20_block_dry_run_proves() -> Result<()> {
    ensure!(
        !cfg!(debug_assertions),
        "SP1 dry-run execute does not finish in the unoptimized test profile. Re-run with --release:\n\
         cargo nextest run --release -p base-system-tests --run-ignored all \\\n\
         -E 'test(b20_block_dry_run)'"
    );

    let (system, provider) =
        cobalt::start_cobalt_stack(SystemTestStackBuilder::new().with_force_batch_submission())
            .await?;
    let receipt = send_b20_transfer(&provider).await?;
    let block_number = receipt.block_number().expect("mined B-20 transfer must have a block");

    let rollup_provider =
        RootProvider::<Base>::new_http(system.l2_stack().builder_consensus_rpc_url());
    zk_dry_run::wait_for_safe_l2(&rollup_provider, block_number).await?;

    let service = InProcessProverService::start().await?;
    let _host = InProcessZkHost::start(&system, service.url()).await?;

    // Pin rollup `head_l1`; local L1 finality lags the batches that made this block safe.
    let l1_head = rollup_provider.optimism_sync_status().await?.head_l1.hash;
    let stats = zk_dry_run::prove_block_range_with_dry_run_stats(
        service.url().clone(),
        block_number,
        l1_head,
        "b20-zk-dry-run",
    )
    .await?;
    ensure!(
        stats.total_instruction_cycles > 0,
        "dry-run of a B-20 block must report non-zero instruction cycles"
    );

    Ok(())
}

async fn send_b20_transfer(provider: &RootProvider<Base>) -> Result<BaseTransactionReceipt> {
    let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
        .wrap_err("Failed to parse system test private key")?;
    let recipient = ANVIL_ACCOUNT_6.address;
    balance::wait_for_balance(provider, admin.address()).await?;

    let b20 = B20PrecompileClient::new(provider, &admin, common::L2_CHAIN_ID)
        .with_receipt_timeout(balance::TX_RECEIPT_TIMEOUT);
    b20.activate_feature(ActivationFeature::B20Asset.id()).await?;

    let params = B20PrecompileClient::token_params(
        "ZK B20",
        "ZKB20",
        admin.address(),
        U256::from(INITIAL_SUPPLY),
        admin.address(),
    );
    let token = b20.create_token(B20Variant::Asset, params, B256::repeat_byte(0x21)).await?;
    b20.wait_for_token_code(token, balance::TX_RECEIPT_TIMEOUT, common::BLOCK_POLL_INTERVAL)
        .await?;

    let receipt = b20
        .send_call_receipt(
            token,
            IB20::transferCall { to: recipient, amount: U256::from(TRANSFER_AMOUNT) },
            "transfer B-20 token",
        )
        .await?;
    ensure!(receipt.status(), "B-20 transfer receipt must report success");
    Ok(receipt)
}
