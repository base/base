//! Ethereum Mainnet hardfork starting points

use alloy_primitives::{U256, uint};

/// Frontier hard fork activation block is 0.
pub const MAINNET_FRONTIER_BLOCK: u64 = 0;
/// Homestead hard fork activation block is 1150000.
pub const MAINNET_HOMESTEAD_BLOCK: u64 = 1_150_000;
/// Dao hard fork activation  is 1920000.
pub const MAINNET_DAO_BLOCK: u64 = 1_920_000;
///Tangerine hard fork activation block is 2463000.
pub const MAINNET_TANGERINE_BLOCK: u64 = 2_463_000;
/// Spurious Dragon hard fork activation block is 2675000.
pub const MAINNET_SPURIOUS_DRAGON_BLOCK: u64 = 2_675_000;
/// Byzantium hard fork activation block is 4370000.
pub const MAINNET_BYZANTIUM_BLOCK: u64 = 4_370_000;
/// Constantinople hard fork activation block is 7280000, same as [`MAINNET_PETERSBURG_BLOCK`].
pub const MAINNET_CONSTANTINOPLE_BLOCK: u64 = MAINNET_PETERSBURG_BLOCK;
/// Petersburg hard fork activation block is 7280000.
pub const MAINNET_PETERSBURG_BLOCK: u64 = 7_280_000;
/// Istanbul hard fork activation block is 9069000.
pub const MAINNET_ISTANBUL_BLOCK: u64 = 9_069_000;
/// Muir Glacier hard fork activation block is 9200000.
pub const MAINNET_MUIR_GLACIER_BLOCK: u64 = 9_200_000;
/// Berlin hard fork activation block is 12244000.
pub const MAINNET_BERLIN_BLOCK: u64 = 12_244_000;
/// London hard fork activation block is 12965000.
pub const MAINNET_LONDON_BLOCK: u64 = 12_965_000;
/// Arrow Glacier hard fork activation block is 13773000.
pub const MAINNET_ARROW_GLACIER_BLOCK: u64 = 13_773_000;
/// Gray Glacier hard fork activation block is 15050000.
pub const MAINNET_GRAY_GLACIER_BLOCK: u64 = 15_050_000;
/// Paris hard fork activation block is 15537394.
pub const MAINNET_PARIS_BLOCK: u64 = 15_537_394;
/// Shanghai hard fork activation block is 17034870.
pub const MAINNET_SHANGHAI_BLOCK: u64 = 17_034_870;
/// Cancun hard fork activation block is 19426587.
pub const MAINNET_CANCUN_BLOCK: u64 = 19_426_587;
/// Prague hard fork activation block is 22431084.
pub const MAINNET_PRAGUE_BLOCK: u64 = 22_431_084;

/// Paris hard fork activation terminal total difficulty is 58_750_000_000_000_000_000_000_U256.
pub const MAINNET_PARIS_TTD: U256 = uint!(58_750_000_000_000_000_000_000_U256);

/// Frontier hard fork activation timestamp is 1438226773.
pub const MAINNET_FRONTIER_TIMESTAMP: u64 = 1_438_226_773;
/// Homestead hard fork activation timestamp is 1457938193.
pub const MAINNET_HOMESTEAD_TIMESTAMP: u64 = 1_457_938_193;
/// Dao hard fork activation timestamp is 1468977640.
pub const MAINNET_DAO_TIMESTAMP: u64 = 1_468_977_640;
/// Tangerine hard fork activation timestamp is 1476753571.
pub const MAINNET_TANGERINE_TIMESTAMP: u64 = 1_476_753_571;
/// Spurious Dragon hard fork activation timestamp is 1479788144.
pub const MAINNET_SPURIOUS_DRAGON_TIMESTAMP: u64 = 1_479_788_144;
/// Byzantium hard fork activation timestamp is 1508131331.
pub const MAINNET_BYZANTIUM_TIMESTAMP: u64 = 1_508_131_331;
/// Constantinople hard fork activation timestamp is 1551340324.
pub const MAINNET_CONSTANTINOPLE_TIMESTAMP: u64 = MAINNET_PETERSBURG_TIMESTAMP;
/// Petersburg hard fork activation timestamp is 1551340324, same as
/// [`MAINNET_PETERSBURG_TIMESTAMP`].
pub const MAINNET_PETERSBURG_TIMESTAMP: u64 = 1_551_340_324;
/// Istanbul hard fork activation timestamp is 1575807909.
pub const MAINNET_ISTANBUL_TIMESTAMP: u64 = 1_575_807_909;
/// Muir Glacier hard fork activation timestamp is 1577953849.
pub const MAINNET_MUIR_GLACIER_TIMESTAMP: u64 = 1_577_953_849;
/// Berlin hard fork activation timestamp is 1618481223.
pub const MAINNET_BERLIN_TIMESTAMP: u64 = 1_618_481_223;
/// London hard fork activation timestamp is 1628166822.
pub const MAINNET_LONDON_TIMESTAMP: u64 = 1_628_166_822;
/// Arrow Glacier hard fork activation timestamp is 1639036523.
pub const MAINNET_ARROW_GLACIER_TIMESTAMP: u64 = 1_639_036_523;
/// Gray Glacier hard fork activation timestamp is 1656586444.
pub const MAINNET_GRAY_GLACIER_TIMESTAMP: u64 = 1_656_586_444;
/// Paris hard fork activation timestamp is 1663224162.
pub const MAINNET_PARIS_TIMESTAMP: u64 = 1_663_224_162;
/// Shanghai hard fork activation timestamp is 1681338455.
pub const MAINNET_SHANGHAI_TIMESTAMP: u64 = 1_681_338_455;
/// Cancun hard fork activation timestamp is 1710338135.
pub const MAINNET_CANCUN_TIMESTAMP: u64 = 1_710_338_135;
/// Prague hard fork activation timestamp is 1746612311.
pub const MAINNET_PRAGUE_TIMESTAMP: u64 = 1_746_612_311;
