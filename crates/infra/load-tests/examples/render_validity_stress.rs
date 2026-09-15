//! Renders the validity-predicate stress profile for a deployed `DoubleCounter` contract.

use std::{env, fs, path::Path};

use alloy_primitives::U256;
use base_load_tests::{
    PredicateAddressConfig, PredicateSlotConfig, PredicateValueConfig, ValidityPredicateConfig,
};
use eyre::{Result, WrapErr, ensure};

const CONTRACT_PLACEHOLDER: &str = "__DOUBLE_COUNTER__";
const PREDICATES_PLACEHOLDER: &str = "__VALIDITY_PREDICATES__";
const COLD_PROFILE: &str = "cold";
const WARM_PROFILE: &str = "warm";
const PREDICATE_COUNT: u64 = 64;
/// Indentation that nests the serialized predicate list under the template's `predicates:` key.
const PREDICATE_INDENT: &str = "    ";

struct Renderer;

impl Renderer {
    /// Builds the stress predicate set for `profile`, reading storage on `address`.
    ///
    /// Puts the parity gate last so both matching and parked transactions perform all 63 stress
    /// reads before the gate decides their outcome. Cold slots include the sender and nonce so
    /// every new transaction addresses state that previous transactions did not read.
    fn predicates(address: &str, profile: &str) -> Vec<ValidityPredicateConfig> {
        let contract = PredicateAddressConfig::Fixed(address.to_string());
        let mut predicates = (1..PREDICATE_COUNT)
            .map(|salt| ValidityPredicateConfig::Storage {
                address: contract.clone(),
                slot: match profile {
                    COLD_PROFILE => PredicateSlotConfig::SenderNonce { salt: U256::from(salt) },
                    WARM_PROFILE => PredicateSlotConfig::Fixed { value: U256::from(salt) },
                    _ => unreachable!("profile is validated before rendering"),
                },
                mask: None,
                op: ">=".to_string(),
                value: PredicateValueConfig::Fixed(U256::ZERO),
            })
            .collect::<Vec<_>>();
        predicates.push(ValidityPredicateConfig::Storage {
            address: contract,
            slot: PredicateSlotConfig::Fixed { value: U256::ZERO },
            mask: Some(U256::from(1)),
            op: "=".to_string(),
            value: PredicateValueConfig::Source("sender_parity".to_string()),
        });
        predicates
    }

