use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::{B256, FixedBytes, b256};
use sha2::{Digest, Sha256};

use crate::{Fork, ForkSchedule, LightClient};

/// Built-in constants for joining a public beacon chain's p2p network without a beacon API.
///
/// ponytail: hard-coded per release. A fork or blob-parameter change missing here makes peers
/// reject our status, and the default checkpoint stops being served about five months after it
/// was taken (`MIN_EPOCHS_FOR_BLOCK_REQUESTS`). Upgrade path: load the eth-clients
/// `config.yaml` and persist the latest finalized root across restarts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeaconNetwork {
    /// Fork versions, genesis and slot timing.
    pub schedule: ForkSchedule,
    /// First epoch of Fulu, from which fork digests mix in the blob parameters.
    pub fulu_epoch: u64,
    /// `(epoch, max_blobs_per_block)` from Electra on: Electra's limit, then `BLOB_SCHEDULE`.
    pub blob_schedule: Vec<(u64, u64)>,
    /// Discovery v5 bootnode ENRs.
    pub bootnodes: Vec<&'static str>,
    /// Finalized epoch-boundary block root to bootstrap from by default.
    pub checkpoint: B256,
}

impl BeaconNetwork {
    /// Returns the constants for the beacon chain of the L1 `chain_id`, if built in.
    pub fn for_chain(chain_id: u64) -> Option<Self> {
        match chain_id {
            1 => Some(Self::mainnet()),
            11_155_111 => Some(Self::sepolia()),
            _ => None,
        }
    }

