use std::{
    any::Any,
    panic::{self, AssertUnwindSafe},
};

use base_consensus_genesis::{BaseHardforkConfig, HardForkConfig};

/// Named hardfork schedules for parametrizing harness tests across protocol upgrades.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForkMatrix {
    forks: Vec<(&'static str, HardForkConfig)>,
}

impl ForkMatrix {
    /// Creates a matrix from explicit fork names and schedules.
    pub fn new(forks: Vec<(&'static str, HardForkConfig)>) -> Self {
        Self { forks }
    }

    /// Returns every cumulative hardfork stage supported by the harness.
    pub fn all() -> Self {
        Self::new(vec![
            ("regolith", HardForkConfig { regolith_time: Some(0), ..Default::default() }),
            (
                "canyon",
                HardForkConfig {
                    regolith_time: Some(0),
                    canyon_time: Some(0),
                    ..Default::default()
                },
            ),
            (
                "delta",
                HardForkConfig {
                    regolith_time: Some(0),
                    canyon_time: Some(0),
                    delta_time: Some(0),
                    ..Default::default()
                },
            ),
            (
                "ecotone",
                HardForkConfig {
                    regolith_time: Some(0),
                    canyon_time: Some(0),
                    delta_time: Some(0),
                    ecotone_time: Some(0),
                    ..Default::default()
                },
            ),
            (
                "fjord",
                HardForkConfig {
                    regolith_time: Some(0),
                    canyon_time: Some(0),
                    delta_time: Some(0),
                    ecotone_time: Some(0),
                    fjord_time: Some(0),
                    ..Default::default()
                },
            ),
            (
                "granite",
                HardForkConfig {
                    regolith_time: Some(0),
                    canyon_time: Some(0),
                    delta_time: Some(0),
                    ecotone_time: Some(0),
                    fjord_time: Some(0),
                    granite_time: Some(0),
                    ..Default::default()
                },
            ),
            (
                "holocene",
                HardForkConfig {
                    regolith_time: Some(0),
                    canyon_time: Some(0),
                    delta_time: Some(0),
                    ecotone_time: Some(0),
                    fjord_time: Some(0),
                    granite_time: Some(0),
                    holocene_time: Some(0),
                    ..Default::default()
                },
            ),
            (
                "pectra-blob-schedule",
                HardForkConfig {
                    regolith_time: Some(0),
                    canyon_time: Some(0),
                    delta_time: Some(0),
                    ecotone_time: Some(0),
                    fjord_time: Some(0),
                    granite_time: Some(0),
                    holocene_time: Some(0),
                    pectra_blob_schedule_time: Some(0),
                    ..Default::default()
                },
            ),
            (
                "isthmus",
                HardForkConfig {
                    regolith_time: Some(0),
                    canyon_time: Some(0),
                    delta_time: Some(0),
                    ecotone_time: Some(0),
                    fjord_time: Some(0),
                    granite_time: Some(0),
                    holocene_time: Some(0),
                    isthmus_time: Some(0),
                    ..Default::default()
                },
            ),
            (
                "jovian",
                HardForkConfig {
                    regolith_time: Some(0),
                    canyon_time: Some(0),
                    delta_time: Some(0),
                    ecotone_time: Some(0),
                    fjord_time: Some(0),
                    granite_time: Some(0),
                    holocene_time: Some(0),
                    isthmus_time: Some(0),
                    jovian_time: Some(0),
                    ..Default::default()
                },
            ),
            (
                "base-v1",
                HardForkConfig {
                    regolith_time: Some(0),
                    canyon_time: Some(0),
                    delta_time: Some(0),
                    ecotone_time: Some(0),
                    fjord_time: Some(0),
                    granite_time: Some(0),
                    holocene_time: Some(0),
                    isthmus_time: Some(0),
                    jovian_time: Some(0),
                    base: Some(BaseHardforkConfig { v1: Some(0) }),
                    ..Default::default()
                },
            ),
        ])
    }

    /// Returns the cumulative forks after Granite and before Isthmus.
    pub fn pre_isthmus() -> Self {
        Self::all().retain(|_, hardforks| {
            hardforks.granite_time.is_some()
                && hardforks.isthmus_time.is_none()
                && hardforks.jovian_time.is_none()
                && hardforks.base.and_then(|base| base.v1).is_none()
        })
    }

    /// Returns the cumulative OP hardforks from Isthmus onward.
    pub fn from_isthmus() -> Self {
        Self::all().retain(|_, hardforks| {
            hardforks.isthmus_time.is_some() && hardforks.base.and_then(|base| base.v1).is_none()
        })
    }

    /// Returns the OP-style fault-proof forks starting at Granite.
    ///
    /// Base V1 is intentionally excluded because it is a standalone Base upgrade,
    /// not part of the upstream fork progression used by the fault-proof matrix.
    pub fn from_granite() -> Self {
        Self::new(vec![
            (
                "granite",
                HardForkConfig {
                    regolith_time: Some(0),
                    canyon_time: Some(0),
                    delta_time: Some(0),
                    ecotone_time: Some(0),
                    fjord_time: Some(0),
                    granite_time: Some(0),
                    ..Default::default()
                },
            ),
            (
                "holocene",
                HardForkConfig {
                    regolith_time: Some(0),
                    canyon_time: Some(0),
                    delta_time: Some(0),
                    ecotone_time: Some(0),
                    fjord_time: Some(0),
                    granite_time: Some(0),
                    holocene_time: Some(0),
                    ..Default::default()
                },
            ),
            (
                "isthmus",
                HardForkConfig {
                    regolith_time: Some(0),
                    canyon_time: Some(0),
                    delta_time: Some(0),
                    ecotone_time: Some(0),
                    fjord_time: Some(0),
                    granite_time: Some(0),
                    holocene_time: Some(0),
                    isthmus_time: Some(0),
                    ..Default::default()
                },
            ),
            (
                "jovian",
                HardForkConfig {
                    regolith_time: Some(0),
                    canyon_time: Some(0),
                    delta_time: Some(0),
                    ecotone_time: Some(0),
                    fjord_time: Some(0),
                    granite_time: Some(0),
                    holocene_time: Some(0),
                    isthmus_time: Some(0),
                    jovian_time: Some(0),
                    ..Default::default()
                },
            ),
        ])
    }

    /// Iterates through the named fork schedules.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, HardForkConfig)> + '_ {
        self.forks.iter().copied()
    }

