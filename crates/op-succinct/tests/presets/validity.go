package presets

import (
	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	"github.com/ethereum-optimism/optimism/op-devstack/presets"
	"github.com/ethereum-optimism/optimism/op-devstack/stack"
	"github.com/ethereum-optimism/optimism/op-devstack/sysgo"
	"github.com/ethereum-optimism/optimism/op-service/eth"
	"github.com/succinctlabs/op-succinct/utils"
)

// ValidityConfig holds configuration for validity proposer tests.
type ValidityConfig struct {
	StartingBlock      uint64
	SubmissionInterval uint64
	RangeProofInterval uint64
	// MaxConcurrentProofRequests limits concurrent proof requests.
	// If nil, the proposer's default is used.
	MaxConcurrentProofRequests *uint64
	// MaxConcurrentWitnessGen limits concurrent witness generation.
	// If nil, the proposer's default is used.
	MaxConcurrentWitnessGen *uint64
	// LoopInterval is the proposer's main loop interval in seconds.
	// The proposer acquires a database lock that expires after this duration.
	// Recovery tests must wait longer than this before restarting the proposer.
	// If nil, the default proposer value (60s) is used.
	LoopInterval *uint64
	// ProvingTimeout is the proving timeout in seconds.
	// If nil, the default (4 hours / 14400s) is used.
	ProvingTimeout *uint64
	EnvFilePath    string

	// AggProofMode selects the SP1 verifier backend ("plonk" or "groth16").
	// Only applies for network proving (i.e. when utils.UseNetworkProver() is true).
	// If nil/empty, defaults to "plonk".
	AggProofMode *string
}

// DefaultValidityConfig returns the default configuration for fast tests.
func DefaultValidityConfig() ValidityConfig {
	loopInterval := uint64(1) // 1s lock expiry; recovery tests wait 2s before restart
	return ValidityConfig{
		StartingBlock:      1,
		SubmissionInterval: 10,
		RangeProofInterval: 10,
		LoopInterval:       &loopInterval,
	}
}

// LongRunningValidityConfig returns configuration optimized for long-running progress tests.
// If NETWORK_PRIVATE_KEY is set, uses larger intervals tuned for network proving.
func LongRunningValidityConfig() ValidityConfig {
	cfg := DefaultValidityConfig()
	maxConcurrentProofRequests := uint64(4)
	maxConcurrentWitnessGen := uint64(4)
	cfg.MaxConcurrentProofRequests = &maxConcurrentProofRequests
	cfg.MaxConcurrentWitnessGen = &maxConcurrentWitnessGen

	provingTimeout := uint64(900) // =15m
	cfg.ProvingTimeout = &provingTimeout

	if utils.UseNetworkProver() {
		cfg.SubmissionInterval = 240
		cfg.RangeProofInterval = 240
	} else {
		cfg.SubmissionInterval = 120
		cfg.RangeProofInterval = 120
	}
	return cfg
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

// ProposerOptions returns the proposer options for this configuration.
func (c ValidityConfig) ProposerOptions() []sysgo.ValidityProposerOption {
	opts := []sysgo.ValidityProposerOption{
		sysgo.WithVPSubmissionInterval(c.SubmissionInterval),
		sysgo.WithVPRangeProofInterval(c.RangeProofInterval),
	}
	if c.MaxConcurrentProofRequests != nil {
		opts = append(opts, sysgo.WithVPMaxConcurrentProofRequests(*c.MaxConcurrentProofRequests))
	}
	if c.MaxConcurrentWitnessGen != nil {
		opts = append(opts, sysgo.WithVPMaxConcurrentWitnessGen(*c.MaxConcurrentWitnessGen))
	}
	if c.LoopInterval != nil {
		opts = append(opts, sysgo.WithVPLoopInterval(*c.LoopInterval))
	}
	if c.ProvingTimeout != nil {
		opts = append(opts, sysgo.WithVPProvingTimeout(*c.ProvingTimeout))
	}
	if c.EnvFilePath != "" {
		opts = append(opts, sysgo.WithVPWriteEnvFile(c.EnvFilePath))
	}
	return opts
}

// WithSuccinctValidityProposer creates a validity proposer with custom configuration.
func WithSuccinctValidityProposer(dest *sysgo.DefaultSingleChainInteropSystemIDs, cfg ValidityConfig, chain L2ChainConfig) stack.CommonOption {
	// Set batcher's MaxBlocksPerSpanBatch to match the submission interval
	maxBlocksPerSpanBatch := int(cfg.SubmissionInterval)
	return withSuccinctPreset(dest, chain, maxBlocksPerSpanBatch, cfg.AggProofMode, func(opt *stack.CombinedOption[*sysgo.Orchestrator], ids sysgo.DefaultSingleChainInteropSystemIDs, l2ChainID eth.ChainID) {
		if !utils.UseNetworkProver() {
			opt.Add(sysgo.WithSuperDeploySP1MockVerifier(ids.L1EL, l2ChainID))
		}
		opt.Add(sysgo.WithSuperDeployOpSuccinctL2OutputOracle(ids.L1CL, ids.L1EL, ids.L2ACL, ids.L2AEL,
			sysgo.WithL2OOStartingBlockNumber(cfg.StartingBlock),
			sysgo.WithL2OOSubmissionInterval(cfg.SubmissionInterval),
			sysgo.WithL2OORangeProofInterval(cfg.RangeProofInterval)))
		opt.Add(sysgo.WithSuperSuccinctValidityProposer(ids.L2AProposer, ids.L1CL, ids.L1EL, ids.L2ACL, ids.L2AEL, cfg.ProposerOptions()...))
	})
}

// WithDefaultSuccinctValidityProposer creates a validity proposer with default configuration.
func WithDefaultSuccinctValidityProposer(dest *sysgo.DefaultSingleChainInteropSystemIDs) stack.CommonOption {
	return WithSuccinctValidityProposer(dest, DefaultValidityConfig(), DefaultL2ChainConfig())
}

// ValiditySystem wraps MinimalWithProposer and provides access to validity-specific features.
type ValiditySystem struct {
	*presets.MinimalWithProposer
	proposer sysgo.ValidityProposer
}

// L2OOClient creates an L2OutputOracle client for the validity system.
func (s *ValiditySystem) L2OOClient(t devtest.T) *utils.L2OOClient {
	l2ooAddr := s.L2Chain.Escape().Deployment().OPSuccinctL2OutputOracleAddr()
	l2oo, err := utils.NewL2OOClient(s.L1EL.EthClient(), l2ooAddr)
	t.Require().NoError(err, "failed to create L2OO client")
	return l2oo
}

// DatabaseURL returns the database URL used by the validity proposer.
func (s *ValiditySystem) DatabaseURL() string {
	return s.proposer.DatabaseURL()
}

// StopProposer stops the validity proposer (for restart testing).
func (s *ValiditySystem) StopProposer() {
	s.proposer.Stop()
}

// StartProposer starts the validity proposer (for restart testing).
func (s *ValiditySystem) StartProposer() {
	s.proposer.Start()
}

// NewValiditySystem creates a new validity test system with custom configuration.
func NewValiditySystem(t devtest.T, cfg ValidityConfig, chain L2ChainConfig) *ValiditySystem {
	var ids sysgo.DefaultSingleChainInteropSystemIDs
	sys, prop := newSystemWithProposer(t, WithSuccinctValidityProposer(&ids, cfg, chain), &ids)

	vp, ok := prop.(sysgo.ValidityProposer)
	t.Require().True(ok, "proposer must implement ValidityProposer")

	return &ValiditySystem{
		MinimalWithProposer: sys,
		proposer:            vp,
	}
}
