#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod block;
pub use block::meter_block;

mod extension;
pub use extension::{MeteringConfig, MeteringExtension};

mod inspector;

mod meter;
pub use meter::{MeterBundleInput, MeterBundleOutput, MeteredOpcodes, PseudoOpcode, meter_bundle};

mod rpc;
pub use rpc::MeteringApiImpl;

mod traits;
pub use traits::MeteringApiServer;

mod types;
pub use types::{MeterBlockResponse, MeterBlockTransactions};

mod transaction;
pub use transaction::{TxValidationError, validate_tx};
