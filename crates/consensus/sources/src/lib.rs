#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod signer;
pub use signer::{
    BlockSigner, BlockSignerError, BlockSignerHandler, BlockSignerStartError, CertificateError,
    ClientCert, RemoteSigner, RemoteSignerError, RemoteSignerHandler, RemoteSignerStartError,
};
