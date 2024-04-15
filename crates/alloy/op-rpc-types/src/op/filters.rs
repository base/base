use crate::op::Transaction;
use alloy::rpc::types::eth::Log as RpcLog;
use alloy_primitives::B256;
use serde::{Deserialize, Deserializer, Serialize};

/// Response of the `eth_getFilterChanges` RPC.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum FilterChanges {
    /// Empty result.
    #[serde(with = "empty_array")]
    Empty,
    /// New logs.
    Logs(Vec<RpcLog>),
    /// New hashes (block or transactions).
    Hashes(Vec<B256>),
    /// New transactions.
    Transactions(Vec<Transaction>),
}
mod empty_array {
    use serde::{Serialize, Serializer};

    pub(super) fn serialize<S>(s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        (&[] as &[()]).serialize(s)
    }
}
impl<'de> Deserialize<'de> for FilterChanges {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Changes {
            Hashes(Vec<B256>),
            Logs(Vec<RpcLog>),
            Transactions(Vec<Transaction>),
        }

        let changes = Changes::deserialize(deserializer)?;
        let changes = match changes {
            Changes::Logs(vals) => {
                if vals.is_empty() {
                    FilterChanges::Empty
                } else {
                    FilterChanges::Logs(vals)
                }
            }
            Changes::Hashes(vals) => {
                if vals.is_empty() {
                    FilterChanges::Empty
                } else {
                    FilterChanges::Hashes(vals)
                }
            }
            Changes::Transactions(vals) => {
                if vals.is_empty() {
                    FilterChanges::Empty
                } else {
                    FilterChanges::Transactions(vals)
                }
            }
        };
        Ok(changes)
    }
}
