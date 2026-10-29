//! Differential parity harness for the L1 fee parameters read from the `L1Block` predeploy.
//!
//! Seeds the same `L1Block` storage into both engines and asserts that
//! [`L1Block::fetch_fee_params`] reads exactly the fee parameters the revm reference's
//! `L1BlockInfo::try_fetch` does, across the execution fork ladder.

use alloy_primitives::{U256, uint};
use base_common_consensus::Predeploys;
use base_common_evm::{BaseSpecId as RevmBaseSpecId, L1BlockInfo};
use base_common_evm2::{BaseEvmTypes, BaseSpecId, L1Block};
use base_common_genesis::BaseUpgrade;
use evm2::{Evm, Precompiles, env::BlockEnv, evm::InMemoryDB};
use revm::{database::InMemoryDB as RevmDb, state::AccountInfo as RevmAccountInfo};

/// Ecotone scalars (base fee scalar `0x1db0`, blob base fee scalar `0x000d2730`) packed next to
/// the sequence number.
const ECOTONE_SCALARS: U256 =
    uint!(0x0000000000000000000000000000000000001db0000d27300000000000000005_U256);
/// Operator fee scalar `0x4e20`, operator fee constant `0x1f4`, DA-footprint gas scalar `0xdead`.
const OPERATOR_SCALARS: U256 =
    uint!(0x000000000000000000000000000000000000dead00004e2000000000000001f4_U256);

fn slots(scalars: U256) -> [(U256, U256); 6] {
    [
        (L1Block::L1_BASE_FEE_SLOT, U256::from(1_000_000_000u64)),
        (L1Block::ECOTONE_L1_FEE_SCALARS_SLOT, scalars),
        (L1Block::L1_OVERHEAD_SLOT, U256::from(188)),
        (L1Block::L1_SCALAR_SLOT, U256::from(684_000)),
        (L1Block::ECOTONE_L1_BLOB_BASE_FEE_SLOT, U256::from(7)),
        (L1Block::OPERATOR_FEE_SCALARS_SLOT, OPERATOR_SCALARS),
    ]
}

fn evm2_fetch(upgrade: BaseUpgrade, scalars: U256) -> base_common_l1_fees::L1FeeParams {
    let mut db = InMemoryDB::default();
    db.insert_account_info(&Predeploys::L1_BLOCK_INFO, evm2::AccountInfo::default());
    for (slot, value) in slots(scalars) {
        db.insert_account_storage(&Predeploys::L1_BLOCK_INFO, &slot, &value);
    }
    let spec = BaseSpecId::new(upgrade);
    let mut evm = Evm::new(
        spec,
        BlockEnv::<BaseEvmTypes>::default(),
        BaseEvmTypes::tx_registry(),
        db,
        Precompiles::base(spec.into()),
    );
    L1Block::fetch_fee_params(&mut evm).expect("L1Block storage is readable")
}

fn revm_fetch(upgrade: BaseUpgrade, scalars: U256) -> L1BlockInfo {
    let mut db = RevmDb::default();
    db.insert_account_info(Predeploys::L1_BLOCK_INFO, RevmAccountInfo::default());
    for (slot, value) in slots(scalars) {
        db.insert_account_storage(Predeploys::L1_BLOCK_INFO, slot, value).unwrap();
    }
    L1BlockInfo::try_fetch(&mut db, U256::ZERO, RevmBaseSpecId::new(upgrade))
        .expect("L1Block storage is readable")
}

#[test]
fn fee_params_match_the_revm_reference_across_forks() {
    for scalars in [ECOTONE_SCALARS, U256::ZERO] {
        for upgrade in BaseUpgrade::EXECUTION_VARIANTS {
            let evm2 = evm2_fetch(upgrade, scalars);
            let revm = revm_fetch(upgrade, scalars);
            let context = format!("{upgrade:?} with scalars {scalars:#x}");
            assert_eq!(evm2.l1_base_fee, revm.l1_base_fee, "{context}");
            assert_eq!(evm2.l1_fee_overhead, revm.l1_fee_overhead, "{context}");
            assert_eq!(evm2.l1_base_fee_scalar, revm.l1_base_fee_scalar, "{context}");
            assert_eq!(evm2.l1_blob_base_fee, revm.l1_blob_base_fee, "{context}");
            assert_eq!(evm2.l1_blob_base_fee_scalar, revm.l1_blob_base_fee_scalar, "{context}");
            assert_eq!(evm2.operator_fee_scalar, revm.operator_fee_scalar, "{context}");
            assert_eq!(evm2.operator_fee_constant, revm.operator_fee_constant, "{context}");
            assert_eq!(evm2.empty_ecotone_scalars, revm.empty_ecotone_scalars, "{context}");
            assert_eq!(
                evm2.da_footprint_gas_scalar,
                revm.da_footprint_gas_scalar.map(U256::from),
                "{context}"
            );
        }
    }
}
