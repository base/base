//! The peer table: signed records learned through peer exchange, bucketed so one source of
//! records cannot crowd out the rest, and the choice of whom to dial.
//!
//! The scheme follows Bitcoin Core's address manager in a smaller form. A record heard from a
//! peer goes into a "new" bucket chosen by the source's network group and the record's group, so
//! all the records one source group sends land in at most [`NEW_BUCKETS_PER_SOURCE_GROUP`] of
//! the [`NEW_BUCKETS`] buckets. A new record never evicts an entry that has not failed, so
//! flooding the table replaces only dead entries. A peer we connected to moves to a "tried"
//! bucket chosen by its own group. Outbound dials go to at most one peer per group, alternating
//! between tried and new entries. Bootnodes are fixed entries that are never evicted and are
//! redialed whenever no outbound connection reaches them.
//!
//! Limits (documented in README.md, "Discovery"): an attacker holding addresses in G distinct
//! groups can fill at most G outbound slots, and at most G × 8 of the new buckets. Sybils inside
//! one group gain nothing over one node. An attacker with addresses in more groups than the node
//! has outbound slots, who also answers every peer exchange the node makes, can still win all
//! slots over time; bootnodes and trusted peers are the anchor against that.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
};

use secp256k1::PublicKey;

use crate::{node::Rng, wire::PeerRecord};

/// Buckets of records heard from others.
pub const NEW_BUCKETS: usize = 64;
/// How many new buckets the records from one source group can reach.
pub const NEW_BUCKETS_PER_SOURCE_GROUP: u64 = 8;
/// Buckets of peers we connected to.
pub const TRIED_BUCKETS: usize = 16;
/// Entries per bucket.
pub const BUCKET_SIZE: usize = 8;
/// An entry that failed this many dials in a row without a success can be evicted.
const TERRIBLE_ATTEMPTS: u32 = 3;
/// Dial backoff: 10 s doubled per failed attempt, at most 10 minutes.
const BACKOFF_BASE_MS: u64 = 10_000;
const BACKOFF_MAX_MS: u64 = 600_000;

/// A peer's identity: its compressed static public key.
pub type PeerId = [u8; 33];

/// The network group of an address: the /16 of a public IPv4 address, the /32 of a public IPv6
/// address. Loopback and private addresses are each their own group (the whole socket address),
/// so a local devnet whose nodes share 127.0.0.1 can still connect them all.
pub fn group(addr: &SocketAddr) -> u64 {
    let local = match addr.ip() {
        IpAddr::V4(ip) => {
            ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified()
        }
        IpAddr::V6(ip) => {
            ip.is_loopback() || ip.is_unspecified() || (ip.segments()[0] & 0xfe00) == 0xfc00
        }
    };
    let mut h = Fnv::new();
    if local {
        h.write(b"local");
        match addr.ip() {
            IpAddr::V4(ip) => h.write(&ip.octets()),
            IpAddr::V6(ip) => h.write(&ip.octets()),
        }
        h.write(&addr.port().to_be_bytes());
    } else {
        match addr.ip() {
            IpAddr::V4(ip) => h.write(&ip.octets()[..2]),
            IpAddr::V6(ip) => h.write(&ip.octets()[..4]),
        }
    }
    h.finish()
}

/// FNV-1a, salted by the table's secret where bucket placement must not be predictable.
struct Fnv(u64);

impl Fnv {
    const fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    const fn finish(&self) -> u64 {
        self.0
    }
}

fn salted(secret: u64, parts: &[u64]) -> u64 {
    let mut h = Fnv::new();
    h.write(&secret.to_be_bytes());
    for p in parts {
        h.write(&p.to_be_bytes());
    }
    h.finish()
}

/// One known peer.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The peer's key.
    pub key: PublicKey,
    /// Where to dial it.
    pub addr: SocketAddr,
    /// Its signed record, `None` for a bootnode given by configuration.
    pub record: Option<PeerRecord>,
    /// Failed dials since the last success.
    pub attempts: u32,
    /// When it was last dialed (ms).
    pub last_attempt: Option<u64>,
    /// When a connection last succeeded (ms), 0 if never.
    pub last_success: u64,
}

