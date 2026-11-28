//! Status handshake and fork id targets.
//!
//! Status: tiger's side is `handshake` in peer.rs (`Status::decode_payload`, then network id,
//! genesis and `ForkSchedule::validate`, rejecting with `UselessPeer`; a Status that does not
//! decode ends the session without a Disconnect). base-reth's side runs the real
//! `EthereumEthHandshake::eth_handshake` over an in-memory stream and records the Disconnect it
//! sends. Both check against the same [`LocalChain`].
//!
//! Fork id: tiger's `ForkSchedule::{fork_id, validate}` against alloy's `ForkFilter` (what
//! base-reth's handshake uses) over random schedules, heads and remote ids.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use alloy_chains::Chain;
use alloy_primitives::{B256, U256};
use base_tiger_net::{ForkActivation, ForkId, ForkSchedule, Head, Status};
use bytes::{Bytes, BytesMut};
use futures::{Sink, Stream};
use reth_eth_wire::{
    CanDisconnect, DisconnectReason, EthVersion, UnifiedStatus, errors::P2PStreamError,
    handshake::EthereumEthHandshake,
};

use super::{
    chain::{LocalChain, filter, reth_id, schedule_ids},
    decode::{Seed, Verdict},
    rlp::{Item, Node, b, int, l},
    rng::Rng,
};

fn fork_id_node(id: ForkId) -> Node {
    l(vec![b(id.hash.to_vec()), int(id.next)])
}

/// A remote fork id: one of the schedule's, a tweaked one, or random.
fn remote_fork_id(rng: &mut Rng, ids: &[ForkId], current: ForkId) -> ForkId {
    match rng.below(6) {
        0 => current,
        1 => ForkId { next: 0, ..current },
        2 => ForkId { next: rng.edge_u64(), ..*rng.pick(ids) },
        3 => ForkId { hash: rng.array(), next: rng.edge_u64() },
        _ => *rng.pick(ids),
    }
}

pub(crate) fn generate_status(rng: &mut Rng) -> Seed {
    let chain = LocalChain::fuzz();
    let version = if rng.one_in(2) { 68 } else { 69 };
    let wire_version =
        if rng.one_in(8) { *rng.pick(&[66u64, 67, 68, 69, 70, 0, 0x100]) } else { version };
    let network = if rng.one_in(8) { rng.edge_u64() } else { chain.network_id };
    let genesis =
        if rng.one_in(8) { B256::from(rng.array::<32>()) } else { chain.schedule.genesis_hash };
    let current = chain.schedule.fork_id(chain.head);
    let fork_id = remote_fork_id(rng, &chain.schedule_ids(), current);
    let head = if rng.one_in(8) { B256::ZERO } else { B256::from(rng.array::<32>()) };
    let tree = if version >= 69 {
        let latest = rng.edge_u64();
        let earliest =
            if rng.one_in(5) { rng.edge_u64() } else { latest.saturating_sub(rng.edge_u64()) };
        l(vec![
            int(wire_version),
            int(network),
            b(genesis.to_vec()),
            fork_id_node(fork_id),
            int(earliest),
            int(latest),
            b(head.to_vec()),
        ])
    } else {
        let td = match rng.below(4) {
            0 => b(vec![0xff; 21]),
            1 => b(vec![0x01; 20]),
            _ => int(rng.edge_u64()),
        };
        l(vec![
            int(wire_version),
            int(network),
            td,
            b(head.to_vec()),
            b(genesis.to_vec()),
            fork_id_node(fork_id),
        ])
    };
    Seed::tree(format!("eth{version}"), tree)
}

fn tiger_status(chain: &LocalChain, version: u64, payload: &[u8]) -> Verdict {
    let status = match Status::decode_payload(version, payload) {
        Ok(s) => s,
        // peer.rs maps a decode failure to `SessionError::Incompatible` without a Disconnect.
        Err(e) => return Verdict::code("no-disconnect", e),
    };
    if status.network_id != chain.network_id {
        return Verdict::code("UselessPeer", "network id");
    }
    if status.genesis != chain.schedule.genesis_hash {
        return Verdict::code("UselessPeer", "genesis");
    }
    if let Err(e) = chain.schedule.validate(chain.head, status.fork_id) {
        return Verdict::code("UselessPeer", e);
    }
    Verdict::ok("accepted")
}

