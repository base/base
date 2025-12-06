package presets

import (
	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	"github.com/ethereum-optimism/optimism/op-devstack/presets"
	"github.com/ethereum-optimism/optimism/op-devstack/stack"
	"github.com/ethereum-optimism/optimism/op-devstack/sysgo"
	"github.com/ethereum-optimism/optimism/op-service/eth"
)

// ValidityConfig holds configuration for validity proposer tests.
type ValidityConfig struct {
	StartingBlock      uint64
	SubmissionInterval uint64
	RangeProofInterval uint64
}

// DefaultValidityConfig returns the default configuration.
func DefaultValidityConfig() ValidityConfig {
	return ValidityConfig{
		StartingBlock:      1,
		SubmissionInterval: 10,
		RangeProofInterval: 10,
	}
}

// ExpectedOutputBlock calculates the expected L2 block for the Nth submission.
func (c ValidityConfig) ExpectedOutputBlock(submissionCount int) uint64 {
	rangesPerSubmission := (c.SubmissionInterval + c.RangeProofInterval - 1) / c.RangeProofInterval
	blocksPerSubmission := rangesPerSubmission * c.RangeProofInterval
	return c.StartingBlock + uint64(submissionCount)*blocksPerSubmission
}

// ExpectedRangeCount returns the expected number of range proofs for a given output block.
func (c ValidityConfig) ExpectedRangeCount(outputBlock uint64) int {
	blocksToProve := outputBlock - c.StartingBlock
	return int((blocksToProve + c.RangeProofInterval - 1) / c.RangeProofInterval)
}

// WithSuccinctValidityProposer creates a validity proposer with custom configuration.
func WithSuccinctValidityProposer(dest *sysgo.DefaultSingleChainInteropSystemIDs, cfg ValidityConfig) stack.CommonOption {
	return withSuccinctPreset(dest, func(opt *stack.CombinedOption[*sysgo.Orchestrator], ids sysgo.DefaultSingleChainInteropSystemIDs, l2ChainID eth.ChainID) {
		opt.Add(sysgo.WithSuperDeploySP1MockVerifier(ids.L1EL, l2ChainID))
		opt.Add(sysgo.WithSuperDeployOpSuccinctL2OutputOracle(ids.L1CL, ids.L1EL, ids.L2ACL, ids.L2AEL,
			sysgo.WithL2OOStartingBlockNumber(cfg.StartingBlock),
			sysgo.WithL2OOSubmissionInterval(cfg.SubmissionInterval),
			sysgo.WithL2OORangeProofInterval(cfg.RangeProofInterval)))
		opt.Add(sysgo.WithSuperSuccinctValidityProposer(ids.L2AProposer, ids.L1CL, ids.L1EL, ids.L2ACL, ids.L2AEL,
			sysgo.WithVPSubmissionInterval(cfg.SubmissionInterval),
			sysgo.WithVPRangeProofInterval(cfg.RangeProofInterval),
			sysgo.WithVPMockMode(true)))
	})
}

// WithDefaultSuccinctValidityProposer creates a validity proposer with default configuration.
// This maintains backward compatibility with existing tests.
func WithDefaultSuccinctValidityProposer(dest *sysgo.DefaultSingleChainInteropSystemIDs) stack.CommonOption {
	return WithSuccinctValidityProposer(dest, DefaultValidityConfig())
}

// ValiditySystem wraps MinimalWithProposer and provides access to validity-specific features.
type ValiditySystem struct {
	*presets.MinimalWithProposer
	proposer sysgo.ValidityProposer
}

// DatabaseURL returns the database URL used by the validity proposer.
func (s *ValiditySystem) DatabaseURL() string {
	return s.proposer.DatabaseURL()
}

// NewValiditySystem creates a new validity test system with custom configuration.
func NewValiditySystem(t devtest.T, cfg ValidityConfig) *ValiditySystem {
	var ids sysgo.DefaultSingleChainInteropSystemIDs
	sys, prop := newSystemWithProposer(t, WithSuccinctValidityProposer(&ids, cfg), &ids)

	vp, ok := prop.(sysgo.ValidityProposer)
	t.Require().True(ok, "proposer must implement ValidityProposer")

	return &ValiditySystem{
		MinimalWithProposer: sys,
		proposer:            vp,
	}
}
