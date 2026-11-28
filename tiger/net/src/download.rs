//! The block downloader the node's sync driver calls: headers back from a target hash to a known
//! height, and bodies for a list of headers. Requests run in parallel over the connected peers;
//! every response is validated (headers must chain by hash, bodies must match their header's
//! roots) and bad peers are reported.

use std::{
    collections::{HashMap, VecDeque},
    sync::mpsc::{self, RecvTimeoutError, Sender},
    time::{Duration, Instant},
};

use alloy_primitives::B256;
use alloy_rlp::{Decodable, Header as RlpHeader};

use crate::{
    block::{BlockBody, SealedHeader},
    crypto::PeerId,
    eth::{self, BlockId, HeadersRequest, encode_list, split_request_id},
    network::{Network, Penalty},
    peer::{Peer, Response},
};

/// Downloader limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadConfig {
    /// How long a peer has to answer one request.
    pub request_timeout: Duration,
    /// Requests in flight per peer.
    pub max_inflight_per_peer: usize,
    /// Headers asked for per request (peers serve at most 1024).
    pub headers_per_request: u64,
    /// Bodies asked for per request.
    pub bodies_per_request: usize,
    /// How long to wait for a usable peer before giving up.
    pub peer_wait: Duration,
    /// Most attempts per job before giving up.
    pub max_attempts: u32,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(10),
            max_inflight_per_peer: 2,
            headers_per_request: 512,
            bodies_per_request: 64,
            peer_wait: Duration::from_secs(60),
            max_attempts: 20,
        }
    }
}

