use crate::{
    ForkCondition,
    arbitrum::{mainnet::*, sepolia::*},
    ethereum::{holesky::*, hoodi::*, mainnet::*, sepolia::*},
    hardfork,
};
use alloc::vec::Vec;
use alloy_chains::{Chain, NamedChain};
use alloy_primitives::U256;

hardfork!(
    /// The name of an Ethereum hardfork.
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[derive(Default)]
    EthereumHardfork {
        /// Frontier: <https://blog.ethereum.org/2015/03/03/ethereum-launch-process>.
        Frontier,
        /// Homestead: <https://github.com/ethereum/execution-specs/blob/master/network-upgrades/mainnet-upgrades/homestead.md>.
        Homestead,
        /// The DAO fork: <https://github.com/ethereum/execution-specs/blob/master/network-upgrades/mainnet-upgrades/dao-fork.md>.
        Dao,
        /// Tangerine: <https://github.com/ethereum/execution-specs/blob/master/network-upgrades/mainnet-upgrades/tangerine-whistle.md>.
        Tangerine,
        /// Spurious Dragon: <https://github.com/ethereum/execution-specs/blob/master/network-upgrades/mainnet-upgrades/spurious-dragon.md>.
        SpuriousDragon,
        /// Byzantium: <https://github.com/ethereum/execution-specs/blob/master/network-upgrades/mainnet-upgrades/byzantium.md>.
        Byzantium,
        /// Constantinople: <https://github.com/ethereum/execution-specs/blob/master/network-upgrades/mainnet-upgrades/constantinople.md>.
        Constantinople,
        /// Petersburg: <https://github.com/ethereum/execution-specs/blob/master/network-upgrades/mainnet-upgrades/petersburg.md>.
        Petersburg,
        /// Istanbul: <https://github.com/ethereum/execution-specs/blob/master/network-upgrades/mainnet-upgrades/istanbul.md>.
        Istanbul,
        /// Muir Glacier: <https://github.com/ethereum/execution-specs/blob/master/network-upgrades/mainnet-upgrades/muir-glacier.md>.
        MuirGlacier,
        /// Berlin: <https://github.com/ethereum/execution-specs/blob/master/network-upgrades/mainnet-upgrades/berlin.md>.
        Berlin,
        /// London: <https://github.com/ethereum/execution-specs/blob/master/network-upgrades/mainnet-upgrades/london.md>.
        London,
        /// Arrow Glacier: <https://github.com/ethereum/execution-specs/blob/master/network-upgrades/mainnet-upgrades/arrow-glacier.md>.
        ArrowGlacier,
        /// Gray Glacier: <https://github.com/ethereum/execution-specs/blob/master/network-upgrades/mainnet-upgrades/gray-glacier.md>.
        GrayGlacier,
        /// Paris: <https://github.com/ethereum/execution-specs/blob/master/network-upgrades/mainnet-upgrades/paris.md>.
        Paris,
        /// Shanghai: <https://github.com/ethereum/execution-specs/blob/master/network-upgrades/mainnet-upgrades/shanghai.md>.
        Shanghai,
        /// Cancun: <https://github.com/ethereum/execution-specs/blob/master/network-upgrades/mainnet-upgrades/cancun.md>
        Cancun,
        /// Prague.
        #[default]
        Prague,
        /// Osaka: <https://eips.ethereum.org/EIPS/eip-7607>
        Osaka,
    }
);

impl EthereumHardfork {
    /// Retrieves the activation block for the specified hardfork on the given chain.
    pub fn activation_block(&self, chain: Chain) -> Option<u64> {
        if chain == Chain::mainnet() {
            return self.mainnet_activation_block();
        }
        if chain == Chain::sepolia() {
            return self.sepolia_activation_block();
        }
        if chain == Chain::holesky() {
            return self.holesky_activation_block();
        }
        if chain == Chain::hoodi() {
            return self.hoodi_activation_block();
        }

        None
    }

    /// Retrieves the activation block for the specified hardfork on the Ethereum mainnet.
    pub const fn mainnet_activation_block(&self) -> Option<u64> {
        match self {
            Self::Frontier => Some(MAINNET_FRONTIER_BLOCK),
            Self::Homestead => Some(MAINNET_HOMESTEAD_BLOCK),
            Self::Dao => Some(MAINNET_DAO_BLOCK),
            Self::Tangerine => Some(MAINNET_TANGERINE_BLOCK),
            Self::SpuriousDragon => Some(MAINNET_SPURIOUS_DRAGON_BLOCK),
            Self::Byzantium => Some(MAINNET_BYZANTIUM_BLOCK),
            Self::Constantinople => Some(MAINNET_CONSTANTINOPLE_BLOCK),
            Self::Petersburg => Some(MAINNET_PETERSBURG_BLOCK),
            Self::Istanbul => Some(MAINNET_ISTANBUL_BLOCK),
            Self::MuirGlacier => Some(MAINNET_MUIR_GLACIER_BLOCK),
            Self::Berlin => Some(MAINNET_BERLIN_BLOCK),
            Self::London => Some(MAINNET_LONDON_BLOCK),
            Self::ArrowGlacier => Some(MAINNET_ARROW_GLACIER_BLOCK),
            Self::GrayGlacier => Some(MAINNET_GRAY_GLACIER_BLOCK),
            Self::Paris => Some(MAINNET_PARIS_BLOCK),
            Self::Shanghai => Some(MAINNET_SHANGHAI_BLOCK),
            Self::Cancun => Some(MAINNET_CANCUN_BLOCK),
            Self::Prague => Some(MAINNET_PRAGUE_BLOCK),
            _ => None,
        }
    }

