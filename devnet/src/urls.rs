//! `DevnetUrls` type for managing RPC endpoints.

use std::{fmt, path::Path};

use eyre::{Result, WrapErr};

/// RPC URLs for a devnet instance.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DevnetUrls {
    /// L1 RPC endpoint
    pub l1_rpc: String,
    /// L2 builder RPC endpoint
    pub l2_builder_rpc: String,
    /// L2 client RPC endpoint
    pub l2_client_rpc: String,
    /// L2 builder OP node RPC endpoint
    pub l2_builder_op_rpc: String,
    /// L2 client OP node RPC endpoint
    pub l2_client_op_rpc: String,
}

impl fmt::Display for DevnetUrls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f)?;
        writeln!(f, "RPC URLs:")?;
        writeln!(f, "  L1:            {}", self.l1_rpc)?;
        writeln!(f, "  L2 Builder:    {}", self.l2_builder_rpc)?;
        writeln!(f, "  L2 Client:     {}", self.l2_client_rpc)?;
        writeln!(f, "  L2 Builder CL: {}", self.l2_builder_op_rpc)?;
        write!(f, "  L2 Client CL:  {}", self.l2_client_op_rpc)
    }
}

impl DevnetUrls {
    /// Read `DevnetUrls` from a JSON file.
    pub fn read_from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).wrap_err("Failed to read urls.json")?;
        serde_json::from_str(&content).wrap_err("Failed to parse urls.json")
    }

    /// Write `DevnetUrls` to a JSON file.
    pub fn write_to_file(&self, path: &Path) -> Result<()> {
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content).wrap_err("Failed to write urls.json")
    }
}
