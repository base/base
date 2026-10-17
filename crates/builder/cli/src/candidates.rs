//! Candidate-group (`base_sendRawTransactionCandidates`) CLI arguments.

use core::time::Duration;
use std::{path::PathBuf, sync::Arc};

use base_execution_candidates::{
    CandidateLimits, CandidateRuntime, CandidateSwitch, DEFAULT_MAX_EVALUATIONS,
    DEFAULT_MAX_SIBLING_GAS, DEFAULT_MAX_SIBLINGS, DEFAULT_PROBE_GAS, GroupLedger, PrincipalsFile,
    StaticAccessPolicy,
};
use base_execution_payload_builder::{
    CandidateBuilderConfig, DEFAULT_CANDIDATE_BUILD_DEADLINE, DEFAULT_CANDIDATE_BUILD_WORK_GAS,
    DEFAULT_CANDIDATE_EVAL_WALL_CUTOFF, DEFAULT_INVALID_ATTEMPT_GAS,
};
use eyre::WrapErr;

/// Provisional maximum group lifetime past the next block, in seconds.
pub const DEFAULT_CANDIDATE_MAX_EXPIRY_SECS: u64 = 60;

/// Candidate-group ingress and builder selection settings. Everything is off by default.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct CandidateArgs {
    /// Registers `base_sendRawTransactionCandidates` at startup. When off the method is
    /// unregistered and returns `-32601`.
    #[arg(long = "txpool.candidates.enabled", default_value = "false")]
    pub candidates_enabled: bool,

    /// JSON principals file (`policyVersion`, optional `workGasQuota`, `principals`). Without
    /// one, ingress admits nobody.
    #[arg(long = "txpool.candidates.principals-file")]
    pub principals_file: Option<PathBuf>,

    /// Maximum siblings per group.
    #[arg(long = "txpool.candidates.max-siblings", default_value_t = DEFAULT_MAX_SIBLINGS)]
    pub max_siblings: usize,

    /// Maximum gas limit of any one sibling.
    #[arg(long = "txpool.candidates.max-sibling-gas", default_value_t = DEFAULT_MAX_SIBLING_GAS)]
    pub max_sibling_gas: u64,

    /// Maximum group lifetime past the next block, in seconds.
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
}

impl Default for CandidateArgs {
    fn default() -> Self {
        Self {
            candidates_enabled: false,
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
        }
    }
}

impl CandidateArgs {
    /// Builds the shared candidate runtime, or `None` when ingress is disabled.
    pub fn runtime(&self) -> eyre::Result<Option<Arc<CandidateRuntime>>> {
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
        Ok(Some(Arc::new(CandidateRuntime {
            ledger: GroupLedger::default(),
            policy: Arc::new(StaticAccessPolicy::new(principals)),
            switch: CandidateSwitch::new(true),
            limits: CandidateLimits {
                max_siblings: self.max_siblings,
                max_sibling_gas: self.max_sibling_gas,
                ..CandidateLimits::default()
            },
        })))
    }

    /// Native builder settings for `runtime`, or `None` when builder selection is disabled.
    pub fn builder_config(
        &self,
        runtime: Option<&Arc<CandidateRuntime>>,
    ) -> Option<CandidateBuilderConfig> {
        let runtime = runtime.filter(|_| self.builder_candidates_enabled)?;
        Some(CandidateBuilderConfig {
            runtime: Arc::clone(runtime),
            probe_gas: self.probe_gas,
            eval_wall_cutoff: Duration::from_micros(self.eval_wall_cutoff_us),
            build_deadline: Duration::from_micros(self.build_deadline_us),
            build_work_gas: self.build_work_gas,
            invalid_attempt_gas: self.invalid_attempt_gas,
            server_max_evaluations: self.max_evaluations,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use base_execution_candidates::PrincipalId;
    use clap::Parser;

    use super::*;

    #[derive(Debug, Parser)]
    struct CommandParser {
        #[command(flatten)]
        args: CandidateArgs,
    }

    #[test]
    fn candidates_are_disabled_by_default() {
        let args = CommandParser::parse_from(["test"]).args;
        assert_eq!(args, CandidateArgs::default());
        let runtime = args.runtime().expect("disabled runtime builds");
        assert!(runtime.is_none());
        assert!(args.builder_config(runtime.as_ref()).is_none());
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

        let args = CommandParser::parse_from([
            "test",
            "--txpool.candidates.enabled",
            "--txpool.candidates.principals-file",
            path.to_str().expect("utf-8 path"),
            "--builder.candidates.enabled",
            "--builder.candidates.probe-gas",
            "50000",
        ])
        .args;
        let runtime = args.runtime().expect("runtime builds").expect("ingress is enabled");
        std::fs::remove_dir_all(&dir).expect("cleans up");

        assert_eq!(runtime.policy.principal(sender), Some(PrincipalId(7)));
        assert_eq!(runtime.policy.policy_version(), 3);
        let builder = args.builder_config(Some(&runtime)).expect("builder selection is enabled");
        assert_eq!(builder.probe_gas, 50_000);
    }

    #[test]
    fn builder_selection_requires_its_own_flag() {
        let args = CommandParser::parse_from(["test", "--txpool.candidates.enabled"]).args;
        let runtime = args.runtime().expect("runtime builds");
        assert!(runtime.is_some(), "an absent principals file admits nobody but still runs");
        assert!(args.builder_config(runtime.as_ref()).is_none());
    }
}
