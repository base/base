use std::{collections::BTreeMap, fs, path::Path};

use alloy_dyn_abi::{DynSolError, DynSolValue, ErrorExt, FunctionExt, JsonAbiExt, Specifier};
use alloy_genesis::GenesisAccount;
use alloy_json_abi::{JsonAbi, Param};
use alloy_primitives::{Address, B256, Bytes, keccak256};
use eyre::{Result, WrapErr, ensure, eyre};
use serde::Deserialize;
use serde_json::Value;

/// Standard compiler ABI and bytecode, without compiler tooling at runtime.
#[derive(Debug, Deserialize)]
pub struct ContractArtifact {
    /// Contract ABI.
    pub abi: JsonAbi,
    /// Creation bytecode.
    pub bytecode: Value,
    /// Runtime bytecode.
    #[serde(rename = "deployedBytecode")]
    pub deployed_bytecode: Value,
}

impl ContractArtifact {
    /// Reads linked creation or runtime code.
    pub fn code(&self, runtime: bool) -> Result<Bytes> {
        let value = if runtime { &self.deployed_bytecode } else { &self.bytecode };
        value["object"]
            .as_str()
            .ok_or_else(|| eyre!("missing bytecode"))?
            .parse()
            .map_err(Into::into)
    }

    /// Coerces explicit initialization values using the contract's actual ABI.
    pub fn arguments(params: &[Param], values: &Value) -> Result<Vec<DynSolValue>> {
        let values = values.as_array().ok_or_else(|| eyre!("expected ABI argument array"))?;
        ensure!(
            params.len() == values.len(),
            "contract ABI changed: expected {} arguments, supplied {}",
            params.len(),
            values.len()
        );
        params
            .iter()
            .zip(values)
            .map(|(param, value)| {
                param
                    .resolve()?
                    .coerce_json(value)
                    .wrap_err_with(|| format!("ABI argument {}", param.name))
            })
            .collect()
    }

    /// Builds creation code with ABI-encoded constructor arguments.
    pub fn constructor(&self, values: &Value) -> Result<Bytes> {
        let mut code = self.code(false)?.to_vec();
        if let Some(constructor) = &self.abi.constructor {
            code.extend(
                constructor.abi_encode_input(&Self::arguments(&constructor.inputs, values)?)?,
            );
        } else {
            ensure!(
                values.as_array().is_some_and(Vec::is_empty),
                "contract has no constructor arguments"
            );
        }
        Ok(code.into())
    }

    /// Encodes a contract call, failing when the pinned interface no longer matches.
    pub fn encode(&self, name: &str, values: &Value) -> Result<Bytes> {
        let function = self
            .abi
            .functions
            .get(name)
            .and_then(|list| {
                list.iter().find(|f| Some(f.inputs.len()) == values.as_array().map(Vec::len))
            })
            .ok_or_else(|| eyre!("missing or changed contract method {name}"))?;
        Ok(function.abi_encode_input(&Self::arguments(&function.inputs, values)?)?.into())
    }

    /// Decodes a zero-argument view call.
    pub fn decode(&self, name: &str, data: &[u8]) -> Result<Vec<DynSolValue>> {
        let function = self
            .abi
            .functions
            .get(name)
            .and_then(|functions| functions.first())
            .ok_or_else(|| eyre!("missing contract view {name}"))?;
        Ok(function.abi_decode_output(data)?)
    }

    /// Describes a standard Solidity revert or a custom error from the pinned ABI.
    pub fn revert_reason(&self, data: &[u8]) -> String {
        if let Ok(error) = DynSolError::revert().decode_error(data)
            && let [DynSolValue::String(reason)] = error.body.as_slice()
        {
            return reason.clone();
        }
        if let Ok(error) = DynSolError::panic().decode_error(data) {
            return format!("Solidity panic: {:?}", error.body);
        }
        for error in self.abi.errors.values().flatten() {
            if let Ok(decoded) = error.decode_error(data) {
                return format!("{}: {:?}", error.name, decoded.body);
            }
        }
        format!("unrecognized revert data: {}", alloy_primitives::hex::encode_prefixed(data))
    }
}

