//! Renders the validity-predicate stress and gas-limit pricing profiles for a deployed
//! `DoubleCounter` contract. The predicate set is selected with `--predicates`: `parity-gate`
//! (default) renders the validity-stress gate, `all-true` renders predicates that always pass.

use std::{env, fmt::Write as _, fs, path::Path};

use eyre::{Result, WrapErr, bail, ensure};

const CONTRACT_PLACEHOLDER: &str = "__DOUBLE_COUNTER__";
const PREDICATES_PLACEHOLDER: &str = "__VALIDITY_PREDICATES__";
/// Optional second contract placeholder for profiles whose regular cohort writes `StorageFiller`.
const STORAGE_FILLER_PLACEHOLDER: &str = "__STORAGE_FILLER__";
/// Blocks past the current height at which always-true validity transactions expire. Ingress
/// requires a finite `block_number` upper bound within its 60 s window, measured at the active
/// block cadence. The demo profile targets the devnet's Denim 200 ms blocks, where 240 blocks is
/// 48 s: long enough for presigned transactions to be submitted and evaluated, inside the window.
const ALL_TRUE_EXPIRY_OFFSET_BLOCKS: u64 = 240;

/// Predicate set rendered into the `__VALIDITY_PREDICATES__` placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PredicateSet {
    /// 63 always-true storage reads followed by a `sender_parity` gate on slot 0.
    ParityGate,
    /// 63 always-true storage reads plus the `block_number` expiry bound ingress requires, so
    /// every transaction passes predicate evaluation.
    AllTrue,
}

impl PredicateSet {
    fn parse(name: &str) -> Result<Self> {
        match name {
            "parity-gate" => Ok(Self::ParityGate),
            "all-true" => Ok(Self::AllTrue),
            other => bail!("unknown predicate set '{other}', expected parity-gate or all-true"),
        }
    }
}

struct Renderer;

impl Renderer {
    fn predicates(address: &str, set: PredicateSet) -> String {
        let mut predicates = String::new();

        let (always_true_slots, gate) = match set {
            // Put the parity gate last so both matching and parked transactions perform 64
            // distinct storage reads before the gate decides their outcome.
            PredicateSet::ParityGate => (
                1..64,
                format!(
                    "    - type: storage\n      address: \"{address}\"\n      slot:\n        kind: fixed\n        value: \"0x0\"\n      mask: \"0x1\"\n      op: \"=\"\n      value: \"sender_parity\""
                ),
            ),
            PredicateSet::AllTrue => (
                0..63,
                format!(
                    "    - type: block_number\n      op: \"<=\"\n      offset: \"{ALL_TRUE_EXPIRY_OFFSET_BLOCKS}\""
                ),
            ),
        };
        for slot in always_true_slots {
            writeln!(
                predicates,
                "    - type: storage\n      address: \"{address}\"\n      slot:\n        kind: fixed\n        value: \"0x{slot:x}\"\n      op: \">=\"\n      value: \"0x0\""
            )
            .expect("writing to a String cannot fail");
        }
        predicates.push_str(&gate);

        predicates
    }

