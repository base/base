use alloy_chains::Chain;
use alloy_genesis::Genesis;
use alloy_hardforks::ForkCondition;
use alloy_primitives::Address;

use crate::{BaseChainSpec, BaseChainSpecError, BaseUpgrade};

/// Chain spec builder for a Base chain.
#[derive(Debug, Default)]
pub struct BaseChainSpecBuilder {
    /// Canonical configuration being built.
    pub config: crate::ChainConfig,
    /// Parsed genesis boundary.
    pub genesis: Option<Genesis>,
}

impl BaseChainSpecBuilder {
    /// Construct a new builder from the Base Mainnet chain spec.
    pub fn base_mainnet() -> Self {
        let config = crate::ChainConfig::mainnet().clone();
        let genesis =
            serde_json::from_str(config.genesis_json).expect("Base mainnet genesis must be valid");
        Self { config, genesis: Some(genesis) }
    }

    /// Set the chain ID.
    pub fn chain(mut self, chain: Chain) -> Self {
        self.config.chain_id = chain.id();
        self
    }

    /// Set the genesis block.
    pub fn genesis(mut self, genesis: Genesis) -> Self {
        self.genesis = Some(genesis);
        self
    }

    /// Add the given fork with the given activation condition to the spec.
    pub fn with_fork(mut self, fork: BaseUpgrade, condition: ForkCondition) -> Self {
        self.config.upgrades.insert(fork, condition);
        self
    }

    /// Set the activation registry admin address.
    pub const fn activation_admin_address(mut self, address: Address) -> Self {
        self.config.activation_admin_address = Some(address);
        self
    }

    /// Set or clear the activation registry admin address.
    pub const fn optional_activation_admin_address(mut self, address: Option<Address>) -> Self {
        self.config.activation_admin_address = address;
        self
    }

    /// Enable Bedrock at genesis.
    pub fn bedrock_activated(mut self) -> Self {
        self.config.upgrades.insert(BaseUpgrade::Bedrock, ForkCondition::Block(0));
        self
    }

    /// Enable Regolith at genesis.
    pub fn regolith_activated(mut self) -> Self {
        self = self.bedrock_activated();
        self.config.upgrades.insert(BaseUpgrade::Regolith, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Canyon at genesis.
    pub fn canyon_activated(mut self) -> Self {
        self = self.regolith_activated();
        self.config.upgrades.insert(BaseUpgrade::Canyon, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Ecotone at genesis.
    pub fn ecotone_activated(mut self) -> Self {
        self = self.canyon_activated();
        self.config.upgrades.insert(BaseUpgrade::Ecotone, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Fjord at genesis.
    pub fn fjord_activated(mut self) -> Self {
        self = self.ecotone_activated();
        self.config.upgrades.insert(BaseUpgrade::Fjord, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Granite at genesis.
    pub fn granite_activated(mut self) -> Self {
        self = self.fjord_activated();
        self.config.upgrades.insert(BaseUpgrade::Granite, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Holocene at genesis.
    pub fn holocene_activated(mut self) -> Self {
        self = self.granite_activated();
        self.config.upgrades.insert(BaseUpgrade::Holocene, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Isthmus at genesis.
    pub fn isthmus_activated(mut self) -> Self {
        self = self.holocene_activated();
        self.config.upgrades.insert(BaseUpgrade::Isthmus, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Jovian at genesis.
    pub fn jovian_activated(mut self) -> Self {
        self = self.isthmus_activated();
        self.config.upgrades.insert(BaseUpgrade::Jovian, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Base Azul at genesis.
    pub fn azul_activated(mut self) -> Self {
        self = self.jovian_activated();
        self.config.upgrades.insert(BaseUpgrade::Azul, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Beryl at genesis.
    pub fn beryl_activated(mut self) -> Self {
        self = self.azul_activated();
        self.config.upgrades.insert(BaseUpgrade::Beryl, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Cobalt at genesis.
    pub fn cobalt_activated(mut self) -> Self {
        self = self.beryl_activated();
        self.config.upgrades.insert(BaseUpgrade::Cobalt, ForkCondition::Timestamp(0));
        self
    }

    /// Tries to build the resulting [`BaseChainSpec`].
    ///
    /// # Panics
    ///
    /// This function panics if the genesis is not set ([`Self::genesis`]).
    pub fn try_build(mut self) -> Result<BaseChainSpec, BaseChainSpecError> {
        self.config.genesis.l2.hash = Default::default();
        BaseChainSpec::try_from_config_and_genesis(
            self.config,
            self.genesis.expect("genesis must be set"),
        )
    }

    /// Build the resulting [`BaseChainSpec`].
    ///
    /// # Panics
    ///
    /// This function panics if genesis is not set, or Beryl is scheduled without an activation admin.
    pub fn build(self) -> BaseChainSpec {
        self.try_build().expect("Beryl-enabled chain spec requires activation admin")
    }
}