    /// Retrieves the activation block for the specified hardfork on the Sepolia testnet.
    pub const fn sepolia_activation_block(&self) -> Option<u64> {
        match self {
            Self::Frontier
            | Self::Homestead
            | Self::Dao
            | Self::Tangerine
            | Self::SpuriousDragon
            | Self::Byzantium
            | Self::Constantinople
            | Self::Petersburg
            | Self::Istanbul
            | Self::MuirGlacier
            | Self::Berlin
            | Self::London
            | Self::ArrowGlacier
            | Self::GrayGlacier => Some(0),
            Self::Paris => Some(SEPOLIA_PARIS_BLOCK),
            Self::Shanghai => Some(SEPOLIA_SHANGHAI_BLOCK),
            Self::Cancun => Some(SEPOLIA_CANCUN_BLOCK),
            Self::Prague => Some(SEPOLIA_PRAGUE_BLOCK),
            _ => None,
        }
    }

    /// Retrieves the activation block for the specified hardfork on the holesky testnet.
    const fn holesky_activation_block(&self) -> Option<u64> {
        match self {
            Self::Frontier
            | Self::Dao
            | Self::Tangerine
            | Self::SpuriousDragon
            | Self::Byzantium
            | Self::Constantinople
            | Self::Petersburg
            | Self::Istanbul
            | Self::MuirGlacier
            | Self::Berlin
            | Self::London
            | Self::ArrowGlacier
            | Self::GrayGlacier
            | Self::Paris => Some(0),
            Self::Shanghai => Some(HOLESKY_SHANGHAI_BLOCK),
            Self::Cancun => Some(HOLESKY_CANCUN_BLOCK),
            Self::Prague => Some(HOLESKY_PRAGUE_BLOCK),
            _ => None,
        }
    }

    /// Retrieves the activation block for the specified hardfork on the hoodi testnet.
    const fn hoodi_activation_block(&self) -> Option<u64> {
        match self {
            Self::Frontier
            | Self::Dao
            | Self::Tangerine
            | Self::SpuriousDragon
            | Self::Byzantium
            | Self::Constantinople
            | Self::Petersburg
            | Self::Istanbul
            | Self::MuirGlacier
            | Self::Berlin
            | Self::London
            | Self::ArrowGlacier
            | Self::GrayGlacier
            | Self::Paris
            | Self::Shanghai
            | Self::Cancun => Some(0),
            Self::Prague => Some(HOODI_PRAGUE_BLOCK),
            _ => None,
        }
    }

    /// Retrieves the activation block for the specified hardfork on the Arbitrum Sepolia testnet.
    pub const fn arbitrum_sepolia_activation_block(&self) -> Option<u64> {
        match self {
            Self::Frontier
            | Self::Homestead
            | Self::Dao
            | Self::Tangerine
            | Self::SpuriousDragon
            | Self::Byzantium
            | Self::Constantinople
            | Self::Petersburg
            | Self::Istanbul
            | Self::MuirGlacier
            | Self::Berlin
            | Self::London
            | Self::ArrowGlacier
            | Self::GrayGlacier
            | Self::Paris => Some(0),
            Self::Shanghai => Some(ARBITRUM_SEPOLIA_SHANGHAI_BLOCK),
            Self::Cancun => Some(ARBITRUM_SEPOLIA_CANCUN_BLOCK),
            Self::Prague => Some(ARBITRUM_SEPOLIA_PRAGUE_BLOCK),
            _ => None,
        }
    }

    /// Retrieves the activation block for the specified hardfork on the Arbitrum One mainnet.
    pub const fn arbitrum_activation_block(&self) -> Option<u64> {
        match self {
            Self::Frontier
            | Self::Homestead
            | Self::Dao
            | Self::Tangerine
            | Self::SpuriousDragon
            | Self::Byzantium
            | Self::Constantinople
            | Self::Petersburg
            | Self::Istanbul
            | Self::MuirGlacier
            | Self::Berlin
            | Self::London
            | Self::ArrowGlacier
            | Self::GrayGlacier
            | Self::Paris => Some(0),
            Self::Shanghai => Some(ARBITRUM_ONE_SHANGHAI_BLOCK),
            Self::Cancun => Some(ARBITRUM_ONE_CANCUN_BLOCK),
            Self::Prague => Some(ARBITRUM_ONE_PRAGUE_BLOCK),
            _ => None,
        }
    }

    /// Retrieves the activation timestamp for the specified hardfork on the given chain.
    pub fn activation_timestamp(&self, chain: Chain) -> Option<u64> {
        if chain == Chain::mainnet() {
            return self.mainnet_activation_timestamp();
        }
        if chain == Chain::sepolia() {
            return self.sepolia_activation_timestamp();
        }
        if chain == Chain::holesky() {
            return self.holesky_activation_timestamp();
        }
        if chain == Chain::hoodi() {
            return self.hoodi_activation_timestamp();
        }

        None
    }

