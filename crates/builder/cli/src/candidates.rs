//! Swap Lane (`base_sendSwapTransaction`) and candidate selection CLI arguments.

use core::time::Duration;
use std::{path::PathBuf, sync::Arc};

use base_execution_candidates::{
    CandidateLimits, CandidateRuntime, CandidateSwitch, DEFAULT_MAX_EVALUATIONS,
    DEFAULT_MAX_SIBLING_GAS, DEFAULT_MAX_SIBLINGS, DEFAULT_PROBE_GAS, GroupLedger,
    MAX_SIBLINGS_CEILING, PrincipalsFile, StaticAccessPolicy,
};
use base_execution_payload_builder::{
    CandidateBuilderConfig, DEFAULT_CANDIDATE_BUILD_DEADLINE, DEFAULT_CANDIDATE_BUILD_WORK_GAS,
    DEFAULT_CANDIDATE_EVAL_WALL_CUTOFF, DEFAULT_INVALID_ATTEMPT_GAS,
};
use eyre::WrapErr;

/// Provisional candidate group lifetime past the next block, in seconds.
pub const DEFAULT_CANDIDATE_MAX_EXPIRY_SECS: u64 = 60;

/// Which payload builders the node can route a build to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuilderPath {
    /// Only the flashblocks builder runs. It parks candidate groups and never evaluates them.
    FlashblocksOnly,
    /// The native builder runs, alone or after the payload-builder cutover.
    Native,
}

/// Candidate capabilities a node runs, independent of which ingress endpoints expose them.
#[derive(Debug, Clone)]
pub struct CandidateCapabilities {
    /// Ledger, access policy, kill switch and limits shared by ingress, pool and builder.
    pub runtime: Arc<CandidateRuntime>,
    /// Lifetime of an admitted group past the next block, in seconds.
    pub group_lifetime_secs: u64,
    /// Native builder selection settings, or `None` when builder selection is off.
    pub builder: Option<CandidateBuilderConfig>,
}

