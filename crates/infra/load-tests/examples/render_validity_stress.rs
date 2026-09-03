//! Renders the validity-predicate stress profile for a deployed `DoubleCounter` contract.

use std::{env, fmt::Write as _, fs, path::Path};

use eyre::{Result, WrapErr, ensure};

const CONTRACT_PLACEHOLDER: &str = "__DOUBLE_COUNTER__";
const PREDICATES_PLACEHOLDER: &str = "__VALIDITY_PREDICATES__";

struct Renderer;

impl Renderer {
    fn predicates(address: &str) -> String {
        let mut predicates = String::new();

        // Put the parity gate last so both matching and parked transactions perform 64 distinct
        // storage reads before the gate decides their outcome.
        for slot in 1..64 {
            writeln!(
                predicates,
                "    - type: storage\n      address: \"{address}\"\n      slot:\n        kind: fixed\n        value: \"0x{slot:x}\"\n      op: \">=\"\n      value: \"0x0\""
            )
            .expect("writing to a String cannot fail");
        }
        write!(
            predicates,
            "    - type: storage\n      address: \"{address}\"\n      slot:\n        kind: fixed\n        value: \"0x0\"\n      mask: \"0x1\"\n      op: \"=\"\n      value: \"sender_parity\""
        )
        .expect("writing to a String cannot fail");

        predicates
    }

    fn render(template: &str, address: &str) -> Result<String> {
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

        Ok(template
            .replace(CONTRACT_PLACEHOLDER, address)
            .replace(PREDICATES_PLACEHOLDER, &Self::predicates(address)))
    }
}

fn main() -> Result<()> {
    let args = env::args().collect::<Vec<_>>();
    ensure!(
        args.len() == 4,
        "usage: {} <template> <contract-address> <output>",
        args.first().map(String::as_str).unwrap_or("render_validity_stress")
    );

    let template = fs::read_to_string(Path::new(&args[1]))
        .wrap_err_with(|| format!("failed to read template {}", args[1]))?;
    let rendered = Renderer::render(&template, &args[2])?;
    fs::write(Path::new(&args[3]), rendered)
        .wrap_err_with(|| format!("failed to write rendered config {}", args[3]))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{PREDICATES_PLACEHOLDER, Renderer};

    const ADDRESS: &str = "0x1234567890123456789012345678901234567890";
    const TEMPLATE: &str = include_str!("validity-stress.yaml.template");

    #[test]
    fn renders_exactly_64_storage_predicates() {
        let result = Renderer::render(TEMPLATE, ADDRESS).unwrap();

        assert_eq!(result.matches("    - type: storage").count(), 64);
        assert_eq!(result.matches("      mask: \"0x1\"").count(), 1);
        assert_eq!(result.matches("      op: \">=\"").count(), 63);
        assert_eq!(result.matches(&format!("      address: \"{ADDRESS}\"")).count(), 64);
        assert!(result.contains("sender_count: 800"));
        assert!(result.contains("target_gps: 600000000"));
        assert!(result.contains("transaction_submission_rpcs:\n  - \"http://localhost:7545\""));
        assert!(result.contains("in_flight_per_sender: 12"));
        assert!(result.contains("max_total_in_flight: 9600"));
        assert!(result.contains("ratio: 1.0"));
        assert!(result.contains("priority_lead_ratio: 0.10"));
        assert!(result.contains("priority_lead_multiplier: 2"));
        assert!(result.contains("priority_fee_divisor: 2"));
        for slot in 0..64 {
            assert_eq!(result.matches(&format!("        value: \"0x{slot:x}\"")).count(), 1);
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
    fn rejects_address_without_hex_prefix() {
        let address = ADDRESS.trim_start_matches("0x");

        assert!(Renderer::render(TEMPLATE, address).is_err());
    }
}