impl Entry {
    const fn terrible(&self) -> bool {
        self.attempts >= TERRIBLE_ATTEMPTS && self.last_success == 0
    }

    fn ready(&self, now: u64) -> bool {
        let Some(last) = self.last_attempt else { return true };
        let backoff =
            BACKOFF_BASE_MS.saturating_mul(1 << self.attempts.min(16)).min(BACKOFF_MAX_MS);
        now >= last.saturating_add(backoff)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Location {
    New(usize),
    Tried(usize),
    Fixed,
}

/// The table.
#[derive(Debug)]
pub struct PeerTable {
    secret: u64,
    new: Vec<Vec<Entry>>,
    tried: Vec<Vec<Entry>>,
    fixed: Vec<Entry>,
    index: HashMap<PeerId, Location>,
    banned: HashMap<PeerId, u64>,
    banned_ips: HashMap<IpAddr, u64>,
}

impl PeerTable {
    /// An empty table with fixed bootnode entries. `secret` salts bucket placement.
    pub fn new(secret: u64, bootnodes: &[(PublicKey, SocketAddr)]) -> Self {
        let mut table = Self {
            secret,
            new: vec![Vec::new(); NEW_BUCKETS],
            tried: vec![Vec::new(); TRIED_BUCKETS],
            fixed: Vec::new(),
            index: HashMap::new(),
            banned: HashMap::new(),
            banned_ips: HashMap::new(),
        };
        for (key, addr) in bootnodes {
            table.index.insert(key.serialize(), Location::Fixed);
            table.fixed.push(Entry {
                key: *key,
                addr: *addr,
                record: None,
                attempts: 0,
                last_attempt: None,
                last_success: 0,
            });
        }
        table
    }

    fn new_bucket(&self, source: &SocketAddr, addr: &SocketAddr) -> usize {
        let source_group = group(source);
        let offset = salted(self.secret, &[group(addr)]) % NEW_BUCKETS_PER_SOURCE_GROUP;
        (salted(self.secret, &[source_group, offset]) % NEW_BUCKETS as u64) as usize
    }

    fn tried_bucket(&self, addr: &SocketAddr) -> usize {
        (salted(self.secret, &[group(addr), 1]) % TRIED_BUCKETS as u64) as usize
    }

    fn entry_mut(&mut self, id: &PeerId) -> Option<&mut Entry> {
        let location = *self.index.get(id)?;
        let list = match location {
            Location::New(b) => &mut self.new[b],
            Location::Tried(b) => &mut self.tried[b],
            Location::Fixed => &mut self.fixed,
        };
        list.iter_mut().find(|e| e.key.serialize() == *id)
    }

    /// Adds or updates a verified record heard from `source`. Returns whether the table changed.
    pub fn add_learned(&mut self, record: PeerRecord, source: &SocketAddr) -> bool {
        let id = record.key.serialize();
        if let Some(entry) = self.entry_mut(&id) {
            let newer = entry.record.as_ref().is_none_or(|r| record.seq > r.seq);
            if newer && entry.record.is_some() {
                entry.addr = record.addr;
                entry.record = Some(record);
                return true;
            }
            return false;
        }
        let bucket = self.new_bucket(source, &record.addr);
        let list = &mut self.new[bucket];
        if list.len() >= BUCKET_SIZE {
            let Some(victim) = list.iter().position(Entry::terrible) else { return false };
            let removed = list.swap_remove(victim);
            self.index.remove(&removed.key.serialize());
        }
        let list = &mut self.new[bucket];
        list.push(Entry {
            key: record.key,
            addr: record.addr,
            record: Some(record),
            attempts: 0,
            last_attempt: None,
            last_success: 0,
        });
        self.index.insert(id, Location::New(bucket));
        true
    }

    /// Records a dial attempt.
    pub fn attempted(&mut self, id: &PeerId, now: u64) {
        if let Some(entry) = self.entry_mut(id) {
            entry.last_attempt = Some(now);
        }
    }

    /// Records a failed dial.
    pub fn failed(&mut self, id: &PeerId) {
        if let Some(entry) = self.entry_mut(id) {
            entry.attempts = entry.attempts.saturating_add(1);
        }
    }

    /// Records a successful outbound connection: moves the entry to a tried bucket, pushing the
    /// stalest entry of a full bucket back to the new table.
    pub fn succeeded(&mut self, id: &PeerId, now: u64) {
        let Some(location) = self.index.get(id).copied() else { return };
        if let Some(entry) = self.entry_mut(id) {
            entry.attempts = 0;
            entry.last_success = now;
        }
        let Location::New(bucket) = location else { return };
        let position =
            self.new[bucket].iter().position(|e| e.key.serialize() == *id).expect("indexed");
        let entry = self.new[bucket].swap_remove(position);
        let tried = self.tried_bucket(&entry.addr);
        if self.tried[tried].len() >= BUCKET_SIZE {
            let stalest = (0..self.tried[tried].len())
                .min_by_key(|i| self.tried[tried][*i].last_success)
                .expect("bucket is full");
            let demoted = self.tried[tried].swap_remove(stalest);
            let back = self.new_bucket(&demoted.addr, &demoted.addr);
            if self.new[back].len() < BUCKET_SIZE {
                self.index.insert(demoted.key.serialize(), Location::New(back));
                self.new[back].push(demoted);
            } else {
                self.index.remove(&demoted.key.serialize());
            }
        }
        self.index.insert(*id, Location::Tried(tried));
        self.tried[tried].push(entry);
    }

    /// Bans a peer and its IP until `until` (ms).
    pub fn ban(&mut self, id: &PeerId, ip: IpAddr, until: u64) {
        self.banned.insert(*id, until);
        self.banned_ips.insert(ip, until);
    }

    /// Whether a peer or its IP is banned at `now`.
    pub fn is_banned(&self, id: &PeerId, ip: &IpAddr, now: u64) -> bool {
        self.banned.get(id).is_some_and(|until| now < *until)
            || self.banned_ips.get(ip).is_some_and(|until| now < *until)
    }

    /// Forgets expired bans.
    pub fn expire_bans(&mut self, now: u64) {
        self.banned.retain(|_, until| now < *until);
        self.banned_ips.retain(|_, until| now < *until);
    }

    /// A peer to dial: a fixed bootnode first when one is ready and not connected, else an entry
    /// from tried or new (alternating at random), whose group no outbound connection uses.
    pub fn select_dial(
        &self,
        now: u64,
        rng: &mut Rng,
        connected: &dyn Fn(&PeerId) -> bool,
        groups_in_use: &dyn Fn(u64) -> bool,
    ) -> Option<(PublicKey, SocketAddr)> {
        let usable = |e: &Entry| {
            let id = e.key.serialize();
            e.ready(now) && !connected(&id) && !self.is_banned(&id, &e.addr.ip(), now)
        };
        if let Some(e) = self.fixed.iter().find(|e| usable(e)) {
            return Some((e.key, e.addr));
        }
        for _ in 0..64 {
            let buckets = if rng.below(2) == 0 { &self.tried } else { &self.new };
            let bucket = &buckets[rng.below(buckets.len() as u64) as usize];
            if bucket.is_empty() {
                continue;
            }
            let e = &bucket[rng.below(bucket.len() as u64) as usize];
            if usable(e) && !groups_in_use(group(&e.addr)) {
                return Some((e.key, e.addr));
            }
        }
        None
    }

    /// Up to `n` random signed records to answer a peer exchange, tried ones first.
    pub fn sample(&self, n: usize, rng: &mut Rng) -> Vec<PeerRecord> {
        let mut out = Vec::new();
        for list in [&self.tried, &self.new] {
            let mut all: Vec<&PeerRecord> =
                list.iter().flatten().filter_map(|e| e.record.as_ref()).collect();
            while out.len() < n && !all.is_empty() {
                let i = rng.below(all.len() as u64) as usize;
                out.push(all.swap_remove(i).clone());
            }
        }
        out
    }

    /// The number of entries in (new, tried).
    pub fn len(&self) -> (usize, usize) {
        (self.new.iter().map(Vec::len).sum(), self.tried.iter().map(Vec::len).sum())
    }

    /// Whether the table holds no learned entries.
    pub fn is_empty(&self) -> bool {
        self.len() == (0, 0)
    }

    /// Iterates over every entry (for tests and simulations).
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.fixed.iter().chain(self.tried.iter().flatten()).chain(self.new.iter().flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secp256k1::{SECP256K1, SecretKey};

    fn record(i: u32, ip: [u8; 4]) -> PeerRecord {
        let mut bytes = [1u8; 32];
        bytes[..4].copy_from_slice(&i.to_be_bytes());
        let secret = SecretKey::from_byte_array(bytes).unwrap();
        PeerRecord::signed(&secret, &[0; 32], SocketAddr::from((ip, 30400)), 1, 0)
    }

    #[test]
    fn one_source_group_reaches_few_buckets() {
        let mut table = PeerTable::new(42, &[]);
        let attacker: SocketAddr = "66.1.0.1:1".parse().unwrap();
        for i in 0..10_000u32 {
            let ip = [66, 1, (i >> 8) as u8, i as u8];
            table.add_learned(record(i, ip), &attacker);
        }
        // Records from one source group across many record groups... here one record group
        // (66.1/16), so one bucket.
        assert!(table.len().0 <= BUCKET_SIZE);
        let mut table = PeerTable::new(42, &[]);
        for i in 0..10_000u32 {
            let ip = [(i % 200) as u8 + 1, (i / 200) as u8, 0, 1];
            table.add_learned(record(i, ip), &attacker);
        }
        assert!(
            table.len().0 <= NEW_BUCKETS_PER_SOURCE_GROUP as usize * BUCKET_SIZE,
            "{:?}",
            table.len()
        );
    }

    #[test]
    fn floods_do_not_evict_live_entries() {
        let mut table = PeerTable::new(7, &[]);
        let honest_source: SocketAddr = "20.0.0.1:1".parse().unwrap();
        let honest = record(1, [30, 0, 0, 1]);
        table.add_learned(honest.clone(), &honest_source);
        // An attacker in the same source group floods the same bucket.
        for i in 2..5000u32 {
            table.add_learned(
                record(i, [30, 0, (i >> 8) as u8, i as u8]),
                &"20.0.9.9:1".parse().unwrap(),
            );
        }
        assert!(table.entries().any(|e| e.key == honest.key));
    }

    #[test]
    fn dials_one_peer_per_group_and_backs_off() {
        let boot = SecretKey::from_byte_array([9; 32]).unwrap();
        let boot_pub = PublicKey::from_secret_key(SECP256K1, &boot);
        let mut table = PeerTable::new(1, &[(boot_pub, "8.8.8.8:1".parse().unwrap())]);
        let mut rng = Rng::new(3);
        let first = table.select_dial(0, &mut rng, &|_| false, &|_| false).unwrap();
        assert_eq!(first.0, boot_pub);
        table.attempted(&boot_pub.serialize(), 0);
        table.failed(&boot_pub.serialize());
        assert!(table.select_dial(1000, &mut rng, &|_| false, &|_| false).is_none());
        assert!(table.select_dial(20_001, &mut rng, &|_| false, &|_| false).is_some());
        table.add_learned(record(5, [40, 0, 0, 1]), &"1.1.1.1:1".parse().unwrap());
        let g = group(&"40.0.0.1:30400".parse().unwrap());
        let chosen =
            table.select_dial(1000, &mut rng, &|id| *id == boot_pub.serialize(), &|x| x == g);
        assert!(chosen.is_none());
    }
}
