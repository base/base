//! Top-level command implementations for the unified Base binary.

mod bootnode;
mod command;
mod integrated_upgrade_signal;
pub(crate) use command::BaseCommand;
mod rpc;
mod sequencer;
mod update;
