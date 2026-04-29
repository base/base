use std::{
    any::Any,
    future::Future,
    panic::{self, AssertUnwindSafe},
};

use base_common_genesis::HardForkConfig;

/// A function that activates a single hardfork on a [`HardForkConfig`].
pub type ForkSetter = fn(&mut HardForkConfig);

/// All supported hardfork stages in canonical order. Each setter activates
/// exactly one additional fork; [`ForkMatrix::all`] applies these in sequence
/// to produce a cumulative snapshot after each step.
///
/// To add a new fork: insert one entry here in the correct position. All
/// matrix constructors update automatically.
static FORK_PROGRESSION: &[(&str, ForkSetter)] = &[
    ("regolith", |h| h.regolith_time = Some(0)),
    ("canyon", |h| h.canyon_time = Some(0)),
    ("delta", |h| h.delta_time = Some(0)),
    ("ecotone", |h| h.ecotone_time = Some(0)),
    ("fjord", |h| h.fjord_time = Some(0)),
    ("granite", |h| h.granite_time = Some(0)),
    ("holocene", |h| h.holocene_time = Some(0)),
    ("pectra-blob-schedule", |h| h.pectra_blob_schedule_time = Some(0)),
    ("isthmus", |h| h.isthmus_time = Some(0)),
    ("jovian", |h| h.jovian_time = Some(0)),
    ("azul", |h| h.base.azul = Some(0)),
];

/// Named hardfork schedules for parametrizing harness tests across protocol upgrades.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForkMatrix {
    forks: Vec<(&'static str, HardForkConfig)>,
}

impl ForkMatrix {
    /// Returns every cumulative hardfork stage supported by the harness.
    ///
    /// Each entry activates one additional fork on top of all previous ones,
    /// derived automatically from [`FORK_PROGRESSION`].
    pub fn all() -> Self {
        Self::build(FORK_PROGRESSION, HardForkConfig::default())
    }

    /// Returns the cumulative forks from Granite through Holocene (pre-Isthmus).
    ///
    /// Includes the `pectra-blob-schedule` compatibility patch, which sits
    /// between Holocene and Isthmus in the progression.
    pub fn pre_isthmus() -> Self {
        Self::all().retain(|_, h| h.granite_time.is_some() && h.isthmus_time.is_none())
    }

    /// Returns the cumulative OP hardforks from Isthmus onward.
    ///
    /// Base-specific forks (e.g. `azul`) are excluded.
    pub fn from_isthmus() -> Self {
        Self::all().retain(|_, h| h.isthmus_time.is_some() && h.base.is_empty())
    }

    /// Returns the canonical OP fault-proof fork progression from Granite onward.
    ///
    /// The `pectra-blob-schedule` compatibility patch (a Base Sepolia-only quirk)
    /// and Base-specific forks are excluded; this matrix covers only the upstream
    /// Base mainnet upgrade sequence.
    pub fn from_granite() -> Self {
        static PROGRESSION: &[(&str, ForkSetter)] = &[
            ("granite", |h| h.granite_time = Some(0)),
            ("holocene", |h| h.holocene_time = Some(0)),
            ("isthmus", |h| h.isthmus_time = Some(0)),
            ("jovian", |h| h.jovian_time = Some(0)),
        ];
        Self::build(
            PROGRESSION,
            HardForkConfig {
                regolith_time: Some(0),
                canyon_time: Some(0),
                delta_time: Some(0),
                ecotone_time: Some(0),
                fjord_time: Some(0),
                ..Default::default()
            },
        )
    }

    /// Iterates through the named fork schedules.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, HardForkConfig)> + '_ {
        self.forks.iter().copied()
    }

    /// Keeps only the fork schedules matching the predicate.
    pub fn retain<F>(mut self, mut f: F) -> Self
    where
        F: FnMut(&'static str, HardForkConfig) -> bool,
    {
        self.forks.retain(|(name, config)| f(name, *config));
        self
    }

    /// Runs a test closure once per configured fork, annotating any panic with the fork name.
    pub fn run<F>(&self, mut test: F)
    where
        F: FnMut(&'static str, HardForkConfig),
    {
        for (name, config) in self.iter() {
            if let Err(e) = panic::catch_unwind(AssertUnwindSafe(|| test(name, config))) {
                Self::panic_with_fork_context(name, e);
            }
        }
    }

    /// Async version of [`run`](ForkMatrix::run) for tests that call async sequencer methods.
    pub async fn run_async<F, Fut>(&self, mut test: F)
    where
        F: FnMut(&'static str, HardForkConfig) -> Fut,
        Fut: Future<Output = ()>,
    {
        for (name, config) in self.iter() {
            test(name, config).await;
        }
    }

    fn build(progression: &[(&'static str, ForkSetter)], base: HardForkConfig) -> Self {
        let mut config = base;
        Self {
            forks: progression
                .iter()
                .map(|(name, apply)| {
                    apply(&mut config);
                    (*name, config)
                })
                .collect(),
        }
    }

    fn panic_with_fork_context(fork: &str, payload: Box<dyn Any + Send + 'static>) -> ! {
        let msg = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("non-string panic payload");
        panic!("fork matrix case `{fork}` failed: {msg}");
    }
}

#[cfg(test)]
mod tests {
    use std::any::Any;

    use base_common_genesis::{HardForkConfig, RollupConfig};

    use super::ForkMatrix;

    struct MatrixFixture;

    impl MatrixFixture {
        fn rollup_config(hardforks: HardForkConfig) -> RollupConfig {
            RollupConfig { block_time: 2, hardforks, ..Default::default() }
        }

        fn panic_message(payload: Box<dyn Any + Send>) -> String {
            payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "non-string panic payload".to_owned())
        }
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
                "azul",
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
            let cfg = MatrixFixture::rollup_config(hardforks);
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
                    assert!(!cfg.is_base_azul_active(0));
                }
                "azul" => {
                    assert!(cfg.is_jovian_active(0));
                    assert!(cfg.is_base_azul_active(0));
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

        let message = MatrixFixture::panic_message(panic);
        assert!(message.contains("granite"));
        assert!(message.contains("boom"));
    }
}
