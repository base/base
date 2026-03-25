#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

#[cfg(target_os = "linux")]
use base_proof_tee_nitro_enclave::NitroEnclave;

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> eyre::Result<()> {
    NitroEnclave::new()?.run().await
}

#[cfg(not(target_os = "linux"))]
fn main() {
    panic!("base-prover-nitro-enclave only supports Linux (AWS Nitro Enclaves)");
}
