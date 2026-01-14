// Package fpdefense tests proposer defense mechanisms against challenges.
package fpdefense

import (
	"context"
	"testing"

	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	opspresets "github.com/succinctlabs/op-succinct/presets"
	"github.com/succinctlabs/op-succinct/utils"
)

// TestFaultProofProposer_DefendsWithProofAfterChallenge verifies:
// 1. Proposer creates a valid game
// 2. Malicious challenger challenges the game
// 3. Proposer detects challenge and generates proof
// 4. Proposer submits proof on-chain
// 5. Game resolves as DEFENDER_WINS
func TestFaultProofProposer_DefendsWithProofAfterChallenge(gt *testing.T) {
	t := devtest.ParallelT(gt)
	require := t.Require()
	logger := t.Logger()

	// === SETUP ===
	// Configure proposer with fast finality mode for proof generation
	proposerCfg := opspresets.FastFinalityFPProposerConfig()
	proposerCfg.ProposalIntervalInBlocks = 40
	proposerCfg.RangeSplitCount = 1
	proposerCfg.MaxConcurrentRangeProofs = 1
	proposerCfg.MaxProveDuration = 60 // Give proposer 60s to respond to challenge

	// Configure challenger to always challenge valid games (malicious mode)
	challengerCfg := opspresets.DefaultFPChallengerConfig()
	challengerCfg.MaliciousChallengePercentage = 100.0

	sys := opspresets.NewFaultProofSystem(t, proposerCfg, opspresets.DefaultL2ChainConfig(),
		opspresets.WithChallenger(challengerCfg))

	dgf := sys.DgfClient(t)
	ctx, cancel := context.WithTimeout(t.Ctx(), utils.LongTimeout())
	defer cancel()

	// === PHASE 1: Wait for game creation ===
	logger.Info("Phase 1: Waiting for game creation")
	utils.WaitForGameCount(ctx, t, dgf, 1)

	game, err := dgf.GameAtIndex(ctx, 0)
	require.NoError(err, "failed to get first game")
	logger.Info("Game created", "proxy", game.Proxy.Hex())

	fdg, err := utils.NewFdgClient(sys.L1EL.EthClient(), game.Proxy)
	require.NoError(err, "failed to create FDG client")

	// Verify initial status
	initialStatus, err := fdg.ProposalStatus(ctx)
	require.NoError(err)
	require.Equal(utils.Unchallenged, initialStatus, "game should start Unchallenged")

	// === PHASE 2: Wait for challenge ===
	logger.Info("Phase 2: Waiting for challenge")
	utils.WaitForProposalStatus(ctx, t, fdg, utils.Challenged)
	logger.Info("Game has been challenged")

	// Stop challenger - no longer needed, cleaner logs
	sys.StopChallenger()
	logger.Info("Challenger stopped")

	// === PHASE 3: Wait for proof submission ===
	logger.Info("Phase 3: Waiting for proposer to submit proof")
	utils.WaitForProposalStatus(ctx, t, fdg, utils.ChallengedAndValidProofProvided)
	logger.Info("Proposer submitted valid proof")

	// Verify proof was recorded
	proven, err := fdg.IsProven(ctx)
	require.NoError(err)
	require.True(proven, "game should be proven")

	// === PHASE 4: Wait for resolution ===
	logger.Info("Phase 4: Waiting for game resolution")
	utils.WaitForDefenderWins(ctx, t, fdg)
	logger.Info("Game resolved as DEFENDER_WINS")

	// Verify final status
	finalStatus, err := fdg.Status(ctx)
	require.NoError(err)
	require.Equal(uint8(utils.DefenderWins), finalStatus)

	logger.Info("Defense test complete: Proposer successfully defended challenge")
}
