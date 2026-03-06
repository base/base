// These tests validate that the proposer skips game CREATION when vkeys don't match.
// This simulates a hardfork scenario where the game implementation is upgraded
// with new vkeys that don't match the proposer's computed vkeys.
package fpdefense

import (
	"context"
	"testing"
	"time"

	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	opspresets "github.com/succinctlabs/op-succinct/presets"
	"github.com/succinctlabs/op-succinct/utils"
)

// Fake vkeys that definitely won't match the proposer's computed vkeys.
// These simulate "old" vkeys from a previous version (pre-hardfork).
var (
	fakeAggregationVkey     = "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
	fakeRangeVkeyCommitment = "0xcafebabecafebabecafebabecafebabecafebabecafebabecafebabecafebabe"
)

// TestGameCreationSkipOnVkeyMismatch validates that proposer correctly detects
// vkey mismatch after a simulated hardfork (implementation upgrade) and stops
// creating new games.
//
// This test uses the Contract Upgrade approach rather than test-only config options,
// which more accurately mirrors actual hardfork scenarios.
//
// Test Flow:
// Phase 1 (Normal Operation):
//   - Deploy system with real vkeys from ELF
//   - Wait for at least one game to be created (proves normal operation)
//
// Phase 2 (Simulated Hardfork):
//   - Deploy new game implementation with fake vkeys
//   - Upgrade factory using setImplementation()
//   - Record game count after upgrade
//
// Phase 3 (Verify Skip Behavior):
//   - Wait for 2-3x proposal interval
//   - Assert game count hasn't increased
//   - Proposer should detect vkey mismatch via on_chain_vkeys_match()
func TestGameCreationSkipOnVkeyMismatch(gt *testing.T) {
	t := devtest.ParallelT(gt)
	require := t.Require()
	logger := t.Logger()

	// ═══════════════════════════════════════════════════════════════════════
	// Phase 1: Deploy system normally and verify games are created
	// ═══════════════════════════════════════════════════════════════════════

	cfg := opspresets.DefaultFPProposerConfig()
	cfg.ProposalIntervalInBlocks = 20 // Use reasonable interval
	cfg.FastFinalityMode = false

	sys := opspresets.NewFaultProofSystem(t, cfg, opspresets.DefaultL2ChainConfig())
	dgf := sys.DgfClient(t)

	ctx, cancel := context.WithTimeout(t.Ctx(), utils.LongTimeout())
	defer cancel()

	// Wait for at least one game to be created (proves normal operation)
	logger.Info("Phase 1: Waiting for initial game creation (normal operation)")
	utils.WaitForGameCount(ctx, t, dgf, 1)

	initialCount, err := dgf.GameCount(ctx)
	require.NoError(err)
	logger.Info("Initial games created", "count", initialCount)
	require.GreaterOrEqual(initialCount, uint64(1),
		"At least one game should be created before upgrade")

	// ═══════════════════════════════════════════════════════════════════════
	// Phase 2: Simulate hardfork by upgrading game implementation
	// ═══════════════════════════════════════════════════════════════════════

	logger.Info("Phase 2: Upgrading game implementation with fake vkeys via Forge script",
		"fakeAggVkey", fakeAggregationVkey[:20]+"...",
		"fakeRangeVkey", fakeRangeVkeyCommitment[:20]+"...")

	// Deploy and upgrade via Forge script (same script operators use in production)
	newImplAddr, err := sys.UpgradeGameImplWithFakeVkeys(ctx, t, fakeAggregationVkey, fakeRangeVkeyCommitment)
	require.NoError(err, "failed to upgrade game implementation via Forge script")
	logger.Info("Factory upgraded to new implementation via Forge script", "address", newImplAddr)

	// Record game count after upgrade
	gameCountAfterUpgrade, err := dgf.GameCount(ctx)
	require.NoError(err)
	logger.Info("Game count after upgrade", "count", gameCountAfterUpgrade)

	// ═══════════════════════════════════════════════════════════════════════
	// Phase 3: Verify proposer stops creating games
	// ═══════════════════════════════════════════════════════════════════════

	// Wait for sufficient time (3x proposal interval)
	// This gives proposer enough time to attempt game creation multiple times
	waitBlocks := cfg.ProposalIntervalInBlocks * 3
	waitDuration := time.Duration(waitBlocks*2) * time.Second // ~2s per block in test env

	logger.Info("Phase 3: Waiting to verify no new games created",
		"waitDuration", waitDuration,
		"waitBlocks", waitBlocks,
		"expectedBehavior", "proposer should skip due to vkey mismatch")

	// Periodically check that no new games are created
	ticker := time.NewTicker(10 * time.Second)
	defer ticker.Stop()

	checkTimeout := time.After(waitDuration)
	for {
		select {
		case <-checkTimeout:
			// Final check - game count should not have increased
			finalGameCount, err := dgf.GameCount(ctx)
			require.NoError(err)

			logger.Info("Final verification",
				"gameCountAfterUpgrade", gameCountAfterUpgrade,
				"finalGameCount", finalGameCount)

			// Assert no new games were created after upgrade
			require.Equal(gameCountAfterUpgrade, finalGameCount,
				"Proposer should NOT create new games after vkey mismatch. "+
					"Expected %d games, got %d. "+
					"on_chain_vkeys_match() should have returned false.",
				gameCountAfterUpgrade, finalGameCount)

			logger.Info("Test passed: proposer correctly stopped creating games after vkey mismatch")
			return

		case <-ticker.C:
			currentCount, err := dgf.GameCount(ctx)
			require.NoError(err)
			logger.Info("Periodic game count check",
				"current", currentCount,
				"afterUpgrade", gameCountAfterUpgrade)

			// Game count should not increase
			require.Equal(gameCountAfterUpgrade, currentCount,
				"Game was unexpectedly created despite vkey mismatch")

		case <-ctx.Done():
			require.Fail("Test timed out while waiting for game count verification")
			return
		}
	}
}
