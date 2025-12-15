package presets

import (
	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	"github.com/ethereum-optimism/optimism/op-devstack/presets"
	"github.com/ethereum-optimism/optimism/op-devstack/stack"
	"github.com/ethereum-optimism/optimism/op-devstack/sysgo"
	"github.com/ethereum-optimism/optimism/op-service/eth"
	"github.com/succinctlabs/op-succinct/utils"
)

// FaultProofConfig holds configuration for fault proof proposer tests.
type FaultProofConfig struct {
	// Contract deployment configuration
	MaxChallengeDuration         uint64
	MaxProveDuration             uint64
	DisputeGameFinalityDelaySecs uint64

	// Proposer configuration
	ProposalIntervalInBlocks uint64
	FetchInterval            uint64
	RangeSplitCount          uint64
	MaxConcurrentRangeProofs uint64
	FastFinalityMode         bool
	FastFinalityProvingLimit uint64
}

// DefaultFaultProofConfig returns the default configuration.
func DefaultFaultProofConfig() FaultProofConfig {
	return FaultProofConfig{
		MaxChallengeDuration:         10, // Low for tests (vs 7 days production)
		MaxProveDuration:             10, // Low for tests (vs 1 day production)
		DisputeGameFinalityDelaySecs: 60, // Low for tests (vs 7 days production)
		ProposalIntervalInBlocks:     10,
		FetchInterval:                1,
		RangeSplitCount:              1,
		MaxConcurrentRangeProofs:     1,
		FastFinalityMode:             false,
		FastFinalityProvingLimit:     1,
	}
}

// FastFinalityFaultProofConfig returns configuration with fast finality mode enabled.
func FastFinalityFaultProofConfig() FaultProofConfig {
	cfg := DefaultFaultProofConfig()
	cfg.FastFinalityMode = true
	return cfg
}

// WithSuccinctFPProposer creates a fault proof proposer with custom configuration.
func WithSuccinctFPProposer(dest *sysgo.DefaultSingleChainInteropSystemIDs, cfg FaultProofConfig) stack.CommonOption {
	return withSuccinctPreset(dest, func(opt *stack.CombinedOption[*sysgo.Orchestrator], ids sysgo.DefaultSingleChainInteropSystemIDs, l2ChainID eth.ChainID) {
		opt.Add(sysgo.WithSuperDeploySP1MockVerifier(ids.L1EL, l2ChainID))
		opt.Add(sysgo.WithSuperDeployOPSuccinctFaultDisputeGame(ids.L1CL, ids.L1EL, ids.L2ACL, ids.L2AEL,
			sysgo.WithFdgL2StartingBlockNumber(1),
			sysgo.WithFdgMaxChallengeDuration(cfg.MaxChallengeDuration),
			sysgo.WithFdgMaxProveDuration(cfg.MaxProveDuration),
			sysgo.WithFdgDisputeGameFinalityDelaySecs(cfg.DisputeGameFinalityDelaySecs)))
		opt.Add(sysgo.WithSuperSuccinctFaultProofProposer(ids.L2AProposer, ids.L1CL, ids.L1EL, ids.L2ACL, ids.L2AEL,
			sysgo.WithFPProposalIntervalInBlocks(cfg.ProposalIntervalInBlocks),
			sysgo.WithFPFetchInterval(cfg.FetchInterval),
			sysgo.WithFPRangeSplitCount(cfg.RangeSplitCount),
			sysgo.WithFPMaxConcurrentRangeProofs(cfg.MaxConcurrentRangeProofs),
			sysgo.WithFPFastFinalityMode(cfg.FastFinalityMode),
			sysgo.WithFPFastFinalityProvingLimit(cfg.FastFinalityProvingLimit)))
	})
}

// WithDefaultSuccinctFPProposer creates a fault proof proposer with default configuration.
func WithDefaultSuccinctFPProposer(dest *sysgo.DefaultSingleChainInteropSystemIDs) stack.CommonOption {
	return WithSuccinctFPProposer(dest, DefaultFaultProofConfig())
}

// WithSuccinctFPProposerFastFinality creates a fault proof proposer optimized for fast finality.
func WithSuccinctFPProposerFastFinality(dest *sysgo.DefaultSingleChainInteropSystemIDs) stack.CommonOption {
	return WithSuccinctFPProposer(dest, FastFinalityFaultProofConfig())
}

// FaultProofSystem wraps MinimalWithProposer and provides access to faultproof-specific features.
type FaultProofSystem struct {
	*presets.MinimalWithProposer
	proposer sysgo.FaultProofProposer
}

// DgfClient creates a DisputeGameFactory client for the faultproof system.
func (s *FaultProofSystem) DgfClient(t devtest.T) *utils.DgfClient {
	dgfAddr := s.L2Chain.Escape().Deployment().DisputeGameFactoryProxyAddr()
	dgf, err := utils.NewDgfClient(s.L1EL.EthClient(), dgfAddr)
	t.Require().NoError(err, "failed to create DGF client")
	return dgf
}

// StopProposer stops the faultproof proposer (for restart testing).
func (s *FaultProofSystem) StopProposer() {
	s.proposer.Stop()
}

// StartProposer starts the faultproof proposer (for restart testing).
func (s *FaultProofSystem) StartProposer() {
	s.proposer.Start()
}

// NewFaultProofSystem creates a new fault proof test system with custom configuration.
func NewFaultProofSystem(t devtest.T, cfg FaultProofConfig) *FaultProofSystem {
	var ids sysgo.DefaultSingleChainInteropSystemIDs
	sys, prop := newSystemWithProposer(t, WithSuccinctFPProposer(&ids, cfg), &ids)

	fp, ok := prop.(sysgo.FaultProofProposer)
	t.Require().True(ok, "proposer must implement FaultProofProposer")

	return &FaultProofSystem{
		MinimalWithProposer: sys,
		proposer:            fp,
	}
}