    /// Retrieves the activation timestamp for the specified hardfork on the Ethereum mainnet.
    pub const fn mainnet_activation_timestamp(&self) -> Option<u64> {
        match self {
            Self::Frontier => Some(MAINNET_FRONTIER_TIMESTAMP),
            Self::Homestead => Some(MAINNET_HOMESTEAD_TIMESTAMP),
            Self::Dao => Some(MAINNET_DAO_TIMESTAMP),
            Self::Tangerine => Some(MAINNET_TANGERINE_TIMESTAMP),
            Self::SpuriousDragon => Some(MAINNET_SPURIOUS_DRAGON_TIMESTAMP),
            Self::Byzantium => Some(MAINNET_BYZANTIUM_TIMESTAMP),
            Self::Constantinople => Some(MAINNET_CONSTANTINOPLE_TIMESTAMP),
            Self::Petersburg => Some(MAINNET_PETERSBURG_TIMESTAMP),
            Self::Istanbul => Some(MAINNET_ISTANBUL_TIMESTAMP),
            Self::MuirGlacier => Some(MAINNET_MUIR_GLACIER_TIMESTAMP),
            Self::Berlin => Some(MAINNET_BERLIN_TIMESTAMP),
            Self::London => Some(MAINNET_LONDON_TIMESTAMP),
            Self::ArrowGlacier => Some(MAINNET_ARROW_GLACIER_TIMESTAMP),
            Self::GrayGlacier => Some(MAINNET_GRAY_GLACIER_TIMESTAMP),
            Self::Paris => Some(MAINNET_PARIS_TIMESTAMP),
            Self::Shanghai => Some(MAINNET_SHANGHAI_TIMESTAMP),
            Self::Cancun => Some(MAINNET_CANCUN_TIMESTAMP),
            Self::Prague => Some(MAINNET_PRAGUE_TIMESTAMP),
            // upcoming hardforks
            _ => None,
        }
    }

    /// Retrieves the activation timestamp for the specified hardfork on the Sepolia testnet.
    pub const fn sepolia_activation_timestamp(&self) -> Option<u64> {
        match self {
            Self::Frontier
            | Self::Homestead
            | Self::Dao
            | Self::Tangerine
            | Self::SpuriousDragon
            | Self::Byzantium
            | Self::Constantinople
            | Self::Petersburg
            | Self::Istanbul
            | Self::MuirGlacier
            | Self::Berlin
            | Self::London
            | Self::ArrowGlacier
            | Self::GrayGlacier
            | Self::Paris => Some(SEPOLIA_PARIS_TIMESTAMP),
            Self::Shanghai => Some(SEPOLIA_SHANGHAI_TIMESTAMP),
            Self::Cancun => Some(SEPOLIA_CANCUN_TIMESTAMP),
            _ => None,
        }
    }

    /// Retrieves the activation timestamp for the specified hardfork on the Holesky testnet.
    pub const fn holesky_activation_timestamp(&self) -> Option<u64> {
        match self {
            Self::Frontier
            | Self::Homestead
            | Self::Dao
            | Self::Tangerine
            | Self::SpuriousDragon
            | Self::Byzantium
            | Self::Constantinople
            | Self::Petersburg
            | Self::Istanbul
            | Self::MuirGlacier
            | Self::Berlin
            | Self::London
            | Self::ArrowGlacier
            | Self::GrayGlacier
            | Self::Paris => Some(HOLESKY_PARIS_TIMESTAMP),
            Self::Shanghai => Some(HOLESKY_SHANGHAI_TIMESTAMP),
            Self::Cancun => Some(HOLESKY_CANCUN_TIMESTAMP),
            _ => None,
        }
    }

    /// Retrieves the activation timestamp for the specified hardfork on the Hoodi testnet.
    pub const fn hoodi_activation_timestamp(&self) -> Option<u64> {
        match self {
            Self::Prague => Some(HOODI_PRAGUE_TIMESTAMP),
            Self::Frontier
            | Self::Homestead
            | Self::Dao
            | Self::Tangerine
            | Self::SpuriousDragon
            | Self::Byzantium
            | Self::Constantinople
            | Self::Petersburg
            | Self::Istanbul
            | Self::MuirGlacier
            | Self::Berlin
            | Self::London
            | Self::ArrowGlacier
            | Self::GrayGlacier
            | Self::Paris
            | Self::Shanghai
            | Self::Cancun => Some(0),
            _ => None,
        }
    }

    /// Retrieves the activation timestamp for the specified hardfork on the Arbitrum Sepolia
    /// testnet.
    pub const fn arbitrum_sepolia_activation_timestamp(&self) -> Option<u64> {
        match self {
            Self::Frontier
            | Self::Homestead
            | Self::Dao
            | Self::Tangerine
            | Self::SpuriousDragon
            | Self::Byzantium
            | Self::Constantinople
            | Self::Petersburg
            | Self::Istanbul
            | Self::MuirGlacier
            | Self::Berlin
            | Self::London
            | Self::ArrowGlacier
            | Self::GrayGlacier
            | Self::Paris => Some(ARBITRUM_SEPOLIA_PARIS_TIMESTAMP),
            Self::Shanghai => Some(ARBITRUM_SEPOLIA_SHANGHAI_TIMESTAMP),
            Self::Cancun => Some(ARBITRUM_SEPOLIA_CANCUN_TIMESTAMP),
            Self::Prague => Some(ARBITRUM_SEPOLIA_PRAGUE_TIMESTAMP),
            _ => None,
        }
    }