/// Swap Lane ingress and builder candidate selection settings. Everything is off by default.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct CandidateArgs {
    /// Runs the candidate capabilities (ledger, policy and pool support) and registers
    /// `base_sendSwapTransaction` at startup. When off the method is unregistered and returns
    /// `-32601`.
    #[arg(long = "txpool.candidates.enabled", default_value = "false")]
    pub candidates_enabled: bool,

    /// Acknowledges that the group ledger, sibling tombstones and principal usage live only in
    /// memory. After a restart, budgets reset and a reorg can restore a pre-restart winner as a
    /// plain transaction (design §4.6, Q16). Required with `--txpool.candidates.enabled` until
    /// the ledger is recoverable; use only on devnets and tests.
    #[arg(long = "txpool.candidates.ephemeral-ledger", default_value = "false")]
    pub ephemeral_ledger: bool,

    /// JSON principals file (`policyVersion`, optional `workGasQuota`, `principals`). Without
    /// one, ingress admits nobody.
    #[arg(long = "txpool.candidates.principals-file")]
    pub principals_file: Option<PathBuf>,

    /// Maximum transactions per swap, counting the default transaction and its alternatives.
    #[arg(long = "txpool.candidates.max-siblings", default_value_t = DEFAULT_MAX_SIBLINGS)]
    pub max_siblings: usize,

    /// Maximum gas limit of any one swap transaction, including a default transaction submitted
    /// alone.
    #[arg(long = "txpool.candidates.max-sibling-gas", default_value_t = DEFAULT_MAX_SIBLING_GAS)]
    pub max_sibling_gas: u64,

    /// Lifetime of an admitted candidate group past the next block, in seconds, rounded up to
    /// whole blocks. Senders sign no expiry; a default transaction submitted alone has no
    /// node-assigned lifetime.
    #[arg(
        long = "txpool.candidates.max-expiry-secs",
        default_value_t = DEFAULT_CANDIDATE_MAX_EXPIRY_SECS
    )]
    pub max_expiry_secs: u64,

    /// Evaluates candidate groups in the native builder. When off, accepted groups stay parked.
    #[arg(long = "builder.candidates.enabled", default_value = "false")]
    pub builder_candidates_enabled: bool,

    /// Gas cap for each score-read probe (`G_probe`).
    #[arg(long = "builder.candidates.probe-gas", default_value_t = DEFAULT_PROBE_GAS)]
    pub probe_gas: u64,

    /// Wall time candidate handling may spend per payload build, in microseconds.
    #[arg(
        long = "builder.candidates.eval-wall-cutoff-us",
        default_value_t = DEFAULT_CANDIDATE_EVAL_WALL_CUTOFF.as_micros() as u64
    )]
    pub eval_wall_cutoff_us: u64,

    /// Time after a build's pool scan starts past which no candidate work starts, in
    /// microseconds.
    #[arg(
        long = "builder.candidates.build-deadline-us",
        default_value_t = DEFAULT_CANDIDATE_BUILD_DEADLINE.as_micros() as u64
    )]
    pub build_deadline_us: u64,

    /// Work gas all candidate rounds of one build may reserve.
    #[arg(
        long = "builder.candidates.build-work-gas",
        default_value_t = DEFAULT_CANDIDATE_BUILD_WORK_GAS
    )]
    pub build_work_gas: u64,

    /// Work charged for a sibling that fails pre-EVM validation.
    #[arg(
        long = "builder.candidates.invalid-attempt-gas",
        default_value_t = DEFAULT_INVALID_ATTEMPT_GAS
    )]
    pub invalid_attempt_gas: u64,

    /// Server cap on evaluations per group, applied below the signed `maxEvaluations`.
    #[arg(long = "txpool.candidates.max-evaluations", default_value_t = DEFAULT_MAX_EVALUATIONS)]
    pub max_evaluations: u16,

    /// Journal a `BUILDER_CANDIDATE_TRIAL` event per sibling outcome. Off by default: round
    /// events already carry per-outcome counts, and trials multiply event volume by the
    /// sibling count.
    #[arg(long = "builder.candidates.trial-events", default_value_t = false)]
    pub trial_events: bool,
}

impl Default for CandidateArgs {
    fn default() -> Self {
        Self {
            candidates_enabled: false,
            ephemeral_ledger: false,
            principals_file: None,
            max_siblings: DEFAULT_MAX_SIBLINGS,
            max_sibling_gas: DEFAULT_MAX_SIBLING_GAS,
            max_expiry_secs: DEFAULT_CANDIDATE_MAX_EXPIRY_SECS,
            builder_candidates_enabled: false,
            probe_gas: DEFAULT_PROBE_GAS,
            eval_wall_cutoff_us: DEFAULT_CANDIDATE_EVAL_WALL_CUTOFF.as_micros() as u64,
            build_deadline_us: DEFAULT_CANDIDATE_BUILD_DEADLINE.as_micros() as u64,
            build_work_gas: DEFAULT_CANDIDATE_BUILD_WORK_GAS,
            invalid_attempt_gas: DEFAULT_INVALID_ATTEMPT_GAS,
            max_evaluations: DEFAULT_MAX_EVALUATIONS,
            trial_events: false,
        }
    }
}

