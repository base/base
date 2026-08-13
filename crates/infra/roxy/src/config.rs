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

    /// Named backend: `name=url[,url...]`. At least one is required.
    #[arg(
        long = "backend",
        env = "ROXY_BACKEND",
        value_name = "NAME=URL[,URL...]",
        value_parser = Backend::parse
    )]
    pub backends: Vec<Backend>,
}

impl Config {
    /// Validates backends and returns them.
    ///
    /// Requires at least one backend and unique names. Request routing across
    /// multiple backends is not implemented yet; callers currently use the first
    /// entry for forwarding.
    pub fn backends(&self) -> anyhow::Result<&[Backend]> {
        ensure!(
            !self.backends.is_empty(),
            "at least one --backend is required (e.g. --backend rpcs=http://127.0.0.1:8545)"
        );

        let mut seen = HashSet::new();
        for backend in &self.backends {
            if !seen.insert(backend.name.as_str()) {
                bail!("duplicate backend name '{}'", backend.name);
            }
        }

        Ok(&self.backends)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(backends: Vec<Backend>) -> Config {
        Config { listen_addr: "127.0.0.1:0".parse().expect("addr"), backends }
    }

    #[test]
    fn backends_requires_at_least_one() {
        let empty = config_with(vec![]);
        let error = empty.backends().expect_err("empty");
        assert!(error.to_string().contains("at least one --backend"), "error={error}");
    }

    #[test]
    fn backends_allows_multiple_unique_names() {
        let config = config_with(vec![
            Backend::parse("rpcs=http://127.0.0.1:1").expect("parse"),
            Backend::parse("flashblocks=http://127.0.0.1:2").expect("parse"),
        ]);
        let backends = config.backends().expect("two backends");
        assert_eq!(backends.len(), 2, "both backends retained");
        assert_eq!(backends[0].name, "rpcs");
        assert_eq!(backends[1].name, "flashblocks");
    }

    #[test]
    fn backends_rejects_duplicate_names() {
        let config = config_with(vec![
            Backend::parse("rpcs=http://127.0.0.1:1").expect("parse"),
            Backend::parse("rpcs=http://127.0.0.1:2").expect("parse"),
        ]);
        let error = config.backends().expect_err("duplicate name");
        assert!(error.to_string().contains("duplicate backend name 'rpcs'"), "error={error}");
    }
}
