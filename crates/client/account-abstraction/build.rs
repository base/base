//! Build script for EntryPoint simulation bytecode
//!
//! This script handles the EntryPoint/EntryPointSimulations bytecode for ERC-4337 gas estimation.
//!
//! ## Structure
//!
//! ```text
//! contracts/
//! ├── solidity/       # Solidity sources (v0_6/, v0_7/, v0_8/)
//! └── bytecode/       # Extracted deployed bytecode (.hex files)
//! ```
//!
//! ## How it works
//!
//! 1. Checks for pre-extracted bytecode in `contracts/bytecode/`
//! 2. If not found, tries to extract from forge artifacts in `contracts/solidity/*/out/`
//! 3. If still not found and forge is available, compiles from source
//! 4. Generates Rust constants for the bytecode
//!
//! ## Version differences
//!
//! - v0.6: `simulateHandleOp` is in EntryPoint.sol itself
//! - v0.7+: `simulateHandleOp` is in a separate EntryPointSimulations.sol

use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;

fn main() {
    println!("cargo:rerun-if-changed=contracts/bytecode");
    println!("cargo:rerun-if-changed=contracts/solidity");
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Get bytecode for each version
    // v0.6: Uses EntryPointSimulationsV06.sol (custom simulation contract that returns instead of reverts)
    let v06_bytecode = get_bytecode(
        &manifest_dir,
        "v0_6",
        "v0.6",
        "EntryPointSimulationsV06.sol",
        "EntryPointSimulationsV06",
    );
    
    // v0.7+: Uses EntryPointSimulations.sol (separate contract)
    let v07_bytecode = get_bytecode(
        &manifest_dir,
        "v0_7",
        "v0.7",
        "EntryPointSimulations.sol",
        "EntryPointSimulations",
    );
    let v08_bytecode = get_bytecode(
        &manifest_dir,
        "v0_8",
        "v0.8",
        "EntryPointSimulations.sol",
        "EntryPointSimulations",
    );
    let v09_bytecode = get_bytecode(
        &manifest_dir,
        "v0_9",
        "v0.9",
        "EntryPointSimulations.sol",
        "EntryPointSimulations",
    );

    // Generate the Rust module
    generate_bytecode_module(
        &out_dir,
        v06_bytecode.as_deref(),
        v07_bytecode.as_deref(),
        v08_bytecode.as_deref(),
        v09_bytecode.as_deref(),
    );
}

/// Get bytecode for a specific version
///
/// Tries in order:
/// 1. Pre-extracted hex file in contracts/bytecode/
/// 2. Extract from forge artifacts in contracts/solidity/*/out/
/// 3. Compile from source (if forge available)
///
/// For v0.6, also patches the SenderCreator immutable address.
fn get_bytecode(
    manifest_dir: &Path,
    version_dir: &str,
    version_name: &str,
    contract_sol: &str,
    contract_name: &str,
) -> Option<String> {
    let bytecode_dir = manifest_dir.join("contracts").join("bytecode");
    let solidity_dir = manifest_dir.join("contracts").join("solidity").join(version_dir);
    let forge_out_dir = solidity_dir.join("out");
    let is_v06 = version_dir == "v0_6";

    // 1. Check for pre-extracted bytecode
    let hex_file = bytecode_dir.join(format!("{}_{}.hex", contract_name, version_name));
    if let Some(bytecode) = read_hex_file(&hex_file) {
        if !bytecode.is_empty() {
            // For v0.6, patch the SenderCreator address
            let bytecode = if is_v06 {
                patch_v06_sender_creator(&bytecode)
            } else {
                bytecode
            };
            return Some(bytecode);
        }
    }

    // 2. Try to extract from forge artifacts
    let json_file = forge_out_dir.join(contract_sol).join(format!("{}.json", contract_name));

    if json_file.exists() {
        if let Some(bytecode) = extract_deployed_bytecode(&json_file) {
            // For v0.6, patch the SenderCreator address before saving
            let bytecode = if is_v06 {
                patch_v06_sender_creator(&bytecode)
            } else {
                bytecode
            };
            // Save patched bytecode for next time
            let _ = fs::create_dir_all(&bytecode_dir);
            let _ = fs::write(&hex_file, &bytecode);
            return Some(bytecode);
        }
    }

    // 3. Try to compile from source
    if solidity_dir.exists() && is_forge_available() {
        if compile_contracts(&solidity_dir).is_ok() {
            // Try to extract again
            if let Some(bytecode) = extract_deployed_bytecode(&json_file) {
                // For v0.6, patch the SenderCreator address before saving
                let bytecode = if is_v06 {
                    patch_v06_sender_creator(&bytecode)
                } else {
                    bytecode
                };
                let _ = fs::create_dir_all(&bytecode_dir);
                let _ = fs::write(&hex_file, &bytecode);
                return Some(bytecode);
            }
        }
    }

    eprintln!(
        "cargo:warning={} bytecode for {} not available. \
         Add to contracts/bytecode/{}_{}.hex or compile with forge.",
        contract_name, version_name, contract_name, version_name
    );
    None
}

