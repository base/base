use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, U256};
use alloy_rpc_types::TransactionRequest;
use alloy_sol_types::{SolCall, sol};
use async_trait::async_trait;

use super::Payload;
use crate::{
    Result,
    workload::{
        SeededRng,
        chain_prep::{ChainPrepContext, create2_address, deploy_create2},
    },
};

sol! {
    function fillStorage(uint256 slotCount, uint256 seed) external;
    function writeStorage(uint256 newCount, uint256 existingCount, uint256 seed) external;

    /// `test-utils/contracts/src/StorageFiller.sol`, solc 0.8.28, optimizer 200 runs, no CBOR
    /// metadata.
    #[sol(bytecode = "6080604052348015600e575f5ffd5b505f80805260205260017fad3228b676f7d3cd4284a5443f17f1962b36e491b30a40b2405849e597ba5fb581905580556102928061004b5f395ff3fe608060405234801561000f575f5ffd5b506004361061003f575f3560e01c80631be4016214610043578063a47b328914610058578063c49c1f8c14610073575b5f5ffd5b610056610051366004610205565b610086565b005b61006160015481565b60405190815260200160405180910390f35b610056610081366004610225565b610119565b6040805160208082018490523360601b6bffffffffffffffffffffffff1916828401524360548301524460748084019190915283518084039091018152609490920190925280519101205f5b83811015610113576100e4818361024e565b6100ef90600161024e565b5f806100fb848661024e565b815260208101919091526040015f20556001016100d2565b50505050565b6001545f610127858361024e565b9050815b818110156101545761013e81600161024e565b5f8281526020819052604090205560010161012b565b5060018190556040805160208082018690523360601b6bffffffffffffffffffffffff1916828401524360548301524460748084019190915283518084039091018152609490920190925280519101205f5b858110156101fc5760408051602080820185905281830184905282518083038401815260609092019092528051910120805f806101e38884610273565b815260208101919091526040015f2055506001016101a6565b50505050505050565b5f5f60408385031215610216575f5ffd5b50508035926020909101359150565b5f5f5f60608486031215610237575f5ffd5b505081359360208301359350604090920135919050565b8082018082111561026d57634e487b7160e01b5f52601160045260245ffd5b92915050565b5f8261028d57634e487b7160e01b5f52601260045260245ffd5b50069056")]
    contract StorageFiller {}
}

/// Gas budgeted per fresh storage slot. A cold zero -> non-zero SSTORE is 22,100
/// gas; the extra ~900 covers the per-iteration loop, keccak mixing, and warm
/// SLOAD overhead so the tx does not run out of gas on the last slots.
const STORAGE_GAS_PER_SLOT: u64 = 23_000;

/// Gas budgeted per overwritten slot: a cold non-zero -> non-zero SSTORE is 5,000
/// gas, plus the keccak and modulo that pick the key.
const STORAGE_GAS_PER_EXISTING_SLOT: u64 = 6_000;

/// Fixed per-tx gas overhead: 21,000 intrinsic + calldata + selector dispatch +
/// the single keccak256 over the slot-key preimage.
const STORAGE_GAS_BASE: u64 = 30_000;

/// Fixed per-tx gas overhead of `writeStorage`, which also updates its slot counter.
const WRITE_STORAGE_GAS_BASE: u64 = 35_000;

/// Generates storage-heavy transactions that write to contract slots.
#[derive(Debug, Clone)]
pub struct StoragePayload {
    /// Contract address to call.
    pub contract: Address,
    /// Number of storage slots to write.
    pub slots_per_tx: u32,
    /// Mean fraction of written slots that are fresh; the rest overwrite existing slots.
    pub new_slot_ratio: f64,
}

impl StoragePayload {
    /// Creates a new storage payload.
    pub const fn new(contract: Address, slots_per_tx: u32, new_slot_ratio: f64) -> Self {
        Self { contract, slots_per_tx, new_slot_ratio }
    }

    /// Address `prepare` deploys `StorageFiller` to when it is missing.
    pub fn default_contract() -> Address {
        create2_address(&StorageFiller::BYTECODE)
    }
}

#[async_trait]
impl Payload for StoragePayload {
    fn name(&self) -> &'static str {
        "storage"
    }

    fn uses_runner_recipient(&self) -> bool {
        false
    }

    fn generate(&self, rng: &mut SeededRng, _from: Address, _to: Address) -> TransactionRequest {
        let seed: u64 = rng.random();
        let (data, gas_limit) = if self.new_slot_ratio >= 1.0 {
            let data = fillStorageCall {
                slotCount: U256::from(self.slots_per_tx),
                seed: U256::from(seed),
            }
            .abi_encode();
            (data, u64::from(self.slots_per_tx) * STORAGE_GAS_PER_SLOT + STORAGE_GAS_BASE)
        } else {
            // Rounds up with probability equal to the fractional part, so the mean
            // fresh-slot count matches the ratio even when it is below one per tx.
            let fresh =
                (f64::from(self.slots_per_tx) * self.new_slot_ratio + rng.random::<f64>()) as u32;
            let existing = self.slots_per_tx - fresh;
            let data = writeStorageCall {
                newCount: U256::from(fresh),
                existingCount: U256::from(existing),
                seed: U256::from(seed),
            }
            .abi_encode();
            let gas = u64::from(fresh) * STORAGE_GAS_PER_SLOT
                + u64::from(existing) * STORAGE_GAS_PER_EXISTING_SLOT
                + WRITE_STORAGE_GAS_BASE;
            (data, gas)
        };

        TransactionRequest::default()
            .with_to(self.contract)
            .with_input(Bytes::from(data))
            .with_gas_limit(gas_limit)
    }

    async fn prepare(&self, ctx: &mut ChainPrepContext<'_>) -> Result<()> {
        if self.contract != Self::default_contract() {
            return Ok(());
        }
        deploy_create2(ctx, &StorageFiller::BYTECODE).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_write(request: &TransactionRequest) -> writeStorageCall {
        writeStorageCall::abi_decode(request.input.input().unwrap()).unwrap()
    }

    #[test]
    fn full_ratio_writes_only_fresh_slots() {
        let request = StoragePayload::new(Address::ZERO, 10, 1.0).generate(
            &mut SeededRng::new(1),
            Address::ZERO,
            Address::ZERO,
        );

        let call = fillStorageCall::abi_decode(request.input.input().unwrap()).unwrap();
        assert_eq!(call.slotCount, U256::from(10));
        assert_eq!(request.gas, Some(260_000));
    }

    #[test]
    fn zero_ratio_overwrites_every_slot() {
        let request = StoragePayload::new(Address::ZERO, 10, 0.0).generate(
            &mut SeededRng::new(1),
            Address::ZERO,
            Address::ZERO,
        );

        let call = decode_write(&request);
        assert_eq!((call.newCount, call.existingCount), (U256::ZERO, U256::from(10)));
        assert_eq!(request.gas, Some(95_000));
    }

    #[test]
    fn fractional_ratio_matches_mean_fresh_slots() {
        let payload = StoragePayload::new(Address::ZERO, 3, 0.1);
        let mut rng = SeededRng::new(7);
        let mut fresh = 0u64;
        for _ in 0..10_000 {
            let call = decode_write(&payload.generate(&mut rng, Address::ZERO, Address::ZERO));
            assert_eq!(call.newCount + call.existingCount, U256::from(3));
            fresh += call.newCount.to::<u64>();
        }

        // Expected 0.3 fresh slots per tx -> 3,000 over 10,000 txs.
        assert!((2_850..=3_150).contains(&fresh), "fresh slots: {fresh}");
    }
}
