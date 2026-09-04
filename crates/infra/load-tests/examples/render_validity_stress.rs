//! Renders the validity-predicate stress profile for a deployed `DoubleCounter` contract.

use std::{env, fmt::Write as _, fs, path::Path};

use eyre::{Result, WrapErr, ensure};

const CONTRACT_PLACEHOLDER: &str = "__DOUBLE_COUNTER__";
const PREDICATES_PLACEHOLDER: &str = "__VALIDITY_PREDICATES__";
const COLD_PROFILE: &str = "cold";
const WARM_PROFILE: &str = "warm";

struct Renderer;

impl Renderer {
    fn predicates(address: &str, profile: &str) -> String {
        let mut predicates = String::new();

        // Put the parity gate last so both matching and parked transactions perform all 63 stress
        // reads before the gate decides their outcome. Cold slots include the sender and nonce so
        // every new transaction addresses state that previous transactions did not read.
        for salt in 1..64 {
            match profile {
                COLD_PROFILE => writeln!(
                    predicates,
                    "    - type: storage\n      address: \"{address}\"\n      slot:\n        kind: sender_nonce\n        salt: \"0x{salt:x}\"\n      op: \">=\"\n      value: \"0x0\""
                ),
                WARM_PROFILE => writeln!(
                    predicates,
                    "    - type: storage\n      address: \"{address}\"\n      slot:\n        kind: fixed\n        value: \"0x{salt:x}\"\n      op: \">=\"\n      value: \"0x0\""
                ),
                _ => unreachable!("profile is validated before rendering"),
            }
            .expect("writing to a String cannot fail");
        }
        write!(
            predicates,
            "    - type: storage\n      address: \"{address}\"\n      slot:\n        kind: fixed\n        value: \"0x0\"\n      mask: \"0x1\"\n      op: \"=\"\n      value: \"sender_parity\""
        )
        .expect("writing to a String cannot fail");

        predicates
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

        Ok(template
            .replace(CONTRACT_PLACEHOLDER, address)
            .replace(PREDICATES_PLACEHOLDER, &Self::predicates(address, profile)))
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
    use super::{COLD_PROFILE, PREDICATES_PLACEHOLDER, Renderer, WARM_PROFILE};

    const ADDRESS: &str = "0x1234567890123456789012345678901234567890";
    const TEMPLATE: &str = include_str!("validity-stress.yaml.template");

    #[test]
    fn renders_cold_profile_with_transaction_unique_slots() {
        let result = Renderer::render(TEMPLATE, ADDRESS, COLD_PROFILE).unwrap();

        assert_eq!(result.matches("    - type: storage").count(), 64);
        assert_eq!(result.matches("        kind: sender_nonce").count(), 63);
        assert_eq!(result.matches("        kind: fixed").count(), 1);
        assert_eq!(result.matches("      mask: \"0x1\"").count(), 1);
        assert_eq!(result.matches("      op: \">=\"").count(), 63);
        assert_eq!(result.matches(&format!("      address: \"{ADDRESS}\"")).count(), 64);
        assert!(result.contains("sender_count: 800"));
        assert!(result.contains("target_gps: 600000000"));
        assert!(result.contains("transaction_submission_rpcs:\n  - \"http://localhost:7545\""));
        assert!(result.contains("in_flight_per_sender: 12"));
        assert!(result.contains("max_total_in_flight: 400"));
        assert!(result.contains("ratio: 1.0"));
        assert!(result.contains("priority_lead_ratio: 0.10"));
        assert!(result.contains("priority_lead_multiplier: 2"));
        assert!(result.contains("priority_fee_divisor: 2"));
        for salt in 1..64 {
            assert_eq!(result.matches(&format!("        salt: \"0x{salt:x}\"")).count(), 1);
        }

        let last_predicate = result.rsplit_once("    - type: storage").unwrap().1;
        assert!(last_predicate.contains("        value: \"0x0\""));
        assert!(last_predicate.contains("      mask: \"0x1\""));
        assert!(last_predicate.contains("      op: \"=\""));
        assert!(last_predicate.contains("      value: \"sender_parity\""));
        assert!(!result.contains(PREDICATES_PLACEHOLDER));
        assert!(!result.contains("__DOUBLE_COUNTER__"));
    }

    #[test]
    fn renders_fixed_slot_warm_comparison() {
        let result = Renderer::render(TEMPLATE, ADDRESS, WARM_PROFILE).unwrap();

        assert_eq!(result.matches("    - type: storage").count(), 64);
        assert_eq!(result.matches("        kind: fixed").count(), 64);
        assert!(!result.contains("        kind: sender_nonce"));
        for slot in 0..64 {
            assert_eq!(result.matches(&format!("        value: \"0x{slot:x}\"")).count(), 1);
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
