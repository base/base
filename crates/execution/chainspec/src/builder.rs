use alloy_chains::Chain;
use alloy_genesis::Genesis;
use alloy_hardforks::Hardfork;
use base_common_chains::BaseUpgrade;
use derive_more::From;
use reth_chainspec::ChainSpecBuilder;
use reth_ethereum_forks::{ChainHardforks, EthereumHardfork, ForkCondition};
use reth_primitives_traits::SealedHeader;

use crate::BaseChainSpec;

/// Chain spec builder for a Base chain.
#[derive(Debug, Default, From)]
pub struct BaseChainSpecBuilder {
    /// [`ChainSpecBuilder`]
    inner: ChainSpecBuilder,
}

impl BaseChainSpecBuilder {
    /// Construct a new builder from the Base Mainnet chain spec.
    pub fn base_mainnet() -> Self {
        let mut inner = ChainSpecBuilder::default()
            .chain(crate::BASE_MAINNET.chain)
            .genesis(crate::BASE_MAINNET.genesis.clone());
        let forks = crate::BASE_MAINNET.hardforks.clone();
        inner = inner.with_forks(forks);
        Self { inner }
    }

    /// Set the chain ID.
    pub fn chain(mut self, chain: Chain) -> Self {
        self.inner = self.inner.chain(chain);
        self
    }

    /// Set the genesis block.
    pub fn genesis(mut self, genesis: Genesis) -> Self {
        self.inner = self.inner.genesis(genesis);
        self
    }

    /// Add the given fork with the given activation condition to the spec.
    pub fn with_fork<H: Hardfork>(mut self, fork: H, condition: ForkCondition) -> Self {
        self.inner = self.inner.with_fork(fork, condition);
        self
    }

    /// Add the given forks with the given activation condition to the spec.
    pub fn with_forks(mut self, forks: ChainHardforks) -> Self {
        self.inner = self.inner.with_forks(forks);
        self
    }

    /// Remove the given fork from the spec.
    pub fn without_fork(mut self, fork: BaseUpgrade) -> Self {
        self.inner = self.inner.without_fork(fork);
        self
    }

    /// Enable Bedrock at genesis.
    pub fn bedrock_activated(mut self) -> Self {
        self.inner = self.inner.paris_activated();
        self.inner = self.inner.with_fork(BaseUpgrade::Bedrock, ForkCondition::Block(0));
        self
    }

    /// Enable Regolith at genesis.
    pub fn regolith_activated(mut self) -> Self {
        self = self.bedrock_activated();
        self.inner = self.inner.with_fork(BaseUpgrade::Regolith, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Canyon at genesis.
    pub fn canyon_activated(mut self) -> Self {
        self = self.regolith_activated();
        self.inner = self.inner.with_fork(EthereumHardfork::Shanghai, ForkCondition::Timestamp(0));
        self.inner = self.inner.with_fork(BaseUpgrade::Canyon, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Ecotone at genesis.
    pub fn ecotone_activated(mut self) -> Self {
        self = self.canyon_activated();
        self.inner = self.inner.with_fork(EthereumHardfork::Cancun, ForkCondition::Timestamp(0));
        self.inner = self.inner.with_fork(BaseUpgrade::Ecotone, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Fjord at genesis.
    pub fn fjord_activated(mut self) -> Self {
        self = self.ecotone_activated();
        self.inner = self.inner.with_fork(BaseUpgrade::Fjord, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Granite at genesis.
    pub fn granite_activated(mut self) -> Self {
        self = self.fjord_activated();
        self.inner = self.inner.with_fork(BaseUpgrade::Granite, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Holocene at genesis.
    pub fn holocene_activated(mut self) -> Self {
        self = self.granite_activated();
        self.inner = self.inner.with_fork(BaseUpgrade::Holocene, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Isthmus at genesis.
    pub fn isthmus_activated(mut self) -> Self {
        self = self.holocene_activated();
        self.inner = self.inner.with_fork(BaseUpgrade::Isthmus, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Jovian at genesis.
    pub fn jovian_activated(mut self) -> Self {
        self = self.isthmus_activated();
        self.inner = self.inner.with_fork(BaseUpgrade::Jovian, ForkCondition::Timestamp(0));
        self
    }

    /// Enable Base Azul at genesis.
    pub fn azul_activated(mut self) -> Self {
        self = self.jovian_activated();
        self.inner = self.inner.with_fork(EthereumHardfork::Osaka, ForkCondition::Timestamp(0));
        self.inner = self.inner.with_fork(BaseUpgrade::Azul, ForkCondition::Timestamp(0));
        self
    }

    /// Build the resulting [`BaseChainSpec`].
    ///
    /// # Panics
    ///
    /// This function panics if the chain ID and genesis is not set ([`Self::chain`] and
    /// [`Self::genesis`]).
    pub fn build(self) -> BaseChainSpec {
        let mut inner = self.inner.build();
        inner.genesis_header = SealedHeader::seal_slow(BaseChainSpec::make_genesis_header(
            &inner.genesis,
            &inner.hardforks,
        ));
        BaseChainSpec { inner }
    }
}
