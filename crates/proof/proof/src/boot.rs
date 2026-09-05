//! This module contains the prologue phase of the client program, pulling in the boot information
//! through the `PreimageOracle` ABI as local keys.

use alloc::vec::Vec;

use alloy_genesis::ChainConfig;
use alloy_primitives::{Address, B256, U256, uint};
use base_common_genesis::{BaseUpgrade, RollupConfig};
use base_proof_preimage::{PreimageKey, PreimageOracleClient, errors::PreimageOracleError};
use serde::{Deserialize, Serialize};

use crate::{ScheduleId, errors::OracleProviderError};

/// The local key identifier for the L1 head hash.
///
/// This key is used to retrieve the L1 block hash that contains all the data
/// necessary to derive the disputed L2 blocks. The L1 head serves as the
/// starting point for L1 data extraction during the derivation process.
pub const L1_HEAD_KEY: U256 = uint!(1_U256);

/// The local key identifier for the agreed L2 output root.
///
/// This key retrieves the baseline L2 output root that both parties agree upon.
/// It represents the last known good state before the disputed blocks and serves
/// as the starting point for derivation verification.
pub const L2_OUTPUT_ROOT_KEY: U256 = uint!(2_U256);

/// The local key identifier for the disputed L2 output root claim.
///
/// This key retrieves the user's claimed L2 output root at the target block.
/// The fault proof will compare the derived output root against this claim
/// to determine if the claim is valid or invalid.
pub const L2_CLAIM_KEY: U256 = uint!(3_U256);

/// The local key identifier for the disputed L2 block number.
///
/// This key retrieves the L2 block number at which the output root disagreement
/// occurs. The derivation process will produce blocks up to this number to
/// verify the claim.
pub const L2_CLAIM_BLOCK_NUMBER_KEY: U256 = uint!(4_U256);

/// The local key identifier for the L2 chain ID.
///
/// This key retrieves the L2 network identifier, which is used to load the
/// appropriate rollup configuration and ensure network-specific validation
/// rules are applied correctly.
pub const L2_CHAIN_ID_KEY: U256 = uint!(5_U256);

/// The local key identifier for the L2 rollup configuration.
///
/// This key retrieves the rollup configuration served by the L2 node. For fixed built-in chains,
/// only its contract-backed upgrade activation timestamps are used; all static derivation
/// parameters come from the compiled chain configuration. The mutable local devnet uses the full
/// node-served configuration.
pub const L2_ROLLUP_CONFIG_KEY: U256 = uint!(6_U256);

/// The local key identifier for the L1 chain configuration.
///
/// This key is used as a fallback to retrieve the chain configuration from
/// the preimage oracle when no hardcoded configuration is available for the
/// given chain ID. Oracle-loaded configs require additional validation.
pub const L1_CONFIG_KEY: U256 = uint!(7_U256);

/// The local key identifier for the proposer address.
///
/// This key retrieves the address of the proposer that will submit the proof
/// transaction on-chain. The enclave includes this address in the proof journal
/// so on-chain verification can match it against the actual `msg.sender`.
pub const PROPOSER_KEY: U256 = uint!(8_U256);

/// The local key identifier for the intermediate block interval.
///
/// This key retrieves the number of L2 blocks between intermediate output root
/// checkpoints. The enclave uses this to sample the correct intermediate roots
/// when constructing the aggregate proof journal, matching the on-chain
/// `AggregateVerifier`'s `INTERMEDIATE_BLOCK_INTERVAL`.
pub const INTERMEDIATE_BLOCK_INTERVAL_KEY: U256 = uint!(9_U256);

/// The local key identifier for the L1 head block number.
///
/// This key retrieves the block number corresponding to `L1_HEAD_KEY`, allowing
/// the enclave to reference the L1 head number without an extra lookup.
pub const L1_HEAD_NUMBER_KEY: U256 = uint!(10_U256);

/// L2 block number used to pin the activated upgrade schedule.
pub const L2_SCHEDULE_BLOCK_NUMBER_KEY: U256 = uint!(11_U256);

