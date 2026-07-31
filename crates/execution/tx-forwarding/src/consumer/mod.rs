//! Transaction pool consumer for one forwarding destination.

mod config;
pub(super) use config::ConsumerConfig;

mod metrics;

mod validator;

mod task;
pub(super) use task::DestinationConsumer;
