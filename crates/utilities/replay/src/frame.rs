use serde::{Deserialize, Serialize};

/// Lossless physical JSONL envelope; the embedded JSON text retains raw params.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReplayFrame {
    /// Version 2 on write; absent only when reading legacy unframed records.
    pub version: Option<u8>,
    /// Complete logical record serialized as a JSON string, not a nested object.
    #[serde(default)]
    pub record: String,
}
