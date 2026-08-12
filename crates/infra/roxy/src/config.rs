//! CLI / env configuration for the Roxy HTTP server.

use std::{collections::HashSet, net::SocketAddr};

use anyhow::{bail, ensure};
use clap::Args;

use crate::Backend;

/// Configuration for the Roxy HTTP server.
#[derive(Args, Debug, Clone)]
pub struct Config {
    /// Socket address to bind the HTTP server to.
    #[arg(long, env = "ROXY_LISTEN_ADDR", default_value = "0.0.0.0:8545")]
    pub listen_addr: SocketAddr,

    /// Named backend: `name=url[,url...]`. Exactly one backend is required for now.
    #[arg(
        long = "backend",
        env = "ROXY_BACKEND",
        value_name = "NAME=URL[,URL...]",
        value_parser = Backend::parse
    )]
    pub backends: Vec<Backend>,
}

impl Config {
    /// Validates config and returns the single configured backend.
    pub fn backend(&self) -> anyhow::Result<&Backend> {
        ensure!(
            !self.backends.is_empty(),
            "exactly one --backend is required (e.g. --backend rpcs=http://127.0.0.1:8545)"
        );
        ensure!(
            self.backends.len() == 1,
            "exactly one --backend is required for now, found {}",
            self.backends.len()
        );

        let mut seen = HashSet::new();
        for backend in &self.backends {
            if !seen.insert(backend.name.as_str()) {
                bail!("duplicate backend name '{}'", backend.name);
            }
        }

        Ok(&self.backends[0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(backends: Vec<Backend>) -> Config {
        Config { listen_addr: "127.0.0.1:0".parse().expect("addr"), backends }
    }

    #[test]
    fn backend_requires_exactly_one() {
        let empty = config_with(vec![]);
        let error = empty.backend().expect_err("empty");
        assert!(error.to_string().contains("exactly one --backend"), "error={error}");

        let two = config_with(vec![
            Backend::parse("a=http://127.0.0.1:1").expect("parse"),
            Backend::parse("b=http://127.0.0.1:2").expect("parse"),
        ]);
        let error = two.backend().expect_err("two backends");
        assert!(error.to_string().contains("exactly one --backend"), "error={error}");
    }

    #[test]
    fn backend_returns_the_only_entry() {
        let config =
            config_with(vec![Backend::parse("rpcs=http://127.0.0.1:8545").expect("parse")]);
        let backend = config.backend().expect("one backend");
        assert_eq!(backend.name, "rpcs");
    }
}
