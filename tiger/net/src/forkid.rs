//! EIP-2124 fork identifiers, computed and checked exactly as reth does for a Base chain spec.
//!
//! The fork hash is CRC-32 over the genesis hash followed by each passed activation (block
//! numbers first, then timestamps after genesis, each as a big-endian u64, duplicates skipped).
//! `next` is the first activation not yet reached, or 0. base-reth puts the fork id in its eth
//! Status message and, under the ENR key `opel`, in its discv5 record.

use alloy_primitives::B256;
use alloy_rlp::{RlpDecodable, RlpEncodable};

/// Values above this are timestamps when guessing what a remote `next` refers to.
const TIMESTAMP_BEFORE_ETHEREUM_MAINNET: u64 = 1_300_000_000;

/// A fork activation as base-reth's chain spec lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkActivation {
    /// Activates at a block number.
    Block(u64),
    /// Activates at a block timestamp.
    Timestamp(u64),
}

/// The head a fork id is computed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Head {
    /// Block number of the head.
    pub number: u64,
    /// Timestamp of the head.
    pub timestamp: u64,
}

/// An EIP-2124 fork identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct ForkId {
    /// CRC-32 of the genesis hash and all passed fork activations.
    pub hash: [u8; 4],
    /// The next activation, 0 if none is scheduled.
    pub next: u64,
}

/// Why a remote fork id is incompatible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkIdError {
    /// The remote is on our fork but announces a fork we have already passed without it.
    RemoteStale,
    /// The fork hash matches no past or future fork of ours: another chain or a fork split.
    IncompatibleOrStale,
}

/// A chain's fork schedule: the genesis and the activations in base-reth's chain spec order.
///
/// For a Base chain, the order is (from `crates/execution/chainspec/src/upgrades.rs`): Frontier
/// through Gray Glacier and Paris at block 0, Bedrock (its block), then the timestamps of
/// Regolith, Shanghai=Canyon, Ecotone, Fjord, Granite, Holocene, Isthmus (with Prague), Jovian,
/// Azul (with Osaka), Beryl, Cobalt, Denim, Everest, leaving out upgrades that are not scheduled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkSchedule {
    /// The genesis block hash.
    pub genesis_hash: B256,
    /// The genesis timestamp; timestamp forks at or before it count as genesis forks.
    pub genesis_timestamp: u64,
    /// Activations in chain spec order.
    pub forks: Vec<ForkActivation>,
}

impl ForkSchedule {
    /// The schedule of a Base chain: `bedrock_block` and the activation timestamps of the
    /// scheduled upgrades from Regolith on, in upgrade order.
    pub fn base(
        genesis_hash: B256,
        genesis_timestamp: u64,
        bedrock_block: u64,
        upgrade_timestamps: &[u64],
    ) -> Self {
        let mut forks = vec![ForkActivation::Block(0); 14];
        forks.push(ForkActivation::Block(bedrock_block));
        forks.extend(upgrade_timestamps.iter().map(|t| ForkActivation::Timestamp(*t)));
        Self { genesis_hash, genesis_timestamp, forks }
    }

    /// The fork hash checkpoints: (activation value, is_timestamp) in the order they are hashed,
    /// with duplicates removed as reth does.
    fn checkpoints(&self) -> Vec<(u64, bool)> {
        let mut out = Vec::new();
        let mut current = 0u64;
        for fork in &self.forks {
            if let ForkActivation::Block(block) = fork
                && *block != current
            {
                out.push((*block, false));
                current = *block;
            }
        }
        for fork in &self.forks {
            if let ForkActivation::Timestamp(time) = fork
                && *time > self.genesis_timestamp
                && *time != current
            {
                out.push((*time, true));
                current = *time;
            }
        }
        out
    }

    fn passed(head: &Head, (value, is_time): (u64, bool)) -> bool {
        if is_time { head.timestamp >= value } else { head.number >= value }
    }

    /// The fork id at `head`.
    pub fn fork_id(&self, head: Head) -> ForkId {
        let mut crc = Crc32::new();
        crc.update(self.genesis_hash.as_slice());
        for checkpoint in self.checkpoints() {
            if !Self::passed(&head, checkpoint) {
                return ForkId { hash: crc.value(), next: checkpoint.0 };
            }
            crc.update(&checkpoint.0.to_be_bytes());
        }
        ForkId { hash: crc.value(), next: 0 }
    }

    /// Checks a remote fork id against ours at `head`, with EIP-2124's rules.
    pub fn validate(&self, head: Head, remote: ForkId) -> Result<(), ForkIdError> {
        let checkpoints = self.checkpoints();
        // hashes[i] is the fork hash after the first i checkpoints.
        let mut hashes = Vec::with_capacity(checkpoints.len() + 1);
        let mut crc = Crc32::new();
        crc.update(self.genesis_hash.as_slice());
        hashes.push(crc.value());
        for checkpoint in &checkpoints {
            crc.update(&checkpoint.0.to_be_bytes());
            hashes.push(crc.value());
        }
        let passed = checkpoints.iter().take_while(|c| Self::passed(&head, **c)).count();
        let Some(index) = hashes.iter().position(|h| *h == remote.hash) else {
            return Err(ForkIdError::IncompatibleOrStale);
        };
        if index == passed {
            // Same fork. Reject only if the remote announces a next fork we already passed.
            if remote.next != 0 {
                // As alloy's ForkFilter: a value above a timestamp older than Ethereum mainnet
                // is a timestamp, anything smaller a block number.
                let is_time = remote.next > TIMESTAMP_BEFORE_ETHEREUM_MAINNET;
                let reached = if is_time {
                    head.timestamp >= remote.next
                } else {
                    head.number >= remote.next
                };
                if reached {
                    return Err(ForkIdError::RemoteStale);
                }
            }
            return Ok(());
        }
        if index < passed {
            // The remote is behind: it must know the fork that comes next for it.
            return if checkpoints[index].0 == remote.next {
                Ok(())
            } else {
                Err(ForkIdError::RemoteStale)
            };
        }
        // The remote is ahead on our schedule: it is compatible, we will catch up.
        Ok(())
    }
}

