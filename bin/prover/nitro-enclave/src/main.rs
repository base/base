#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use base_proof_tee_nitro_enclave::NitroEnclave;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    NitroEnclave::new()?.run().await
}