impl CandidateArgs {
    /// Rejects flag combinations that start a node unable to use what it enables.
    pub fn validate(&self, path: BuilderPath) -> eyre::Result<()> {
        if !self.candidates_enabled {
            eyre::ensure!(
                !self.builder_candidates_enabled,
                "--builder.candidates.enabled requires --txpool.candidates.enabled: without the \
                 candidate runtime no group reaches the builder"
            );
            return Ok(());
        }
        eyre::ensure!(
            (2..=MAX_SIBLINGS_CEILING).contains(&self.max_siblings),
            "--txpool.candidates.max-siblings must be between 2 and {MAX_SIBLINGS_CEILING}"
        );
        // Fail closed: the ledger cannot be recovered after a restart (design §4.4, §4.6).
        eyre::ensure!(
            self.ephemeral_ledger,
            "--txpool.candidates.enabled requires --txpool.candidates.ephemeral-ledger: the \
             candidate ledger does not survive restarts"
        );
        for (flag, value) in [
            ("--txpool.candidates.max-sibling-gas", self.max_sibling_gas),
            ("--txpool.candidates.max-expiry-secs", self.max_expiry_secs),
        ] {
            eyre::ensure!(value > 0, "{flag} must be positive");
        }
        if !self.builder_candidates_enabled {
            return Ok(());
        }
        eyre::ensure!(
            path == BuilderPath::Native,
            "--builder.candidates.enabled requires --builder.payload-builder-cutover or \
             --builder.basic-payload-builder: the flashblocks builder parks candidate groups"
        );
        for (flag, value) in [
            ("--builder.candidates.probe-gas", self.probe_gas),
            ("--builder.candidates.eval-wall-cutoff-us", self.eval_wall_cutoff_us),
            ("--builder.candidates.build-deadline-us", self.build_deadline_us),
            ("--builder.candidates.build-work-gas", self.build_work_gas),
            ("--txpool.candidates.max-evaluations", u64::from(self.max_evaluations)),
        ] {
            eyre::ensure!(value > 0, "{flag} must be positive with --builder.candidates.enabled");
        }
        Ok(())
    }

