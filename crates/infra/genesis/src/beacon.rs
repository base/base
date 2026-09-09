use std::{fmt, sync::Arc};

use alloy_consensus::Header;
use coins_bip39::{English, Mnemonic};
use eth2_key_derivation::DerivedKey;
use eth2_keystore::{
    Keystore, KeystoreBuilder,
    json_keystore::{Kdf, Pbkdf2, Prf},
    keypair_from_secret,
};
use eyre::{Result, eyre};
use lighthouse_state_processing::{
    common::DepositDataTree,
    upgrade::{
        electra::upgrade_state_to_electra, upgrade_to_altair, upgrade_to_bellatrix,
        upgrade_to_capella, upgrade_to_deneb, upgrade_to_fulu,
    },
};
use lighthouse_types::{
    BeaconState, ChainSpec, Config, Epoch, Eth1Data, EthSpec, ExecutionPayloadFulu,
    ExecutionPayloadHeaderFulu, MinimalEthSpec, Validator,
};
use rand::Rng;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use ssz::Encode;

/// In-process beacon genesis and one encrypted development validator.
pub struct BeaconGenesis {
    /// Beacon configuration.
    pub config: String,
    /// Fulu SSZ state.
    pub ssz: Vec<u8>,
    /// Encrypted EIP-2335 keystore.
    pub keystore: Keystore,
    /// Random keystore password, deliberately omitted from diagnostic formatting.
    pub password: eth2_key_derivation::PlainText,
}

impl fmt::Debug for BeaconGenesis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BeaconGenesis")
            .field("validator", &self.keystore.pubkey())
            .finish_non_exhaustive()
    }
}

impl BeaconGenesis {
    /// Public, insecure development mnemonic; never use these keys on a real network.
    pub const MNEMONIC: &str = "test test test test test test test test test test test junk";

    /// Applies the rendered YAML configuration to Lighthouse's minimal preset.
    pub fn chain_spec(config: &str, slot_duration: u64) -> Result<ChainSpec> {
        let default_spec = MinimalEthSpec::default_spec();
        let mut settings =
            serde_json::to_value(Config::from_chain_spec::<MinimalEthSpec>(&default_spec))?;
        for (key, value) in serde_yaml::from_str::<serde_json::Map<String, Value>>(config)? {
            settings[key] = value;
        }
        settings["SLOT_DURATION_MS"] =
            json!(slot_duration.checked_mul(1000).ok_or_else(|| eyre!("slot duration overflow"))?);
        serde_json::from_value::<Config>(settings)?
            .apply_to_chain_spec::<MinimalEthSpec>(&default_spec)
            .ok_or_else(|| eyre!("invalid minimal beacon configuration"))
    }

