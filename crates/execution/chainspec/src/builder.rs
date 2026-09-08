use alloy_chains::Chain;
use alloy_genesis::Genesis;
use alloy_hardforks::Hardfork;
use alloy_primitives::{Address, U256};
use base_common_genesis::BaseUpgrade;
use reth_ethereum_forks::{ChainHardforks, EthereumHardfork, ForkCondition};

use crate::{BaseChainSpec, BaseChainSpecError};

/// Chain spec builder for a Base chain.
#[derive(Debug, Default)]
pub struct BaseChainSpecBuilder {
    /// Chain identity.
    chain: Option<Chain>,
    /// Genesis boundary configuration.
    genesis: Option<Genesis>,
    /// Configured execution forks.
    hardforks: ChainHardforks,
    /// Activation registry admin address.
    activation_admin_address: Option<Address>,
}

impl BaseChainSpecBuilder {
    /// Construct a new builder from the Base Mainnet chain spec.
    pub fn base_mainnet() -> Self {
        let base_mainnet = BaseChainSpec::mainnet();
        Self {
            chain: Some(base_mainnet.chain),
            genesis: Some(base_mainnet.genesis),
            hardforks: base_mainnet.hardforks,
            activation_admin_address: base_mainnet.activation_admin_address,
        }
    }

    /// Set the chain ID.
    pub fn chain(mut self, chain: Chain) -> Self {
        self.chain = Some(chain);
        self
    }

    /// Set the genesis block.
    pub fn genesis(mut self, genesis: Genesis) -> Self {
        self.genesis = Some(genesis);
        self
    }

    /// Add the given fork with the given activation condition to the spec.
    pub fn with_fork<H: Hardfork>(mut self, fork: H, condition: ForkCondition) -> Self {
        self.hardforks.insert(fork, condition);
        self
    }

    /// Add the given forks with the given activation condition to the spec.
    pub fn with_forks(mut self, forks: ChainHardforks) -> Self {
        self.hardforks = forks;
        self
    }

    /// Set the activation registry admin address.
    pub const fn activation_admin_address(mut self, address: Address) -> Self {
        self.activation_admin_address = Some(address);
        self
    }

    /// Set or clear the activation registry admin address.
    pub const fn optional_activation_admin_address(mut self, address: Option<Address>) -> Self {
        self.activation_admin_address = address;
        self
    }

    /// Remove the given fork from the spec.
    pub fn without_fork(mut self, fork: BaseUpgrade) -> Self {
        self.hardforks.remove(&fork);
        self
    }

    /// Enable Bedrock at genesis.
    pub fn bedrock_activated(mut self) -> Self {
        for fork in EthereumHardfork::VARIANTS
            .iter()
            .take_while(|fork| **fork != EthereumHardfork::Shanghai)
        {
            if *fork != EthereumHardfork::Dao {
                self.hardforks.insert(*fork, ForkCondition::Block(0));
            }
        }
        self.hardforks.insert(
            EthereumHardfork::Paris,
            ForkCondition::TTD {
                activation_block_number: 0,
                total_difficulty: U256::ZERO,
                fork_block: Some(0),
            },
        );
        self.hardforks.insert(BaseUpgrade::Bedrock, ForkCondition::Block(0));
        self
    }

    /// Enable Regolith at genesis.
    pub fn regolith_activated(mut self) -> Self {
        self = self.bedrock_activated();
        self.hardforks.insert(BaseUpgrade::Regolith, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Canyon at genesis.
    pub fn canyon_activated(mut self) -> Self {
        self = self.regolith_activated();
        self.hardforks.insert(EthereumHardfork::Shanghai, ForkCondition::Timestamp(0));
        self.hardforks.insert(BaseUpgrade::Canyon, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Ecotone at genesis.
    pub fn ecotone_activated(mut self) -> Self {
        self = self.canyon_activated();
        self.hardforks.insert(EthereumHardfork::Cancun, ForkCondition::Timestamp(0));
        self.hardforks.insert(BaseUpgrade::Ecotone, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Fjord at genesis.
    pub fn fjord_activated(mut self) -> Self {
        self = self.ecotone_activated();
        self.hardforks.insert(BaseUpgrade::Fjord, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Granite at genesis.
    pub fn granite_activated(mut self) -> Self {
        self = self.fjord_activated();
        self.hardforks.insert(BaseUpgrade::Granite, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Holocene at genesis.
    pub fn holocene_activated(mut self) -> Self {
        self = self.granite_activated();
        self.hardforks.insert(BaseUpgrade::Holocene, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Isthmus at genesis.
    pub fn isthmus_activated(mut self) -> Self {
        self = self.holocene_activated();
        self.hardforks.insert(BaseUpgrade::Isthmus, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Jovian at genesis.
    pub fn jovian_activated(mut self) -> Self {
        self = self.isthmus_activated();
        self.hardforks.insert(BaseUpgrade::Jovian, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Base Azul at genesis.
    pub fn azul_activated(mut self) -> Self {
        self = self.jovian_activated();
        self.hardforks.insert(EthereumHardfork::Osaka, ForkCondition::Timestamp(0));
        self.hardforks.insert(BaseUpgrade::Azul, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Beryl at genesis.
    pub fn beryl_activated(mut self) -> Self {
        self = self.azul_activated();
        self.hardforks.insert(BaseUpgrade::Beryl, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Cobalt at genesis.
    pub fn cobalt_activated(mut self) -> Self {
        self = self.beryl_activated();
        self.hardforks.insert(BaseUpgrade::Cobalt, ForkCondition::Timestamp(0));
        self
    }

    /// Tries to build the resulting [`BaseChainSpec`].
    ///
    /// # Panics
    ///
    /// This function panics if the chain ID and genesis is not set ([`Self::chain`] and
    /// [`Self::genesis`]).
    pub fn try_build(self) -> Result<BaseChainSpec, BaseChainSpecError> {
        let mut spec = BaseChainSpec {
            chain: self.chain.expect("chain ID must be set"),
            genesis: self.genesis.expect("genesis must be set"),
            hardforks: self.hardforks,
            activation_admin_address: self.activation_admin_address,
            paris_block_and_final_difficulty: Some((0, U256::ZERO)),
            ..Default::default()
        };
        BaseChainSpec::validate_beryl_activation_admin(
            &spec.hardforks,
            spec.activation_admin_address,
            spec.chain.id(),
        )?;
        spec.refresh_genesis_header();
        Ok(spec)
    }

    /// Build the resulting [`BaseChainSpec`].
    ///
    /// # Panics
    ///
    /// This function panics if the chain ID and genesis is not set ([`Self::chain`] and
    /// [`Self::genesis`]), or if Beryl is scheduled without an activation registry admin address.
    pub fn build(self) -> BaseChainSpec {
        self.try_build().expect("Beryl-enabled chain spec requires activation admin")
    }
}