    /// Retrieves the activation timestamp for the specified hardfork on the Arbitrum One mainnet.
    pub const fn arbitrum_activation_timestamp(&self) -> Option<u64> {
        match self {
            Self::Frontier
            | Self::Homestead
            | Self::Dao
            | Self::Tangerine
            | Self::SpuriousDragon
            | Self::Byzantium
            | Self::Constantinople
            | Self::Petersburg
            | Self::Istanbul
            | Self::MuirGlacier
            | Self::Berlin
            | Self::London
            | Self::ArrowGlacier
            | Self::GrayGlacier
            | Self::Paris => Some(ARBITRUM_ONE_PARIS_TIMESTAMP),
            Self::Shanghai => Some(ARBITRUM_ONE_SHANGHAI_TIMESTAMP),
            Self::Cancun => Some(ARBITRUM_ONE_CANCUN_TIMESTAMP),
            Self::Prague => Some(ARBITRUM_ONE_PRAGUE_TIMESTAMP),
            _ => None,
        }
    }

    /// Ethereum mainnet list of hardforks.
    pub const fn mainnet() -> [(Self, ForkCondition); 18] {
        [
            (Self::Frontier, ForkCondition::Block(MAINNET_FRONTIER_BLOCK)),
            (Self::Homestead, ForkCondition::Block(MAINNET_HOMESTEAD_BLOCK)),
            (Self::Dao, ForkCondition::Block(MAINNET_DAO_BLOCK)),
            (Self::Tangerine, ForkCondition::Block(MAINNET_TANGERINE_BLOCK)),
            (Self::SpuriousDragon, ForkCondition::Block(MAINNET_SPURIOUS_DRAGON_BLOCK)),
            (Self::Byzantium, ForkCondition::Block(MAINNET_BYZANTIUM_BLOCK)),
            (Self::Constantinople, ForkCondition::Block(MAINNET_CONSTANTINOPLE_BLOCK)),
            (Self::Petersburg, ForkCondition::Block(MAINNET_PETERSBURG_BLOCK)),
            (Self::Istanbul, ForkCondition::Block(MAINNET_ISTANBUL_BLOCK)),
            (Self::MuirGlacier, ForkCondition::Block(MAINNET_MUIR_GLACIER_BLOCK)),
            (Self::Berlin, ForkCondition::Block(MAINNET_BERLIN_BLOCK)),
            (Self::London, ForkCondition::Block(MAINNET_LONDON_BLOCK)),
            (Self::ArrowGlacier, ForkCondition::Block(MAINNET_ARROW_GLACIER_BLOCK)),
            (Self::GrayGlacier, ForkCondition::Block(MAINNET_GRAY_GLACIER_BLOCK)),
            (
                Self::Paris,
                ForkCondition::TTD {
                    activation_block_number: MAINNET_PARIS_BLOCK,
                    fork_block: None,
                    total_difficulty: MAINNET_PARIS_TTD,
                },
            ),
            (Self::Shanghai, ForkCondition::Timestamp(MAINNET_SHANGHAI_TIMESTAMP)),
            (Self::Cancun, ForkCondition::Timestamp(MAINNET_CANCUN_TIMESTAMP)),
            (Self::Prague, ForkCondition::Timestamp(MAINNET_PRAGUE_TIMESTAMP)),
        ]
    }

    /// Ethereum sepolia list of hardforks.
    pub const fn sepolia() -> [(Self, ForkCondition); 16] {
        [
            (Self::Frontier, ForkCondition::Block(0)),
            (Self::Homestead, ForkCondition::Block(0)),
            (Self::Dao, ForkCondition::Block(0)),
            (Self::Tangerine, ForkCondition::Block(0)),
            (Self::SpuriousDragon, ForkCondition::Block(0)),
            (Self::Byzantium, ForkCondition::Block(0)),
            (Self::Constantinople, ForkCondition::Block(0)),
            (Self::Petersburg, ForkCondition::Block(0)),
            (Self::Istanbul, ForkCondition::Block(0)),
            (Self::MuirGlacier, ForkCondition::Block(0)),
            (Self::Berlin, ForkCondition::Block(0)),
            (Self::London, ForkCondition::Block(0)),
            (
                Self::Paris,
                ForkCondition::TTD {
                    activation_block_number: SEPOLIA_PARIS_BLOCK,
                    fork_block: Some(SEPOLIA_PARIS_FORK_BLOCK),
                    total_difficulty: SEPOLIA_PARIS_TTD,
                },
            ),
            (Self::Shanghai, ForkCondition::Timestamp(SEPOLIA_SHANGHAI_TIMESTAMP)),
            (Self::Cancun, ForkCondition::Timestamp(SEPOLIA_CANCUN_TIMESTAMP)),
            (Self::Prague, ForkCondition::Timestamp(SEPOLIA_PRAGUE_TIMESTAMP)),
        ]
    }

    /// Ethereum holesky list of hardforks.
    pub const fn holesky() -> [(Self, ForkCondition); 16] {
        [
            (Self::Frontier, ForkCondition::Block(0)),
            (Self::Homestead, ForkCondition::Block(0)),
            (Self::Dao, ForkCondition::Block(0)),
            (Self::Tangerine, ForkCondition::Block(0)),
            (Self::SpuriousDragon, ForkCondition::Block(0)),
            (Self::Byzantium, ForkCondition::Block(0)),
            (Self::Constantinople, ForkCondition::Block(0)),
            (Self::Petersburg, ForkCondition::Block(0)),
            (Self::Istanbul, ForkCondition::Block(0)),
            (Self::MuirGlacier, ForkCondition::Block(0)),
            (Self::Berlin, ForkCondition::Block(0)),
            (Self::London, ForkCondition::Block(0)),
            (
                Self::Paris,
                ForkCondition::TTD {
                    activation_block_number: 0,
                    fork_block: Some(0),
                    total_difficulty: U256::ZERO,
                },
            ),
            (Self::Shanghai, ForkCondition::Timestamp(HOLESKY_SHANGHAI_TIMESTAMP)),
            (Self::Cancun, ForkCondition::Timestamp(HOLESKY_CANCUN_TIMESTAMP)),
            (Self::Prague, ForkCondition::Timestamp(HOLESKY_PRAGUE_TIMESTAMP)),
        ]
    }