/// The boot information for the client program.
///
/// [`BootInfo`] contains all the essential parameters needed to initialize the fault proof
/// client program. It separates verified inputs (cryptographically committed) from user
/// inputs (requiring validation through derivation).
///
/// This structure is loaded during the prologue phase from the preimage oracle and
/// establishes the initial state for the fault proof computation.
///
/// # Security Model
/// The boot information follows a two-tier security model:
/// - **Verified inputs**: Committed by the fault proof system, trusted
/// - **User inputs**: Provided by the claimant, must be verified through execution
///
/// # Usage in Fault Proof
/// 1. Load boot info from preimage oracle during prologue
/// 2. Initialize derivation pipeline with verified L1 head and safe L2 output
/// 3. Derive L2 blocks up to the claimed block number
/// 4. Compare derived output root with user's claim
/// 5. Proof succeeds if outputs match, fails otherwise
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootInfo {
    /// The L1 head hash containing safe L2 chain data for reproduction.
    ///
    /// This hash identifies the L1 block that contains all the data necessary
    /// to derive the L2 chain up to the disputed block. It serves as the
    /// starting point for L1 data extraction during derivation.
    ///
    /// **Security**: Verified input committed by the fault proof system.
    pub l1_head: B256,
    /// The agreed upon safe L2 output root.
    ///
    /// This represents the last known good L2 state that both parties agree upon.
    /// It serves as the starting point for derivation and the baseline against
    /// which the disputed claim is evaluated.
    ///
    /// **Security**: Verified input committed by the fault proof system.
    pub agreed_l2_output_root: B256,
    /// The disputed L2 output root claim.
    ///
    /// This is the user's claim about what the L2 output root should be at the
    /// target block number. The fault proof will derive the actual output root
    /// and compare it against this claim to determine validity.
    ///
    /// **Security**: User-submitted input requiring verification.
    pub claimed_l2_output_root: B256,
    /// The L2 block number being disputed.
    ///
    /// This specifies the target L2 block number at which the output root
    /// disagreement occurs. The derivation process will produce blocks up to
    /// this number and compute the resulting output root.
    ///
    /// **Security**: User-submitted input requiring verification.
    pub claimed_l2_block_number: u64,
    /// The L2 chain identifier.
    ///
    /// Used to identify which L2 network this proof applies to and to load
    /// the appropriate rollup configuration. This prevents cross-chain
    /// replay attacks and ensures proper network-specific validation.
    ///
    /// **Security**: Verified input committed by the fault proof system.
    pub chain_id: u64,
    /// The trusted activation registry admin address for Base precompile execution.
    ///
    /// **Security**: Derived from the built-in chain config, not from the oracle-provided rollup
    /// config fallback. This may be `None` only when Beryl is not scheduled; Beryl-enabled configs
    /// without a static admin are rejected during boot loading.
    #[serde(default)]
    pub activation_admin_address: Option<Address>,
    /// The rollup configuration for the L2 chain.
    ///
    /// Contains all the network-specific parameters needed for proper L2 block
    /// derivation, including genesis configuration, system addresses, gas limits,
    /// and upgrade activation heights.
    ///
    /// **Security**: Fixed built-in chains use trusted compiled derivation parameters. Their
    /// oracle-provided contract-backed upgrade timestamps are separately bound by `schedule_id`;
    /// the mutable local devnet and unknown chains use the oracle-provided configuration.
    pub rollup_config: RollupConfig,
    /// An optional configuration for the l1 chain associated with the l2 chain.
    ///
    /// **Security**: Loaded from built-in config (secure) or oracle (requires validation).
    pub l1_config: ChainConfig,
    /// The proposer address that will submit the proof transaction on-chain.
    ///
    /// Included in the proof journal so on-chain verification can match it against
    /// the actual `msg.sender` (gameCreator). Defaults to `Address::ZERO` when not set.
    ///
    /// **Security**: User-submitted input; the on-chain contract validates that the
    /// transaction sender matches this value.
    #[serde(default)]
    pub proposer: Address,
    /// The number of L2 blocks between intermediate output root checkpoints.
    ///
    /// Used by the enclave to sample the correct intermediate roots when
    /// constructing the aggregate proof journal. Defaults to 0 when not set.
    #[serde(default)]
    pub intermediate_block_interval: u64,
    /// The block number of the L1 head.
    ///
    /// Stored alongside `l1_head` so the enclave can reference the L1 head
    /// block number without an extra lookup. Defaults to 0 when not set.
    #[serde(default)]
    pub l1_head_number: u64,
    /// The locally derived schedule ID for this proof attempt.
    #[serde(default)]
    pub schedule_id: B256,
}

