//! Canonical network environment variable names shared by the proof services.
//!
//! The proof binaries historically read the same network endpoints under different,
//! binary-specific env var names (for example `BASE_PROPOSER_L1_ETH_RPC`, `L1_ETH_URL`,
//! and `L1_NODE_ADDRESS` all hold an L1 execution RPC URL). [`NetworkEnv`] defines one
//! canonical name per role, and [`EnvAlias`] lets a binary accept the canonical name in
//! addition to its existing name.
//!
//! Aliases are applied to the clap [`Command`] before parsing by pointing the existing
//! argument's `env` at the canonical name. This keeps clap's normal precedence (command
//! line flag over env var), keeps required arguments working, and never mutates the
//! process environment, so it is safe to call from inside an async runtime.

use std::{
    ffi::{OsStr, OsString},
    fmt,
};

use clap::{Command, Error, Parser, error::ErrorKind};

/// Canonical env var names for the network endpoints used by the proof services.
#[derive(Debug, Clone, Copy)]
pub struct NetworkEnv;

impl NetworkEnv {
    /// L1 execution layer JSON-RPC URL.
    pub const L1_ETH_RPC: &'static str = "BASE_L1_ETH_RPC";
    /// L1 beacon API URL.
    pub const L1_BEACON_RPC: &'static str = "BASE_L1_BEACON_RPC";
    /// L2 execution layer JSON-RPC URL.
    pub const L2_ETH_RPC: &'static str = "BASE_L2_ETH_RPC";
    /// L2 rollup (consensus node) RPC URL.
    pub const ROLLUP_RPC: &'static str = "BASE_ROLLUP_RPC";
    /// Prover service RPC URL used to request proofs.
    pub const PROVER_RPC: &'static str = "BASE_PROVER_RPC";
    /// Prover service RPC URL used by proving workers to claim jobs.
    pub const PROVER_WORKER_RPC: &'static str = "BASE_PROVER_WORKER_RPC";
}

/// Maps a canonical env var name onto the existing env var name of one CLI argument.
///
/// Resolution rules, per alias:
/// - only the existing name is set: the existing name is used, unchanged;
/// - only the canonical name is set: the argument reads the canonical name;
/// - both are set to the same value: the existing name is used;
/// - both are set to different values: parsing fails with [`EnvAliasConflict`].
///
/// A variable counts as set when it is present in the environment, even if empty,
/// matching clap's own env handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvAlias {
    /// Canonical env var name, usually one of the [`NetworkEnv`] constants.
    pub canonical: &'static str,
    /// Existing env var name declared on the CLI argument.
    pub legacy: &'static str,
}

impl EnvAlias {
    /// Creates an alias from a canonical env var name to an existing one.
    pub const fn new(canonical: &'static str, legacy: &'static str) -> Self {
        Self { canonical, legacy }
    }

    /// Returns the env var name the argument should read instead of [`Self::legacy`], if any.
    ///
    /// `lookup` returns the value of an env var, or `None` when it is unset.
    pub fn resolve(
        &self,
        lookup: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Option<&'static str>, EnvAliasConflict> {
        match (lookup(self.canonical), lookup(self.legacy)) {
            (Some(canonical), Some(legacy)) if canonical != legacy => {
                Err(EnvAliasConflict { alias: *self })
            }
            (Some(_), None) => Ok(Some(self.canonical)),
            _ => Ok(None),
        }
    }

    /// Returns `true` if any argument in `cmd` or its subcommands reads [`Self::legacy`].
    pub fn is_declared_in(&self, cmd: &Command) -> bool {
        cmd.get_arguments().any(|arg| arg.get_env() == Some(OsStr::new(self.legacy)))
            || cmd.get_subcommands().any(|sub| self.is_declared_in(sub))
    }

    /// Points every argument in `cmd` and its subcommands that reads `from` at `to`.
    pub fn rebind(cmd: Command, from: &str, to: &'static str) -> Command {
        cmd.mut_args(|arg| if arg.get_env() == Some(OsStr::new(from)) { arg.env(to) } else { arg })
            .mut_subcommands(|sub| Self::rebind(sub, from, to))
    }

