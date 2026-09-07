//! Block deduplication pass.
//!
//! Identifies structurally identical non-fallthrough blocks (same opcode + immediate sequence)
//! and eliminates duplicates by marking them as dead code and redirecting predecessors to a
//! single canonical copy.

use alloy_primitives::map::HashMap;
use oxc_index::IndexVec;
use revm_bytecode::opcode as op;
use smallvec::SmallVec;

use super::block_analysis::{Block, BlockData, Snapshots};
use crate::bytecode::{Bytecode, Inst, InstData, InstFlags};

/// Dedup key: instruction-level structural fingerprint, CFG successor blocks, and terminator
/// jump kind.
///
/// The fingerprint encodes each instruction's opcode plus its (interned) immediate so that
/// PUSH operations with the same value, and DUPN/SWAPN/EXCHANGE with the same operand, hash
/// equal across blocks. Raw bytes are intentionally not consulted — all immediate data is
/// pre-interned at parse time on `InstData`.
///
/// Structural equality alone is insufficient for JUMP-terminated blocks because block
/// analysis may resolve identical copies to different static targets depending on incoming
/// stack context (e.g. different return-address values), so successors are part of the key.
///
/// The `is_multi_jump` discriminator prevents MULTI_JUMP dispatcher blocks from colliding
/// with STATIC_JUMP blocks that happen to share the same fingerprint and successor set.
#[derive(Clone, PartialEq, Eq, Hash)]
struct DedupKey {
    /// Instruction fingerprint: per-instruction `(opcode, immediate)` tuples encoded as bytes.
    fingerprint: SmallVec<[u8; 32]>,
    /// CFG successors for this block. Two structurally identical blocks are only merged when
    /// their successor sets also match.
    succs: SmallVec<[Block; 4]>,
    /// Whether the block's terminator is a MULTI_JUMP.
    is_multi_jump: bool,
}

impl<'a> Bytecode<'a> {
    /// Deduplicate structurally identical non-fallthrough blocks.
    ///
    /// Eligible blocks are those whose terminator cannot fall through (diverging instructions
    /// like REVERT/STOP/RETURN, or unconditional JUMP). For every group of byte-identical
    /// blocks, keeps one canonical copy while marking them as dead code.
    /// A redirect entry is stored in [`Bytecode::redirects`] so the translator can map
    /// dead instructions to the canonical block's IR block. The CFG is rebuilt after each
    /// iteration so successor/predecessor lists stay consistent.
    ///
    /// `local_snapshots` are the block-local snapshots computed by `block_analysis_local`
    /// (before the global fixpoint). After merging, the canonical block's snapshots are
    /// restored to these local values because the global snapshots may be context-specific
    /// and stale once multiple predecessors are merged.
    #[instrument(name = "dedup", level = "debug", skip_all)]
    pub(crate) fn dedup_blocks(&mut self, local_snapshots: &Snapshots) {
        let mut key_to_blocks = HashMap::<DedupKey, SmallVec<[Block; 4]>>::default();
        let mut total_deduped = 0usize;
        loop {
            key_to_blocks.clear();
            let deduped = self.dedup_blocks_once(&mut key_to_blocks, local_snapshots);
            total_deduped += deduped;
            if deduped == 0 {
                break;
            }
            self.refresh_cfg();
        }
        debug!(deduped = total_deduped, "finished");
    }

