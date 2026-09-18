//! Real-client acceptance scenarios and their observable evidence checks.

mod activation;
pub use activation::Schedule;

mod batch;
pub use batch::{BatchAttribution, BatchObserver, Submission, SubmittedChannel, TransferTarget};

mod blob;
pub use blob::BlobEvidence;

mod evidence;
pub use evidence::Evidence;

mod glamsterdam;
pub use glamsterdam::{run, scenario};

mod header;
pub use header::AuthenticatedHeader;

mod rpc;
pub use rpc::Rpc;

mod submissions;
pub use submissions::Submissions;

mod transfer;
pub use transfer::{Transfer, TransferRequest};