    fn is_address(address: &str) -> bool {
        address.len() == 42
            && address.starts_with("0x")
            && address[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
    }

    fn render(
        template: &str,
        address: &str,
        storage_filler: Option<&str>,
        set: PredicateSet,
    ) -> Result<String> {
        ensure!(
            Self::is_address(address),
            "contract address must be a 20-byte 0x-prefixed hex value"
        );
        ensure!(
            template.contains(CONTRACT_PLACEHOLDER)
                && template.matches(PREDICATES_PLACEHOLDER).count() == 1,
            "template must contain the contract placeholder and exactly one predicates placeholder"
        );
        let mut rendered = template
            .replace(CONTRACT_PLACEHOLDER, address)
            .replace(PREDICATES_PLACEHOLDER, &Self::predicates(address, set));
        if rendered.contains(STORAGE_FILLER_PLACEHOLDER) {
            let filler = storage_filler
                .ok_or_else(|| eyre::eyre!("template needs --storage-filler <address>"))?;
            ensure!(
                Self::is_address(filler),
                "StorageFiller address must be a 20-byte 0x-prefixed hex value"
            );
            rendered = rendered.replace(STORAGE_FILLER_PLACEHOLDER, filler);
        }
        Ok(rendered)
    }
}

fn main() -> Result<()> {
    let mut args = env::args().collect::<Vec<_>>();
    let program = args.first().cloned().unwrap_or_else(|| "render_validity_stress".to_string());
    let usage = format!(
        "usage: {program} [--predicates parity-gate|all-true] [--storage-filler <address>] <template> <contract-address> <output>"
    );

    let mut set = PredicateSet::ParityGate;
    let mut storage_filler = None;
    loop {
        match args.get(1).map(String::as_str) {
            Some("--predicates") => {
                ensure!(args.len() >= 3, "{usage}");
                set = PredicateSet::parse(&args[2])?;
                args.drain(1..3);
            }
            Some("--storage-filler") => {
                ensure!(args.len() >= 3, "{usage}");
                storage_filler = Some(args[2].clone());
                args.drain(1..3);
            }
            _ => break,
        }
    }
    ensure!(args.len() == 4, "{usage}");

    let template = fs::read_to_string(Path::new(&args[1]))
        .wrap_err_with(|| format!("failed to read template {}", args[1]))?;
    let rendered = Renderer::render(&template, &args[2], storage_filler.as_deref(), set)?;
    fs::write(Path::new(&args[3]), rendered)
        .wrap_err_with(|| format!("failed to write rendered config {}", args[3]))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{PREDICATES_PLACEHOLDER, PredicateSet, Renderer, STORAGE_FILLER_PLACEHOLDER};

    const ADDRESS: &str = "0x1234567890123456789012345678901234567890";
    const FILLER: &str = "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd";
    const TEMPLATE: &str = include_str!("validity-stress.yaml.template");
    const GAS_LIMIT_PRICING_TEMPLATE: &str = include_str!("gas-limit-pricing.yaml.template");

    #[test]
    fn renders_exactly_64_storage_predicates() {
        let result = Renderer::render(TEMPLATE, ADDRESS, None, PredicateSet::ParityGate).unwrap();

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
        assert!(!result.contains(STORAGE_FILLER_PLACEHOLDER));
        assert!(
            Renderer::render(GAS_LIMIT_PRICING_TEMPLATE, ADDRESS, None, PredicateSet::AllTrue)
                .is_err(),
            "the demo template needs a StorageFiller address"
        );
    }

    #[test]
    fn renders_all_true_predicates_with_expiry_for_gas_limit_pricing() {
        let result = Renderer::render(
            GAS_LIMIT_PRICING_TEMPLATE,
            ADDRESS,
            Some(FILLER),
            PredicateSet::AllTrue,
        )
        .unwrap();

        assert_eq!(result.matches("    - type: storage").count(), 63);
        assert_eq!(result.matches("    - type: block_number").count(), 1);
        assert_eq!(result.matches("      op: \">=\"").count(), 63);
        assert_eq!(result.matches("      op: \"<=\"").count(), 1);
        assert_eq!(result.matches("      offset: \"240\"").count(), 1);
        assert_eq!(result.matches("sender_parity").count(), 0, "no gating predicate");
        for slot in 0..63 {
            assert_eq!(result.matches(&format!("        value: \"0x{slot:x}\"")).count(), 1);
        }
        assert_eq!(
            result.matches(&format!("contract: \"{ADDRESS}\"")).count(),
            1,
            "revert_arb target"
        );
        assert_eq!(
            result.matches(&format!("contract: \"{FILLER}\"")).count(),
            1,
            "regular cohort target"
        );
        assert!(
            result.contains("    type: storage\n    contract: \"")
                && result.contains("    slots_per_tx: 4")
        );
        assert!(result.contains("sender_count: 200"));
        assert!(result.contains("duration: \"60s\""));
        assert!(result.contains("priority_lead_ratio: 0.10"));
        assert!(result.contains("priority_lead_multiplier: 20"));
        assert!(result.contains("priority_fee_divisor: 1"));
        assert!(result.contains("    type: revert_arb\n"));
        assert!(result.contains("    gas_limit: 500000"));
        assert!(!result.contains(PREDICATES_PLACEHOLDER));
        assert!(!result.contains("__DOUBLE_COUNTER__"));
    }

    #[test]
    fn rejects_address_without_hex_prefix() {
        let address = ADDRESS.trim_start_matches("0x");

        assert!(Renderer::render(TEMPLATE, address, None, PredicateSet::ParityGate).is_err());
    }

    #[test]
    fn rejects_unknown_predicate_set() {
        assert!(PredicateSet::parse("always").is_err());
        assert_eq!(PredicateSet::parse("all-true").unwrap(), PredicateSet::AllTrue);
    }
}