    /// Single dedup iteration. Returns the number of blocks deduped.
    fn dedup_blocks_once(
        &mut self,
        key_to_blocks: &mut HashMap<DedupKey, SmallVec<[Block; 4]>>,
        local_snapshots: &Snapshots,
    ) -> usize {
        for bid in self.cfg.blocks.indices() {
            let block = &self.cfg.blocks[bid];
            let term = &self.insts[block.terminator()];

            // Only dedup blocks whose terminator cannot fall through (diverging or
            // unconditional JUMP). JUMPI blocks fall through to position-dependent targets.
            if term.can_fall_through() {
                continue;
            }

            // Reachable JUMPDESTs with dynamic jumps cannot be deduped because any
            // JUMPDEST could be reached from an unresolved dynamic JUMP with an
            // arbitrary stack context.
            let first = &self.insts[block.insts.start];
            if self.has_dynamic_jumps && first.is_reachable_jumpdest(true) {
                continue;
            }

            // Skip blocks containing PC — it returns the current program counter,
            // so identical bytecode at different positions produces different results.
            if block.insts().any(|i| self.insts[i].opcode == op::PC) {
                continue;
            }

            // Skip unresolved dynamic jumps — any target is possible at runtime.
            if term.is_jump() && !term.flags.contains(InstFlags::STATIC_JUMP) {
                continue;
            }

            let fingerprint = block_fingerprint(&self.insts, block);
            if fingerprint.is_empty() {
                continue;
            }

            let is_multi_jump = term.flags.contains(InstFlags::MULTI_JUMP);
            key_to_blocks
                .entry(DedupKey { fingerprint, succs: block.succs.clone(), is_multi_jump })
                .or_default()
                .push(bid);
        }

        let mut deduped = 0usize;
        for group in key_to_blocks.values() {
            if group.len() < 2 {
                continue;
            }
            let (&canonical, dups) = group.split_first().unwrap();
            let canonical_first_inst = self.cfg.blocks[canonical].insts.start;

            // Restore block-local snapshots for the canonical block. The global
            // fixpoint may have recorded context-specific constants that become stale
            // once the canonical block serves multiple predecessors after merging.
            // Block-local constants (computed without incoming stack state) remain valid.
            self.snapshots.restore_from(self.cfg.blocks[canonical].insts(), local_snapshots);

            for &dup in dups {
                deduped += 1;
                trace!("deduped: {from} -> {to}", from = dup, to = canonical);

                // Mark all instructions in the duplicate block as dead.
                let dup_block = &self.cfg.blocks[dup];
                for i in dup_block.insts() {
                    self.insts[i].flags |= InstFlags::DEAD_CODE;
                }

                let dup_first_inst = dup_block.insts.start;
                self.redirects.insert(dup_first_inst, canonical_first_inst);

                // If the duplicate was a reachable JUMPDEST, the canonical must be too
                // so that sections/gas-checks treat it as a jump target entry point.
                if self.insts[dup_first_inst].is_jumpdest()
                    && self.insts[dup_first_inst].is_jumpdest_reachable()
                    && self.insts[canonical_first_inst].is_jumpdest()
                {
                    self.insts[canonical_first_inst].set_jumpdest_reachable();
                }

                // Merge multi-jump targets from the duplicate into the canonical block.
                // Without this, valid case PCs unique to the duplicate dispatcher would
                // be lost and fall into the default InvalidJump path at runtime.
                let canonical_term = self.cfg.blocks[canonical].terminator();
                let dup_term = dup_block.terminator();
                if self.insts[dup_term].flags.contains(InstFlags::MULTI_JUMP)
                    && let Some(dup_targets) = self.multi_jump_targets.remove(&dup_term)
                {
                    let canonical_targets =
                        self.multi_jump_targets.get_mut(&canonical_term).unwrap();
                    for t in dup_targets {
                        if !canonical_targets.contains(&t) {
                            canonical_targets.push(t);
                        }
                    }
                }

                // Redirect predecessors that reach the duplicate via static jumps.
                for &pred in &dup_block.preds {
                    let term_inst = self.cfg.blocks[pred].terminator();
                    let term = &self.insts[term_inst];
                    let is_static = term.is_static_jump()
                        && !term.flags.contains(InstFlags::INVALID_JUMP)
                        && !term.flags.contains(InstFlags::MULTI_JUMP)
                        && term.static_jump_target() == dup_first_inst;
                    if is_static {
                        self.insts[term_inst].set_static_jump_target(canonical_first_inst);
                    }
                    // Multi-jump target PCs are not rewritten here: each carries a
                    // distinct PC that callers push as a return address. The targets
                    // were already merged into the canonical block above, and
                    // Translator callsites resolve redirects to map dead
                    // instruction targets to the canonical IR entry.
                }
            }
        }

        deduped
    }
}

