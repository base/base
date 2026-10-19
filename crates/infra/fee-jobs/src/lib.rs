#![doc = include_str!("../README.md")]

mod cli;
pub use cli::{
    Cli, ConfirmArgs, DisburseArgs, FeeCommand, JobArgs, LogArgs, ProcessArgs, SweepArgs,
    TxManagerCli,
};

mod confirm;
pub use confirm::{ConfirmJob, ConfirmWindow, FinalizeRevert, PortalErrors};

mod contracts;
pub use contracts::{BalanceTracker, FeeDisburser, RegistrarController};

mod client;
pub use client::AlloyFeeReader;

mod job;
pub use job::{FeeJob, FeeJobError, FeeJobKind, FeeJobOutcome, FeeReader, FeeSender};