    /// Builds the minimal, one-validator Fulu genesis from the final execution header.
    pub fn generate(header: &Header, chain_id: u64, slot_duration: u64) -> Result<Self> {
        let config = include_str!("../assets/l1-cl-config.yaml.template")
            .replace("${CHAIN_ID}", &chain_id.to_string())
            .replace("${GENESIS_TIME}", &header.timestamp.to_string())
            .replace("${SLOT_DURATION}", &slot_duration.to_string());
        let spec = Self::chain_spec(&config, slot_duration)?;
        let seed = Mnemonic::<English>::new_from_phrase(Self::MNEMONIC)?.to_seed(None)?;
        let withdrawal = [12381, 3600, 0, 0]
            .into_iter()
            .fold(DerivedKey::from_seed(&seed).map_err(|e| eyre!("{e:?}"))?, |key, index| {
                key.child(index)
            });
        let key: lighthouse_bls::Keypair =
            keypair_from_secret(withdrawal.child(0).secret()).map_err(|e| eyre!("{e:?}"))?;
        let withdrawal_key =
            keypair_from_secret(withdrawal.secret()).map_err(|e| eyre!("{e:?}"))?;
        let mut credentials: [u8; 32] = Sha256::digest(withdrawal_key.pk.as_ssz_bytes()).into();
        credentials[0] = 0;
        let hash = header.hash_slow();
        let mut state = BeaconState::<MinimalEthSpec>::new(
            header.timestamp,
            Eth1Data {
                deposit_root: DepositDataTree::create(&[], 0, 32).root(),
                deposit_count: 0,
                block_hash: hash,
            },
            &spec,
        );
        state
            .validators_mut()
            .push(Validator {
                pubkey: key.pk.clone().into(),
                withdrawal_credentials: credentials.into(),
                effective_balance: 32_000_000_000,
                slashed: false,
                activation_eligibility_epoch: Epoch::new(0),
                activation_epoch: Epoch::new(0),
                exit_epoch: Epoch::new(u64::MAX),
                withdrawable_epoch: Epoch::new(u64::MAX),
            })
            .map_err(|e| eyre!("{e:?}"))?;
        state.balances_mut().push(32_000_000_000).map_err(|e| eyre!("{e:?}"))?;
        state.fill_randao_mixes_with(hash).map_err(|e| eyre!("{e:?}"))?;
        upgrade_to_altair(&mut state, &spec).map_err(|e| eyre!("{e:?}"))?;
        upgrade_to_bellatrix(&mut state, &spec).map_err(|e| eyre!("{e:?}"))?;
        upgrade_to_capella(&mut state, &spec).map_err(|e| eyre!("{e:?}"))?;
        upgrade_to_deneb(&mut state, &spec).map_err(|e| eyre!("{e:?}"))?;
        state = upgrade_state_to_electra(&mut state, Epoch::new(0), Epoch::new(0), &spec)
            .map_err(|e| eyre!("{e:?}"))?;
        // Match the existing preloaded-validator generator, which has no deposit requests.
        *state.deposit_requests_start_index_mut().map_err(|e| eyre!("{e:?}"))? = 0;
        let committee = Arc::new(state.get_next_sync_committee(&spec).map_err(|e| eyre!("{e:?}"))?);
        *state.current_sync_committee_mut().map_err(|e| eyre!("{e:?}"))? = Arc::clone(&committee);
        *state.next_sync_committee_mut().map_err(|e| eyre!("{e:?}"))? = committee;
        upgrade_to_fulu(&mut state, &spec).map_err(|e| eyre!("{e:?}"))?;
        *state.genesis_validators_root_mut() =
            state.update_validators_tree_hash_cache().map_err(|e| eyre!("{e:?}"))?;
        let empty_payload = ExecutionPayloadFulu::<MinimalEthSpec>::default();
        let mut payload = ExecutionPayloadHeaderFulu::from(&empty_payload);
        payload.parent_hash = header.parent_hash.into();
        payload.fee_recipient = header.beneficiary;
        payload.state_root = header.state_root;
        payload.receipts_root = header.receipts_root;
        payload.logs_bloom =
            header.logs_bloom.as_slice().to_vec().try_into().map_err(|e| eyre!("{e:?}"))?;
        payload.prev_randao = header.mix_hash;
        payload.block_number = header.number;
        payload.gas_limit = header.gas_limit;
        payload.gas_used = header.gas_used;
        payload.timestamp = header.timestamp;
        payload.extra_data = header.extra_data.to_vec().try_into().map_err(|e| eyre!("{e:?}"))?;
        payload.base_fee_per_gas =
            alloy_primitives::U256::from(header.base_fee_per_gas.unwrap_or_default());
        payload.block_hash = hash.into();
        payload.blob_gas_used = header.blob_gas_used.unwrap_or_default();
        payload.excess_blob_gas = header.excess_blob_gas.unwrap_or_default();
        *state.latest_execution_payload_header_fulu_mut().map_err(|e| eyre!("{e:?}"))? = payload;
        let password = alloy_primitives::hex::encode(rand::rng().random::<[u8; 32]>()).into_bytes();
        let keystore = KeystoreBuilder::new(&key, &password, "m/12381/3600/0/0/0".to_owned())
            .map_err(|e| eyre!("{e:?}"))?
            .kdf(Kdf::Pbkdf2(Pbkdf2 {
                c: 262144,
                dklen: 32,
                prf: Prf::HmacSha256,
                salt: rand::rng().random::<[u8; 32]>().to_vec().into(),
            }))
            .build()
            .map_err(|e| eyre!("{e:?}"))?;
        Ok(Self { config, ssz: state.as_ssz_bytes(), keystore, password: password.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_both_yaml_styles_for_blob_schedules() -> Result<()> {
        for schedule in [
            "BLOB_SCHEDULE: [{EPOCH: 0, MAX_BLOBS_PER_BLOCK: 21}]",
            "BLOB_SCHEDULE:\n  - EPOCH: 0\n    MAX_BLOBS_PER_BLOCK: 21",
        ] {
            let spec = BeaconGenesis::chain_spec(
                &format!("SECONDS_PER_SLOT: 12\nFULU_FORK_EPOCH: 0\n{schedule}"),
                12,
            )?;
            assert_eq!(spec.max_blobs_per_block(Epoch::new(0)), 21);
        }
        Ok(())
    }

    #[test]
    fn beacon_state_is_deterministic_and_binds_the_execution_header() -> Result<()> {
        let header =
            Header { timestamp: 1_800_000_000, gas_limit: 60_000_000, ..Default::default() };
        let first = BeaconGenesis::generate(&header, 1337, 12)?;
        let spec = BeaconGenesis::chain_spec(&first.config, 12)?;
        let el: Value = serde_json::from_str(
            &include_str!("../assets/l1-el-genesis.json.template")
                .replace("${CHAIN_ID}", "1337")
                .replace("${GENESIS_TIME_HEX}", "0x6b49d200")
                .replace("${BALANCE}", "0x0"),
        )?;
        assert_eq!(el["config"]["bpo2Time"], 0);
        assert_eq!(
            spec.max_blobs_per_block(Epoch::new(0)),
            el["config"]["blobSchedule"]["bpo2"]["max"].as_u64().unwrap()
        );
        let second = BeaconGenesis::generate(&header, 1337, 12)?;
        assert_eq!(first.ssz, second.ssz);
        assert_ne!(first.password.as_bytes(), second.password.as_bytes());
        let mut changed = header;
        changed.state_root = alloy_primitives::B256::repeat_byte(1);
        assert_ne!(first.ssz, BeaconGenesis::generate(&changed, 1337, 12)?.ssz);
        let key = first
            .keystore
            .decrypt_keypair(first.password.as_bytes())
            .map_err(|e| eyre!("{e:?}"))?;
        assert_eq!(key.pk.as_hex_string(), format!("0x{}", first.keystore.pubkey()));
        Ok(())
    }
}
