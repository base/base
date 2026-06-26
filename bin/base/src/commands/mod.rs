//! Top-level command implementations for the unified Base binary.

mod bootnode;
mod command;
pub(crate) use command::BaseCommand;
mod node;
mod rpc;
mod sequencer;
mod update;