/// CRC-32 (IEEE, reflected, polynomial 0xEDB88320), incremental.
#[derive(Debug, Clone, Copy)]
struct Crc32(u32);

impl Crc32 {
    const fn new() -> Self {
        Self(0xffff_ffff)
    }

    fn update(&mut self, data: &[u8]) {
        for byte in data {
            self.0 ^= *byte as u32;
            for _ in 0..8 {
                let mask = (self.0 & 1).wrapping_neg();
                self.0 = (self.0 >> 1) ^ (0xedb8_8320 & mask);
            }
        }
    }

    const fn value(&self) -> [u8; 4] {
        (!self.0).to_be_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;

    fn base_mainnet() -> ForkSchedule {
        ForkSchedule::base(
            b256!("f712aa9241cc24369b143cf6dce85f0902a9731e70d66818a3a5845b296c73dd"),
            1_686_789_347,
            0,
            &[
                1_686_789_347, // Regolith (at genesis, so not hashed)
                1_704_992_401, // Canyon
                1_710_374_401, // Ecotone
                1_720_627_201, // Fjord
                1_726_070_401, // Granite
                1_736_445_601, // Holocene
                1_746_806_401, // Isthmus
                1_764_691_201, // Jovian
            ],
        )
    }

    fn at(timestamp: u64) -> Head {
        Head { number: 0, timestamp }
    }

    #[test]
    fn base_mainnet_fork_ids_match_base_reth() {
        // Expected values from base-reth's `base_mainnet_forkids` test.
        let s = base_mainnet();
        let cases = [
            (0, [0x67, 0xda, 0x02, 0x60], 1_704_992_401),
            (1_704_992_400, [0x67, 0xda, 0x02, 0x60], 1_704_992_401),
            (1_704_992_401, [0x3c, 0x28, 0x3c, 0xb3], 1_710_374_401),
            (1_710_374_401, [0x51, 0xcc, 0x98, 0xb3], 1_720_627_201),
            (1_720_627_201, [0xe4, 0x01, 0x0e, 0xb9], 1_726_070_401),
            (1_726_070_401, [0xbc, 0x38, 0xf9, 0xca], 1_736_445_601),
            (1_736_445_601, [0x3a, 0x2a, 0xf1, 0x83], 1_746_806_401),
            (1_746_806_401, [0x86, 0x72, 0x8b, 0x4e], 1_764_691_201),
        ];
        for (timestamp, hash, next) in cases {
            assert_eq!(s.fork_id(at(timestamp)), ForkId { hash, next }, "at {timestamp}");
        }
    }

    #[test]
    fn ethereum_mainnet_fork_ids_match_eip2124() {
        let mainnet = ForkSchedule {
            genesis_hash: b256!("d4e56740f876aef8c010b86a40d5f56745a118d0906a34e69aec8c0db1cb8fa3"),
            genesis_timestamp: 0,
            forks: [1_150_000, 1_920_000, 2_463_000, 2_675_000, 4_370_000, 7_280_000, 7_280_000]
                .map(ForkActivation::Block)
                .to_vec(),
        };
        let block = |number| Head { number, timestamp: 0 };
        assert_eq!(
            mainnet.fork_id(block(0)),
            ForkId { hash: [0xfc, 0x64, 0xec, 0x04], next: 1_150_000 }
        );
        assert_eq!(
            mainnet.fork_id(block(1_920_000)),
            ForkId { hash: [0x91, 0xd1, 0xf9, 0x48], next: 2_463_000 }
        );
        assert_eq!(
            mainnet.fork_id(block(4_370_000)),
            ForkId { hash: [0xa0, 0x0b, 0xc3, 0x24], next: 7_280_000 }
        );
        assert_eq!(
            mainnet.fork_id(block(7_987_396)),
            ForkId { hash: [0x66, 0x8d, 0xb0, 0xaf], next: 0 }
        );

        // EIP-2124 validation vectors (subset), local head at 7_987_396 (Petersburg).
        let head = block(7_987_396);
        let ok = |hash, next| mainnet.validate(head, ForkId { hash, next });
        assert_eq!(ok([0x66, 0x8d, 0xb0, 0xaf], 0), Ok(()));
        assert_eq!(ok([0x66, 0x8d, 0xb0, 0xaf], u64::MAX), Ok(()));
        // Remote at Byzantium knowing Petersburg is next: fine.
        assert_eq!(ok([0xa0, 0x0b, 0xc3, 0x24], 7_280_000), Ok(()));
        // Remote at Byzantium unaware of Petersburg: stale.
        assert_eq!(ok([0xa0, 0x0b, 0xc3, 0x24], 0), Err(ForkIdError::RemoteStale));
        // Unknown hash: another chain.
        assert_eq!(ok([0xaf, 0xec, 0x6b, 0x27], 0), Err(ForkIdError::IncompatibleOrStale));
        // Local at Byzantium, remote at Petersburg (ahead): compatible.
        assert_eq!(
            mainnet.validate(block(7_279_999), ForkId { hash: [0x66, 0x8d, 0xb0, 0xaf], next: 0 }),
            Ok(())
        );
        // Same fork, remote announces a fork we already passed.
        assert_eq!(
            mainnet.validate(
                block(7_279_999),
                ForkId { hash: [0xa0, 0x0b, 0xc3, 0x24], next: 7_279_999 }
            ),
            Err(ForkIdError::RemoteStale)
        );
    }
}