/// Read bytecode from a .hex file
fn read_hex_file(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().strip_prefix("0x").unwrap_or(s.trim()).to_string())
        .filter(|s| !s.is_empty())
}

/// Extract deployedBytecode from a forge JSON artifact
fn extract_deployed_bytecode(json_path: &Path) -> Option<String> {
    let content = fs::read_to_string(json_path).ok()?;
    let json: Value = serde_json::from_str(&content).ok()?;

    let bytecode = json
        .get("deployedBytecode")?
        .get("object")?
        .as_str()?
        .strip_prefix("0x")
        .unwrap_or(json.get("deployedBytecode")?.get("object")?.as_str()?)
        .to_string();

    if bytecode.is_empty() {
        None
    } else {
        Some(bytecode)
    }
}

/// Patch v0.6 simulation bytecode with the correct SenderCreator address
/// 
/// The simulation contract has `SenderCreator private immutable senderCreator = new SenderCreator();`
/// which creates a SenderCreator in the constructor. The address is embedded in the bytecode as an immutable.
/// 
/// Forge artifacts don't fill in immutable values, so we need to patch them manually.
/// Since the real EntryPoint v0.6 is deployed at 0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789
/// and creates SenderCreator with nonce 1, the SenderCreator address is deterministic:
/// 0x7fc98430eAEdbb6070B35B39D798725049088348
/// 
/// Immutable reference locations (from forge artifact):
/// - offset 5755 (32 bytes)
/// - offset 15187 (32 bytes)
fn patch_v06_sender_creator(bytecode: &str) -> String {
    // SenderCreator address padded to 32 bytes (left-padded with zeros)
    // Address: 0x7fc98430eAEdbb6070B35B39D798725049088348
    const SENDER_CREATOR_PADDED: &str = "0000000000000000000000007fc98430eaedbb6070b35b39d798725049088348";
    
    // Immutable reference offsets (in bytes, from forge artifact)
    const OFFSET_1: usize = 5755;
    const OFFSET_2: usize = 15187;
    
    let mut patched = bytecode.to_string();
    
    // Convert byte offsets to hex character offsets (2 hex chars per byte)
    let hex_offset_1 = OFFSET_1 * 2;
    let hex_offset_2 = OFFSET_2 * 2;
    
    // Patch the first location
    if patched.len() >= hex_offset_1 + 64 {
        patched.replace_range(hex_offset_1..hex_offset_1 + 64, SENDER_CREATOR_PADDED);
        eprintln!("cargo:warning=Patched v0.6 SenderCreator at offset {}", OFFSET_1);
    }
    
    // Patch the second location
    if patched.len() >= hex_offset_2 + 64 {
        patched.replace_range(hex_offset_2..hex_offset_2 + 64, SENDER_CREATOR_PADDED);
        eprintln!("cargo:warning=Patched v0.6 SenderCreator at offset {}", OFFSET_2);
    }
    
    patched
}

