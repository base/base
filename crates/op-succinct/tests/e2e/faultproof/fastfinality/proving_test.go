package fpfastfinality

import (
	"context"
	"testing"
	"time"

	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	opspresets "github.com/succinctlabs/op-succinct/presets"
	"github.com/succinctlabs/op-succinct/utils"
)

func TestFaultProofProposer_RangeSplitOne(gt *testing.T) {
	cfg := opspresets.FastFinalityFaultProofConfig()
	cfg.ProposalIntervalInBlocks = 40
	cfg.RangeSplitCount = 1
	cfg.MaxConcurrentRangeProofs = 1
	waitForDefenderWinsAtIndex(gt, 0, 10*time.Minute, cfg)
}

func TestFaultProofProposer_RangeSplitSixteen(gt *testing.T) {
	cfg := opspresets.FastFinalityFaultProofConfig()
	cfg.ProposalIntervalInBlocks = 40
	cfg.RangeSplitCount = 16
	cfg.MaxConcurrentRangeProofs = 16
	waitForDefenderWinsAtIndex(gt, 0, 10*time.Minute, cfg)
}

func TestFaultProofProposer_RangeSplitTwo_ThreeGames(gt *testing.T) {
	cfg := opspresets.FastFinalityFaultProofConfig()
	cfg.RangeSplitCount = 2
	cfg.MaxConcurrentRangeProofs = 2
	cfg.FastFinalityProvingLimit = 4
	waitForDefenderWinsAtIndex(gt, 2, 60*time.Minute, cfg)
}

func waitForDefenderWinsAtIndex(gt *testing.T, index int, timeout time.Duration, cfg opspresets.FaultProofConfig) {
	t := devtest.SerialT(gt)
	sys := opspresets.NewFaultProofSystem(t, cfg)
	require := t.Require()
	logger := t.Logger()
	ctx, cancel := context.WithTimeout(t.Ctx(), timeout)
	defer cancel()

	dgfAddr := sys.L2Chain.Escape().Deployment().DisputeGameFactoryProxyAddr()
	logger.Info("Dispute Game Factory Address:", "address", dgfAddr.Hex())
	dgf, err := utils.NewDgfClient(sys.L1EL.EthClient(), dgfAddr)
	require.NoError(err, "failed to create Dispute Game Factory client")

	utils.WaitForGameCount(ctx, t, dgf, uint64(index+1))

	game, err := dgf.GameAtIndex(ctx, uint64(index))
	require.NoError(err, "failed to get game from factory")

	fdg, err := utils.NewFdgClient(sys.L1EL.EthClient(), game.Proxy)
	require.NoError(err, "failed to create Fault Dispute Game client")

	utils.WaitForDefenderWins(ctx, t, fdg)
	t.Logger().Info("Dispute game defender wins", "gameIndex", index)
}