    /// Applies `aliases` to `cmd` using the process environment.
    pub fn apply(mut cmd: Command, aliases: &[Self]) -> Result<Command, EnvAliasConflict> {
        for alias in aliases {
            if let Some(env) = alias.resolve(|name| std::env::var_os(name))? {
                cmd = Self::rebind(cmd, alias.legacy, env);
            }
        }
        Ok(cmd)
    }

    /// Parses `P` from the process arguments and environment, honoring `aliases`.
    ///
    /// Drop-in replacement for [`Parser::parse`]: prints the error and exits on failure.
    pub fn parse<P: Parser>(aliases: &[Self]) -> P {
        let mut cmd = Self::apply(P::command(), aliases).unwrap_or_else(|err| {
            Error::raw(ErrorKind::ArgumentConflict, format!("{err}\n")).exit()
        });
        let mut matches = cmd.get_matches_mut();
        P::from_arg_matches_mut(&mut matches).unwrap_or_else(|err| err.format(&mut cmd).exit())
    }
}

/// Error returned when a canonical env var and its existing alias hold different values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvAliasConflict {
    /// The conflicting alias.
    pub alias: EnvAlias,
}

impl fmt::Display for EnvAliasConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "env vars {} and {} are both set to different values; set only one",
            self.alias.canonical, self.alias.legacy
        )
    }
}

impl std::error::Error for EnvAliasConflict {}

#[cfg(test)]
mod tests {
    use clap::Arg;
    use rstest::rstest;

    use super::*;

    const ALIAS: EnvAlias = EnvAlias::new("CANONICAL", "LEGACY");

    fn lookup(
        canonical: Option<&'static str>,
        legacy: Option<&'static str>,
    ) -> impl Fn(&str) -> Option<OsString> {
        move |name| match name {
            "CANONICAL" => canonical.map(OsString::from),
            "LEGACY" => legacy.map(OsString::from),
            _ => None,
        }
    }

    #[rstest]
    #[case::unset(None, None, None)]
    #[case::canonical_only(Some("a"), None, Some("CANONICAL"))]
    #[case::legacy_only(None, Some("a"), None)]
    #[case::both_equal(Some("a"), Some("a"), None)]
    fn resolve_selects_env(
        #[case] canonical: Option<&'static str>,
        #[case] legacy: Option<&'static str>,
        #[case] expected: Option<&'static str>,
    ) {
        assert_eq!(ALIAS.resolve(lookup(canonical, legacy)), Ok(expected));
    }

    #[test]
    fn resolve_rejects_different_values() {
        let err = ALIAS.resolve(lookup(Some("a"), Some("b"))).unwrap_err();
        assert_eq!(err, EnvAliasConflict { alias: ALIAS });
        assert!(err.to_string().contains("CANONICAL"));
        assert!(err.to_string().contains("LEGACY"));
    }

    #[test]
    fn rebind_updates_nested_subcommands() {
        let cmd = Command::new("root")
            .arg(Arg::new("other").long("other").env("OTHER"))
            .subcommand(Command::new("sub").arg(Arg::new("rpc").long("rpc").env("LEGACY")));

        assert!(ALIAS.is_declared_in(&cmd));
        let cmd = EnvAlias::rebind(cmd, ALIAS.legacy, ALIAS.canonical);

        assert!(!ALIAS.is_declared_in(&cmd));
        let sub = cmd.find_subcommand("sub").unwrap();
        let rpc = sub.get_arguments().find(|arg| arg.get_id() == "rpc").unwrap();
        assert_eq!(rpc.get_env(), Some(OsStr::new("CANONICAL")));
        let other = cmd.get_arguments().find(|arg| arg.get_id() == "other").unwrap();
        assert_eq!(other.get_env(), Some(OsStr::new("OTHER")));
    }
}
