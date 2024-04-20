mod transaction;
use crate::op::transaction::Transaction;
use alloy::rpc::types::eth::FilterChanges as EthFilterChanges;

pub type FilterChanges = EthFilterChanges<Transaction>;