    /// Ethereum mainnet, from `eth-clients/mainnet`.
    pub fn mainnet() -> Self {
        Self {
            schedule: ForkSchedule {
                genesis_validators_root: b256!(
                    "0x4b363db94e286120d76eb905340fdd4e54bfe9f06bf33ff6cf5ad27f511bfe95"
                ),
                genesis_time: 1_606_824_023,
                seconds_per_slot: 12,
                forks: Self::forks(&[
                    (0x00000000, 0),
                    (0x01000000, 74_240),
                    (0x02000000, 144_896),
                    (0x03000000, 194_048),
                    (0x04000000, 269_568),
                    (0x05000000, 364_032),
                    (0x06000000, 411_392),
                ]),
                gloas_epoch: u64::MAX,
            },
            fulu_epoch: 411_392,
            blob_schedule: vec![(364_032, 9), (412_672, 15), (419_072, 21)],
            bootnodes: vec![
                "enr:-Iu4QLm7bZGdAt9NSeJG0cEnJohWcQTQaI9wFLu3Q7eHIDfrI4cwtzvEW3F3VbG9XdFXlrHyFGeXPn9snTCQJ9bnMRABgmlkgnY0gmlwhAOTJQCJc2VjcDI1NmsxoQIZdZD6tDYpkpEfVo5bgiU8MGRjhcOmHGD2nErK0UKRrIN0Y3CCIyiDdWRwgiMo",
                "enr:-Iu4QEDJ4Wa_UQNbK8Ay1hFEkXvd8psolVK6OhfTL9irqz3nbXxxWyKwEplPfkju4zduVQj6mMhUCm9R2Lc4YM5jPcIBgmlkgnY0gmlwhANrfESJc2VjcDI1NmsxoQJCYz2-nsqFpeEj6eov9HSi9QssIVIVNr0I89J1vXM9foN0Y3CCIyiDdWRwgiMo",
                "enr:-Ku4QImhMc1z8yCiNJ1TyUxdcfNucje3BGwEHzodEZUan8PherEo4sF7pPHPSIB1NNuSg5fZy7qFsjmUKs2ea1Whi0EBh2F0dG5ldHOIAAAAAAAAAACEZXRoMpD1pf1CAAAAAP__________gmlkgnY0gmlwhBLf22SJc2VjcDI1NmsxoQOVphkDqal4QzPMksc5wnpuC3gvSC8AfbFOnZY_On34wIN1ZHCCIyg",
                "enr:-Ku4QP2xDnEtUXIjzJ_DhlCRN9SN99RYQPJL92TMlSv7U5C1YnYLjwOQHgZIUXw6c-BvRg2Yc2QsZxxoS_pPRVe0yK8Bh2F0dG5ldHOIAAAAAAAAAACEZXRoMpD1pf1CAAAAAP__________gmlkgnY0gmlwhBLf22SJc2VjcDI1NmsxoQMeFF5GrS7UZpAH2Ly84aLK-TyvH-dRo0JM1i8yygH50YN1ZHCCJxA",
                "enr:-Ku4QPp9z1W4tAO8Ber_NQierYaOStqhDqQdOPY3bB3jDgkjcbk6YrEnVYIiCBbTxuar3CzS528d2iE7TdJsrL-dEKoBh2F0dG5ldHOIAAAAAAAAAACEZXRoMpD1pf1CAAAAAP__________gmlkgnY0gmlwhBLf22SJc2VjcDI1NmsxoQMw5fqqkw2hHC4F5HZZDPsNmPdB1Gi8JPQK7pRc9XHh-oN1ZHCCKvg",
                "enr:-Le4QPUXJS2BTORXxyx2Ia-9ae4YqA_JWX3ssj4E_J-3z1A-HmFGrU8BpvpqhNabayXeOZ2Nq_sbeDgtzMJpLLnXFgAChGV0aDKQtTA_KgEAAAAAIgEAAAAAAIJpZIJ2NIJpcISsaa0Zg2lwNpAkAIkHAAAAAPA8kv_-awoTiXNlY3AyNTZrMaEDHAD2JKYevx89W0CcFJFiskdcEzkH_Wdv9iW42qLK79ODdWRwgiMohHVkcDaCI4I",
                "enr:-Le4QLHZDSvkLfqgEo8IWGG96h6mxwe_PsggC20CL3neLBjfXLGAQFOPSltZ7oP6ol54OvaNqO02Rnvb8YmDR274uq8ChGV0aDKQtTA_KgEAAAAAIgEAAAAAAIJpZIJ2NIJpcISLosQxg2lwNpAqAX4AAAAAAPA8kv_-ax65iXNlY3AyNTZrMaEDBJj7_dLFACaxBfaI8KZTh_SSJUjhyAyfshimvSqo22WDdWRwgiMohHVkcDaCI4I",
                "enr:-Le4QH6LQrusDbAHPjU_HcKOuMeXfdEB5NJyXgHWFadfHgiySqeDyusQMvfphdYWOzuSZO9Uq2AMRJR5O4ip7OvVma8BhGV0aDKQtTA_KgEAAAAAIgEAAAAAAIJpZIJ2NIJpcISLY9ncg2lwNpAkAh8AgQIBAAAAAAAAAAmXiXNlY3AyNTZrMaECDYCZTZEksF-kmgPholqgVt8IXr-8L7Nu7YrZ7HUpgxmDdWRwgiMohHVkcDaCI4I",
                "enr:-Le4QIqLuWybHNONr933Lk0dcMmAB5WgvGKRyDihy1wHDIVlNuuztX62W51voT4I8qD34GcTEOTmag1bcdZ_8aaT4NUBhGV0aDKQtTA_KgEAAAAAIgEAAAAAAIJpZIJ2NIJpcISLY04ng2lwNpAkAh8AgAIBAAAAAAAAAA-fiXNlY3AyNTZrMaEDscnRV6n1m-D9ID5UsURk0jsoKNXt1TIrj8uKOGW6iluDdWRwgiMohHVkcDaCI4I",
                "enr:-KG4QIH7EyRfHFmXLZaG6j0bMvow18k63nKWPfppuKh6iBHMPGM93HX3W3hl7jZdv_Hz8yd_jXVHX2loStJKOZqNRu0BgmlkgnY0gmlwhNRj2kKDaXA2kCoAHKALAA0CAAAAAAAAAF6Jc2VjcDI1NmsxoQMngQgKvKJR49-jugrZ_05LhAHbUlSCwZ_HyqVCb3SXYoN1ZHCCTrqEdWRwNoJOug",
                "enr:-KG4QJIyNiCpvXrnK8dugxmFckcIduvuQraNlX0GlKwF-XyPeZ-ZG7_yHhsr08K85X1utedECuRXhXiPYJoMogs8ai4BgmlkgnY0gmlwhIHUpj2DaXA2kCYEqIAABAHQAAAAA2GdUACJc2VjcDI1NmsxoQNQbzy36fddhPGH1I6D5rQyj8zUDGWQAkkWS37qBLB_yIN1ZHCCIyiEdWRwNoIjKA",
                "enr:-KG4QDU7s2q7Cl_qGr2BucsrhN1bKywwstBqMLUtR6f_pOejVAjXLAQsFBOCSgALH_Oy7eshQ2ic7CbFwRZIxZGIMMsBgmlkgnY0gmlwhJB-_BiDaXA2kCQAYYABAADQAAAAAYEgYAGJc2VjcDI1NmsxoQLACT5Njs8OjnCiL4_11wgqunT0BPxQ5PndoKoF6ICNWYN1ZHCCIyiEdWRwNoIjKA",
                "enr:-KG4QD_qJswcSJKmI_kjUZ-QbUuLUzniIakQbJKgh4YFzluGEKikFzaVoIbS7jpbw2K9hjmWTn7Ha3zyNIm0Ysu-dSgBgmlkgnY0gmlwhLKc14yDaXA2kCoBBP8A9DxKAAAAAAAAAAGJc2VjcDI1NmsxoQOVp1YSg2ZkGenRZi4iGebFira2xZrER7F_WW55-Rd3boN1ZHCCIyiEdWRwNoIjKA",
                "enr:-KG4QIFbm7kLOmOeiDwjSxLXEN0Ms4advV742CYLpGUCndm2XvAnM9uKVdWpydb8Gpstk44eFDlZlPDyV2gX7uH7KHgBgmlkgnY0gmlwhAXfXlGDaXA2kCoBBP8C8BytAAAAAAAAAAGJc2VjcDI1NmsxoQIAizAK-MKs-s08GzGzoPVIaBmfdKqosqxSkwxUuV2UnIN1ZHCCIyiEdWRwNoIjKA",
                "enr:-LK4QA8FfhaAjlb_BXsXxSfiysR7R52Nhi9JBt4F8SPssu8hdE1BXQQEtVDC3qStCW60LSO7hEsVHv5zm8_6Vnjhcn0Bh2F0dG5ldHOIAAAAAAAAAACEZXRoMpC1MD8qAAAAAP__________gmlkgnY0gmlwhAN4aBKJc2VjcDI1NmsxoQJerDhsJ-KxZ8sHySMOCmTO6sHM3iCFQ6VMvLTe948MyYN0Y3CCI4yDdWRwgiOM",
                "enr:-LK4QKWrXTpV9T78hNG6s8AM6IO4XH9kFT91uZtFg1GcsJ6dKovDOr1jtAAFPnS2lvNltkOGA9k29BUN7lFh_sjuc9QBh2F0dG5ldHOIAAAAAAAAAACEZXRoMpC1MD8qAAAAAP__________gmlkgnY0gmlwhANAdd-Jc2VjcDI1NmsxoQLQa6ai7y9PMN5hpLe5HmiJSlYzMuzP7ZhwRiwHvqNXdoN0Y3CCI4yDdWRwgiOM",
                "enr:-IS4QPi-onjNsT5xAIAenhCGTDl4z-4UOR25Uq-3TmG4V3kwB9ljLTb_Kp1wdjHNj-H8VVLRBSSWVZo3GUe3z6k0E-IBgmlkgnY0gmlwhKB3_qGJc2VjcDI1NmsxoQMvAfgB4cJXvvXeM6WbCG86CstbSxbQBSGx31FAwVtOTYN1ZHCCIyg",
                "enr:-KG4QPUf8-g_jU-KrwzG42AGt0wWM1BTnQxgZXlvCEIfTQ5hSmptkmgmMbRkpOqv6kzb33SlhPHJp7x4rLWWiVq5lSECgmlkgnY0gmlwhFPlR9KDaXA2kCoGxcAJAAAVAAAAAAAAABCJc2VjcDI1NmsxoQLdUv9Eo9sxCt0tc_CheLOWnX59yHJtkBSOL7kpxdJ6GYN1ZHCCIyiEdWRwNoIjKA",
            ],
            // Slot 15_535_744 (epoch 485_492).
            checkpoint: b256!("0xdddc4848d8d4c7a3bbabb29e704ccd2ca206fb574dc54374ca7ee51f890e8fb9"),
        }
    }