    /// Ethereum Hoodi list of hardforks.
    pub const fn hoodi() -> [(Self, ForkCondition); 16] {
        [
            (Self::Frontier, ForkCondition::Block(0)),
            (Self::Homestead, ForkCondition::Block(0)),
            (Self::Dao, ForkCondition::Block(0)),
            (Self::Tangerine, ForkCondition::Block(0)),
            (Self::SpuriousDragon, ForkCondition::Block(0)),
            (Self::Byzantium, ForkCondition::Block(0)),
            (Self::Constantinople, ForkCondition::Block(0)),
            (Self::Petersburg, ForkCondition::Block(0)),
            (Self::Istanbul, ForkCondition::Block(0)),
            (Self::MuirGlacier, ForkCondition::Block(0)),
            (Self::Berlin, ForkCondition::Block(0)),
            (Self::London, ForkCondition::Block(0)),
            (
                Self::Paris,
                ForkCondition::TTD {
                    activation_block_number: 0,
                    fork_block: Some(0),
                    total_difficulty: U256::ZERO,
                },
            ),
            (Self::Shanghai, ForkCondition::Timestamp(0)),
            (Self::Cancun, ForkCondition::Timestamp(0)),
            (Self::Prague, ForkCondition::Timestamp(HOODI_PRAGUE_TIMESTAMP)),
        ]
    }

    /// Convert an u64 into an `EthereumHardfork`.
    pub const fn from_mainnet_block_number(num: u64) -> Self {
        match num {
            _i if num < MAINNET_HOMESTEAD_BLOCK => Self::Frontier,
            _i if num < MAINNET_DAO_BLOCK => Self::Homestead,
            _i if num < MAINNET_TANGERINE_BLOCK => Self::Dao,
            _i if num < MAINNET_SPURIOUS_DRAGON_BLOCK => Self::Tangerine,
            _i if num < MAINNET_BYZANTIUM_BLOCK => Self::SpuriousDragon,
            _i if num < MAINNET_CONSTANTINOPLE_BLOCK => Self::Byzantium,
            _i if num < MAINNET_ISTANBUL_BLOCK => Self::Constantinople,
            _i if num < MAINNET_MUIR_GLACIER_BLOCK => Self::Istanbul,
            _i if num < MAINNET_BERLIN_BLOCK => Self::MuirGlacier,
            _i if num < MAINNET_LONDON_BLOCK => Self::Berlin,
            _i if num < MAINNET_ARROW_GLACIER_BLOCK => Self::London,
            _i if num < MAINNET_PARIS_BLOCK => Self::ArrowGlacier,
            _i if num < MAINNET_SHANGHAI_BLOCK => Self::Paris,
            _i if num < MAINNET_CANCUN_BLOCK => Self::Shanghai,
            _i if num < MAINNET_PRAGUE_BLOCK => Self::Cancun,
            _ => Self::Prague,
        }
    }

    /// Reverse lookup to find the hardfork given a chain ID and block timestamp.
    /// Returns the active hardfork at the given timestamp for the specified chain.
    pub fn from_chain_and_timestamp(chain: Chain, timestamp: u64) -> Option<Self> {
        let named = chain.named()?;

        match named {
            NamedChain::Mainnet => Some(match timestamp {
                _i if timestamp < MAINNET_HOMESTEAD_TIMESTAMP => Self::Frontier,
                _i if timestamp < MAINNET_DAO_TIMESTAMP => Self::Homestead,
                _i if timestamp < MAINNET_TANGERINE_TIMESTAMP => Self::Dao,
                _i if timestamp < MAINNET_SPURIOUS_DRAGON_TIMESTAMP => Self::Tangerine,
                _i if timestamp < MAINNET_BYZANTIUM_TIMESTAMP => Self::SpuriousDragon,
                _i if timestamp < MAINNET_PETERSBURG_TIMESTAMP => Self::Byzantium,
                _i if timestamp < MAINNET_ISTANBUL_TIMESTAMP => Self::Petersburg,
                _i if timestamp < MAINNET_MUIR_GLACIER_TIMESTAMP => Self::Istanbul,
                _i if timestamp < MAINNET_BERLIN_TIMESTAMP => Self::MuirGlacier,
                _i if timestamp < MAINNET_LONDON_TIMESTAMP => Self::Berlin,
                _i if timestamp < MAINNET_ARROW_GLACIER_TIMESTAMP => Self::London,
                _i if timestamp < MAINNET_GRAY_GLACIER_TIMESTAMP => Self::ArrowGlacier,
                _i if timestamp < MAINNET_PARIS_TIMESTAMP => Self::GrayGlacier,
                _i if timestamp < MAINNET_SHANGHAI_TIMESTAMP => Self::Paris,
                _i if timestamp < MAINNET_CANCUN_TIMESTAMP => Self::Shanghai,
                _i if timestamp < MAINNET_PRAGUE_TIMESTAMP => Self::Cancun,
                _ => Self::Prague,
            }),
            NamedChain::Sepolia => Some(match timestamp {
                _i if timestamp < SEPOLIA_PARIS_TIMESTAMP => Self::London,
                _i if timestamp < SEPOLIA_SHANGHAI_TIMESTAMP => Self::Paris,
                _i if timestamp < SEPOLIA_CANCUN_TIMESTAMP => Self::Shanghai,
                _i if timestamp < SEPOLIA_PRAGUE_TIMESTAMP => Self::Cancun,
                _ => Self::Prague,
            }),
            NamedChain::Holesky => Some(match timestamp {
                _i if timestamp < HOLESKY_SHANGHAI_TIMESTAMP => Self::Paris,
                _i if timestamp < HOLESKY_CANCUN_TIMESTAMP => Self::Shanghai,
                _i if timestamp < HOLESKY_PRAGUE_TIMESTAMP => Self::Cancun,
                _ => Self::Prague,
            }),
            NamedChain::Hoodi => Some(match timestamp {
                _i if timestamp < HOODI_PRAGUE_TIMESTAMP => Self::Cancun,
                _ => Self::Prague,
            }),
            NamedChain::Arbitrum => Some(match timestamp {
                _i if timestamp < ARBITRUM_ONE_SHANGHAI_TIMESTAMP => Self::Paris,
                _i if timestamp < ARBITRUM_ONE_CANCUN_TIMESTAMP => Self::Shanghai,
                _i if timestamp < ARBITRUM_ONE_PRAGUE_TIMESTAMP => Self::Cancun,
                _ => Self::Prague,
            }),
            NamedChain::ArbitrumSepolia => Some(match timestamp {
                _i if timestamp < ARBITRUM_SEPOLIA_SHANGHAI_TIMESTAMP => Self::Paris,
                _i if timestamp < ARBITRUM_SEPOLIA_CANCUN_TIMESTAMP => Self::Shanghai,
                _i if timestamp < ARBITRUM_SEPOLIA_PRAGUE_TIMESTAMP => Self::Cancun,
                _ => Self::Prague,
            }),
            _ => None,
        }
    }
}