/// Builds a structural fingerprint of a block from instruction data, without consulting the
/// raw bytecode.
///
/// For each non-dead instruction in the block, encodes the opcode followed by the immediate
/// payload (interned U256Idx for `PUSH*`, immediate byte for `DUPN`/`SWAPN`/`EXCHANGE`).
/// JUMP/JUMPI carry no immediate and so contribute only their opcode; jump targets are
/// already factored into the surrounding `DedupKey` via the block's CFG successors.
fn block_fingerprint(insts: &IndexVec<Inst, InstData>, block: &BlockData) -> SmallVec<[u8; 32]> {
    let mut buf: SmallVec<[u8; 32]> = SmallVec::new();
    for i in block.insts() {
        let data = &insts[i];
        buf.push(data.opcode);
        if data.imm_len() > 0 {
            let mut imm = &data.data.to_ne_bytes()[..];
            while let [0, rest @ ..] = imm {
                imm = rest;
            }
            buf.extend_from_slice(imm);
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use revm_bytecode::opcode as op;

    use crate::bytecode::{
        AnalysisConfig, InstFlags, passes::block_analysis::tests::analyze_asm_with,
    };

    #[test]
    fn dedup_identical_revert_blocks() {
        // Two JUMPI branches each fall through to an identical `PUSH1 0x00 / DUP1 / REVERT`.
        let bytecode = analyze_asm_with(
            "
            PUSH0
            CALLDATALOAD
            PUSH %bb2
            JUMPI
            ; revert A
            PUSH1 0x00
            DUP1
            REVERT
        bb2:
            JUMPDEST
            PUSH0
            CALLDATALOAD
            PUSH %bb4
            JUMPI
            ; revert B (identical to A)
            PUSH1 0x00
            DUP1
            REVERT
        bb4:
            JUMPDEST
            STOP
        ",
            AnalysisConfig::DEDUP,
        );

        // One of the two `PUSH1 0x00 / DUP1 / REVERT` blocks should be redirected.
        assert_eq!(
            bytecode.redirects.len(),
            1,
            "expected exactly 1 redirect for 2 identical revert blocks",
        );
    }

    #[test]
    fn dedup_preserves_unique_blocks() {
        // Two different diverging blocks — they should NOT be deduplicated.
        let bytecode = analyze_asm_with(
            "
            PUSH0
            CALLDATALOAD
            PUSH %target
            JUMPI
            ; revert with 0
            PUSH1 0x00
            DUP1
            REVERT
        target:
            JUMPDEST
            STOP
        ",
            AnalysisConfig::DEDUP,
        );

        assert!(bytecode.redirects.is_empty(), "should not dedup different blocks");
    }

    #[test]
    fn dedup_three_identical_blocks() {
        // Three identical `PUSH1 0x00 / DUP1 / REVERT` blocks.
        let bytecode = analyze_asm_with(
            "
            PUSH0
            CALLDATALOAD
            PUSH %bb2
            JUMPI
            ; revert A
            PUSH1 0x00
            DUP1
            REVERT
        bb2:
            JUMPDEST
            PUSH0
            CALLDATALOAD
            PUSH %bb4
            JUMPI
            ; revert B
            PUSH1 0x00
            DUP1
            REVERT
        bb4:
            JUMPDEST
            PUSH0
            CALLDATALOAD
            PUSH %bb6
            JUMPI
            ; revert C
            PUSH1 0x00
            DUP1
            REVERT
        bb6:
            JUMPDEST
            STOP
        ",
            AnalysisConfig::DEDUP,
        );

        let redirect_count = bytecode.redirects.len();
        assert_eq!(redirect_count, 2, "expected 2 redirects, got {redirect_count}");
    }

    #[test]
    fn dedup_skips_pc_opcode() {
        // Two identical `PC / PUSH1 0x00 / REVERT` blocks — PC is position-dependent,
        // so they must NOT be deduplicated.
        let bytecode = analyze_asm_with(
            "
            PUSH0
            CALLDATALOAD
            PUSH %bb2
            JUMPI
            ; PC + revert A
            PC
            PUSH1 0x00
            REVERT
        bb2:
            JUMPDEST
            PUSH0
            CALLDATALOAD
            PUSH %bb4
            JUMPI
            ; PC + revert B (same bytes, different PC value)
            PC
            PUSH1 0x00
            REVERT
        bb4:
            JUMPDEST
            STOP
        ",
            AnalysisConfig::DEDUP,
        );

        assert!(bytecode.redirects.is_empty(), "should not dedup blocks containing PC");
    }

    #[test]
    fn dedup_jump_same_target() {
        // Two byte-identical fallthrough JUMP tails resolved to the same target.
        let bytecode = analyze_asm_with(
            "
            CALLDATASIZE
            PUSH %path1
            JUMPI
            PUSH0
            PUSH1 0xFF
            JUMPI
            PUSH %target
            JUMP
        path1:
            JUMPDEST
            PUSH0
            PUSH1 0xFF
            JUMPI
            PUSH %target
            JUMP
        target:
            JUMPDEST
            STOP
        ",
            AnalysisConfig::DEDUP,
        );
        assert_eq!(bytecode.redirects.len(), 1, "same-target JUMP tails should be deduped");
    }

    #[test]
    fn dedup_jump_different_targets() {
        // Two byte-identical non-JUMPDEST JUMP tails with different resolved static targets.
        // Must NOT be merged — the JUMP target is context-sensitive.
        // Regression test for:
        // revmc-dedup-non-jumpdest-fallthrough-jump-tail-static-target-confusion
        let bytecode = analyze_asm_with(
            "
            CALLDATASIZE
            PUSH %path1
            JUMPI

            PUSH %ret0
            PUSH0
            PUSH1 0xFF
            JUMPI
            PUSH1 0x42
            SWAP1
            JUMP

        path1:
            JUMPDEST
            PUSH %ret1
            PUSH0
            PUSH1 0xFF
            JUMPI
            PUSH1 0x42
            SWAP1
            JUMP

        ret0:
            JUMPDEST
            POP
            PUSH1 0xAA
            STOP

        ret1:
            JUMPDEST
            POP
            PUSH1 0xBB
            STOP
        ",
            AnalysisConfig::DEDUP,
        );

        assert!(
            bytecode.redirects.is_empty(),
            "different-target JUMP tails must not be deduped, got {} redirect(s)",
            bytecode.redirects.len(),
        );
    }

    #[test]
    fn dedup_weth() {
        let code = fixture_entry_code(include_str!("../../../upstream-assets/weth.json"));
        let mut bytecode = crate::Bytecode::test(code);
        bytecode.config = AnalysisConfig::DEDUP;
        bytecode.analyze().unwrap();

        assert_eq!(bytecode.redirects.len(), 13);
    }

    fn fixture_entry_code(json: &str) -> Vec<u8> {
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let case = v.as_object().unwrap().values().next().unwrap();
        let to = case["transaction"][0]["to"].as_str().unwrap();
        let code = case["pre"][to]["code"].as_str().unwrap().trim_start_matches("0x");
        revm_primitives::hex::decode(code).unwrap()
    }

    #[test]
    fn dedup_skips_dynamic_jump_sstore_tail() {
        // Two non-JUMPDEST byte-identical `SSTORE ; JUMP` tails with unresolved dynamic
        // JUMP terminators must NOT be deduped.
        let bytecode = analyze_asm_with(
            "
            PUSH0
            CALLDATALOAD
            PUSH1 0x20
            CALLDATALOAD
            PUSH %path_b
            JUMPI

            ; path A
            PUSH1 0x11
            PUSH1 0x01
            PUSH0
            PUSH %not_taken_a
            JUMPI
            SSTORE
            JUMP

        not_taken_a:
            JUMPDEST
            INVALID

        path_b:
            JUMPDEST
            PUSH1 0x22
            PUSH1 0x02
            PUSH0
            PUSH %not_taken_b
            JUMPI
            SSTORE
            JUMP

        not_taken_b:
            JUMPDEST
            INVALID

        done:
            JUMPDEST
            STOP
        ",
            AnalysisConfig::DEDUP,
        );

        assert!(
            bytecode.redirects.is_empty(),
            "should not dedup blocks ending in unresolved dynamic JUMP",
        );
    }

    #[test]
    fn dedup_skips_const_true_dynamic_jumpi_tail() {
        // Two non-JUMPDEST byte-identical tails ending in const-true `JUMPI` with
        // unresolved dynamic targets must NOT be deduped. The constant condition
        // makes the terminator non-fallthrough, but the target is still arbitrary.
        let bytecode = analyze_asm_with(
            "
            CALLDATASIZE
            PUSH %path_b
            JUMPI

            ; tail A
            PUSH1 0x01
            PUSH0
            CALLDATALOAD
            JUMPI
            STOP

        path_b:
            JUMPDEST
            PUSH0
            PUSH %after_b
            JUMPI

            ; tail B
            PUSH1 0x01
            PUSH0
            CALLDATALOAD
            JUMPI
            STOP

        after_b:
            JUMPDEST
            INVALID
        ",
            AnalysisConfig::DEDUP,
        );

        let dynamic_jumpis = bytecode
            .iter_insts()
            .filter(|(_, data)| {
                data.opcode == op::JUMPI
                    && data.has_const_jumpi_condition()
                    && !data.flags.contains(InstFlags::STATIC_JUMP)
            })
            .count();
        assert_eq!(dynamic_jumpis, 2, "expected two const-true dynamic JUMPI tails");
        assert!(
            bytecode.redirects.is_empty(),
            "should not dedup blocks ending in unresolved dynamic JUMPI",
        );
    }

    #[test]
    fn dedup_allows_const_true_static_jumpi_tail() {
        // The dynamic-target guard should not block const-true JUMPI tails whose
        // target is known and whose CFG successors match.
        let bytecode = analyze_asm_with(
            "
            CALLDATASIZE
            PUSH %path_b
            JUMPI

            ; tail A
            PUSH1 0x01
            PUSH %target
            JUMPI
            STOP

        path_b:
            JUMPDEST
            PUSH0
            PUSH %after_b
            JUMPI

            ; tail B
            PUSH1 0x01
            PUSH %target
            JUMPI
            STOP

        after_b:
            JUMPDEST
            INVALID

        target:
            JUMPDEST
            STOP
        ",
            AnalysisConfig::DEDUP,
        );

        let static_jumpis = bytecode
            .iter_insts()
            .filter(|(_, data)| {
                data.opcode == op::JUMPI
                    && data.has_const_jumpi_condition()
                    && data.flags.contains(InstFlags::STATIC_JUMP)
            })
            .count();
        assert!(
            static_jumpis >= 2,
            "expected at least two const-condition static JUMPI instructions",
        );
        assert_eq!(
            bytecode.redirects.len(),
            1,
            "same-target const-true static JUMPI tails should be deduped",
        );
    }

    #[test]
    fn dedup_multi_jump_source_merges_targets() {
        // Two byte-identical MULTI_JUMP dispatcher blocks (dispatcher_a, dispatcher_b)
        // each route to two identical JUMPDEST+STOP return blocks. After the return
        // blocks are deduped, the two dispatchers become identical in (bytes, succs).
        // Dedup must merge their multi_jump_targets so the canonical switch emits cases
        // for ALL valid return PCs.
        let bytecode = analyze_asm_with(
            "
            ; Branch on first calldata word.
            PUSH0
            CALLDATALOAD
            PUSH %path_b
            JUMPI

            ; Branch on second calldata word.
            PUSH1 0x20
            CALLDATALOAD
            PUSH %path_a1
            JUMPI
            PUSH %ret0
            PUSH %dispatcher_a
            JUMP
        path_a1:
            JUMPDEST
            PUSH %ret1
            PUSH %dispatcher_a
            JUMP

        path_b:
            JUMPDEST
            PUSH1 0x20
            CALLDATALOAD
            PUSH %path_b1
            JUMPI
            PUSH %ret2
            PUSH %dispatcher_b
            JUMP
        path_b1:
            JUMPDEST
            PUSH %ret3
            PUSH %dispatcher_b
            JUMP

        ret0:
            JUMPDEST
            STOP
        ret1:
            JUMPDEST
            STOP
        ret2:
            JUMPDEST
            STOP
        ret3:
            JUMPDEST
            STOP

        dispatcher_a:
            JUMPDEST
            JUMP
        dispatcher_b:
            JUMPDEST
            JUMP
        ",
            AnalysisConfig::DEDUP,
        );

        // The four STOP blocks are deduped (3 redirects), and one dispatcher is deduped
        // (1 redirect) = 4 total.
        assert_eq!(bytecode.redirects.len(), 4);

        // Find the canonical dispatcher's multi_jump_targets — it must contain all 4
        // original return PCs (possibly redirected to the canonical STOP inst).
        let canonical_dispatcher_term = bytecode
            .multi_jump_targets
            .keys()
            .find(|&&inst| !bytecode.insts[inst].is_dead_code())
            .expect("should have a live MULTI_JUMP dispatcher");
        let targets = &bytecode.multi_jump_targets[canonical_dispatcher_term];
        assert_eq!(
            targets.len(),
            4,
            "canonical dispatcher should have 4 multi-jump targets (merged), got {targets:?}",
        );
    }

    #[test]
    fn dedup_clears_stale_noop_flags() {
        // Regression test for stale NOOP flags surviving dedup.
        //
        // Two byte-identical RETURN tails reached via different paths:
        // - Path 0 pushes known constants (0x11, 0x22) → DSE can mark ADD as NOOP because the
        //   output is a known constant (0x33).
        // - Path 1 pushes CALLDATASIZE twice → ADD output is dynamic.
        //
        // After dedup merges the tails, the canonical block's snapshots are restored
        // to local (which lack the incoming constants), but DSE re-runs and must NOT
        // mark ADD as NOOP since its output is no longer a known constant.
        let bytecode = analyze_asm_with(
            "
            CALLDATASIZE
            PUSH %path1
            JUMPI

            ; Path 0: known constants.
            PUSH1 0x11
            PUSH1 0x22
            PUSH %tail0
            JUMP

        path1:
            JUMPDEST
            ; Path 1: dynamic values.
            CALLDATASIZE
            CALLDATASIZE
            PUSH %tail1
            JUMP

        tail0:
            JUMPDEST
            ADD
            PUSH0
            MSTORE
            PUSH1 0x20
            PUSH0
            RETURN

        tail1:
            JUMPDEST
            ADD
            PUSH0
            MSTORE
            PUSH1 0x20
            PUSH0
            RETURN
        ",
            AnalysisConfig::DEDUP,
        );

        // Dedup should have merged the two tails.
        assert_eq!(bytecode.redirects.len(), 1);

        // The canonical block's ADD must NOT be marked NOOP — its output is dynamic
        // under the merged (local) snapshots.
        let canonical_start = *bytecode.redirects.values().next().unwrap();
        let add_inst = canonical_start + 1; // JUMPDEST, ADD
        assert!(
            !bytecode.insts[add_inst].flags.contains(InstFlags::NOOP),
            "canonical ADD must not be NOOP after dedup (stale flag from context-specific DSE)",
        );
    }

    #[test]
    fn dedup_invalidates_stale_const_snapshots() {
        // Two byte-identical RETURN tails reached with different incoming constants.
        // After dedup the canonical block must NOT retain stale const snapshots.
        let bytecode = analyze_asm_with(
            "
            CALLDATASIZE
            PUSH %path1
            JUMPI

            PUSH1 0xAA
            PUSH %tail0
            JUMP

        path1:
            JUMPDEST
            PUSH1 0xBB
            PUSH %tail1
            JUMP

        tail0:
            JUMPDEST
            PUSH1 0x01
            ADD
            PUSH0
            MSTORE
            PUSH1 0x20
            PUSH0
            RETURN

        tail1:
            JUMPDEST
            PUSH1 0x01
            ADD
            PUSH0
            MSTORE
            PUSH1 0x20
            PUSH0
            RETURN
        ",
            AnalysisConfig::DEDUP,
        );

        // Dedup should have merged the two tails.
        assert_eq!(bytecode.redirects.len(), 1);

        // The canonical block's ADD must not carry a stale const_output
        // (it would be 0xAB from the first context, but the second expects 0xBC).
        let canonical_start = *bytecode.redirects.values().next().unwrap();
        let add_inst = canonical_start + 2; // JUMPDEST, PUSH1, ADD
        assert!(
            bytecode.const_output(add_inst).is_none(),
            "canonical ADD should have no const_output after dedup (was stale)",
        );
    }
}
