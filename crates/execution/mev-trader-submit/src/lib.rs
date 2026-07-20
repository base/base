//! Phase-B in-node MEV submit safe-prefix (rung-1 + rung-2, funds-0, dry-run).
//!
//! This crate is the Rust in-node port of the `TypeScript` verification prototypes
//! `scripts/arb-dryrun/blink-unsigned-assembler.ts` (rung-1) and
//! `scripts/arb-dryrun/rung2-ephemeral-signer.ts` (rung-2). It consumes the
//! measurement-only [`base_mev_trader::BackrunPlan`] and produces:
//!
//! * **rung-1** ([`assembler`]) — the executor calldata for
//!   `BlinkAtomicExecutor.executeBlinkOfaAtomic`, an unsigned EIP-1559 backrun
//!   envelope, a structurally-invalid dummy-signature serialization, and the
//!   two Blink OFA channel structures (inclusion + attribution). Assembly and
//!   serialization ONLY — nothing is ever transmitted.
//! * **rung-2** ([`signer`]) — a throwaway, in-memory, unfunded ephemeral k256
//!   keypair that signs the rung-1 envelope once and is verified entirely
//!   offline (ecrecover + field integrity). No key is ever loaded, persisted,
//!   logged, or returned.
//!
//! ## Red-line (compile-time enforced)
//!
//! The entire module tree is gated behind the `phase-b` Cargo feature. In the
//! default build — the shape the deployed node binary is compiled in — this
//! crate is an EMPTY lib: no dependency is resolved and no signer/submit code
//! path exists. The crate is additionally never a dependency of any node binary
//! crate, so even a `phase-b` build is not linked into `base-reth-node`.
//!
//! There is deliberately NO real private-key loader (file/env/argv/keystore/
//! homedir) and NO real submission sink (only a spawned loopback anvil in the
//! e2e test). Real signing with a persistent hot wallet and real bundle
//! submission are the rung-3 boundary and remain unavailable here.

// Without `phase-b` the crate is intentionally empty: zero submit surface.
#![cfg(feature = "phase-b")]

pub mod assembler;
pub mod fee;
pub mod signer;

/// The Blink OFA native-ETH kickback recipient enforced inside the executor
/// backrun. Mirrors `BLINK_OFA_KICKBACK_RECIPIENT` in the TS prototype and the
/// `NATIVE_KICKBACK_RECIPIENT` constant compiled into `BlinkAtomicExecutor`.
pub const BLINK_OFA_KICKBACK_RECIPIENT: alloy_primitives::Address =
    alloy_primitives::address!("743be0db30148336a3db479f19d4e1828b293869");

/// The minimum kickback share (basis points) the executor pays to the recipient.
/// Mirrors `BLINK_OFA_MIN_KICKBACK_BPS` in the TS prototype; the executor pays
/// `ceil(75%)` of realized profit, i.e. at least this share.
pub const BLINK_OFA_MIN_KICKBACK_BPS: u32 = 7_500;
