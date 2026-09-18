//! Lighthouse beacon genesis and mnemonic-derived validator keystores.

use std::path::Path;

use alloy_consensus::Header;
use alloy_primitives::{B256, U256};
use eyre::{Result, eyre};
use lighthouse_bls::SignatureBytes;
use lighthouse_keystore::{KeystoreBuilder, keypair_from_secret};
use lighthouse_state::{common::DepositDataTree, initialize_beacon_state_from_eth1};
use lighthouse_types::{
    ChainSpec, Config, Deposit, DepositData, ExecutionPayloadHeader, ExecutionPayloadHeaderFulu,
    MinimalEthSpec, Transactions, Withdrawals,
};
use lighthouse_wallet::{
    KeyType,
    bip39::{Language, Mnemonic, Seed},
    recover_validator_secret_from_mnemonic,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use ssz::Encode;
use tree_hash::TreeHash;

use crate::{GenesisConfig, GenesisOutput};

/// Consensus genesis generation using the same mnemonic as the existing devnet.
#[derive(Debug)]
pub struct BeaconGenesis;

impl BeaconGenesis {
    /// Public development mnemonic shared by execution accounts and the validator.
    pub const MNEMONIC: &str = "test test test test test test test test test test test junk";

    /// Generate Fulu state and Lighthouse-compatible validator files.
    pub fn generate(config: &GenesisConfig, header: &Header, output: &Path) -> Result<()> {
        let yaml = include_str!("../assets/beacon.yaml")
            .replace("${GENESIS_TIME}", &config.timestamp.unwrap_or_default().to_string())
            .replace("${CHAIN_ID}", &config.l1_chain_id.to_string())
            .replace("${SLOT_DURATION}", &config.slot_duration.to_string());
        let mut values =
            serde_json::to_value(Config::from_chain_spec::<MinimalEthSpec>(&ChainSpec::minimal()))?;
        let overrides: Value = yaml_serde::from_str(&yaml)?;
        for (key, value) in overrides.as_object().ok_or_else(|| eyre!("invalid beacon template"))? {
            values[key] = if value.is_number() && values[key].is_string() {
                json!(value.to_string())
            } else {
                value.clone()
            };
        }
        values["SLOT_DURATION_MS"] = json!(
            config
                .slot_duration
                .checked_mul(1000)
                .ok_or_else(|| eyre!("slot duration overflow"))?
        );
        values["BLOB_SCHEDULE"] = json!([{"EPOCH": 0, "MAX_BLOBS_PER_BLOCK": 21}]);
        let network: Config = serde_json::from_value(values)?;
        let spec = network
            .apply_to_chain_spec::<MinimalEthSpec>(&ChainSpec::minimal())
            .ok_or_else(|| eyre!("incompatible beacon preset"))?;
        GenesisOutput::write(
            output.join("cl/config.yaml"),
            yaml_serde::to_string(&network)?.as_bytes(),
        )?;

        let mnemonic =
            Mnemonic::from_phrase(Self::MNEMONIC, Language::English).map_err(|e| eyre!("{e:?}"))?;
        let seed = Seed::new(&mnemonic, "");
        let mut deposits = Vec::new();
        let mut tree = DepositDataTree::create(&[], 0, 32);
        for index in 0..config.validator_count.get() {
            let (voting, path) =
                recover_validator_secret_from_mnemonic(seed.as_bytes(), index, KeyType::Voting)
                    .map_err(|e| eyre!("{e:?}"))?;
            let (withdrawal, _) =
                recover_validator_secret_from_mnemonic(seed.as_bytes(), index, KeyType::Withdrawal)
                    .map_err(|e| eyre!("{e:?}"))?;
            let voting = keypair_from_secret(voting.as_bytes()).map_err(|e| eyre!("{e:?}"))?;
            let withdrawal =
                keypair_from_secret(withdrawal.as_bytes()).map_err(|e| eyre!("{e:?}"))?;
            let mut credentials: [u8; 32] = Sha256::digest(withdrawal.pk.serialize()).into();
            credentials[0] = 0;
            let mut data = DepositData {
                pubkey: voting.pk.clone().into(),
                withdrawal_credentials: B256::from(credentials),
                amount: 32_000_000_000,
                signature: SignatureBytes::empty(),
            };
            data.signature = data.create_signature(&voting.sk, &spec);
            // Genesis applies deposits sequentially, so each proof covers the tree prefix.
            tree.push_leaf(data.tree_hash_root()).map_err(|e| eyre!("{e:?}"))?;
            let (_, proof) = tree.generate_proof(index as usize).map_err(|e| eyre!("{e:?}"))?;
            deposits.push(Deposit { data, proof: proof.try_into().map_err(|e| eyre!("{e:?}"))? });

            let password = B256::random().to_string();
            let keystore = KeystoreBuilder::new(&voting, password.as_bytes(), path.to_string())
                .map_err(|e| eyre!("{e:?}"))?
                .build()
                .map_err(|e| eyre!("{e:?}"))?;
            let public_key = format!("0x{}", keystore.pubkey());
            let json = keystore.to_json_string().map_err(|e| eyre!("{e:?}"))?;
            for directory in ["cl/validator_data/validators", "cl/validator_keys/keys"] {
                GenesisOutput::write_secret(
                    output.join(directory).join(&public_key).join("voting-keystore.json"),
                    json.as_bytes(),
                )?;
            }
            for directory in ["cl/validator_data/secrets", "cl/validator_keys/secrets"] {
                GenesisOutput::write_secret(
                    output.join(directory).join(&public_key),
                    password.as_bytes(),
                )?;
            }
        }
        let payload = ExecutionPayloadHeaderFulu::<MinimalEthSpec> {
            parent_hash: header.parent_hash.into(),
            fee_recipient: header.beneficiary,
            state_root: header.state_root,
            receipts_root: header.receipts_root,
            logs_bloom: header
                .logs_bloom
                .as_slice()
                .to_vec()
                .try_into()
                .map_err(|e| eyre!("{e:?}"))?,
            prev_randao: header.mix_hash,
            block_number: header.number,
            gas_limit: header.gas_limit,
            gas_used: header.gas_used,
            timestamp: header.timestamp,
            extra_data: header.extra_data.to_vec().try_into().map_err(|e| eyre!("{e:?}"))?,
            base_fee_per_gas: U256::from(header.base_fee_per_gas.unwrap_or_default()),
            block_hash: header.hash_slow().into(),
            transactions_root: Transactions::<MinimalEthSpec>::default().tree_hash_root(),
            withdrawals_root: Withdrawals::<MinimalEthSpec>::default().tree_hash_root(),
            blob_gas_used: header.blob_gas_used.unwrap_or_default(),
            excess_blob_gas: header.excess_blob_gas.unwrap_or_default(),
        };
        let mut state = initialize_beacon_state_from_eth1::<MinimalEthSpec>(
            header.hash_slow(),
            header.timestamp,
            deposits,
            Some(ExecutionPayloadHeader::Fulu(payload)),
            &spec,
        )
        .map_err(|e| eyre!("beacon genesis: {e:?}"))?;
        // Validators are preinstalled, not deposits made to an L1 deposit contract.
        // Match the existing offline generator's Fulu fork metadata and empty deposit contract.
        state.fork_mut().previous_version = spec.electra_fork_version;
        *state.deposit_requests_start_index_mut().map_err(|e| eyre!("{e:?}"))? = 0;
        state.eth1_data_mut().deposit_count = 0;
        state.eth1_data_mut().deposit_root = DepositDataTree::create(&[], 0, 32).root();
        *state.eth1_deposit_index_mut() = 0;
        GenesisOutput::write(output.join("cl/genesis.ssz"), &state.as_ssz_bytes())?;

        for file in ["cl/deploy_block.txt", "cl/deposit_contract_block.txt"] {
            GenesisOutput::write(output.join(file), b"0\n")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use clap::{Args, Command, FromArgMatches};
    use lighthouse_keystore::Keystore;
    use lighthouse_types::{BeaconState, ChainSpec as BeaconChainSpec, Config, MinimalEthSpec};
    use reth_chainspec::ChainSpec;
    use tempfile::tempdir;

    use crate::{BeaconGenesis, ExecutionGenesis, GenesisCommand, GenesisForge};

    #[test]
    fn matches_reference_beacon_state_and_keystore_identity() {
        let matches = GenesisCommand::augment_args(Command::new("genesis"))
            .try_get_matches_from(["genesis", "--timestamp", "1700000000"])
            .unwrap();
        let config = GenesisCommand::from_arg_matches(&matches).unwrap().config.resolve().unwrap();
        let genesis = ChainSpec::from(ExecutionGenesis::l1(&config, BTreeMap::new()).unwrap());
        let directory = tempdir().unwrap();
        BeaconGenesis::generate(&config, genesis.genesis_header(), directory.path()).unwrap();
        assert_eq!(
            GenesisForge::digest(&fs::read(directory.path().join("cl/genesis.ssz")).unwrap()),
            "41dcec51a3774fe7bbb60b49502bbd6f75898bd17d814041d770893716c8a56c"
        );
        let validators = directory.path().join("cl/validator_data/validators");
        let validator = fs::read_dir(validators).unwrap().next().unwrap().unwrap();
        let keystore =
            Keystore::from_json_file(validator.path().join("voting-keystore.json")).unwrap();
        let password = fs::read(
            directory.path().join("cl/validator_data/secrets").join(validator.file_name()),
        )
        .unwrap();
        let keypair = keystore.decrypt_keypair(&password).unwrap();
        assert_eq!(format!("0x{}", keystore.pubkey()), validator.file_name().to_str().unwrap());
        assert_eq!(keystore.public_key().unwrap(), keypair.pk);
    }

    #[test]
    fn multiple_validators_have_matching_active_state_and_keystores() {
        let matches = GenesisCommand::augment_args(Command::new("genesis"))
            .try_get_matches_from([
                "genesis",
                "--timestamp",
                "1700000000",
                "--validator-count",
                "3",
            ])
            .unwrap();
        let config = GenesisCommand::from_arg_matches(&matches).unwrap().config.resolve().unwrap();
        let genesis = ChainSpec::from(ExecutionGenesis::l1(&config, BTreeMap::new()).unwrap());
        let directory = tempdir().unwrap();
        BeaconGenesis::generate(&config, genesis.genesis_header(), directory.path()).unwrap();
        let network: Config =
            yaml_serde::from_slice(&fs::read(directory.path().join("cl/config.yaml")).unwrap())
                .unwrap();
        let spec =
            network.apply_to_chain_spec::<MinimalEthSpec>(&BeaconChainSpec::minimal()).unwrap();
        let state = BeaconState::<MinimalEthSpec>::from_ssz_bytes(
            &fs::read(directory.path().join("cl/genesis.ssz")).unwrap(),
            &spec,
        )
        .unwrap();
        assert_eq!(state.validators().len(), 3);
        for path in [
            "cl/validator_data/validators",
            "cl/validator_keys/keys",
            "cl/validator_data/secrets",
            "cl/validator_keys/secrets",
        ] {
            assert_eq!(fs::read_dir(directory.path().join(path)).unwrap().count(), 3);
        }
        for (index, validator) in state.validators().iter().enumerate() {
            assert!(validator.is_active_at(state.current_epoch()));
            assert_eq!(validator.effective_balance, 32_000_000_000);
            let public_key = format!("{}", validator.pubkey);
            let validator_path = directory
                .path()
                .join("cl/validator_data/validators")
                .join(&public_key)
                .join("voting-keystore.json");
            let keystore = Keystore::from_json_file(&validator_path).unwrap();
            let secret_path = directory.path().join("cl/validator_data/secrets").join(&public_key);
            let password = fs::read(&secret_path).unwrap();
            let keypair = keystore.decrypt_keypair(&password).unwrap();
            assert_eq!(keypair.pk.serialize(), validator.pubkey.serialize());
            assert_eq!(keystore.path().unwrap(), format!("m/12381/3600/{index}/0/0"));
            assert_eq!(
                fs::read(validator_path).unwrap(),
                fs::read(
                    directory
                        .path()
                        .join("cl/validator_keys/keys")
                        .join(&public_key)
                        .join("voting-keystore.json")
                )
                .unwrap()
            );
            assert_eq!(
                password,
                fs::read(directory.path().join("cl/validator_keys/secrets").join(&public_key))
                    .unwrap()
            );
        }
    }
}