    /// Validates the flags and builds the candidate capabilities, or `None` when they are off.
    pub fn capabilities(&self, path: BuilderPath) -> eyre::Result<Option<CandidateCapabilities>> {
        self.validate(path)?;
        if !self.candidates_enabled {
            return Ok(None);
        }
        let principals = match &self.principals_file {
            Some(path) => serde_json::from_slice(
                &std::fs::read(path).wrap_err_with(|| format!("reading {}", path.display()))?,
            )
            .wrap_err_with(|| format!("parsing {}", path.display()))?,
            None => PrincipalsFile { policy_version: 0, work_gas_quota: None, principals: vec![] },
        };
        let runtime = Arc::new(CandidateRuntime {
            ledger: GroupLedger::default(),
            policy: Arc::new(StaticAccessPolicy::new(principals)),
            switch: CandidateSwitch::new(true),
            limits: CandidateLimits {
                max_siblings: self.max_siblings,
                max_sibling_gas: self.max_sibling_gas,
                ..CandidateLimits::default()
            },
        });
        let builder = self.builder_candidates_enabled.then(|| CandidateBuilderConfig {
            runtime: Arc::clone(&runtime),
            probe_gas: self.probe_gas,
            eval_wall_cutoff: Duration::from_micros(self.eval_wall_cutoff_us),
            build_deadline: Duration::from_micros(self.build_deadline_us),
            build_work_gas: self.build_work_gas,
            invalid_attempt_gas: self.invalid_attempt_gas,
            server_max_evaluations: self.max_evaluations,
            trial_events: self.trial_events,
        });
        Ok(Some(CandidateCapabilities {
            runtime,
            group_lifetime_secs: self.max_expiry_secs,
            builder,
        }))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use base_execution_candidates::{Capability, PrincipalId};
    use clap::Parser;

    use super::*;

    #[derive(Debug, Parser)]
    struct CommandParser {
        #[command(flatten)]
        args: CandidateArgs,
    }

    fn parse(flags: &[&str]) -> CandidateArgs {
        CommandParser::parse_from(core::iter::once("test").chain(flags.iter().copied())).args
    }

    const INGRESS: [&str; 2] =
        ["--txpool.candidates.enabled", "--txpool.candidates.ephemeral-ledger"];

    fn rejection(flags: &[&str], path: BuilderPath) -> String {
        parse(flags).capabilities(path).expect_err("unusable combination").to_string()
    }

    #[test]
    fn candidates_are_disabled_by_default() {
        let args = CommandParser::parse_from(["test"]).args;
        assert_eq!(args, CandidateArgs::default());
        assert!(args.capabilities(BuilderPath::FlashblocksOnly).expect("disabled").is_none());
    }

    #[test]
    fn principals_file_authorizes_its_senders() {
        let dir = std::env::temp_dir().join(format!("candidate-args-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("principals.json");
        let sender = Address::repeat_byte(0x11);
        std::fs::write(
            &path,
            format!(r#"{{"policyVersion":3,"principals":[{{"id":7,"senders":["{sender}"]}}]}}"#),
        )
        .expect("writes principals");

        let args = parse(&[
            INGRESS[0],
            INGRESS[1],
            "--txpool.candidates.principals-file",
            path.to_str().expect("utf-8 path"),
            "--txpool.candidates.max-expiry-secs",
            "30",
            "--builder.candidates.enabled",
            "--builder.candidates.probe-gas",
            "50000",
        ]);
        let capabilities =
            args.capabilities(BuilderPath::Native).expect("valid flags").expect("enabled");
        std::fs::remove_dir_all(&dir).expect("cleans up");

        let policy = &capabilities.runtime.policy;
        assert_eq!(policy.principal(sender, Capability::CandidateSelection), Some(PrincipalId(7)));
        assert_eq!(policy.principal(sender, Capability::SingleAdmission), Some(PrincipalId(7)));
        assert_eq!(policy.policy_version(), 3);
        assert_eq!(capabilities.group_lifetime_secs, 30);
        assert_eq!(capabilities.builder.expect("builder selection is enabled").probe_gas, 50_000);
    }

    #[test]
    fn builder_selection_requires_its_own_flag() {
        let capabilities = parse(&INGRESS).capabilities(BuilderPath::FlashblocksOnly);
        let capabilities = capabilities.expect("ingress alone is usable").expect("enabled");
        assert!(capabilities.builder.is_none(), "an absent principals file admits nobody");
    }

    #[test]
    fn enabling_candidates_requires_acknowledging_the_ephemeral_ledger() {
        let err = rejection(&["--txpool.candidates.enabled"], BuilderPath::Native);
        assert!(err.contains("ephemeral-ledger"), "got {err}");
    }

    #[test]
    fn max_siblings_past_the_request_ceiling_is_rejected() {
        let err = rejection(
            &[INGRESS[0], INGRESS[1], "--txpool.candidates.max-siblings", "17"],
            BuilderPath::Native,
        );
        assert!(err.contains("max-siblings"), "got {err}");
    }

    #[test]
    fn builder_selection_without_the_runtime_is_rejected() {
        let err = rejection(&["--builder.candidates.enabled"], BuilderPath::Native);
        assert!(err.contains("--txpool.candidates.enabled"), "got {err}");
    }

    #[test]
    fn builder_selection_needs_a_native_builder_path() {
        let flags = [INGRESS[0], INGRESS[1], "--builder.candidates.enabled"];
        let err = rejection(&flags, BuilderPath::FlashblocksOnly);
        assert!(err.contains("parks candidate groups"), "got {err}");
        assert!(parse(&flags).capabilities(BuilderPath::Native).is_ok());
    }

    #[test]
    fn zero_knobs_that_disable_an_enabled_capability_are_rejected() {
        for (flag, builder) in [
            ("--txpool.candidates.max-expiry-secs", false),
            ("--txpool.candidates.max-sibling-gas", false),
            ("--builder.candidates.probe-gas", true),
            ("--builder.candidates.eval-wall-cutoff-us", true),
            ("--builder.candidates.build-deadline-us", true),
            ("--builder.candidates.build-work-gas", true),
            ("--txpool.candidates.max-evaluations", true),
        ] {
            let mut flags = vec![INGRESS[0], INGRESS[1], flag, "0"];
            if builder {
                flags.push("--builder.candidates.enabled");
            }
            let err = rejection(&flags, BuilderPath::Native);
            assert!(err.contains(flag), "{flag}: got {err}");
        }
    }
}