/// Check if forge is available
fn is_forge_available() -> bool {
    Command::new("forge")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Compile contracts using forge
fn compile_contracts(solidity_dir: &Path) -> Result<(), String> {
    let output = Command::new("forge")
        .args(["build", "--root", solidity_dir.to_str().unwrap()])
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

/// Generate the Rust bytecode module
fn generate_bytecode_module(
    out_dir: &Path,
    v06: Option<&str>,
    v07: Option<&str>,
    v08: Option<&str>,
    v09: Option<&str>,
) {
    let dest_path = out_dir.join("entrypoint_bytecode.rs");
    let mut file = fs::File::create(&dest_path).expect("Failed to create bytecode file");

    writeln!(file, "// Auto-generated by build.rs - do not edit").unwrap();
    writeln!(file, "//").unwrap();
    writeln!(file, "// EntryPoint/EntryPointSimulations deployed bytecode for state overrides").unwrap();
    writeln!(file, "// Source: https://github.com/eth-infinitism/account-abstraction").unwrap();
    writeln!(file).unwrap();

    // v0.6 - EntryPointSimulationsV06 (returns instead of reverts)
    write_bytecode_const(
        &mut file,
        "ENTRYPOINT_V06_SIMULATIONS_DEPLOYED_BYTECODE",
        "v0.6 EntryPointSimulationsV06 (returns instead of reverts)",
        v06,
        "EntryPointSimulationsV06_v0.6.hex",
    );
    writeln!(file).unwrap();

    // v0.7 - EntryPointSimulations
    write_bytecode_const(
        &mut file,
        "ENTRYPOINT_V07_SIMULATIONS_DEPLOYED_BYTECODE",
        "v0.7 EntryPointSimulations",
        v07,
        "EntryPointSimulations_v0.7.hex",
    );
    writeln!(file).unwrap();

    // v0.8 - EntryPointSimulations
    write_bytecode_const(
        &mut file,
        "ENTRYPOINT_V08_SIMULATIONS_DEPLOYED_BYTECODE",
        "v0.8 EntryPointSimulations",
        v08,
        "EntryPointSimulations_v0.8.hex",
    );
    writeln!(file).unwrap();

    // v0.9 - EntryPointSimulations
    write_bytecode_const(
        &mut file,
        "ENTRYPOINT_V09_SIMULATIONS_DEPLOYED_BYTECODE",
        "v0.9 EntryPointSimulations",
        v09,
        "EntryPointSimulations_v0.9.hex",
    );
    writeln!(file).unwrap();

    // Helper functions
    writeln!(file, "/// Check if v0.6 simulation bytecode is available").unwrap();
    writeln!(file, "#[inline]").unwrap();
    writeln!(file, "pub const fn has_v06_bytecode() -> bool {{").unwrap();
    writeln!(file, "    !ENTRYPOINT_V06_SIMULATIONS_DEPLOYED_BYTECODE.is_empty()").unwrap();
    writeln!(file, "}}").unwrap();
    writeln!(file).unwrap();

    writeln!(file, "/// Check if v0.7 simulation bytecode is available").unwrap();
    writeln!(file, "#[inline]").unwrap();
    writeln!(file, "pub const fn has_v07_bytecode() -> bool {{").unwrap();
    writeln!(file, "    !ENTRYPOINT_V07_SIMULATIONS_DEPLOYED_BYTECODE.is_empty()").unwrap();
    writeln!(file, "}}").unwrap();
    writeln!(file).unwrap();

    writeln!(file, "/// Check if v0.8 simulation bytecode is available").unwrap();
    writeln!(file, "#[inline]").unwrap();
    writeln!(file, "pub const fn has_v08_bytecode() -> bool {{").unwrap();
    writeln!(file, "    !ENTRYPOINT_V08_SIMULATIONS_DEPLOYED_BYTECODE.is_empty()").unwrap();
    writeln!(file, "}}").unwrap();
    writeln!(file).unwrap();

    writeln!(file, "/// Check if v0.9 simulation bytecode is available").unwrap();
    writeln!(file, "#[inline]").unwrap();
    writeln!(file, "pub const fn has_v09_bytecode() -> bool {{").unwrap();
    writeln!(file, "    !ENTRYPOINT_V09_SIMULATIONS_DEPLOYED_BYTECODE.is_empty()").unwrap();
    writeln!(file, "}}").unwrap();
}

fn write_bytecode_const(
    file: &mut fs::File,
    name: &str,
    description: &str,
    bytecode: Option<&str>,
    hex_file: &str,
) {
    match bytecode {
        Some(bc) if !bc.is_empty() => {
            writeln!(file, "/// {} deployed bytecode", description).unwrap();
            writeln!(file, "pub const {}: &str = \"{}\";", name, bc).unwrap();
        }
        _ => {
            writeln!(file, "/// {} bytecode (not available)", description).unwrap();
            writeln!(file, "/// Add to contracts/bytecode/{}", hex_file).unwrap();
            writeln!(file, "pub const {}: &str = \"\";", name).unwrap();
        }
    }
}
