package longrunning

import (
	"fmt"
	"testing"

	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	"github.com/ethereum-optimism/optimism/op-service/eth"
	opspresets "github.com/succinctlabs/op-succinct/presets"
	"github.com/succinctlabs/op-succinct/utils"
)

// TestFaultProofProposer_Progress verifies the proposer keeps up with L2 finalization
// and produces correct root claims. Fails if the lag between finalized L2 blocks and
// the latest game exceeds the allowed threshold, or if any root claim is incorrect.
func TestFaultProofProposer_Progress(gt *testing.T) {
	t := devtest.ParallelT(gt)
	cfg := opspresets.LongRunningFaultProofConfig()
	sys, dgf := setupFaultProofSystem(t, cfg, opspresets.LongRunningL2ChainConfig())

	err := utils.RunProgressTest(func() error {
		return checkLatestGame(t, sys, dgf, nil)
	})
	t.Require().NoError(err, "proposer progress check failed")
}

// TestFaultProofProposer_FastFinality_Progress verifies fast finality mode keeps up with L2 finalization,
// ensures games are being proven (not just created), and verifies root claims are correct.
func TestFaultProofProposer_FastFinality_Progress(gt *testing.T) {
	t := devtest.ParallelT(gt)
	cfg := opspresets.LongRunningFastFinalityFaultProofConfig()
	sys, dgf := setupFaultProofSystem(t, cfg, opspresets.LongRunningL2ChainConfig())

	err := utils.RunProgressTest(func() error {
		return checkLatestGame(t, sys, dgf, &cfg)
	})
	t.Require().NoError(err, "proposer progress check failed")

	// Verify fast finality is proving games
	ctx := t.Ctx()
	firstGame, err := dgf.GameAtIndex(ctx, 0)
	t.Require().NoError(err)
	fdg, err := utils.NewFdgClient(sys.L1EL.EthClient(), firstGame.Proxy)
	t.Require().NoError(err)
	proven, err := fdg.IsProven(ctx)
	t.Require().NoError(err)
	t.Require().True(proven, "fast finality did not prove first game")
}

func setupFaultProofSystem(t devtest.T, cfg opspresets.FaultProofConfig, chain opspresets.L2ChainConfig) (*opspresets.FaultProofSystem, *utils.DgfClient) {
	sys := opspresets.NewFaultProofSystem(t, cfg, chain)
	t.Log("=== Stack is running ===")
	return sys, sys.DgfClient(t)
}

// checkLatestGame verifies the latest game's lag and root claim correctness.
// If cfg is provided, also checks anchor state lag (for fast finality mode).
func checkLatestGame(t devtest.T, sys *opspresets.FaultProofSystem, dgf *utils.DgfClient, cfg *opspresets.FaultProofConfig) error {
	ctx := t.Ctx()
	l2Finalized := sys.L2EL.BlockRefByLabel(eth.Finalized)

	game, err := dgf.LatestGame(ctx)
	if err != nil {
		return err
	}
	if game == nil {
		t.Logf("Games: 0 | L2 finalized: %d | waiting...", l2Finalized.Number)
		return nil
	}

	fdg, err := utils.NewFdgClient(sys.L1EL.EthClient(), game.Proxy)
	if err != nil {
		return err
	}

	gameL2Block, err := fdg.L2BlockNumber(ctx)
	if err != nil {
		return err
	}

	// Check proposer lag
	var lag uint64
	if l2Finalized.Number > gameL2Block {
		lag = l2Finalized.Number - gameL2Block
	}
	maxLag := utils.MaxProposerLag()
	t.Logf("L2 Finalized: %d | Latest Game L2 Block: %d | Lag: %d blocks (max: %d)",
		l2Finalized.Number, gameL2Block, lag, maxLag)
	if lag > maxLag {
		return fmt.Errorf("proposer lag %d exceeds max %d", lag, maxLag)
	}

	// Check anchor state lag (fast finality only)
	if cfg != nil {
		l2BlockTime := sys.L2Chain.Escape().RollupConfig().BlockTime
		anchorL2Block, err := fdg.AnchorL2BlockNumber(ctx, sys.L1EL.EthClient(), game.GameType)
		if err != nil {
			return err
		}
		anchorLagBlocks := gameL2Block - anchorL2Block
		anchorLagSeconds := anchorLagBlocks * l2BlockTime
		t.Logf("Anchor Lag: game L2=%d, anchor L2=%d, lag=%d blocks (%ds), max=%ds",
			gameL2Block, anchorL2Block, anchorLagBlocks, anchorLagSeconds, cfg.MaxChallengeDuration)
		if anchorLagSeconds > cfg.MaxChallengeDuration {
			return fmt.Errorf("anchor lag %d seconds exceeds max %d seconds", anchorLagSeconds, cfg.MaxChallengeDuration)
		}
	}

	// Check root claim correctness
	rootClaim, err := fdg.RootClaim(ctx)
	if err != nil {
		return fmt.Errorf("get root claim: %w", err)
	}
	return utils.VerifyOutputRoot(ctx, sys.L2EL.Escape().L2EthClient(), gameL2Block, rootClaim)
}
