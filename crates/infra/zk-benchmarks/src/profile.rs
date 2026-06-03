//! Built-in benchmark runtime profiles.

use url::Url;

use crate::BenchmarkProfile;

/// Built-in benchmark profiles.
#[derive(Debug)]
pub struct Profiles;

impl Profiles {
    /// Returns the built-in local devnet profile.
    pub fn devnet() -> BenchmarkProfile {
        BenchmarkProfile {
            name: "devnet".to_string(),
            l2_rpc_url: Url::parse("http://localhost:8645").expect("valid devnet l2 rpc url"),
            rollup_rpc_url: Url::parse("http://localhost:8649")
                .expect("valid devnet rollup rpc url"),
            zk_prover_url: Url::parse("http://localhost:9000").expect("valid devnet zk prover url"),
            l2_chain_id: 84_538_453,
        }
    }

    /// Resolves a profile by name.
    pub fn get(name: &str) -> Option<BenchmarkProfile> {
        match name {
            "devnet" => Some(Self::devnet()),
            _ => None,
        }
    }
}
