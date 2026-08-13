//! CLI / env configuration for the Roxy HTTP server.

use std::{collections::HashSet, net::SocketAddr};

use anyhow::ensure;
use clap::Args;

use crate::Backend;

/// Configuration for the Roxy HTTP server.
#[derive(Args, Debug, Clone)]
pub struct Config {
    /// Socket address to bind the HTTP server to.
    #[arg(long, env = "ROXY_LISTEN_ADDR", default_value = "0.0.0.0:8545")]
    pub listen_addr: SocketAddr,

    /// Named backend in `name=url[,url...]` format. May be repeated.
    #[arg(
        long = "backend",
        env = "ROXY_BACKENDS",
        value_name = "NAME=URL[,URL...]",
        value_delimiter = ';',
        required = true,
        value_parser = Backend::parse
    )]
    pub backends: Vec<Backend>,
}

impl Config {
    /// Validates relationships between configured options.
    pub fn validate(&self) -> anyhow::Result<()> {
        let mut names = HashSet::new();
        for backend in &self.backends {
            ensure!(
                names.insert(backend.name.as_str()),
                "duplicate backend name '{}'",
                backend.name
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{env, ffi::OsStr, process::Command as ProcessCommand};

    use clap::{Command, FromArgMatches};

    use super::*;

    #[test]
    fn parses_repeated_backend_flags() {
        let matches = Config::augment_args(Command::new("roxy"))
            .try_get_matches_from([
                "roxy",
                "--backend",
                "rpcs=http://127.0.0.1:8545",
                "--backend",
                "flashblocks=http://127.0.0.1:8546",
            ])
            .expect("valid repeated backend flags");
        let config = Config::from_arg_matches(&matches).expect("backend config");

        assert_eq!(config.backends.len(), 2, "both backend flags must be parsed");
        assert_eq!(config.backends[0].name, "rpcs", "first backend must be preserved");
        assert_eq!(config.backends[1].name, "flashblocks", "second backend must be preserved");
    }

    #[test]
    fn configures_plural_semicolon_delimited_env() {
        if env::var_os("ROXY_ENV_TEST_CHILD").is_some() {
            let matches = Config::augment_args(Command::new("roxy"))
                .try_get_matches_from(["roxy"])
                .expect("valid environment backends");
            let config = Config::from_arg_matches(&matches).expect("backend config");

            assert_eq!(config.backends.len(), 2, "both environment backends must be parsed");
            assert_eq!(config.backends[0].name, "rpcs", "first backend must be preserved");
            assert_eq!(config.backends[1].name, "flashblocks", "second backend must be preserved");
            return;
        }

        let command = Config::augment_args(Command::new("roxy"));
        let argument = command
            .get_arguments()
            .find(|argument| argument.get_id() == "backends")
            .expect("backends argument");

        assert_eq!(
            argument.get_env(),
            Some(OsStr::new("ROXY_BACKENDS")),
            "backend environment variable must be plural"
        );
        assert_eq!(
            argument.get_value_delimiter(),
            Some(';'),
            "environment entries must be semicolon-delimited"
        );

        let status = ProcessCommand::new(env::current_exe().expect("current test executable"))
            .args(["--exact", "config::tests::configures_plural_semicolon_delimited_env"])
            .env("ROXY_ENV_TEST_CHILD", "1")
            .env("ROXY_BACKENDS", "rpcs=http://127.0.0.1:8545;flashblocks=http://127.0.0.1:8546")
            .status()
            .expect("run environment parsing subprocess");
        assert!(status.success(), "environment parsing subprocess must pass");
    }

    #[test]
    fn rejects_duplicate_backend_names() {
        let matches = Config::augment_args(Command::new("roxy"))
            .try_get_matches_from([
                "roxy",
                "--backend",
                "rpcs=http://127.0.0.1:8545",
                "--backend",
                "rpcs=http://127.0.0.1:8546",
            ])
            .expect("individually valid backends");
        let config = Config::from_arg_matches(&matches).expect("backend config");

        let error = config.validate().expect_err("duplicate backend name");
        assert!(
            error.to_string().contains("duplicate backend name 'rpcs'"),
            "error must identify the duplicate name: {error}"
        );
    }

    #[test]
    fn rejects_empty_delimited_backend_entries() {
        let error = Config::augment_args(Command::new("roxy"))
            .try_get_matches_from(["roxy", "--backend", "rpcs=http://127.0.0.1:8545;"])
            .expect_err("empty backend entry");

        assert!(
            error.to_string().contains("entry is empty"),
            "error must identify the empty backend entry: {error}"
        );
    }
}
