mod transaction;
use alloy::rpc::types::eth::FilterChanges as EthFilterChanges;
use crate::op::transaction::Transaction;

pub type FilterChanges = EthFilterChanges<Transaction>;