    /// Sepolia, from `eth-clients/sepolia`.
    pub fn sepolia() -> Self {
        Self {
            schedule: ForkSchedule {
                genesis_validators_root: b256!(
                    "0xd8ea171f3c94aea21ebc42a1ed61052acf3f9209c00e4efbaaddac09ed9b8078"
                ),
                genesis_time: 1_655_733_600,
                seconds_per_slot: 12,
                forks: Self::forks(&[
                    (0x90000069, 0),
                    (0x90000070, 50),
                    (0x90000071, 100),
                    (0x90000072, 56_832),
                    (0x90000073, 132_608),
                    (0x90000074, 222_464),
                    (0x90000075, 272_640),
                    (0x90000076, 353_024),
                ]),
                gloas_epoch: 353_024,
            },
            fulu_epoch: 272_640,
            blob_schedule: vec![(222_464, 9), (274_176, 15), (275_712, 21)],
            bootnodes: vec![
                "enr:-KG4QCK5YeEoL55e2hoS6nCregwx0Zd6NQ3rhVDfeg5Q8ozUNmUYTskpmuqo2WYFo3z24-cWC9qrU3yYKDSJ299lh8sBgmlkgnY0gmlwhNRj2kKDaXA2kCoAHKALAA0CAAAAAAAAAF6Jc2VjcDI1NmsxoQLzqnTxu_nlM8V_semraAjfbH9HZpcbVUCXH2qanVPsroN1ZHCCTruEdWRwNoJOuw",
                "enr:-KG4QF0FvRL7Eqc4oURFhOkS0V6guntLnw54dYgTruM7z9TAMWhRpCrxZ7Pd536-q4qlwdW13czht8_UEWwGyJesu1gBgmlkgnY0gmlwhIHUpj2DaXA2kCYEqIAABAHQAAAAA2GdUACJc2VjcDI1NmsxoQL5iA7gNCs4SDmnXz8Isacq0EJbJfvV_uJlccoHxHU5ZYN1ZHCCI4yEdWRwNoIjjA",
                "enr:-KG4QI4reJ1D_BwCwg6EKAuo2HEWoIVVNjphtOTJP2gzPVLSTYM3NFwp39TAKw-7QiQ2NVts7DK4rjJR2BEcAwh3BckBgmlkgnY0gmlwhJB-_BiDaXA2kCQAYYABAADQAAAAAYEgYAGJc2VjcDI1NmsxoQLXzHa5K0M3F4pqErIhleMByA8votAUhUXylRT6SWX2HoN1ZHCCI4yEdWRwNoIjjA",
                "enr:-KG4QI1KOrogxK8u3Oc0QLdgkNTbAPuAMtixa6Vx05N-Bl7IOCVURUvqZ2N6JA97ts7YG1B4D3hQvZ9uQlCPYVjy1DABgmlkgnY0gmlwhLKc14yDaXA2kCoBBP8A9DxKAAAAAAAAAAGJc2VjcDI1NmsxoQLB0ZhHGRmVwXja_4o-GRN1VVJYRI11F45CTAlu1s00Q4N1ZHCCI4yEdWRwNoIjjA",
                "enr:-KG4QKU4YfXfB3_BVI7u0VvXnSJI6cqo-tCRm-Ggh3XxBImcYvrUoKUDbIjJjG9-QphuH6gzScdf69t597M0nHut4kABgmlkgnY0gmlwhAXfXlGDaXA2kCoBBP8C8BytAAAAAAAAAAGJc2VjcDI1NmsxoQL0y83XKpPgvY7XReWg9S8bdI2UUIe5dE0N7rjOIIj4xYN1ZHCCI4yEdWRwNoIjjA",
                "enr:-Iu4QKvMF7Ne_RSQoZGvavTuZ1QA5_Pgeb0nq_hrjhU8s0UDV3KhcMXJkGwOWhsDGZL3ISjL0CTP-hfoTjZtEtCEwR4BgmlkgnY0gmlwhAOAaySJc2VjcDI1NmsxoQNta5b_bexSSwwrGW2Re24MjfMntzFd0f2SAxQtMj3ueYN0Y3CCIyiDdWRwgiMo",
                "enr:-KG4QJejf8KVtMeAPWFhN_P0c4efuwu1pZHELTveiXUeim6nKYcYcMIQpGxxdgT2Xp9h-M5pr9gn2NbbwEAtxzu50Y8BgmlkgnY0gmlwhEEVkQCDaXA2kCoBBPnAEJg4AAAAAAAAAAGJc2VjcDI1NmsxoQLEh_eVvk07AQABvLkTGBQTrrIOQkzouMgSBtNHIRUxOIN1ZHCCIyiEdWRwNoIjKA",
            ],
            // Slot 11_459_936 (epoch 358_123).
            checkpoint: b256!("0xcd2b7068f9fccea690e790a2461c444cd0b6ee93e12eb51ab18ec999f3cfda14"),
        }
    }