    /// Serializes `predicates` to YAML, indenting each line to nest under `predicates:`.
    fn render_predicates(predicates: &[ValidityPredicateConfig]) -> Result<String> {
        let yaml = serde_yaml::to_string(predicates)
            .wrap_err("failed to serialize validity predicates")?;
        Ok(yaml
            .lines()
            .map(|line| format!("{PREDICATE_INDENT}{line}"))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn render(template: &str, address: &str, profile: &str) -> Result<String> {
        ensure!(
            address.len() == 42
                && address.starts_with("0x")
                && address[2..].bytes().all(|byte| byte.is_ascii_hexdigit()),
            "contract address must be a 20-byte 0x-prefixed hex value"
        );
        ensure!(
            template.matches(CONTRACT_PLACEHOLDER).count() == 1
                && template.matches(PREDICATES_PLACEHOLDER).count() == 1,
            "template must contain each validity-stress placeholder exactly once"
        );
        ensure!(
            matches!(profile, COLD_PROFILE | WARM_PROFILE),
            "predicate profile must be 'cold' or 'warm'"
        );

        let predicates = Self::render_predicates(&Self::predicates(address, profile))?;
        Ok(template
            .replace(CONTRACT_PLACEHOLDER, address)
            .replace(PREDICATES_PLACEHOLDER, &predicates))
    }
}

fn main() -> Result<()> {
    let args = env::args().collect::<Vec<_>>();
    ensure!(
        args.len() == 5,
        "usage: {} <template> <contract-address> <cold|warm> <output>",
        args.first().map(String::as_str).unwrap_or("render_validity_stress")
    );

    let template = fs::read_to_string(Path::new(&args[1]))
        .wrap_err_with(|| format!("failed to read template {}", args[1]))?;
    let rendered = Renderer::render(&template, &args[2], &args[3])?;
    fs::write(Path::new(&args[4]), rendered)
        .wrap_err_with(|| format!("failed to write rendered config {}", args[4]))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;
    use base_load_tests::{
        PredicateSlotConfig, PredicateValueConfig, TestConfig, ValidityConfig,
        ValidityPredicateConfig,
    };

    use super::{COLD_PROFILE, PREDICATES_PLACEHOLDER, Renderer, WARM_PROFILE};

    const ADDRESS: &str = "0x1234567890123456789012345678901234567890";
    const TEMPLATE: &str = include_str!("validity-stress.yaml.template");

    /// Renders `profile` and parses it back through the load tester's real config type, proving
    /// the serialized YAML is both well-formed and semantically what the profile intends.
    fn render_validity(profile: &str) -> ValidityConfig {
        let rendered = Renderer::render(TEMPLATE, ADDRESS, profile).unwrap();
        assert!(!rendered.contains(PREDICATES_PLACEHOLDER));
        assert!(!rendered.contains("__DOUBLE_COUNTER__"));
        let config: TestConfig = serde_yaml::from_str(&rendered).unwrap();
        config.validity.validate().unwrap();
        config.validity
    }

    /// Extracts the storage slot and comparison value from a storage predicate.
    fn storage_slot_value(
        predicate: &ValidityPredicateConfig,
    ) -> (&PredicateSlotConfig, &PredicateValueConfig) {
        match predicate {
            ValidityPredicateConfig::Storage { slot, value, .. } => (slot, value),
            other => panic!("expected storage predicate, got {other:?}"),
        }
    }

    #[test]
    fn renders_cold_profile_with_transaction_unique_slots() {
        let validity = render_validity(COLD_PROFILE);

        assert_eq!(validity.ratio, 1.0);
        assert_eq!(validity.priority_lead_ratio, 0.10);
        assert_eq!(validity.priority_lead_multiplier, 2);
        assert_eq!(validity.priority_fee_divisor, 2);
        assert_eq!(validity.predicates.len(), 64);

        let (gate, stress) = validity.predicates.split_last().unwrap();
        for (index, predicate) in stress.iter().enumerate() {
            let salt = u64::try_from(index + 1).unwrap();
            match storage_slot_value(predicate) {
                (
                    PredicateSlotConfig::SenderNonce { salt: got },
                    PredicateValueConfig::Fixed(v),
                ) => {
                    assert_eq!(*got, U256::from(salt));
                    assert_eq!(*v, U256::ZERO);
                }
                other => panic!("expected cold sender-nonce predicate, got {other:?}"),
            }
        }

        match gate {
            ValidityPredicateConfig::Storage { slot, mask, value, .. } => {
                assert!(
                    matches!(slot, PredicateSlotConfig::Fixed { value } if *value == U256::ZERO)
                );
                assert_eq!(*mask, Some(U256::from(1)));
                assert!(
                    matches!(value, PredicateValueConfig::Source(source) if source == "sender_parity")
                );
            }
            other => panic!("expected storage gate predicate, got {other:?}"),
        }
    }

    #[test]
    fn renders_fixed_slot_warm_comparison() {
        let validity = render_validity(WARM_PROFILE);

        assert_eq!(validity.predicates.len(), 64);
        let (_gate, stress) = validity.predicates.split_last().unwrap();
        for (index, predicate) in stress.iter().enumerate() {
            let expected = u64::try_from(index + 1).unwrap();
            match storage_slot_value(predicate) {
                (PredicateSlotConfig::Fixed { value }, _) => {
                    assert_eq!(*value, U256::from(expected));
                }
                other => panic!("expected warm fixed-slot predicate, got {other:?}"),
            }
            assert!(!matches!(
                predicate,
                ValidityPredicateConfig::Storage {
                    slot: PredicateSlotConfig::SenderNonce { .. },
                    ..
                }
            ));
        }
    }

    #[test]
    fn rejects_address_without_hex_prefix() {
        let address = ADDRESS.trim_start_matches("0x");

        assert!(Renderer::render(TEMPLATE, address, COLD_PROFILE).is_err());
    }

    #[test]
    fn rejects_unknown_predicate_profile() {
        assert!(Renderer::render(TEMPLATE, ADDRESS, "mixed").is_err());
    }
}
