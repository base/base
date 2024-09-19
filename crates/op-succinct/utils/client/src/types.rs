use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

use crate::boot::BootInfoStruct;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregationInputs {
    pub boot_infos: Vec<BootInfoStruct>,
    pub latest_l1_checkpoint_head: B256,
}