    /// Keeps only the fork schedules that satisfy the predicate.
    pub fn retain<F>(mut self, mut predicate: F) -> Self
    where
        F: FnMut(&'static str, HardForkConfig) -> bool,
    {
        self.forks.retain(|(fork_name, hardforks)| predicate(*fork_name, *hardforks));
        self
    }

    /// Runs a test once per configured fork, annotating any panic with the fork name.
    pub fn run<F>(&self, mut test: F)
    where
        F: FnMut(&'static str, HardForkConfig),
    {
        for (fork_name, hardforks) in self.iter() {
            let result = panic::catch_unwind(AssertUnwindSafe(|| test(fork_name, hardforks)));
            if let Err(payload) = result {
                panic_with_fork_context(fork_name, payload);
            }
        }
    }
}

fn panic_with_fork_context(fork_name: &'static str, payload: Box<dyn Any + Send + 'static>) -> ! {
    let payload_ref = &*payload;
    if let Some(message) = payload_ref.downcast_ref::<String>() {
        panic!("fork matrix case `{fork_name}` failed: {message}");
    }
    if let Some(message) = payload_ref.downcast_ref::<&str>() {
        panic!("fork matrix case `{fork_name}` failed: {message}");
    }
    panic!("fork matrix case `{fork_name}` failed with a non-string panic payload");
}

/// Runs the same test body across every schedule in a [`ForkMatrix`].
#[macro_export]
macro_rules! test_across_forks {
    ($matrix:expr, |$fork_name:ident, $hardforks:ident| $body:block) => {{
        ($matrix).run(|$fork_name, $hardforks| $body);
    }};
    ($matrix:expr, |$hardforks:ident| $body:block) => {{
        ($matrix).run(|_, $hardforks| $body);
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use base_consensus_genesis::RollupConfig;

    fn test_rollup_config(hardforks: HardForkConfig) -> RollupConfig {
        RollupConfig { block_time: 2, hardforks, ..Default::default() }
    }

    fn panic_message(payload: Box<dyn Any + Send>) -> String {
        let payload_ref = &*payload;
        if let Some(message) = payload_ref.downcast_ref::<String>() {
            return message.clone();
        }
        if let Some(message) = payload_ref.downcast_ref::<&str>() {
            return (*message).to_owned();
        }
        "non-string panic payload".to_owned()
    }

    #[test]
    fn all_covers_the_supported_hardfork_progression() {
        let names: Vec<_> = ForkMatrix::all().iter().map(|(name, _)| name).collect();
        assert_eq!(
            names,
            vec![
                "regolith",
                "canyon",
                "delta",
                "ecotone",
                "fjord",
                "granite",
                "holocene",
                "pectra-blob-schedule",
                "isthmus",
                "jovian",
                "base-v1",
            ]
        );
    }

    #[test]
    fn from_granite_matches_the_fault_proof_forks() {
        let names: Vec<_> = ForkMatrix::from_granite().iter().map(|(name, _)| name).collect();
        assert_eq!(names, vec!["granite", "holocene", "isthmus", "jovian"]);
    }

    #[test]
    fn pre_isthmus_includes_pectra_and_excludes_isthmus_and_later() {
        let names: Vec<_> = ForkMatrix::pre_isthmus().iter().map(|(name, _)| name).collect();
        assert_eq!(names, vec!["granite", "holocene", "pectra-blob-schedule"]);
    }

    #[test]
    fn from_isthmus_includes_only_op_forks_from_isthmus_onward() {
        let names: Vec<_> = ForkMatrix::from_isthmus().iter().map(|(name, _)| name).collect();
        assert_eq!(names, vec!["isthmus", "jovian"]);
    }

    #[test]
    fn each_case_is_cumulative_without_enabling_the_next_fork() {
        for (fork_name, hardforks) in ForkMatrix::all().iter() {
            let cfg = test_rollup_config(hardforks);

            match fork_name {
                "regolith" => {
                    assert!(cfg.is_regolith_active(0));
                    assert!(!cfg.is_canyon_active(0));
                }
                "canyon" => {
                    assert!(cfg.is_canyon_active(0));
                    assert!(!cfg.is_delta_active(0));
                }
                "delta" => {
                    assert!(cfg.is_delta_active(0));
                    assert!(!cfg.is_ecotone_active(0));
                }
                "ecotone" => {
                    assert!(cfg.is_ecotone_active(0));
                    assert!(!cfg.is_fjord_active(0));
                }
                "fjord" => {
                    assert!(cfg.is_fjord_active(0));
                    assert!(!cfg.is_granite_active(0));
                }
                "granite" => {
                    assert!(cfg.is_granite_active(0));
                    assert!(!cfg.is_holocene_active(0));
                }
                "holocene" => {
                    assert!(cfg.is_holocene_active(0));
                    assert!(!cfg.is_pectra_blob_schedule_active(0));
                    assert!(!cfg.is_isthmus_active(0));
                }
                "pectra-blob-schedule" => {
                    assert!(cfg.is_holocene_active(0));
                    assert!(cfg.is_pectra_blob_schedule_active(0));
                    assert!(!cfg.is_isthmus_active(0));
                }
                "isthmus" => {
                    assert!(cfg.is_isthmus_active(0));
                    assert!(!cfg.is_jovian_active(0));
                }
                "jovian" => {
                    assert!(cfg.is_jovian_active(0));
                    assert!(!cfg.is_base_v1_active(0));
                }
                "base-v1" => {
                    assert!(cfg.is_jovian_active(0));
                    assert!(cfg.is_base_v1_active(0));
                }
                _ => unreachable!("unexpected fork {fork_name}"),
            }
        }
    }

    #[test]
    fn run_includes_the_fork_name_in_panics() {
        let panic = std::panic::catch_unwind(|| {
            ForkMatrix::from_granite().run(|fork_name, _| {
                assert_ne!(fork_name, "granite", "boom");
            });
        })
        .expect_err("granite case must panic");

        let message = panic_message(panic);
        assert!(message.contains("granite"));
        assert!(message.contains("boom"));
    }
}