    fn forks(forks: &[(u32, u64)]) -> Vec<Fork> {
        forks
            .iter()
            .map(|&(version, epoch)| Fork {
                current_version: FixedBytes(version.to_be_bytes()),
                epoch,
            })
            .collect()
    }

    /// Returns the slot at the current wall-clock time.
    pub fn current_slot(&self) -> u64 {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        now.saturating_sub(self.schedule.genesis_time) / self.schedule.seconds_per_slot
    }

    /// Returns the epoch at the current wall-clock time.
    pub fn current_epoch(&self) -> u64 {
        self.current_slot() / LightClient::SLOTS_PER_EPOCH
    }

    /// Returns `compute_fork_digest` at `epoch`.
    pub fn fork_digest(&self, epoch: u64) -> FixedBytes<4> {
        let mut digest = self.schedule.fork_data_root(epoch);
        if epoch >= self.fulu_epoch {
            let (blob_epoch, max_blobs) = self
                .blob_schedule
                .iter()
                .rev()
                .find(|(blob_epoch, _)| *blob_epoch <= epoch)
                .copied()
                .unwrap_or_default();
            let mask = Sha256::new()
                .chain_update(blob_epoch.to_le_bytes())
                .chain_update(max_blobs.to_le_bytes())
                .finalize();
            digest.iter_mut().zip(mask).for_each(|(byte, mask)| *byte ^= mask);
        }
        FixedBytes::from_slice(&digest[..4])
    }