/// Helper methods for Ethereum forks.
#[auto_impl::auto_impl(&, Arc)]
pub trait EthereumHardforks {
    /// Retrieves [`ForkCondition`] by an [`EthereumHardfork`]. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition;

    /// Convenience method to check if an [`EthereumHardfork`] is active at a given timestamp.
    fn is_ethereum_fork_active_at_timestamp(&self, fork: EthereumHardfork, timestamp: u64) -> bool {
        self.ethereum_fork_activation(fork).active_at_timestamp(timestamp)
    }

    /// Convenience method to check if an [`EthereumHardfork`] is active at a given block number.
    fn is_ethereum_fork_active_at_block(&self, fork: EthereumHardfork, block_number: u64) -> bool {
        self.ethereum_fork_activation(fork).active_at_block(block_number)
    }

    /// Convenience method to check if [`EthereumHardfork::Shanghai`] is active at a given
    /// timestamp.
    fn is_shanghai_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.is_ethereum_fork_active_at_timestamp(EthereumHardfork::Shanghai, timestamp)
    }

    /// Convenience method to check if [`EthereumHardfork::Cancun`] is active at a given timestamp.
    fn is_cancun_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.is_ethereum_fork_active_at_timestamp(EthereumHardfork::Cancun, timestamp)
    }

    /// Convenience method to check if [`EthereumHardfork::Prague`] is active at a given timestamp.
    fn is_prague_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.is_ethereum_fork_active_at_timestamp(EthereumHardfork::Prague, timestamp)
    }

    /// Convenience method to check if [`EthereumHardfork::Osaka`] is active at a given timestamp.
    fn is_osaka_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.is_ethereum_fork_active_at_timestamp(EthereumHardfork::Osaka, timestamp)
    }

    /// Convenience method to check if [`EthereumHardfork::Byzantium`] is active at a given block
    /// number.
    fn is_byzantium_active_at_block(&self, block_number: u64) -> bool {
        self.is_ethereum_fork_active_at_block(EthereumHardfork::Byzantium, block_number)
    }

    /// Convenience method to check if [`EthereumHardfork::SpuriousDragon`] is active at a given
    /// block number.
    fn is_spurious_dragon_active_at_block(&self, block_number: u64) -> bool {
        self.is_ethereum_fork_active_at_block(EthereumHardfork::SpuriousDragon, block_number)
    }

    /// Convenience method to check if [`EthereumHardfork::Homestead`] is active at a given block
    /// number.
    fn is_homestead_active_at_block(&self, block_number: u64) -> bool {
        self.is_ethereum_fork_active_at_block(EthereumHardfork::Homestead, block_number)
    }

    /// Convenience method to check if [`EthereumHardfork::London`] is active at a given block
    /// number.
    fn is_london_active_at_block(&self, block_number: u64) -> bool {
        self.is_ethereum_fork_active_at_block(EthereumHardfork::London, block_number)
    }

    /// Convenience method to check if [`EthereumHardfork::Constantinople`] is active at a given
    /// block number.
    fn is_constantinople_active_at_block(&self, block_number: u64) -> bool {
        self.is_ethereum_fork_active_at_block(EthereumHardfork::Constantinople, block_number)
    }

    /// Convenience method to check if [`EthereumHardfork::Paris`] is active at a given block
    /// number.
    fn is_paris_active_at_block(&self, block_number: u64) -> bool {
        self.is_ethereum_fork_active_at_block(EthereumHardfork::Paris, block_number)
    }
}

/// A type allowing to configure activation [`ForkCondition`]s for a given list of
/// [`EthereumHardfork`]s.
#[derive(Debug, Clone)]
pub struct EthereumChainHardforks {
    forks: Vec<(EthereumHardfork, ForkCondition)>,
}

