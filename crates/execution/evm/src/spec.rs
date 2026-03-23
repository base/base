//! Contains the [`OpSpecId`] type and its implementation.

pub use base_alloy_chains::{OpSpecId, UnknownOpHardfork};

#[cfg(test)]
mod tests {
    use std::vec;

    use revm::primitives::hardfork::SpecId;

    use super::*;

    #[test]
    fn test_op_spec_id_eth_spec_compatibility() {
        // Define test cases: (OpSpecId, enabled in ETH specs, enabled in OP specs)
        let test_cases = [
            (
                OpSpecId::BEDROCK,
                vec![
                    (SpecId::MERGE, true),
                    (SpecId::SHANGHAI, false),
                    (SpecId::CANCUN, false),
                    (SpecId::default(), false),
                ],
                vec![(OpSpecId::BEDROCK, true), (OpSpecId::REGOLITH, false)],
            ),
            (
                OpSpecId::REGOLITH,
                vec![
                    (SpecId::MERGE, true),
                    (SpecId::SHANGHAI, false),
                    (SpecId::CANCUN, false),
                    (SpecId::default(), false),
                ],
                vec![(OpSpecId::BEDROCK, true), (OpSpecId::REGOLITH, true)],
            ),
            (
                OpSpecId::CANYON,
                vec![
                    (SpecId::MERGE, true),
                    (SpecId::SHANGHAI, true),
                    (SpecId::CANCUN, false),
                    (SpecId::default(), false),
                ],
                vec![
                    (OpSpecId::BEDROCK, true),
                    (OpSpecId::REGOLITH, true),
                    (OpSpecId::CANYON, true),
                ],
            ),
            (
                OpSpecId::ECOTONE,
                vec![
                    (SpecId::MERGE, true),
                    (SpecId::SHANGHAI, true),
                    (SpecId::CANCUN, true),
                    (SpecId::default(), false),
                ],
                vec![
                    (OpSpecId::BEDROCK, true),
                    (OpSpecId::REGOLITH, true),
                    (OpSpecId::CANYON, true),
                    (OpSpecId::ECOTONE, true),
                ],
            ),
            (
                OpSpecId::FJORD,
                vec![
                    (SpecId::MERGE, true),
                    (SpecId::SHANGHAI, true),
                    (SpecId::CANCUN, true),
                    (SpecId::default(), false),
                ],
                vec![
                    (OpSpecId::BEDROCK, true),
                    (OpSpecId::REGOLITH, true),
                    (OpSpecId::CANYON, true),
                    (OpSpecId::ECOTONE, true),
                    (OpSpecId::FJORD, true),
                ],
            ),
            (
                OpSpecId::JOVIAN,
                vec![
                    (SpecId::PRAGUE, true),
                    (SpecId::SHANGHAI, true),
                    (SpecId::CANCUN, true),
                    (SpecId::MERGE, true),
                ],
                vec![
                    (OpSpecId::BEDROCK, true),
                    (OpSpecId::REGOLITH, true),
                    (OpSpecId::CANYON, true),
                    (OpSpecId::ECOTONE, true),
                    (OpSpecId::FJORD, true),
                    (OpSpecId::HOLOCENE, true),
                    (OpSpecId::ISTHMUS, true),
                ],
            ),
            (
                OpSpecId::BASE_V1,
                vec![
                    (SpecId::OSAKA, true),
                    (SpecId::PRAGUE, true),
                    (SpecId::SHANGHAI, true),
                    (SpecId::CANCUN, true),
                    (SpecId::MERGE, true),
                ],
                vec![
                    (OpSpecId::BEDROCK, true),
                    (OpSpecId::REGOLITH, true),
                    (OpSpecId::CANYON, true),
                    (OpSpecId::ECOTONE, true),
                    (OpSpecId::FJORD, true),
                    (OpSpecId::HOLOCENE, true),
                    (OpSpecId::ISTHMUS, true),
                    (OpSpecId::JOVIAN, true),
                ],
            ),
        ];

        for (op_spec, eth_tests, op_tests) in test_cases {
            // Test ETH spec compatibility
            for (eth_spec, expected) in eth_tests {
                assert_eq!(
                    op_spec.into_eth_spec().is_enabled_in(eth_spec),
                    expected,
                    "{:?} should {} be enabled in ETH {:?}",
                    op_spec,
                    if expected { "" } else { "not " },
                    eth_spec
                );
            }

            // Test OP spec compatibility
            for (other_op_spec, expected) in op_tests {
                assert_eq!(
                    op_spec.is_enabled_in(other_op_spec),
                    expected,
                    "{:?} should {} be enabled in OP {:?}",
                    op_spec,
                    if expected { "" } else { "not " },
                    other_op_spec
                );
            }
        }
    }

    #[test]
    fn default_op_spec_id() {
        assert_eq!(OpSpecId::default(), OpSpecId::ISTHMUS);
    }
}