    /// Returns whether objects under fork `digest` use the Gloas layout, or `None` if the
    /// digest belongs to no fork of this network.
    pub fn is_gloas_digest(&self, digest: FixedBytes<4>) -> Option<bool> {
        self.schedule
            .forks
            .iter()
            .map(|fork| fork.epoch)
            .chain(self.blob_schedule.iter().map(|(epoch, _)| *epoch))
            .find(|epoch| self.fork_digest(*epoch) == digest)
            .map(|epoch| epoch >= self.schedule.gloas_epoch)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::fixed_bytes;

    use super::*;

    #[test]
    fn fork_digests_match_live_enrs() {
        let mainnet = BeaconNetwork::mainnet();
        let digest = fixed_bytes!("0x8c9f62fe");
        assert_eq!(mainnet.fork_digest(484_939), digest);
        assert_eq!(mainnet.is_gloas_digest(digest), Some(false));

        let sepolia = BeaconNetwork::sepolia();
        let digest = fixed_bytes!("0x669e6c11");
        assert_eq!(sepolia.fork_digest(357_570), digest);
        assert_eq!(sepolia.is_gloas_digest(digest), Some(true));
        assert_eq!(sepolia.is_gloas_digest(fixed_bytes!("0x8c9f62fe")), None);
    }

    #[test]
    fn bootnodes_parse() {
        for network in [BeaconNetwork::mainnet(), BeaconNetwork::sepolia()] {
            for bootnode in network.bootnodes {
                bootnode.parse::<discv5::Enr>().unwrap();
            }
        }
    }
}