/// One incoming message for reth's handshake, recording the Disconnect it sends.
#[derive(Debug, Default)]
struct OneMessage {
    incoming: Option<BytesMut>,
    disconnect: Option<DisconnectReason>,
}

impl Stream for OneMessage {
    type Item = Result<BytesMut, P2PStreamError>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.incoming.take().map(Ok))
    }
}

impl Sink<Bytes> for OneMessage {
    type Error = P2PStreamError;

    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, _: Bytes) -> Result<(), Self::Error> {
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl CanDisconnect<Bytes> for OneMessage {
    fn disconnect(
        &mut self,
        reason: DisconnectReason,
    ) -> Pin<Box<dyn Future<Output = Result<(), P2PStreamError>> + Send + '_>> {
        self.disconnect = Some(reason);
        Box::pin(async { Ok(()) })
    }
}

/// base-reth's local Status for the chain.
pub(crate) fn reth_local_status(chain: &LocalChain, version: u64) -> UnifiedStatus {
    let eth69 = version >= 69;
    UnifiedStatus::builder()
        .version(if eth69 { EthVersion::Eth69 } else { EthVersion::Eth68 })
        .chain(Chain::from_id(chain.network_id))
        .genesis(chain.schedule.genesis_hash)
        .forkid(chain.fork_filter().current())
        .blockhash(chain.head_hash)
        .total_difficulty((!eth69).then_some(U256::ZERO))
        .earliest_block(eth69.then_some(0))
        .latest_block(eth69.then_some(chain.head.number))
        .build()
}

fn reth_status(chain: &LocalChain, version: u64, payload: &[u8]) -> Verdict {
    let mut message = BytesMut::from(&[0x00][..]);
    message.extend_from_slice(payload);
    let mut stream = OneMessage { incoming: Some(message), disconnect: None };
    let result = futures::executor::block_on(
        EthereumEthHandshake(&mut stream)
            .eth_handshake(reth_local_status(chain, version), chain.fork_filter()),
    );
    match result {
        Ok(_) => Verdict::ok("accepted"),
        Err(e) => match stream.disconnect {
            Some(reason) => Verdict::code(format!("{reason:?}"), e),
            None => Verdict::code("no-disconnect", e),
        },
    }
}

pub(crate) fn check_status(case: &str, payload: &[u8]) -> (Verdict, Verdict) {
    let chain = LocalChain::fuzz();
    let version = if case == "eth69" { 69 } else { 68 };
    (tiger_status(&chain, version, payload), reth_status(&chain, version, payload))
}

/// Fork id parameters as an RLP tree, so the generic mutations vary them:
/// `[genesis_timestamp, bedrock, [timestamps...], head_number, head_timestamp, [hash, next]]`.
pub(crate) fn generate_forkid(rng: &mut Rng) -> Seed {
    let genesis_ts = *rng.pick(&[0u64, 1_686_789_347, 1_700_000_000]);
    let bedrock = *rng.pick(&[0u64, 0, 105_235_063, 1]);
    // Upgrade timestamps in upgrade order: non-decreasing, as every real chain config has them,
    // except in the `*-unsorted` cases.
    let mut times = Vec::new();
    let mut t = genesis_ts;
    for _ in 0..rng.below(10) {
        if !rng.one_in(3) {
            t += rng.below(100_000) as u64;
        }
        times.push(t);
    }
    let unsorted = rng.one_in(10) && times.len() > 1;
    if unsorted {
        let (i, j) = (rng.below(times.len()), rng.below(times.len()));
        times.swap(i, j);
    }
    let schedule = ForkSchedule::base(B256::from(rng.array::<32>()), genesis_ts, bedrock, &times);
    let pick_time = |rng: &mut Rng| -> u64 {
        match times.get(rng.below(times.len() + 1)) {
            Some(t) => t.wrapping_add(rng.below(3) as u64).wrapping_sub(1),
            None => rng.edge_u64(),
        }
    };
    let head_ts = pick_time(rng);
    let head_number = if rng.one_in(10) { rng.edge_u64() } else { bedrock + rng.below(3) as u64 };
    let ids = schedule_ids(&schedule);
    let current = schedule.fork_id(Head { number: head_number, timestamp: head_ts });
    let remote = remote_fork_id(rng, &ids, current);
    let remote = if rng.one_in(4) { ForkId { next: pick_time(rng), ..remote } } else { remote };
    let case = match (rng.one_in(4), unsorted) {
        (true, false) => "compute",
        (true, true) => "compute-unsorted",
        (false, false) => "validate",
        (false, true) => "validate-unsorted",
    };
    let tree = l(vec![
        int(genesis_ts),
        int(bedrock),
        l(times.iter().map(|t| int(*t)).collect()),
        int(head_number),
        int(head_ts),
        b(schedule.genesis_hash.to_vec()),
        fork_id_node(remote),
    ]);
    Seed::tree(case, tree)
}