impl EthereumChainHardforks {
    /// Creates a new [`EthereumChainHardforks`] with the given list of forks.
    pub fn new(forks: impl IntoIterator<Item = (EthereumHardfork, ForkCondition)>) -> Self {
        let mut forks = forks.into_iter().collect::<Vec<_>>();
        forks.sort();
        Self { forks }
    }

    /// Creates a new [`EthereumChainHardforks`] with Mainnet configuration.
    pub fn mainnet() -> Self {
        Self::new(EthereumHardfork::mainnet())
    }

    /// Creates a new [`EthereumChainHardforks`] with Sepolia configuration.
    pub fn sepolia() -> Self {
        Self::new(EthereumHardfork::sepolia())
    }

    /// Creates a new [`EthereumChainHardforks`] with Holesky configuration.
    pub fn holesky() -> Self {
        Self::new(EthereumHardfork::holesky())
    }

    /// Creates a new [`EthereumChainHardforks`] with Hoodi configuration.
    pub fn hoodi() -> Self {
        Self::new(EthereumHardfork::hoodi())
    }
}

impl EthereumHardforks for EthereumChainHardforks {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        let Ok(idx) = self.forks.binary_search_by(|(f, _)| f.cmp(&fork)) else {
            return ForkCondition::Never;
        };

        self.forks[idx].1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::str::FromStr;

    #[test]
    fn check_hardfork_from_str() {
        let hardfork_str = [
            "frOntier",
            "homEstead",
            "dao",
            "tAngerIne",
            "spurIousdrAgon",
            "byzAntium",
            "constantinople",
            "petersburg",
            "istanbul",
            "muirglacier",
            "bErlin",
            "lonDon",
            "arrowglacier",
            "grayglacier",
            "PARIS",
            "ShAnGhAI",
            "CaNcUn",
            "PrAguE",
        ];
        let expected_hardforks = [
            EthereumHardfork::Frontier,
            EthereumHardfork::Homestead,
            EthereumHardfork::Dao,
            EthereumHardfork::Tangerine,
            EthereumHardfork::SpuriousDragon,
            EthereumHardfork::Byzantium,
            EthereumHardfork::Constantinople,
            EthereumHardfork::Petersburg,
            EthereumHardfork::Istanbul,
            EthereumHardfork::MuirGlacier,
            EthereumHardfork::Berlin,
            EthereumHardfork::London,
            EthereumHardfork::ArrowGlacier,
            EthereumHardfork::GrayGlacier,
            EthereumHardfork::Paris,
            EthereumHardfork::Shanghai,
            EthereumHardfork::Cancun,
            EthereumHardfork::Prague,
        ];

        let hardforks: Vec<EthereumHardfork> =
            hardfork_str.iter().map(|h| EthereumHardfork::from_str(h).unwrap()).collect();

        assert_eq!(hardforks, expected_hardforks);
    }

    #[test]
    fn check_nonexistent_hardfork_from_str() {
        assert!(EthereumHardfork::from_str("not a hardfork").is_err());
    }

