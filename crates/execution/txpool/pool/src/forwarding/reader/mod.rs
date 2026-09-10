//! Transaction pool reader for one forwarding destination.

mod config;
pub(super) use config::ReaderConfig;

mod metrics;

mod validator;

mod task;
pub(super) use task::DestinationReader;