/// Built Base contract artifacts and exported preinstall constants.
#[derive(Debug)]
pub struct ContractArtifacts {
    /// Concrete contracts indexed by name.
    pub contracts: BTreeMap<String, ContractArtifact>,
    /// Preinstalled third-party contracts, using the Base Permit2 template.
    pub preinstalls: BTreeMap<Address, GenesisAccount>,
    /// Base's named predeploy addresses.
    pub predeploys: BTreeMap<String, Address>,
    /// Fingerprint of the revision and all artifact contents for output reuse.
    pub fingerprint: B256,
}

impl ContractArtifacts {
    /// Loads an already-built directory. Never downloads or invokes a compiler.
    pub fn load(path: &Path) -> Result<Self> {
        let revision = fs::read(path.join("revision.txt")).wrap_err_with(|| format!(
            "missing contract artifacts in {}; run `just contracts` or set BASE_GENESIS_ARTIFACTS to an exported directory",
            path.display()
        ))?;
        let mut files = fs::read_dir(path)?
            .map(|entry| entry.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        files.sort();
        let mut fingerprint = revision;
        let mut contracts = BTreeMap::new();
        for file in files.iter().filter(|p| p.extension().is_some_and(|e| e == "json")) {
            let bytes = fs::read(file)?;
            let name = file
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| eyre!("invalid artifact filename"))?;
            fingerprint.extend_from_slice(name.as_bytes());
            fingerprint.extend_from_slice(keccak256(&bytes).as_slice());
            if !["preinstalls", "predeploys"].contains(&name) {
                contracts.insert(
                    name.to_owned(),
                    serde_json::from_slice(&bytes)
                        .wrap_err_with(|| format!("invalid artifact {name}"))?,
                );
            }
        }
        Ok(Self {
            contracts,
            fingerprint: keccak256(fingerprint),
            preinstalls: serde_json::from_slice(&fs::read(path.join("preinstalls.json"))?)?,
            predeploys: serde_json::from_slice(&fs::read(path.join("predeploys.json"))?)?,
        })
    }

    /// Gets a required contract, not a deployment script.
    pub fn get(&self, name: &str) -> Result<&ContractArtifact> {
        self.contracts
            .get(name)
            .ok_or_else(|| eyre!("missing Base contract {name}; rebuild contract artifacts"))
    }

    /// Resolves a named Base predeploy from the pinned constants.
    pub fn predeploy(&self, name: &str) -> Result<Address> {
        self.predeploys.get(name).copied().ok_or_else(|| {
            eyre!("missing Base predeploy {name}; rebuild or update contract artifacts")
        })
    }

    /// Resolves a required preinstalled account from the pinned constants.
    pub fn preinstall(&self, address: Address) -> Result<&GenesisAccount> {
        self.preinstalls.get(&address).ok_or_else(|| {
            eyre!("missing Base preinstall {address}; rebuild or update contract artifacts")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_views_and_solidity_errors_are_descriptive() -> Result<()> {
        let artifact: ContractArtifact = serde_json::from_value(serde_json::json!({
            "abi": [{"type":"error","name":"InvalidSchedule","inputs":[]}],
            "bytecode": {"object":"0x"}, "deployedBytecode": {"object":"0x"}
        }))?;
        assert!(
            artifact
                .decode("missing", &[])
                .unwrap_err()
                .to_string()
                .contains("missing contract view")
        );
        assert!(
            artifact
                .revert_reason(&keccak256("InvalidSchedule()")[..4])
                .contains("InvalidSchedule")
        );
        let mut revert = keccak256("Error(string)")[..4].to_vec();
        revert.extend(
            DynSolValue::Tuple(vec![DynSolValue::String("already active".into())])
                .abi_encode_params(),
        );
        assert_eq!(artifact.revert_reason(&revert), "already active");
        assert!(artifact.revert_reason(&[0xab]).contains("0xab"));
        Ok(())
    }
}