/// Why a download failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadError {
    /// No peer was available for `peer_wait`.
    NoPeers,
    /// A job failed `max_attempts` times.
    Exhausted(&'static str),
    /// The target block is unknown to every asked peer.
    NotFound,
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Downloads blocks from the network.
#[derive(Debug, Clone)]
pub struct Downloader {
    network: Network,
    config: DownloadConfig,
}

/// What validating one response produced.
enum Step<J> {
    /// The job is complete.
    Done,
    /// Part of the job remains (a short but valid answer).
    Remaining(J),
}

/// Why a response was rejected.
struct Rejected(Penalty);

impl Downloader {
    /// A downloader over `network`.
    pub const fn new(network: Network, config: DownloadConfig) -> Self {
        Self { network, config }
    }

    /// Runs `jobs` across peers until all are done. `send` issues a job's request on a peer;
    /// `handle` validates a response for a job.
    fn run<J: Clone>(
        &self,
        jobs: Vec<J>,
        send: impl Fn(&Peer, &J, u64, &std::sync::mpsc::Sender<Response>) -> bool,
        mut handle: impl FnMut(&J, &PeerId, Vec<u8>) -> Result<Step<J>, Rejected>,
    ) -> Result<(), DownloadError> {
        let (tx, rx) = mpsc::channel();
        let mut queue: VecDeque<(J, u32)> = jobs.into_iter().map(|j| (j, 0)).collect();
        let mut inflight: HashMap<u64, (J, u32, std::sync::Arc<Peer>, Instant)> = HashMap::new();
        let mut next_tag = 0u64;
        let mut idle_since = Instant::now();
        while !queue.is_empty() || !inflight.is_empty() {
            // Dispatch queued jobs to peers with capacity.
            while let Some((job, attempts)) = queue.pop_front() {
                if attempts >= self.config.max_attempts {
                    return Err(DownloadError::Exhausted("too many failed attempts"));
                }
                let Some(peer) = self.network.select_peer(self.config.max_inflight_per_peer) else {
                    queue.push_front((job, attempts));
                    break;
                };
                let tag = next_tag;
                next_tag += 1;
                if send(&peer, &job, tag, &tx) {
                    inflight.insert(tag, (job, attempts, peer, Instant::now()));
                } else {
                    queue.push_front((job, attempts));
                }
            }
            if inflight.is_empty() {
                if idle_since.elapsed() > self.config.peer_wait {
                    return Err(DownloadError::NoPeers);
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            idle_since = Instant::now();
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(response) => {
                    let Some((job, attempts, peer, _)) = inflight.remove(&response.tag) else {
                        continue;
                    };
                    let Some(payload) = response.payload else {
                        queue.push_back((job, attempts + 1));
                        continue;
                    };
                    match handle(&job, &peer.id, payload) {
                        Ok(Step::Done) => {}
                        Ok(Step::Remaining(rest)) => queue.push_front((rest, attempts)),
                        Err(Rejected(penalty)) => {
                            self.network.report(&peer.id, penalty);
                            queue.push_back((job, attempts + 1));
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => unreachable!("we hold a sender"),
            }
            let now = Instant::now();
            let expired: Vec<u64> = inflight
                .iter()
                .filter(|(_, (_, _, _, sent))| now - *sent > self.config.request_timeout)
                .map(|(tag, _)| *tag)
                .collect();
            for tag in expired {
                let (job, attempts, peer, _) = inflight.remove(&tag).unwrap();
                peer.cancel_tag(tag);
                self.network.report(&peer.id, Penalty::Timeout);
                queue.push_back((job, attempts + 1));
            }
        }
        Ok(())
    }

    /// Fetches headers for one request from any peer, checking only that they decode and chain
    /// in the requested direction.
    pub fn headers(&self, request: HeadersRequest) -> Result<Vec<SealedHeader>, DownloadError> {
        let mut out = Vec::new();
        let mut not_found = 0;
        self.run(vec![request], request_headers, |request, _, payload| {
            let headers = valid_headers(request, &payload)?;
            if headers.is_empty() {
                not_found += 1;
                if not_found >= 3 {
                    return Ok(Step::Done);
                }
                return Err(Rejected(Penalty::Useless));
            }
            out = headers;
            Ok(Step::Done)
        })?;
        if out.is_empty() {
            return Err(DownloadError::NotFound);
        }
        Ok(out)
    }

    /// Downloads the headers from `target` back to height `stop_at` (inclusive), at most `max`
    /// of them, in descending order and linked by parent hash. The caller checks whether the
    /// lowest header's parent is in its chain, and calls again from that parent if not (a reorg
    /// deeper than `stop_at`) or if `max` cut the run short.
    pub fn headers_back(
        &self,
        target: B256,
        stop_at: u64,
        max: u64,
    ) -> Result<Vec<SealedHeader>, DownloadError> {
        assert!(max > 0);
        let top = self
            .headers(HeadersRequest {
                start: BlockId::Hash(target),
                limit: 1,
                skip: 0,
                reverse: false,
            })?
            .remove(0);
        if top.hash != target {
            return Err(DownloadError::Exhausted("peer returned another header"));
        }
        let number = top.number();
        let low = stop_at.max(number.saturating_sub(max - 1));
        if number <= low {
            return Ok(vec![top]);
        }
        // Fetch [low, number - 1] by number in parallel chunks, then stitch by hash from the top.
        let size = self.config.headers_per_request;
        let mut chunks = Vec::new();
        let mut start = low;
        while start < number {
            let limit = size.min(number - start);
            chunks.push(HeadersRequest {
                start: BlockId::Number(start),
                limit,
                skip: 0,
                reverse: false,
            });
            start += limit;
        }
        let mut fetched: HashMap<u64, Vec<SealedHeader>> = HashMap::new();
        self.run(chunks, request_headers, |request, _, payload| {
            let headers = valid_headers(request, &payload)?;
            if headers.is_empty() {
                return Err(Rejected(Penalty::Useless));
            }
            let BlockId::Number(first) = request.start else { unreachable!() };
            let got = headers.len() as u64;
            fetched.insert(first, headers);
            if got < request.limit {
                return Ok(Step::Remaining(HeadersRequest {
                    start: BlockId::Number(first + got),
                    limit: request.limit - got,
                    ..*request
                }));
            }
            Ok(Step::Done)
        })?;
        let mut by_number: Vec<SealedHeader> = fetched.into_values().flatten().collect();
        by_number.sort_by_key(|h| std::cmp::Reverse(h.number()));
        let mut out = Vec::with_capacity(by_number.len() + 1);
        let mut expected = top.header.parent_hash;
        out.push(top);
        let mut index = 0;
        while index < by_number.len() {
            if by_number[index].hash == expected {
                expected = by_number[index].header.parent_hash;
                out.push(by_number[index].clone());
                index += 1;
                continue;
            }
            // The number-based run diverges from the target's chain here (peers on another tip,
            // or a reorg): fetch the rest by hash, walking back from `expected`.
            let remaining = by_number.len() - index;
            let mut tail = self.headers_by_hash_back(expected, remaining as u64)?;
            out.append(&mut tail);
            break;
        }
        debug_assert!(out.windows(2).all(|w| w[0].header.parent_hash == w[1].hash));
        Ok(out)
    }

    /// Walks back from `hash` with reverse by-hash requests, sequentially, for `count` headers.
    fn headers_by_hash_back(
        &self,
        mut hash: B256,
        count: u64,
    ) -> Result<Vec<SealedHeader>, DownloadError> {
        let mut out = Vec::new();
        while (out.len() as u64) < count {
            let limit = (count - out.len() as u64).min(self.config.headers_per_request);
            let headers = self.headers(HeadersRequest {
                start: BlockId::Hash(hash),
                limit,
                skip: 0,
                reverse: true,
            })?;
            if headers[0].hash != hash {
                return Err(DownloadError::Exhausted("by-hash response starts elsewhere"));
            }
            hash = headers.last().unwrap().header.parent_hash;
            out.extend(headers);
        }
        Ok(out)
    }

    /// Downloads the bodies of `headers` (any order), validated against each header. Returns
    /// them in the same order as `headers`. `progress` is called with the number done so far.
    pub fn bodies(
        &self,
        headers: &[SealedHeader],
        progress: &mut dyn FnMut(usize),
    ) -> Result<Vec<BlockBody>, DownloadError> {
        let mut out: Vec<Option<BlockBody>> = vec![None; headers.len()];
        let indices: Vec<usize> = (0..headers.len()).collect();
        let jobs: Vec<Vec<usize>> =
            indices.chunks(self.config.bodies_per_request).map(|c| c.to_vec()).collect();
        let mut done = 0;
        self.run(
            jobs,
            |peer, job, tag, tx| {
                let hashes: Vec<B256> = job.iter().map(|i| headers[*i].hash).collect();
                peer.request_eth(eth::GET_BLOCK_BODIES, |id| encode_list(&[&id, &hashes]), tag, tx)
                    .is_ok()
            },
            |job, _, payload| {
                let (_, body) =
                    split_request_id(&payload).map_err(|_| Rejected(Penalty::BadResponse))?;
                let bodies = Vec::<BlockBody>::decode(&mut { body })
                    .map_err(|_| Rejected(Penalty::BadResponse))?;
                if bodies.is_empty() {
                    return Err(Rejected(Penalty::Useless));
                }
                if bodies.len() > job.len() {
                    return Err(Rejected(Penalty::BadResponse));
                }
                for (body, index) in bodies.iter().zip(job) {
                    body.validate(&headers[*index].header)
                        .map_err(|_| Rejected(Penalty::BadResponse))?;
                }
                let got = bodies.len();
                for (body, index) in bodies.into_iter().zip(job) {
                    out[*index] = Some(body);
                }
                done += got;
                progress(done);
                if got < job.len() {
                    return Ok(Step::Remaining(job[got..].to_vec()));
                }
                Ok(Step::Done)
            },
        )?;
        Ok(out.into_iter().map(|b| b.expect("every job completed")).collect())
    }
}

/// Issues a GetBlockHeaders request for `request` on `peer`.
fn request_headers(peer: &Peer, request: &HeadersRequest, tag: u64, tx: &Sender<Response>) -> bool {
    peer.request_eth(eth::GET_BLOCK_HEADERS, |id| request.encode_payload(id), tag, tx).is_ok()
}

/// Decodes a BlockHeaders response payload.
fn decode_headers(payload: &[u8]) -> alloy_rlp::Result<Vec<SealedHeader>> {
    let (_, mut body) = split_request_id(payload)?;
    let mut items = RlpHeader::decode_bytes(&mut body, true)?;
    let mut headers = Vec::new();
    while !items.is_empty() {
        headers.push(SealedHeader::decode(&mut items)?);
    }
    Ok(headers)
}

/// Decodes a BlockHeaders response and, unless it is empty, checks it answers `request`.
fn valid_headers(request: &HeadersRequest, payload: &[u8]) -> Result<Vec<SealedHeader>, Rejected> {
    let headers = decode_headers(payload).map_err(|_| Rejected(Penalty::BadResponse))?;
    if !headers.is_empty() {
        check_header_run(request, &headers).map_err(Rejected)?;
    }
    Ok(headers)
}

/// Checks that headers answer `request`: the right start, no more than asked, consecutive
/// numbers in the right direction, and (for consecutive runs) parent hash links.
fn check_header_run(request: &HeadersRequest, headers: &[SealedHeader]) -> Result<(), Penalty> {
    if headers.len() as u64 > request.limit {
        return Err(Penalty::BadResponse);
    }
    let first = &headers[0];
    let start_ok = match request.start {
        BlockId::Hash(hash) => first.hash == hash,
        BlockId::Number(number) => first.number() == number,
    };
    if !start_ok {
        return Err(Penalty::BadResponse);
    }
    let step = request.skip + 1;
    for pair in headers.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let expected = if request.reverse {
            a.number().checked_sub(step)
        } else {
            a.number().checked_add(step)
        };
        if Some(b.number()) != expected {
            return Err(Penalty::BadResponse);
        }
        if request.skip == 0 {
            let linked = if request.reverse {
                a.header.parent_hash == b.hash
            } else {
                b.header.parent_hash == a.hash
            };
            if !linked {
                return Err(Penalty::BadResponse);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;

    fn chain(n: u64) -> Vec<SealedHeader> {
        let mut out: Vec<SealedHeader> = Vec::new();
        for number in 0..n {
            let parent_hash = out.last().map_or(B256::ZERO, |h| h.hash);
            out.push(SealedHeader::seal(Header { number, parent_hash, ..Default::default() }));
        }
        out
    }

    #[test]
    fn header_runs_must_chain() {
        let headers = chain(10);
        let forward =
            HeadersRequest { start: BlockId::Number(2), limit: 5, skip: 0, reverse: false };
        assert!(check_header_run(&forward, &headers[2..7]).is_ok());
        assert!(check_header_run(&forward, &headers[3..8]).is_err());
        let mut broken = headers[2..7].to_vec();
        broken[2] = SealedHeader::seal(Header { number: 4, ..Default::default() });
        assert!(check_header_run(&forward, &broken).is_err());
        let mut reversed = headers[..6].to_vec();
        reversed.reverse();
        let back = HeadersRequest {
            start: BlockId::Hash(headers[5].hash),
            limit: 6,
            skip: 0,
            reverse: true,
        };
        assert!(check_header_run(&back, &reversed).is_ok());
        assert!(check_header_run(&HeadersRequest { limit: 5, ..back }, &reversed).is_err());
    }
}
