use alloy_network::TransactionBuilder;
use alloy_primitives::Address;
use alloy_rpc_types::TransactionRequest;
use async_trait::async_trait;
use base_common_consensus::Eip8130Constants;

use super::{Payload, TransferPayload};
use crate::workload::SeededRng;

/// EOA self-pay native transfer using the published EIP-8430 envelope.
/// Existing consensus types retain their implementation name, EIP-8130.
#[derive(Debug, Default)]
pub struct Eip8430TransferPayload;

/// Conservative declared ceiling for one native value-transfer call, including fresh recipients.
pub const EIP8430_TRANSFER_GAS_LIMIT: u64 = 200_000;

#[async_trait]
impl Payload for Eip8430TransferPayload {
    fn name(&self) -> &'static str {
        "eip8430_transfer"
    }

    fn uses_runner_recipient(&self) -> bool {
        true
    }

    fn generate(&self, rng: &mut SeededRng, from: Address, to: Address) -> TransactionRequest {
        let mut request = TransferPayload::default().generate(rng, from, to);
        // This is a signing discriminator, not an Ethereum typed request.
        request.transaction_type = Some(Eip8130Constants::EIP8130_TX_TYPE);
        // Ceiling, not a gas estimate: calibration uses an actual native receipt.
        request = request.with_gas_limit(EIP8430_TRANSFER_GAS_LIMIT);
        request
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::TestConfig, runner::TxType, workload::WorkloadGenerator};

    /// Both published profiles must map to native payloads with their exact transaction weights.
    #[test]
    fn eip8430_profiles_parse_and_generate_native_requests() {
        for (yaml, weights, native_index) in [
            (include_str!("../../../examples/eip8430-transfer.yaml"), vec![100], 0),
            (
                include_str!("../../../examples/mainnet-mix-eip8430-mainnet-snapshot.yaml"),
                vec![21, 20, 75, 585, 150, 149],
                1,
            ),
        ] {
            let config: TestConfig = serde_yaml::from_str(yaml).expect("native profile must parse");
            let config = config.to_load_config(None).expect("native profile must convert");
            assert_eq!(
                config.transactions.iter().map(|tx| tx.weight).collect::<Vec<_>>(),
                weights,
                "profile must preserve exact transaction weights"
            );
            assert!(
                matches!(config.transactions[native_index].tx_type, TxType::Eip8430Transfer),
                "native workload must select EIP-8430 transfers"
            );
            if native_index == 1 {
                assert!(
                    matches!(config.transactions[0].tx_type, TxType::Transfer),
                    "mainnet mix must retain regular ETH transfers"
                );
                let regular = WorkloadGenerator::from_tx_configs(
                    crate::config::WorkloadConfig::new("test"),
                    &config.transactions[..1],
                    None,
                )
                .expect("regular transfer payload must install");
                assert_eq!(
                    regular.payload_names(),
                    vec!["transfer"],
                    "regular ETH transfers must retain their original payload"
                );
            }
            let generator = WorkloadGenerator::from_tx_configs(
                crate::config::WorkloadConfig::new("test"),
                &config.transactions[native_index..=native_index],
                None,
            )
            .expect("native payload must install");
            assert_eq!(
                generator.payload_names(),
                vec!["eip8430_transfer"],
                "native config must map to native payload"
            );
        }
        let baseline: TestConfig = serde_yaml::from_str(include_str!(
            "../../../examples/mainnet-mix-mainnet-snapshot.yaml"
        ))
        .expect("ordinary baseline must parse");
        let baseline = baseline.to_load_config(None).expect("ordinary baseline must convert");
        assert!(
            matches!(baseline.transactions[0].tx_type, TxType::Transfer),
            "baseline must retain ordinary transfers"
        );
        assert!(
            !baseline.transactions.iter().any(|tx| matches!(tx.tx_type, TxType::Eip8430Transfer)),
            "baseline must not require native support"
        );
        let to = Address::repeat_byte(0xab);
        let request = Eip8430TransferPayload.generate(&mut SeededRng::new(42), Address::ZERO, to);
        assert_eq!(request.transaction_type, Some(0x79), "native signing marker must be set");
        assert_eq!(request.to, Some(to.into()), "runner recipient must be preserved");
        assert_eq!(
            request.gas,
            Some(200_000),
            "native transfer must have the conservative gas ceiling"
        );
    }
}