fn uint(node: &Node) -> Option<u64> {
    match &node.item {
        Item::Bytes(bytes) if bytes.len() <= 8 => {
            Some(bytes.iter().fold(0u64, |acc, b| (acc << 8) | *b as u64))
        }
        _ => None,
    }
}

fn params(input: &[u8]) -> Option<(ForkSchedule, Head, ForkId)> {
    let (node, _) = super::rlp::parse(input)?;
    let Item::List(items) = node.item else { return None };
    let [genesis_ts, bedrock, times, number, timestamp, genesis, remote] = items.as_slice() else {
        return None;
    };
    let Item::List(times) = &times.item else { return None };
    let times: Vec<u64> = times.iter().map(uint).collect::<Option<_>>()?;
    let Item::Bytes(genesis) = &genesis.item else { return None };
    let Item::List(remote) = &remote.item else { return None };
    let [hash, next] = remote.as_slice() else { return None };
    let Item::Bytes(hash) = &hash.item else { return None };
    let schedule = ForkSchedule::base(
        B256::try_from(genesis.as_slice()).ok()?,
        uint(genesis_ts)?,
        uint(bedrock)?,
        &times,
    );
    let head = Head { number: uint(number)?, timestamp: uint(timestamp)? };
    Some((schedule, head, ForkId { hash: hash.as_slice().try_into().ok()?, next: uint(next)? }))
}

/// What makes a schedule and head unlike any real chain's, if anything: base-reth sorts and
/// dedups activations and treats each kind on its own, tiger hashes them in schedule order.
fn unrealistic(schedule: &ForkSchedule, head: Head) -> Option<&'static str> {
    let times: Vec<u64> = schedule
        .forks
        .iter()
        .filter_map(|f| match f {
            ForkActivation::Timestamp(t) => Some(*t),
            ForkActivation::Block(_) => None,
        })
        .collect();
    let bedrock = schedule.forks.iter().find_map(|f| match f {
        ForkActivation::Block(b) if *b > 0 => Some(*b),
        _ => None,
    });
    if times.windows(2).any(|w| w[0] > w[1]) {
        return Some("unsorted-schedule");
    }
    if head.number >= 1_438_269_973 {
        return Some("head-number-above-mainnet-timestamp");
    }
    if let Some(bedrock) = bedrock
        && head.number < bedrock
        && times.iter().any(|t| *t > schedule.genesis_timestamp && head.timestamp >= *t)
    {
        return Some("head-before-bedrock-past-time-fork");
    }
    None
}

pub(crate) fn check_forkid(case: &str, input: &[u8]) -> (Verdict, Verdict) {
    let Some((schedule, head, remote)) = params(input) else {
        return (Verdict::err("parameters"), Verdict::err("parameters"));
    };
    let tag = unrealistic(&schedule, head).unwrap_or("realistic");
    let reth = filter(&schedule, head);
    if case.starts_with("compute") {
        let tiger = schedule.fork_id(head);
        let theirs = reth.current();
        return (
            Verdict::ok(format!(
                "{tag} | {}/{}",
                alloy_primitives::hex::encode(tiger.hash),
                tiger.next
            )),
            Verdict::ok(format!(
                "{tag} | {}/{}",
                alloy_primitives::hex::encode(theirs.hash.0),
                theirs.next
            )),
        );
    }
    let tiger = match schedule.validate(head, remote) {
        Ok(()) => Verdict::ok("compatible"),
        Err(e) => Verdict::err(format!("{tag} {e:?}")),
    };
    let theirs = match reth.validate(reth_id(remote)) {
        Ok(()) => Verdict::ok("compatible"),
        Err(e) => Verdict::err(format!("{tag} {e:?}")),
    };
    (tiger, theirs)
}
