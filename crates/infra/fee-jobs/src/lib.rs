#![doc = include_str!("../README.md")]

mod cli;
pub use cli::{
    Cli, ConfirmArgs, DisburseArgs, FeeCommand, JobArgs, LogArgs, ProcessArgs, SweepArgs,
    TxManagerCli,
};

mod confirm;
pub use confirm::{ConfirmJob, ConfirmOutcome, ConfirmReport, ConfirmTiming, ConfirmWindow};

mod contracts;
pub use contracts::{
    BalanceTracker, CrossDomainMessenger, FeeDisburser, RegistrarController, StandardBridge,
};

mod client;
pub use client::AlloyFeeReader;

mod job;
pub use job::{FeeJob, FeeJobError, FeeJobKind, FeeJobOutcome, FeeReader, FeeSender};
