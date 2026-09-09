use std::{fs, path::PathBuf};

/// A helper to parse a [`Genesis`](alloy_genesis::Genesis) as argument or from disk.
pub fn parse_genesis(s: &str) -> eyre::Result<alloy_genesis::Genesis> {
    // try to read json from path first
    let raw = match fs::read_to_string(PathBuf::from(s)) {
        Ok(raw) => raw,
        Err(io_err) => {
            // valid json may start with "\n", but must contain "{"
            if s.contains('{') {
                s.to_string()
            } else {
                return Err(io_err.into()); // assume invalid path
            }
        }
    };

    Ok(serde_json::from_str(&raw)?)
}
