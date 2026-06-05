use eyre::Result;
use serde_json::{Value, json};

use crate::{
    ActivationState, CalldataOutput, FeatureCatalog, FeatureStatus, NetworkStatus, OutputFormat,
    PrecompileInfo, PrecompileLocation, StatusReport,
};

/// Writes activator command output.
#[derive(Debug, Clone, Copy)]
pub struct OutputWriter;

impl OutputWriter {
    /// Writes the Beryl precompile inventory.
    pub fn write_inventory(format: OutputFormat, items: &[PrecompileInfo]) -> Result<()> {
        match format {
            OutputFormat::Table => Self::write_inventory_table(items),
            OutputFormat::Json => Self::write_json(Self::inventory_json(items)),
        }
    }

    /// Writes encoded activation registry calldata.
    pub fn write_calldata(format: OutputFormat, output: &CalldataOutput) -> Result<()> {
        match format {
            OutputFormat::Table => Self::write_calldata_table(output),
            OutputFormat::Json => Self::write_json(Self::calldata_json(output)),
        }
    }

    /// Writes activation status for all queried networks.
    pub fn write_status(format: OutputFormat, report: &StatusReport) -> Result<()> {
        match format {
            OutputFormat::Table => Self::write_status_table(report),
            OutputFormat::Json => Self::write_json(Self::status_json(report)),
        }
    }

    /// Writes JSON to stdout.
    pub fn write_json(value: Value) -> Result<()> {
        println!("{}", serde_json::to_string_pretty(&value)?);
        Ok(())
    }

    /// Writes the inventory in table form.
    pub fn write_inventory_table(items: &[PrecompileInfo]) -> Result<()> {
        println!("{:<20} {:<50} {:<10} {:<18} note", "name", "location", "installed", "feature");
        for item in items {
            println!(
                "{:<20} {:<50} {:<10} {:<18} {}",
                item.name,
                Self::location_string(item.location),
                Self::upgrade_string(item.installed_at),
                item.activation_feature.map_or("-", |feature| feature.label()),
                item.note
            );
        }
        Ok(())
    }

    /// Writes encoded calldata in table form.
    pub fn write_calldata_table(output: &CalldataOutput) -> Result<()> {
        println!("action:      {}", output.action.method());
        println!("feature:     {}", output.feature.label());
        println!("feature_id:  {}", output.feature_id);
        println!("to:          {}", Self::address_string(output.to));
        println!("data:        {}", Self::bytes_string(output.data.as_ref()));
        Ok(())
    }

    /// Writes activation status in table form.
    pub fn write_status_table(report: &StatusReport) -> Result<()> {
        println!(
            "{:<15} {:<10} {:<15} {:<10} {:<18} state",
            "network", "chain_id", "rpc", "beryl", "feature"
        );
        for network in &report.networks {
            if let Some(error) = &network.error {
                println!(
                    "{:<15} {:<10} {:<15} {:<10} {:<18} {}",
                    network.network,
                    Self::chain_id_string(network),
                    Self::rpc_source_string(network),
                    Self::beryl_string(network),
                    "-",
                    Self::state_string(&ActivationState::Error(error.clone()))
                );
                continue;
            }

            for feature in &network.features {
                println!(
                    "{:<15} {:<10} {:<15} {:<10} {:<18} {}",
                    network.network,
                    Self::chain_id_string(network),
                    Self::rpc_source_string(network),
                    Self::beryl_string(network),
                    feature.feature.label(),
                    Self::state_string(&feature.state)
                );
            }
        }
        Ok(())
    }

    /// Builds JSON for the inventory output.
    pub fn inventory_json(items: &[PrecompileInfo]) -> Value {
        json!({
            "precompiles": items.iter().map(Self::precompile_json).collect::<Vec<_>>(),
            "features": FeatureCatalog::features().iter().map(Self::feature_json).collect::<Vec<_>>(),
        })
    }

    /// Builds JSON for encoded calldata output.
    pub fn calldata_json(output: &CalldataOutput) -> Value {
        json!({
            "action": output.action.method(),
            "feature": output.feature.label(),
            "feature_id": output.feature_id.to_string(),
            "to": Self::address_string(output.to),
            "data": Self::bytes_string(output.data.as_ref()),
        })
    }

    /// Builds JSON for status output.
    pub fn status_json(report: &StatusReport) -> Value {
        json!({
            "networks": report.networks.iter().map(Self::network_json).collect::<Vec<_>>(),
        })
    }