    #[test]
    fn test_reverse_lookup_by_chain_id() {
        // Test major hardforks across all supported Ethereum chains
        let test_cases = [
            // (chain_id, timestamp, expected) - Key transitions for each chain
            // Mainnet
            // At block 0: Frontier
            (Chain::mainnet(), MAINNET_FRONTIER_TIMESTAMP - 1, EthereumHardfork::Frontier),
            (Chain::mainnet(), MAINNET_FRONTIER_TIMESTAMP, EthereumHardfork::Frontier),
            (Chain::mainnet(), MAINNET_HOMESTEAD_TIMESTAMP, EthereumHardfork::Homestead),
            (Chain::mainnet(), MAINNET_DAO_TIMESTAMP, EthereumHardfork::Dao),
            (Chain::mainnet(), MAINNET_TANGERINE_TIMESTAMP, EthereumHardfork::Tangerine),
            (Chain::mainnet(), MAINNET_SPURIOUS_DRAGON_TIMESTAMP, EthereumHardfork::SpuriousDragon),
            (Chain::mainnet(), MAINNET_BYZANTIUM_TIMESTAMP, EthereumHardfork::Byzantium),
            (Chain::mainnet(), MAINNET_PETERSBURG_TIMESTAMP, EthereumHardfork::Petersburg),
            (Chain::mainnet(), MAINNET_ISTANBUL_TIMESTAMP, EthereumHardfork::Istanbul),
            (Chain::mainnet(), MAINNET_MUIR_GLACIER_TIMESTAMP, EthereumHardfork::MuirGlacier),
            (Chain::mainnet(), MAINNET_BERLIN_TIMESTAMP, EthereumHardfork::Berlin),
            (Chain::mainnet(), MAINNET_LONDON_TIMESTAMP, EthereumHardfork::London),
            (Chain::mainnet(), MAINNET_ARROW_GLACIER_TIMESTAMP, EthereumHardfork::ArrowGlacier),
            (Chain::mainnet(), MAINNET_GRAY_GLACIER_TIMESTAMP, EthereumHardfork::GrayGlacier),
            (Chain::mainnet(), MAINNET_PARIS_TIMESTAMP, EthereumHardfork::Paris),
            (Chain::mainnet(), MAINNET_SHANGHAI_TIMESTAMP, EthereumHardfork::Shanghai),
            (Chain::mainnet(), MAINNET_CANCUN_TIMESTAMP, EthereumHardfork::Cancun),
            // Sepolia
            // At block 0: London
            (Chain::sepolia(), SEPOLIA_PARIS_TIMESTAMP - 1, EthereumHardfork::London),
            (Chain::sepolia(), SEPOLIA_PARIS_TIMESTAMP, EthereumHardfork::Paris),
            (Chain::sepolia(), SEPOLIA_SHANGHAI_TIMESTAMP - 1, EthereumHardfork::Paris),
            (Chain::sepolia(), SEPOLIA_SHANGHAI_TIMESTAMP, EthereumHardfork::Shanghai),
            (Chain::sepolia(), SEPOLIA_CANCUN_TIMESTAMP, EthereumHardfork::Cancun),
            (Chain::sepolia(), SEPOLIA_PRAGUE_TIMESTAMP - 1, EthereumHardfork::Cancun),
            (Chain::sepolia(), SEPOLIA_PRAGUE_TIMESTAMP + 1, EthereumHardfork::Prague),
            // Holesky
            // At block 0: Paris
            (Chain::holesky(), HOLESKY_PARIS_TIMESTAMP - 1, EthereumHardfork::Paris),
            (Chain::holesky(), HOLESKY_PARIS_TIMESTAMP, EthereumHardfork::Paris),
            (Chain::holesky(), HOLESKY_SHANGHAI_TIMESTAMP - 1, EthereumHardfork::Paris),
            (Chain::holesky(), HOLESKY_SHANGHAI_TIMESTAMP, EthereumHardfork::Shanghai),
            (Chain::holesky(), HOLESKY_CANCUN_TIMESTAMP, EthereumHardfork::Cancun),
            (Chain::holesky(), HOLESKY_PRAGUE_TIMESTAMP - 1, EthereumHardfork::Cancun),
            (Chain::holesky(), HOLESKY_PRAGUE_TIMESTAMP + 1, EthereumHardfork::Prague),
            // Arbitrum One
            // At block 0: Paris
            (Chain::arbitrum_mainnet(), ARBITRUM_ONE_PARIS_TIMESTAMP - 1, EthereumHardfork::Paris),
            (Chain::arbitrum_mainnet(), ARBITRUM_ONE_PARIS_TIMESTAMP, EthereumHardfork::Paris),
            (
                Chain::arbitrum_mainnet(),
                ARBITRUM_ONE_SHANGHAI_TIMESTAMP - 1,
                EthereumHardfork::Paris,
            ),
            (
                Chain::arbitrum_mainnet(),
                ARBITRUM_ONE_SHANGHAI_TIMESTAMP,
                EthereumHardfork::Shanghai,
            ),
            (Chain::arbitrum_mainnet(), ARBITRUM_ONE_CANCUN_TIMESTAMP, EthereumHardfork::Cancun),
            (
                Chain::arbitrum_mainnet(),
                ARBITRUM_ONE_PRAGUE_TIMESTAMP - 1,
                EthereumHardfork::Cancun,
            ),
            (
                Chain::arbitrum_mainnet(),
                ARBITRUM_ONE_PRAGUE_TIMESTAMP + 1,
                EthereumHardfork::Prague,
            ),
            // Arbitrum Sepolia
            // At block 0: Paris
            (
                Chain::arbitrum_sepolia(),
                ARBITRUM_SEPOLIA_PARIS_TIMESTAMP - 1,
                EthereumHardfork::Paris,
            ),
            (Chain::arbitrum_sepolia(), ARBITRUM_SEPOLIA_PARIS_TIMESTAMP, EthereumHardfork::Paris),
            (
                Chain::arbitrum_sepolia(),
                ARBITRUM_SEPOLIA_SHANGHAI_TIMESTAMP - 1,
                EthereumHardfork::Paris,
            ),
            (
                Chain::arbitrum_sepolia(),
                ARBITRUM_SEPOLIA_SHANGHAI_TIMESTAMP,
                EthereumHardfork::Shanghai,
            ),
            (
                Chain::arbitrum_sepolia(),
                ARBITRUM_SEPOLIA_CANCUN_TIMESTAMP,
                EthereumHardfork::Cancun,
            ),
            (
                Chain::arbitrum_sepolia(),
                ARBITRUM_SEPOLIA_PRAGUE_TIMESTAMP - 1,
                EthereumHardfork::Cancun,
            ),
            (
                Chain::arbitrum_sepolia(),
                ARBITRUM_SEPOLIA_PRAGUE_TIMESTAMP + 1,
                EthereumHardfork::Prague,
            ),
        ];

        for (chain_id, timestamp, expected) in test_cases {
            assert_eq!(
                EthereumHardfork::from_chain_and_timestamp(chain_id, timestamp),
                Some(expected),
                "chain {chain_id} at timestamp {timestamp}"
            );
        }

        // Edge cases
        assert_eq!(
            EthereumHardfork::from_chain_and_timestamp(Chain::from_id(99999), 1000000),
            None
        );
    }

    #[test]
    fn test_timestamp_functions_consistency() {
        let test_cases = [
            (MAINNET_LONDON_TIMESTAMP, EthereumHardfork::London),
            (MAINNET_SHANGHAI_TIMESTAMP, EthereumHardfork::Shanghai),
            (MAINNET_CANCUN_TIMESTAMP, EthereumHardfork::Cancun),
        ];

        for (timestamp, fork) in test_cases {
            assert_eq!(
                EthereumHardfork::from_chain_and_timestamp(Chain::mainnet(), timestamp),
                Some(fork)
            );
            assert_eq!(fork.activation_timestamp(Chain::mainnet()), Some(timestamp));
        }
    }
}