impl BootInfo {
    /// Read an optional local preimage by key.
    ///
    /// Returns `Ok(None)` only when the oracle reports the key as absent, which callers may safely
    /// default. Every other oracle failure — timeout, I/O error, closed channel, etc. — is
    /// propagated as [`OracleProviderError::Preimage`] so a genuine operational failure is not
    /// silently treated as a missing optional value.
    ///
    /// Two error variants are treated as "absent" because backends disagree on how they signal a
    /// miss:
    /// - [`PreimageOracleError::KeyNotFound`], returned by the hosted/enclave oracles.
    /// - [`PreimageOracleError::InvalidPreimageKey`], returned by the in-memory zkVM
    ///   `PreimageStore` on a map miss. This is unambiguous for local keys: they are never
    ///   hash-validated (`check_preimage` skips `PreimageKeyType::Local`), so `InvalidPreimageKey`
    ///   on a local key can only mean the key is absent, never corrupt. Tolerating it preserves the
    ///   backwards-compatibility defaults on the ZK path, which would otherwise abort the load.
    pub async fn get_optional_local<O>(
        oracle: &O,
        key: U256,
    ) -> Result<Option<Vec<u8>>, OracleProviderError>
    where
        O: PreimageOracleClient + Send,
    {
        match oracle.get(PreimageKey::new_local(key.to())).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(PreimageOracleError::KeyNotFound | PreimageOracleError::InvalidPreimageKey) => {
                Ok(None)
            }
            Err(e) => Err(OracleProviderError::Preimage(e)),
        }
    }

    /// Load the boot information from the preimage oracle.
    ///
    /// This method retrieves all the necessary boot parameters from the preimage oracle
    /// using predefined local keys. It handles both verified inputs (from the fault proof
    /// system) and user-submitted inputs that need validation.
    ///
    /// # Arguments
    /// * `oracle` - The preimage oracle client for reading boot data
    ///
    /// # Returns
    /// * `Ok(BootInfo)` - Successfully loaded and validated boot information
    /// * `Err(OracleProviderError)` - Failed to load or parse boot information
    ///
    /// # Errors
    /// This method can fail due to:
    /// - **Preimage errors**: Oracle communication failures or missing keys
    /// - **Slice conversion errors**: Invalid data format for numeric values
    /// - **Serde errors**: Failed to deserialize rollup configuration
    /// - **Missing data**: Required boot parameters not available in oracle
    pub async fn load<O>(oracle: &O) -> Result<Self, OracleProviderError>
    where
        O: PreimageOracleClient + Send,
    {
        let mut l1_head: B256 = B256::ZERO;
        oracle
            .get_exact(PreimageKey::new_local(L1_HEAD_KEY.to()), l1_head.as_mut())
            .await
            .map_err(OracleProviderError::Preimage)?;

        let mut l2_output_root: B256 = B256::ZERO;
        oracle
            .get_exact(PreimageKey::new_local(L2_OUTPUT_ROOT_KEY.to()), l2_output_root.as_mut())
            .await
            .map_err(OracleProviderError::Preimage)?;

        let mut l2_claim: B256 = B256::ZERO;
        oracle
            .get_exact(PreimageKey::new_local(L2_CLAIM_KEY.to()), l2_claim.as_mut())
            .await
            .map_err(OracleProviderError::Preimage)?;

        let l2_claim_block = u64::from_be_bytes(
            oracle
                .get(PreimageKey::new_local(L2_CLAIM_BLOCK_NUMBER_KEY.to()))
                .await
                .map_err(OracleProviderError::Preimage)?
                .as_slice()
                .try_into()
                .map_err(OracleProviderError::SliceConversion)?,
        );
        let chain_id = u64::from_be_bytes(
            oracle
                .get(PreimageKey::new_local(L2_CHAIN_ID_KEY.to()))
                .await
                .map_err(OracleProviderError::Preimage)?
                .as_slice()
                .try_into()
                .map_err(OracleProviderError::SliceConversion)?,
        );

        let activation_admin_address =
            base_common_chains::ChainConfig::beryl_activation_admin_address_by_chain_id(chain_id);

        let ser_cfg = oracle
            .get(PreimageKey::new_local(L2_ROLLUP_CONFIG_KEY.to()))
            .await
            .map_err(OracleProviderError::Preimage)?;
        let oracle_rollup_config: RollupConfig =
            serde_json::from_slice(&ser_cfg).map_err(OracleProviderError::Serde)?;

        // Bind the node-served schedule to the committed boot chain ID before using any of it.
        let rollup_config_chain_id = oracle_rollup_config.l2_chain_id.id();
        if chain_id != rollup_config_chain_id {
            return Err(OracleProviderError::RollupConfigChainIdMismatch {
                boot_chain_id: chain_id,
                rollup_config_chain_id,
            });
        }

        // Fixed built-in chains must execute with their compiled static derivation parameters. Only
        // contract-backed activation timestamps may come from the node, because ScheduleId commits
        // them separately. The local devnet is mutable, so its live node-served config is required.
        let trusted_chain_config =
            base_common_chains::ChainConfig::by_chain_id(chain_id).filter(|chain_config| {
                chain_config.chain_id != base_common_chains::ChainConfig::DEVNET.chain_id
            });
        let mut rollup_config = if let Some(chain_config) = trusted_chain_config {
            let mut trusted_rollup_config = chain_config.rollup_config();
            for upgrade in BaseUpgrade::CONTRACT_VARIANTS {
                trusted_rollup_config.apply_upgrade_activation(
                    upgrade,
                    oracle_rollup_config.upgrades.activation(upgrade),
                );
            }
            trusted_rollup_config
        } else {
            warn!(
                target: "boot_loader",
                chain_id,
                "no fixed trusted rollup config available, falling back to preimage oracle; insecure in production without additional validation"
            );
            oracle_rollup_config
        };

        // Attempt to load the L1 config from the rollup config's L1 chain ID. If there is no config
        // for the chain, fall back to loading the config from the preimage oracle.
        let l1_config = if let Some(config) =
            base_common_chains::L1_CONFIGS.get(&rollup_config.l1_chain_id)
        {
            config.clone()
        } else {
            warn!(
                target: "boot_loader",
                chain_id = rollup_config.l1_chain_id,
                "no l1 config found in built-in mapping, falling back to preimage oracle; insecure in production without additional validation"
            );
            let ser_cfg = oracle
                .get(PreimageKey::new_local(L1_CONFIG_KEY.to()))
                .await
                .map_err(OracleProviderError::Preimage)?;

            serde_json::from_slice(&ser_cfg).map_err(OracleProviderError::Serde)?
        };

        debug!(
            target: "boot_loader",
            l1_head = %l1_head,
            chain_id = chain_id,
            claimed_l2_block_number = l2_claim_block,
            "Successfully loaded boot information"
        );

        // Load proposer address (optional — defaults to zero for backwards compatibility).
        let proposer = match Self::get_optional_local(oracle, PROPOSER_KEY).await? {
            Some(bytes) => {
                let buf: [u8; 20] =
                    bytes.as_slice().try_into().map_err(OracleProviderError::SliceConversion)?;
                Address::from(buf)
            }
            None => {
                debug!(
                    target: "boot_loader",
                    "Proposer preimage not found, defaulting to Address::ZERO"
                );
                Address::ZERO
            }
        };

        // Load intermediate block interval (optional — defaults to 0 for backwards compatibility).
        let intermediate_block_interval =
            match Self::get_optional_local(oracle, INTERMEDIATE_BLOCK_INTERVAL_KEY).await? {
                Some(bytes) => u64::from_be_bytes(
                    bytes.as_slice().try_into().map_err(OracleProviderError::SliceConversion)?,
                ),
                None => {
                    debug!(
                        target: "boot_loader",
                        "Intermediate block interval preimage not found, defaulting to 0"
                    );
                    0
                }
            };

        // Load L1 head block number (optional — defaults to 0 for backwards compatibility).
        let l1_head_number = match Self::get_optional_local(oracle, L1_HEAD_NUMBER_KEY).await? {
            Some(bytes) => u64::from_be_bytes(
                bytes.as_slice().try_into().map_err(OracleProviderError::SliceConversion)?,
            ),
            None => {
                debug!(
                    target: "boot_loader",
                    "L1 head number preimage not found, defaulting to 0"
                );
                0
            }
        };

        // Missing or zero values default to the claimed block.
        let schedule_l2_block_number = match Self::get_optional_local(
            oracle,
            L2_SCHEDULE_BLOCK_NUMBER_KEY,
        )
        .await?
        {
            Some(bytes) => {
                let value = u64::from_be_bytes(
                    bytes.as_slice().try_into().map_err(OracleProviderError::SliceConversion)?,
                );
                if value == 0 { l2_claim_block } else { value }
            }
            None => {
                debug!(
                    target: "boot_loader",
                    "Schedule L2 block number preimage not found, defaulting to claimed L2 block number"
                );
                l2_claim_block
            }
        };

        if rollup_config.block_time == 0 {
            return Err(OracleProviderError::InvalidL2BlockTime);
        }
        if rollup_config.genesis.l2_time == 0 {
            return Err(OracleProviderError::InvalidL2GenesisTimestamp);
        }
        if l2_claim_block < rollup_config.genesis.l2.number {
            return Err(OracleProviderError::L2ClaimBeforeGenesis {
                claim_block: l2_claim_block,
                genesis_block: rollup_config.genesis.l2.number,
            });
        }
        if schedule_l2_block_number < l2_claim_block {
            return Err(OracleProviderError::ScheduleBlockBeforeClaim {
                schedule_block: schedule_l2_block_number,
                claim_block: l2_claim_block,
            });
        }

        // The proven range ends at the claimed block, so execution-fork activation must be evaluated
        // against the claim timestamp, not the (possibly later) schedule pin horizon. A game-wide
        // schedule block only fixes a shared schedule ID across subranges; it must never make an
        // upgrade look active for a subrange whose execution never reaches it.
        let l2_schedule_timestamp = rollup_config.l2_block_timestamp(schedule_l2_block_number);
        let l2_claim_timestamp = rollup_config.l2_block_timestamp(l2_claim_block);

        // Zenith is not contract-backed, so reject it when active within the proven range and remove
        // it when it only activates after the claimed block.
        match rollup_config.upgrades.base.zenith {
            Some(zenith_time) if zenith_time <= l2_claim_timestamp => {
                return Err(OracleProviderError::UncommittedZenithUpgrade);
            }
            Some(_) => rollup_config.upgrades.base.zenith = None,
            None => {}
        }

        let schedule_id = ScheduleId::pin(&mut rollup_config, l2_schedule_timestamp);

        // Only a pinned Beryl schedule requires its trusted built-in admin.
        if activation_admin_address.is_none() && rollup_config.upgrades.base.beryl.is_some() {
            return Err(OracleProviderError::MissingActivationAdminAddress { chain_id });
        }

        Ok(Self {
            l1_head,
            agreed_l2_output_root: l2_output_root,
            claimed_l2_output_root: l2_claim,
            claimed_l2_block_number: l2_claim_block,
            chain_id,
            activation_admin_address,
            rollup_config,
            l1_config,
            proposer,
            intermediate_block_interval,
            l1_head_number,
            schedule_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloc::{boxed::Box, vec::Vec};

    use alloy_primitives::B256;
    use async_trait::async_trait;
    use base_common_chains::ChainConfig as BaseChainConfig;
    use base_common_genesis::{BaseUpgradeConfig, UpgradeConfig};
    use base_proof_preimage::{
        PreimageKey, PreimageOracleClient,
        errors::{PreimageOracleError, PreimageOracleResult},
    };

    use super::*;

    const ORACLE_CHAIN_ID: u64 = 999_999_999;

    struct MockOracle {
        data: Vec<(PreimageKey, Vec<u8>)>,
        /// Keys that should surface an operational failure (`Timeout`) instead of `KeyNotFound`.
        timeout_keys: Vec<PreimageKey>,
        /// When set, a missing key surfaces as `InvalidPreimageKey` rather than `KeyNotFound`,
        /// emulating the in-memory zkVM `PreimageStore` instead of the hosted/enclave oracles.
        miss_returns_invalid_key: bool,
    }

    impl MockOracle {
        fn new() -> Self {
            Self { data: Vec::new(), timeout_keys: Vec::new(), miss_returns_invalid_key: false }
        }

        fn insert(&mut self, key: U256, value: Vec<u8>) {
            self.data.push((PreimageKey::new_local(key.to()), value));
        }

        fn fail_with_timeout(&mut self, key: U256) {
            self.timeout_keys.push(PreimageKey::new_local(key.to()));
        }

        /// Emulate the zkVM `PreimageStore`, which returns `InvalidPreimageKey` on a map miss.
        const fn with_zk_store_miss_semantics(mut self) -> Self {
            self.miss_returns_invalid_key = true;
            self
        }

        fn insert_rollup_config(&mut self, chain_id: u64, rollup_config: &RollupConfig) {
            let mut value =
                serde_json::to_value(rollup_config).expect("rollup config should convert to value");
            value["l2_chain_id"] = serde_json::json!(chain_id);
            self.insert(L2_CHAIN_ID_KEY, chain_id.to_be_bytes().to_vec());
            self.insert(
                L2_ROLLUP_CONFIG_KEY,
                serde_json::to_vec(&value).expect("rollup config should serialize"),
            );
        }
    }

    #[async_trait]
    impl PreimageOracleClient for MockOracle {
        async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
            if self.timeout_keys.contains(&key) {
                return Err(PreimageOracleError::Timeout);
            }
            let miss_error = if self.miss_returns_invalid_key {
                PreimageOracleError::InvalidPreimageKey
            } else {
                PreimageOracleError::KeyNotFound
            };
            self.data
                .iter()
                .find_map(|(entry_key, value)| (*entry_key == key).then(|| value.clone()))
                .ok_or(miss_error)
        }

        async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> PreimageOracleResult<()> {
            let value = self.get(key).await?;
            if value.len() != buf.len() {
                return Err(PreimageOracleError::BufferLengthMismatch(buf.len(), value.len()));
            }

            buf.copy_from_slice(&value);
            Ok(())
        }
    }

    #[tokio::test]
    async fn loads_activation_admin_address_from_builtin_chain_id() {
        let chain_config = BaseChainConfig::ZERONET;
        let rollup_config = chain_config.rollup_config();

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, 40_308_263u64.to_be_bytes().to_vec());
        oracle.insert(L2_CHAIN_ID_KEY, chain_config.chain_id.to_be_bytes().to_vec());
        oracle.insert(
            L2_ROLLUP_CONFIG_KEY,
            serde_json::to_vec(&rollup_config).expect("rollup config should serialize"),
        );

        let boot_info = BootInfo::load(&oracle).await.expect("boot info should load");

        assert_eq!(
            boot_info.activation_admin_address,
            Some(base_common_chains::ZERONET_BERYL_ACTIVATION_ADMIN_ADDRESS)
        );
    }

    #[tokio::test]
    async fn uses_trusted_static_rollup_config_for_builtin_chain() {
        const CLAIM_BLOCK: u64 = 40_308_263;

        let chain_config = BaseChainConfig::MAINNET;
        let upgrades = UpgradeConfig {
            canyon_time: Some(123),
            base: BaseUpgradeConfig {
                azul: Some(456),
                beryl: None,
                cobalt: None,
                denim: None,
                zenith: None,
            },
            ..Default::default()
        };
        let mut oracle_rollup_config = chain_config.rollup_config();
        oracle_rollup_config.seq_window_size += 1;
        oracle_rollup_config.channel_timeout = 777;
        oracle_rollup_config.upgrades = upgrades;
        oracle_rollup_config.upgrades.base.zenith = Some(1);

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, CLAIM_BLOCK.to_be_bytes().to_vec());
        oracle.insert_rollup_config(chain_config.chain_id, &oracle_rollup_config);

        let boot_info = BootInfo::load(&oracle).await.expect("boot info should load");

        let mut expected_rollup_config = chain_config.rollup_config();
        expected_rollup_config.upgrades = upgrades;
        let claim_timestamp = expected_rollup_config.genesis.l2_time
            + (CLAIM_BLOCK - expected_rollup_config.genesis.l2.number)
                * expected_rollup_config.block_time;
        let expected_schedule_id = ScheduleId::pin(&mut expected_rollup_config, claim_timestamp);

        assert_eq!(boot_info.rollup_config, expected_rollup_config);
        assert_eq!(boot_info.schedule_id, expected_schedule_id);
    }

    #[tokio::test]
    async fn uses_live_rollup_config_for_local_devnet() {
        const CLAIM_BLOCK: u64 = 100;

        let chain_config = BaseChainConfig::DEVNET;
        let mut rollup_config = chain_config.rollup_config();
        rollup_config.genesis.l2_time = 1_000;
        rollup_config.seq_window_size += 1;

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, CLAIM_BLOCK.to_be_bytes().to_vec());
        oracle.insert_rollup_config(chain_config.chain_id, &rollup_config);
        oracle.insert(
            L1_CONFIG_KEY,
            serde_json::to_vec(
                base_common_chains::L1_CONFIGS
                    .get(&1)
                    .expect("mainnet L1 config should be available"),
            )
            .expect("L1 config should serialize"),
        );

        let boot_info = BootInfo::load(&oracle).await.expect("boot info should load");

        assert_eq!(boot_info.rollup_config.genesis.l2_time, 1_000);
        assert_eq!(boot_info.rollup_config.seq_window_size, rollup_config.seq_window_size);
    }

    #[tokio::test]
    async fn pins_schedule_to_claimed_l2_timestamp() {
        let chain_config = BaseChainConfig::MAINNET;
        let mut rollup_config = chain_config.rollup_config();
        rollup_config.upgrades = UpgradeConfig {
            regolith_time: Some(100),
            canyon_time: Some(500),
            delta_time: Some(200),
            base: BaseUpgradeConfig { azul: Some(u64::MAX), ..Default::default() },
            ..Default::default()
        };

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, 100u64.to_be_bytes().to_vec());
        oracle.insert(L2_CHAIN_ID_KEY, chain_config.chain_id.to_be_bytes().to_vec());
        oracle.insert(
            L2_ROLLUP_CONFIG_KEY,
            serde_json::to_vec(&rollup_config).expect("rollup config should serialize"),
        );

        let boot_info = BootInfo::load(&oracle).await.expect("boot info should load");

        assert_eq!(boot_info.rollup_config.upgrades.regolith_time, Some(100));
        assert_eq!(boot_info.rollup_config.upgrades.canyon_time, Some(500));
        assert_eq!(boot_info.rollup_config.upgrades.delta_time, Some(200));
        assert_eq!(boot_info.rollup_config.upgrades.base.azul, None);
        let expected = ScheduleId::next_link(
            ScheduleId::next_link(ScheduleId::next_link(B256::ZERO, 0, 100), 1, 500),
            2,
            200,
        );
        assert_eq!(boot_info.schedule_id, expected);
    }

    #[tokio::test]
    async fn pins_schedule_to_schedule_block_override() {
        let chain_config = BaseChainConfig::MAINNET;
        let mut rollup_config = chain_config.rollup_config();
        rollup_config.upgrades = UpgradeConfig {
            regolith_time: Some(100),
            canyon_time: Some(350),
            ..Default::default()
        };

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, 100u64.to_be_bytes().to_vec());
        oracle.insert(L2_SCHEDULE_BLOCK_NUMBER_KEY, 200u64.to_be_bytes().to_vec());
        oracle.insert(L2_CHAIN_ID_KEY, chain_config.chain_id.to_be_bytes().to_vec());
        oracle.insert(
            L2_ROLLUP_CONFIG_KEY,
            serde_json::to_vec(&rollup_config).expect("rollup config should serialize"),
        );

        let boot_info = BootInfo::load(&oracle).await.expect("boot info should load");

        assert_eq!(boot_info.rollup_config.upgrades.canyon_time, Some(350));
        let expected = ScheduleId::next_link(ScheduleId::next_link(B256::ZERO, 0, 100), 1, 350);
        assert_eq!(boot_info.schedule_id, expected);
    }

    #[tokio::test]
    async fn treats_explicit_zero_schedule_block_as_unset() {
        let chain_config = BaseChainConfig::MAINNET;
        let rollup_config = chain_config.rollup_config();

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, 100u64.to_be_bytes().to_vec());
        oracle.insert(L2_SCHEDULE_BLOCK_NUMBER_KEY, 0u64.to_be_bytes().to_vec());
        oracle.insert(L2_CHAIN_ID_KEY, chain_config.chain_id.to_be_bytes().to_vec());
        oracle.insert(
            L2_ROLLUP_CONFIG_KEY,
            serde_json::to_vec(&rollup_config).expect("rollup config should serialize"),
        );

        let boot_info = BootInfo::load(&oracle).await.expect("boot info should load");

        assert_eq!(boot_info.claimed_l2_block_number, 100);
    }

    #[tokio::test]
    async fn normalizes_genesis_active_zero_timestamps() {
        const CLAIM_BLOCK: u64 = 100;

        let chain_config = BaseChainConfig::MAINNET;
        let mut rollup_config = chain_config.rollup_config();
        let genesis_timestamp = rollup_config.genesis.l2_time;
        rollup_config.upgrades.regolith_time = Some(0);
        assert_eq!(rollup_config.upgrades.regolith_time, Some(0), "premise: genesis-active");

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, CLAIM_BLOCK.to_be_bytes().to_vec());
        oracle.insert(L2_CHAIN_ID_KEY, chain_config.chain_id.to_be_bytes().to_vec());
        oracle.insert(
            L2_ROLLUP_CONFIG_KEY,
            serde_json::to_vec(&rollup_config).expect("rollup config should serialize"),
        );

        let boot_info = BootInfo::load(&oracle).await.expect("boot info should load");

        assert_eq!(boot_info.rollup_config.upgrades.regolith_time, Some(genesis_timestamp));

        let claim_timestamp = genesis_timestamp
            + (CLAIM_BLOCK - rollup_config.genesis.l2.number) * rollup_config.block_time;
        assert_eq!(
            boot_info.schedule_id,
            ScheduleId::pin(&mut rollup_config.clone(), claim_timestamp)
        );
    }

    #[tokio::test]
    async fn rejects_schedule_block_before_claim_block() {
        let chain_config = BaseChainConfig::MAINNET;
        let rollup_config = chain_config.rollup_config();

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, 100u64.to_be_bytes().to_vec());
        oracle.insert(L2_SCHEDULE_BLOCK_NUMBER_KEY, 99u64.to_be_bytes().to_vec());
        oracle.insert(L2_CHAIN_ID_KEY, chain_config.chain_id.to_be_bytes().to_vec());
        oracle.insert(
            L2_ROLLUP_CONFIG_KEY,
            serde_json::to_vec(&rollup_config).expect("rollup config should serialize"),
        );

        let err =
            BootInfo::load(&oracle).await.expect_err("schedule block before claim should fail");
        assert!(matches!(
            err,
            OracleProviderError::ScheduleBlockBeforeClaim { schedule_block: 99, claim_block: 100 }
        ));
    }

    #[tokio::test]
    async fn rejects_zero_l2_block_time() {
        let chain_config = BaseChainConfig::MAINNET;
        let mut rollup_config = chain_config.rollup_config();
        rollup_config.block_time = 0;

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, 100u64.to_be_bytes().to_vec());
        oracle.insert_rollup_config(ORACLE_CHAIN_ID, &rollup_config);

        let err = BootInfo::load(&oracle).await.expect_err("zero block time should fail");
        assert!(matches!(err, OracleProviderError::InvalidL2BlockTime));
    }

    #[tokio::test]
    async fn rejects_l2_claim_before_genesis() {
        let chain_config = BaseChainConfig::MAINNET;
        let mut rollup_config = chain_config.rollup_config();
        rollup_config.genesis.l2.number = 101;

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, 100u64.to_be_bytes().to_vec());
        oracle.insert_rollup_config(ORACLE_CHAIN_ID, &rollup_config);

        let err = BootInfo::load(&oracle).await.expect_err("pre-genesis claim should fail");
        assert!(matches!(
            err,
            OracleProviderError::L2ClaimBeforeGenesis { claim_block: 100, genesis_block: 101 }
        ));
    }

    #[tokio::test]
    async fn rejects_active_zenith_upgrade() {
        let chain_config = BaseChainConfig::MAINNET;

        for zenith_timestamp in [0, 1_000] {
            let mut rollup_config = chain_config.rollup_config();
            rollup_config.upgrades.base.zenith = Some(zenith_timestamp);

            let mut oracle = MockOracle::new();
            oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
            oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
            oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
            oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, 100u64.to_be_bytes().to_vec());
            oracle.insert_rollup_config(ORACLE_CHAIN_ID, &rollup_config);

            let err = BootInfo::load(&oracle).await.expect_err("active Zenith upgrade should fail");
            assert!(matches!(err, OracleProviderError::UncommittedZenithUpgrade));
        }
    }

    #[tokio::test]
    async fn clears_future_zenith_upgrade() {
        const CLAIM_BLOCK: u64 = 100;

        let chain_config = BaseChainConfig::MAINNET;
        let mut rollup_config = chain_config.rollup_config();
        let schedule_timestamp = rollup_config.genesis.l2_time
            + (CLAIM_BLOCK - rollup_config.genesis.l2.number) * rollup_config.block_time;
        rollup_config.upgrades.base.zenith = Some(schedule_timestamp + 1);

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, CLAIM_BLOCK.to_be_bytes().to_vec());
        oracle.insert_rollup_config(ORACLE_CHAIN_ID, &rollup_config);

        let boot_info = BootInfo::load(&oracle).await.expect("boot info should load");

        assert_eq!(boot_info.rollup_config.upgrades.base.zenith, None);

        let mut expected_config = rollup_config.clone();
        expected_config.upgrades.base.zenith = None;
        assert_eq!(
            boot_info.schedule_id,
            ScheduleId::pin(&mut expected_config, schedule_timestamp)
        );
    }

    #[tokio::test]
    async fn clears_zenith_activating_after_claim_but_before_schedule_horizon() {
        // A game-wide schedule block pins the shared schedule ID to the game's final block, which
        // can sit past a later Zenith activation even when the proven subrange ends before it.
        // Zenith must be evaluated against the claim timestamp, so this pre-Zenith range still loads.
        const CLAIM_BLOCK: u64 = 150;
        const SCHEDULE_BLOCK: u64 = 200;

        let mut rollup_config = BaseChainConfig::MAINNET.rollup_config();
        rollup_config.genesis.l2.number = 0;
        rollup_config.genesis.l2_time = 2;
        rollup_config.block_time = 2;
        let claim_timestamp =
            rollup_config.genesis.l2_time + CLAIM_BLOCK * rollup_config.block_time;
        let schedule_timestamp =
            rollup_config.genesis.l2_time + SCHEDULE_BLOCK * rollup_config.block_time;
        let zenith_time = claim_timestamp + 1;
        assert!(
            zenith_time <= schedule_timestamp,
            "premise: Zenith active only at the pin horizon"
        );
        rollup_config.upgrades.base.zenith = Some(zenith_time);

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, CLAIM_BLOCK.to_be_bytes().to_vec());
        oracle.insert(L2_SCHEDULE_BLOCK_NUMBER_KEY, SCHEDULE_BLOCK.to_be_bytes().to_vec());
        oracle.insert_rollup_config(ORACLE_CHAIN_ID, &rollup_config);

        let boot_info =
            BootInfo::load(&oracle).await.expect("pre-Zenith subrange should load despite pin");

        assert_eq!(boot_info.rollup_config.upgrades.base.zenith, None);
    }

    #[tokio::test]
    async fn rejects_zenith_active_within_claim_range_despite_schedule_override() {
        // A later schedule pin horizon must not relax the gate: a range that genuinely reaches an
        // uncommitted Zenith activation still has to be rejected.
        const CLAIM_BLOCK: u64 = 150;
        const SCHEDULE_BLOCK: u64 = 200;

        let mut rollup_config = BaseChainConfig::MAINNET.rollup_config();
        rollup_config.genesis.l2.number = 0;
        rollup_config.genesis.l2_time = 2;
        rollup_config.block_time = 2;
        let claim_timestamp =
            rollup_config.genesis.l2_time + CLAIM_BLOCK * rollup_config.block_time;
        rollup_config.upgrades.base.zenith = Some(claim_timestamp);

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, CLAIM_BLOCK.to_be_bytes().to_vec());
        oracle.insert(L2_SCHEDULE_BLOCK_NUMBER_KEY, SCHEDULE_BLOCK.to_be_bytes().to_vec());
        oracle.insert_rollup_config(ORACLE_CHAIN_ID, &rollup_config);

        let err = BootInfo::load(&oracle)
            .await
            .expect_err("Zenith active within the claim range should fail");
        assert!(matches!(err, OracleProviderError::UncommittedZenithUpgrade));
    }

    #[tokio::test]
    async fn pins_schedule_using_cobalt_block_timestamp() {
        const GENESIS_BLOCK: u64 = 1_000;
        const CLAIM_BLOCK: u64 = 1_003;

        let mut rollup_config = BaseChainConfig::MAINNET.rollup_config();
        rollup_config.genesis.l2.number = GENESIS_BLOCK;
        rollup_config.genesis.l2_time = 1_000;
        rollup_config.block_time = 2;
        rollup_config.upgrades = UpgradeConfig {
            base: BaseUpgradeConfig { cobalt: Some(1_004), ..Default::default() },
            ..Default::default()
        };

        for (schedule_block, expected_timestamp) in [(CLAIM_BLOCK, 1_004), (1_007, 1_005)] {
            let mut oracle = MockOracle::new();
            oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
            oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
            oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
            oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, CLAIM_BLOCK.to_be_bytes().to_vec());
            if schedule_block != CLAIM_BLOCK {
                oracle.insert(L2_SCHEDULE_BLOCK_NUMBER_KEY, schedule_block.to_be_bytes().to_vec());
            }
            oracle.insert_rollup_config(ORACLE_CHAIN_ID, &rollup_config);

            let boot_info = BootInfo::load(&oracle).await.expect("boot info should load");
            let mut expected_rollup_config = rollup_config.clone();
            expected_rollup_config.l2_chain_id = boot_info.rollup_config.l2_chain_id;

            assert_eq!(boot_info.rollup_config, expected_rollup_config);
            assert_eq!(
                boot_info.schedule_id,
                ScheduleId::pin(&mut expected_rollup_config, expected_timestamp)
            );
        }
    }

    #[tokio::test]
    async fn gates_zenith_using_cobalt_claim_timestamp() {
        const GENESIS_BLOCK: u64 = 1_000;

        let mut rollup_config = BaseChainConfig::MAINNET.rollup_config();
        rollup_config.genesis.l2.number = GENESIS_BLOCK;
        rollup_config.genesis.l2_time = 1_000;
        rollup_config.block_time = 2;
        rollup_config.upgrades = UpgradeConfig {
            base: BaseUpgradeConfig {
                cobalt: Some(1_004),
                zenith: Some(1_005),
                ..Default::default()
            },
            ..Default::default()
        };

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, 1_003u64.to_be_bytes().to_vec());
        oracle.insert(L2_SCHEDULE_BLOCK_NUMBER_KEY, 1_007u64.to_be_bytes().to_vec());
        oracle.insert_rollup_config(ORACLE_CHAIN_ID, &rollup_config);

        let boot_info =
            BootInfo::load(&oracle).await.expect("Zenith activates after the claimed Cobalt block");
        assert_eq!(boot_info.rollup_config.upgrades.base.zenith, None);

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, 1_007u64.to_be_bytes().to_vec());
        oracle.insert_rollup_config(ORACLE_CHAIN_ID, &rollup_config);

        let err = BootInfo::load(&oracle)
            .await
            .expect_err("Zenith is active at the claimed Cobalt block");
        assert!(matches!(err, OracleProviderError::UncommittedZenithUpgrade));
    }

    #[tokio::test]
    async fn rejects_zero_l2_genesis_timestamp() {
        let chain_config = BaseChainConfig::MAINNET;
        let mut rollup_config = chain_config.rollup_config();
        rollup_config.genesis.l2.number = 0;
        rollup_config.genesis.l2_time = 0;

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, 1u64.to_be_bytes().to_vec());
        oracle.insert_rollup_config(ORACLE_CHAIN_ID, &rollup_config);

        let err = BootInfo::load(&oracle).await.expect_err("zero L2 genesis timestamp should fail");
        assert!(matches!(err, OracleProviderError::InvalidL2GenesisTimestamp));
    }

    #[tokio::test]
    async fn rejects_oracle_rollup_config_with_mismatched_chain_id() {
        let rollup_config = base_common_chains::rollup_config!(BaseChainConfig::SEPOLIA);

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, 40_308_263u64.to_be_bytes().to_vec());
        oracle.insert(L2_CHAIN_ID_KEY, ORACLE_CHAIN_ID.to_be_bytes().to_vec());
        oracle.insert(
            L2_ROLLUP_CONFIG_KEY,
            serde_json::to_vec(&rollup_config).expect("rollup config should serialize"),
        );

        let err = BootInfo::load(&oracle).await.expect_err("boot info should reject mismatch");
        assert!(matches!(
            err,
            OracleProviderError::RollupConfigChainIdMismatch {
                boot_chain_id: ORACLE_CHAIN_ID,
                rollup_config_chain_id: 84532,
            }
        ));
    }

    #[tokio::test]
    async fn accepts_oracle_rollup_config_with_matching_chain_id() {
        let rollup_config = BaseChainConfig::SEPOLIA.rollup_config();
        let mut rollup_config_value =
            serde_json::to_value(&rollup_config).expect("rollup config should convert to value");
        rollup_config_value["l2_chain_id"] = serde_json::json!(ORACLE_CHAIN_ID);
        rollup_config_value["base"]["beryl"] = serde_json::Value::Null;

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, 40_308_263u64.to_be_bytes().to_vec());
        oracle.insert(L2_CHAIN_ID_KEY, ORACLE_CHAIN_ID.to_be_bytes().to_vec());
        oracle.insert(
            L2_ROLLUP_CONFIG_KEY,
            serde_json::to_vec(&rollup_config_value).expect("rollup config should serialize"),
        );

        let boot_info = BootInfo::load(&oracle).await.expect("boot info should load");

        assert_eq!(boot_info.chain_id, ORACLE_CHAIN_ID);
        assert_eq!(boot_info.activation_admin_address, None);
        assert_eq!(boot_info.rollup_config.l2_chain_id.id(), ORACLE_CHAIN_ID);
    }

    #[tokio::test]
    async fn accepts_pre_beryl_oracle_chain_without_activation_admin() {
        let rollup_config = BaseChainConfig::SEPOLIA.rollup_config();
        let mut rollup_config_value =
            serde_json::to_value(&rollup_config).expect("rollup config should convert to value");
        rollup_config_value["l2_chain_id"] = serde_json::json!(ORACLE_CHAIN_ID);
        rollup_config_value["base"] = serde_json::json!({ "beryl": u64::MAX });

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, 40_308_263u64.to_be_bytes().to_vec());
        oracle.insert(L2_CHAIN_ID_KEY, ORACLE_CHAIN_ID.to_be_bytes().to_vec());
        oracle.insert(
            L2_ROLLUP_CONFIG_KEY,
            serde_json::to_vec(&rollup_config_value).expect("rollup config should serialize"),
        );

        let boot_info = BootInfo::load(&oracle).await.expect("pre-Beryl boot info should load");

        assert_eq!(boot_info.activation_admin_address, None);
        assert_eq!(boot_info.rollup_config.upgrades.base.beryl, None);
    }

    #[tokio::test]
    async fn rejects_oracle_rollup_config_with_beryl_and_no_activation_admin() {
        let rollup_config = BaseChainConfig::SEPOLIA.rollup_config();
        let mut rollup_config_value =
            serde_json::to_value(&rollup_config).expect("rollup config should convert to value");
        rollup_config_value["l2_chain_id"] = serde_json::json!(ORACLE_CHAIN_ID);
        rollup_config_value["base"] = serde_json::json!({ "beryl": 1 });

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, 40_308_263u64.to_be_bytes().to_vec());
        oracle.insert(L2_CHAIN_ID_KEY, ORACLE_CHAIN_ID.to_be_bytes().to_vec());
        oracle.insert(
            L2_ROLLUP_CONFIG_KEY,
            serde_json::to_vec(&rollup_config_value).expect("rollup config should serialize"),
        );

        let err = BootInfo::load(&oracle)
            .await
            .expect_err("Beryl-enabled oracle config without activation admin should fail");
        assert!(matches!(
            err,
            OracleProviderError::MissingActivationAdminAddress { chain_id: ORACLE_CHAIN_ID }
        ));
    }

    /// Builds an oracle with all required boot keys present for a built-in chain, so that only the
    /// optional preimage reads remain to exercise.
    fn oracle_with_required_keys() -> MockOracle {
        let chain_config = BaseChainConfig::MAINNET;
        let rollup_config = chain_config.rollup_config();

        let mut oracle = MockOracle::new();
        oracle.insert(L1_HEAD_KEY, B256::repeat_byte(0x11).to_vec());
        oracle.insert(L2_OUTPUT_ROOT_KEY, B256::repeat_byte(0x22).to_vec());
        oracle.insert(L2_CLAIM_KEY, B256::repeat_byte(0x33).to_vec());
        oracle.insert(L2_CLAIM_BLOCK_NUMBER_KEY, 100u64.to_be_bytes().to_vec());
        oracle.insert(L2_CHAIN_ID_KEY, chain_config.chain_id.to_be_bytes().to_vec());
        oracle.insert(
            L2_ROLLUP_CONFIG_KEY,
            serde_json::to_vec(&rollup_config).expect("rollup config should serialize"),
        );
        oracle
    }

    #[tokio::test]
    async fn defaults_optional_keys_when_absent() {
        let oracle = oracle_with_required_keys();

        let boot_info = BootInfo::load(&oracle).await.expect("boot info should load");

        assert_eq!(boot_info.proposer, Address::ZERO);
        assert_eq!(boot_info.intermediate_block_interval, 0);
        assert_eq!(boot_info.l1_head_number, 0);
        assert_eq!(boot_info.claimed_l2_block_number, 100);
    }

    #[tokio::test]
    async fn propagates_non_keynotfound_errors_from_optional_reads() {
        for key in [
            PROPOSER_KEY,
            INTERMEDIATE_BLOCK_INTERVAL_KEY,
            L1_HEAD_NUMBER_KEY,
            L2_SCHEDULE_BLOCK_NUMBER_KEY,
        ] {
            let mut oracle = oracle_with_required_keys();
            oracle.fail_with_timeout(key);

            let err = BootInfo::load(&oracle)
                .await
                .expect_err("operational oracle failure on an optional read should abort the load");
            assert!(
                matches!(err, OracleProviderError::Preimage(PreimageOracleError::Timeout)),
                "expected timeout to propagate, got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn defaults_optional_keys_when_zk_store_reports_invalid_key() {
        // The in-memory zkVM `PreimageStore` returns `InvalidPreimageKey` (not `KeyNotFound`) on a
        // map miss. The optional boot keys must still default there so replaying a witness that
        // predates one of them does not abort the load.
        let oracle = oracle_with_required_keys().with_zk_store_miss_semantics();

        let boot_info = BootInfo::load(&oracle).await.expect("boot info should load");

        assert_eq!(boot_info.proposer, Address::ZERO);
        assert_eq!(boot_info.intermediate_block_interval, 0);
        assert_eq!(boot_info.l1_head_number, 0);
        assert_eq!(boot_info.claimed_l2_block_number, 100);
    }

    #[tokio::test]
    async fn get_optional_local_maps_keynotfound_to_none() {
        let oracle = MockOracle::new();

        let result = BootInfo::get_optional_local(&oracle, PROPOSER_KEY)
            .await
            .expect("absent key should not be an error");
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn get_optional_local_maps_invalid_key_to_none() {
        let oracle = MockOracle::new().with_zk_store_miss_semantics();

        let result = BootInfo::get_optional_local(&oracle, PROPOSER_KEY)
            .await
            .expect("zk store map miss should be treated as absent");
        assert_eq!(result, None);
    }
}
