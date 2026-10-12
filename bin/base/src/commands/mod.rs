//! Top-level command implementations for the unified Base binary.

mod bootnode;
mod command;
pub(crate) use command::BaseCommand;
mod dev;
mod follow;
mod reth;
mod rpc;
mod sequencer;
mod snapshot;
mod update;
