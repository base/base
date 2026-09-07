// Copyright 2019 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

// The basis of this file has been taken from the rust-libp2p codebase:
// https://github.com/libp2p/rust-libp2p

//! Peer selection strategies for queries in the form of iterator-like state machines.
//!
//! Using a peer iterator in a query involves performing the following steps
//! repeatedly and in an alternating fashion:
//!
//!   1. Calling `next` to observe the next state of the iterator and determine
//!      what to do, which is to either issue new requests to peers or continue
//!      waiting for responses.
//!
//!   2. When responses are received or requests fail, providing input to the
//!      iterator via the `on_success` and `on_failure` callbacks,
//!      respectively, followed by repeating step (1).
//!
//! When a call to `next` returns [`Finished`], no more peers can be obtained
//! from the iterator and the results can be obtained from `into_result`.
//!
//! A peer iterator can be finished prematurely at any time through `finish`.
//!
//! [`Finished`]: QueryState::Finished

pub mod closest;
pub mod predicate;

/// The state of the query reported by [`closest::FindNodeQuery::next`] or
/// [`predicate::PredicateQuery::next`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryState<TNodeId> {
    /// The query is waiting for results.
    ///
    /// `Some(peer)` indicates that the query is now waiting for a result
    /// from `peer`, in addition to any other peers for which the query is already
    /// waiting for results.
    ///
    /// `None` indicates that the query is waiting for results and there is no
    /// new peer to contact, despite the query not being at capacity w.r.t.
    /// the permitted parallelism.
    Waiting(Option<TNodeId>),

    /// The query is waiting for results and is at capacity w.r.t. the
    /// permitted parallelism.
    WaitingAtCapacity,

    /// The query finished.
    Finished,
}
