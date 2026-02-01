//! L2 intent configuration for op-deployer.

use alloy_primitives::Address;

use super::accounts;

/// Generates the L2 intent configuration for op-deployer as TOML.
pub fn l2_intent_toml(l1_chain_id: u64, l2_chain_id: u64) -> String {
    let l2_chain_id_hex = format!("{l2_chain_id:#x}");
    let deployer = format_address(accounts::DEPLOYER.address);
    let sequencer = format_address(accounts::SEQUENCER.address);
    let batcher = format_address(accounts::BATCHER.address);
    let proposer = format_address(accounts::PROPOSER.address);
    let challenger = format_address(accounts::CHALLENGER.address);

    format!(
        r#"configType = "custom"
l1ChainID = {l1_chain_id}
fundDevAccounts = true
l1ContractsLocator = "embedded"
l2ContractsLocator = "embedded"

[superchainRoles]
  SuperchainProxyAdminOwner = "{deployer}"
  SuperchainGuardian = "{deployer}"
  ProtocolVersionsOwner = "{deployer}"
  Challenger = "{challenger}"

[[chains]]
  id = "{l2_chain_id_hex}"
  baseFeeVaultRecipient = "{deployer}"
  l1FeeVaultRecipient = "{deployer}"
  sequencerFeeVaultRecipient = "{deployer}"
  operatorFeeVaultRecipient = "{deployer}"
  eip1559DenominatorCanyon = 250
  eip1559Denominator = 50
  eip1559Elasticity = 6
  gasLimit = 60000000
  operatorFeeScalar = 0
  operatorFeeConstant = 0
  chainFeesRecipient = "{deployer}"
  minBaseFee = 1000000000
  daFootprintGasScalar = 0
  [chains.roles]
    l1ProxyAdminOwner = "{deployer}"
    l2ProxyAdminOwner = "{deployer}"
    systemConfigOwner = "{deployer}"
    unsafeBlockSigner = "{sequencer}"
    batcher = "{batcher}"
    proposer = "{proposer}"
    challenger = "{challenger}"
"#,
        batcher = batcher,
        challenger = challenger,
        deployer = deployer,
        l1_chain_id = l1_chain_id,
        l2_chain_id_hex = l2_chain_id_hex,
        proposer = proposer,
        sequencer = sequencer,
    )
}

fn format_address(address: Address) -> String {
    format!("{address:#x}")
}
