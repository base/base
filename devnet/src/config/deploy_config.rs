//! L2 deploy configuration for base-deployer.

use alloy_primitives::Address;

use super::accounts;

/// Generates the L2 deploy configuration as JSON.
///
/// NOTE: This produces the same JSON structure as
/// `etc/scripts/devnet/templates/deploy-config.json.template`.
/// If the schema changes, both must be updated in lockstep.
///
/// The output matches the field schema expected by `base/contracts` forge scripts
/// (i.e. `DeployConfig.s.sol` / `deploy-config/local.json`).
pub fn deploy_config_json(l1_chain_id: u64, l2_chain_id: u64) -> String {
    let deployer = format_address(accounts::DEPLOYER.address);
    let sequencer = format_address(accounts::SEQUENCER.address);
    let batcher = format_address(accounts::BATCHER.address);
    let proposer = format_address(accounts::PROPOSER.address);
    let challenger = format_address(accounts::CHALLENGER.address);

    format!(
        r#"{{
  "baseFeeVaultMinimumWithdrawalAmount": "0x8ac7230489e80000",
  "baseFeeVaultRecipient": "{deployer}",
  "baseFeeVaultWithdrawalNetwork": 0,
  "batchSenderAddress": "{batcher}",
  "disputeGameFinalityDelaySeconds": 302400,
  "delayedWETHWithdrawalDelay": 302400,
  "eip1559Denominator": 50,
  "eip1559DenominatorCanyon": 250,
  "eip1559Elasticity": 6,
  "finalSystemOwner": "{deployer}",
  "fundDevAccounts": true,
  "gasPriceOracleBaseFeeScalar": 1368,
  "gasPriceOracleBlobBaseFeeScalar": 810949,
  "l1ChainId": {l1_chain_id},
  "l1FeeVaultMinimumWithdrawalAmount": "0x8ac7230489e80000",
  "l1FeeVaultRecipient": "{deployer}",
  "l1FeeVaultWithdrawalNetwork": 0,
  "l2BlockTime": 2,
  "l2ChainId": {l2_chain_id},
  "l2GenesisBlockGasLimit": "0x3938700",
  "l2OutputOracleChallenger": "{challenger}",
  "l2OutputOracleProposer": "{proposer}",
  "l2OutputOracleStartingBlockNumber": 1,
  "l2OutputOracleStartingTimestamp": 1,
  "multiproofBlockInterval": 100,
  "multiproofConfigHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "multiproofGameType": 621,
  "multiproofGenesisBlockNumber": 0,
  "multiproofGenesisOutputRoot": "0x0000000000000000000000000000000000000000000000000000000000000001",
  "multiproofIntermediateBlockInterval": 10,
  "nitroEnclaveVerifier": "0x0000000000000000000000000000000000000000",
  "operatorFeeVaultMinimumWithdrawalAmount": "0x8ac7230489e80000",
  "operatorFeeVaultRecipient": "{deployer}",
  "operatorFeeVaultWithdrawalNetwork": 0,
  "p2pSequencerAddress": "{sequencer}",
  "proofMaturityDelaySeconds": 604800,
  "proxyAdminOwner": "{deployer}",
  "respectedGameType": 621,
  "sequencerFeeVaultMinimumWithdrawalAmount": "0x8ac7230489e80000",
  "sequencerFeeVaultRecipient": "{deployer}",
  "sequencerFeeVaultWithdrawalNetwork": 0,
  "sp1Verifier": "0x0000000000000000000000000000000000000000",
  "superchainConfigGuardian": "{deployer}",
  "superchainConfigIncidentResponder": "0x0000000000000000000000000000000000000000",
  "teeChallenger": "{challenger}",
  "teeImageHash": "0x0000000000000000000000000000000000000000000000000000000000000001",
  "teeProposer": "{deployer}",
  "zkAggregationHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "zkRangeHash": "0x0000000000000000000000000000000000000000000000000000000000000000"
}}"#,
    )
}

fn format_address(address: Address) -> String {
    format!("{address:#x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deploy_config_json_valid() {
        let json_str = deploy_config_json(1337, 84538453);
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("deploy config should be valid JSON");

        let obj = parsed.as_object().expect("deploy config should be a JSON object");

        // Verify numeric chain IDs are numbers, not strings.
        assert_eq!(obj["l1ChainId"], 1337);
        assert_eq!(obj["l2ChainId"], 84538453);

        // Verify key role addresses are present and non-empty.
        assert!(obj["finalSystemOwner"].as_str().unwrap().starts_with("0x"));
        assert!(obj["batchSenderAddress"].as_str().unwrap().starts_with("0x"));
        assert!(obj["p2pSequencerAddress"].as_str().unwrap().starts_with("0x"));
        assert!(obj["l2OutputOracleProposer"].as_str().unwrap().starts_with("0x"));
        assert!(obj["l2OutputOracleChallenger"].as_str().unwrap().starts_with("0x"));

        // Verify expected field count matches the template.
        assert!(obj.len() >= 40, "deploy config should have at least 40 fields");
    }
}