    /// Converts one precompile inventory entry to JSON.
    pub fn precompile_json(item: &PrecompileInfo) -> Value {
        json!({
            "name": item.name,
            "location": Self::location_string(item.location),
            "installed_at": Self::upgrade_string(item.installed_at),
            "activation_feature": item.activation_feature.map(|feature| feature.label()),
            "note": item.note,
        })
    }

    /// Converts one activation feature to JSON.
    pub fn feature_json(feature: &crate::FeatureInfo) -> Value {
        json!({
            "feature": feature.feature.label(),
            "id": feature.id.to_string(),
        })
    }

    /// Converts one network status to JSON.
    pub fn network_json(network: &NetworkStatus) -> Value {
        json!({
            "network": network.network,
            "expected_chain_id": network.expected_chain_id,
            "chain_id": network.chain_id,
            "chain_id_matches": network.chain_id.map(|chain_id| chain_id == network.expected_chain_id),
            "beryl_timestamp": network.beryl_timestamp,
            "rpc_source": network.rpc_source.map(crate::RpcSource::label),
            "error": network.error,
            "features": network.features.iter().map(Self::feature_status_json).collect::<Vec<_>>(),
        })
    }

    /// Converts one feature status to JSON.
    pub fn feature_status_json(status: &FeatureStatus) -> Value {
        json!({
            "feature": status.feature.label(),
            "feature_id": status.feature_id.to_string(),
            "state": Self::state_string(&status.state),
        })
    }

    /// Converts a precompile location to a display string.
    pub fn location_string(location: PrecompileLocation) -> String {
        match location {
            PrecompileLocation::Address(address) => Self::address_string(address),
            PrecompileLocation::AddressPrefix(prefix) => {
                format!("prefix:{}", Self::bytes_string(&prefix))
            }
        }
    }

    /// Converts bytes to a `0x`-prefixed hex string.
    pub fn bytes_string(bytes: &[u8]) -> String {
        format!("0x{}", hex::encode(bytes))
    }

    /// Converts an address to a lowercase `0x`-prefixed hex string.
    pub fn address_string(address: alloy_primitives::Address) -> String {
        format!("{address:#x}")
    }

    /// Converts a Base upgrade to a display string.
    pub fn upgrade_string(upgrade: base_common_chains::BaseUpgrade) -> String {
        format!("{upgrade:?}")
    }

    /// Converts an optional chain ID to a display string.
    pub fn chain_id_string(network: &NetworkStatus) -> String {
        match network.chain_id {
            Some(chain_id) if chain_id == network.expected_chain_id => chain_id.to_string(),
            Some(chain_id) => format!("{chain_id}!"),
            None => "-".to_owned(),
        }
    }

    /// Converts an optional Beryl timestamp to a display string.
    pub fn beryl_string(network: &NetworkStatus) -> String {
        network.beryl_timestamp.map_or_else(|| "none".to_owned(), |ts| ts.to_string())
    }

    /// Converts an optional RPC source to a display string.
    pub fn rpc_source_string(network: &NetworkStatus) -> &'static str {
        network.rpc_source.map_or("-", crate::RpcSource::label)
    }

    /// Converts an activation state to a display string.
    pub fn state_string(state: &ActivationState) -> String {
        match state {
            ActivationState::Active => "active".to_owned(),
            ActivationState::Inactive => "inactive".to_owned(),
            ActivationState::Unavailable => "unavailable".to_owned(),
            ActivationState::Error(error) => format!("error: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use base_common_precompiles::ActivationRegistryStorage;

    use super::*;
    use crate::{CalldataAction, CalldataEncoder, FeatureName, PrecompileCatalog};

    #[test]
    fn inventory_json_contains_exported_precompile_rows() {
        let json = OutputWriter::inventory_json(&PrecompileCatalog::beryl());

        let precompiles = json["precompiles"].as_array().expect("precompile array");
        assert!(precompiles.iter().any(|item| item["name"] == "activation-registry"));
        assert!(precompiles.iter().any(|item| item["name"] == "b20-factory"));
    }

    #[test]
    fn calldata_json_includes_to_and_data() {
        let output = CalldataEncoder::encode(CalldataAction::Activate, FeatureName::B20Stablecoin);
        let json = OutputWriter::calldata_json(&output);

        assert_eq!(json["feature"], "b20-stablecoin");
        assert_eq!(json["to"], OutputWriter::address_string(ActivationRegistryStorage::ADDRESS));
        assert!(json["data"].as_str().expect("data string").starts_with("0x"));
    }
}
