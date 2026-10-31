//! Top-level command implementations for the unified Base binary.

mod bootnode;
mod command;
pub(crate) use command::BaseCommand;
mod consensus;
mod dev;
mod execution;
mod follow;
mod replay;
mod reth;
mod rpc;
mod sequencer;
mod snapshot;
mod update;
