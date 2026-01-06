package presets

import (
	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	"github.com/ethereum-optimism/optimism/op-devstack/presets"
	"github.com/ethereum-optimism/optimism/op-devstack/stack"
	"github.com/ethereum-optimism/optimism/op-devstack/sysgo"
	"github.com/ethereum-optimism/optimism/op-service/eth"
	"github.com/succinctlabs/op-succinct/utils"
)

// FPProposerConfig holds configuration for fault proof proposer tests.
type FPProposerConfig struct {
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
	// Timeout is the proving timeout in seconds.
	// If nil, the default (4 hours / 14400s) is used.
	Timeout     *uint64
	EnvFilePath string

	// AggProofMode selects the SP1 verifier backend ("plonk" or "groth16").
	// Only applies for network proving (i.e. when utils.UseNetworkProver() is true).
	// If nil/empty, defaults to "plonk".
	AggProofMode *string
}

// DefaultFPProposerConfig returns the default configuration for fast tests.
func DefaultFPProposerConfig() FPProposerConfig {
	return FPProposerConfig{
		MaxChallengeDuration:         10, // Low for tests (vs 1 hour production)
		MaxProveDuration:             10, // Low for tests (vs 12 hours production)
		DisputeGameFinalityDelaySecs: 30, // Low for tests (vs 7 days production)
		ProposalIntervalInBlocks:     10,
		FetchInterval:                1,
		RangeSplitCount:              1,
		MaxConcurrentRangeProofs:     1,
		FastFinalityMode:             false,
		FastFinalityProvingLimit:     1,
	}
}

// FastFinalityFPProposerConfig returns configuration with fast finality mode enabled.
func FastFinalityFPProposerConfig() FPProposerConfig {
	cfg := DefaultFPProposerConfig()
	cfg.FastFinalityMode = true
	return cfg
}

// LongRunningFPProposerConfig returns configuration optimized for long-running progress tests.
// If NETWORK_PRIVATE_KEY is set, uses larger intervals tuned for network proving.
func LongRunningFPProposerConfig() FPProposerConfig {
	cfg := DefaultFPProposerConfig()
	cfg.MaxChallengeDuration = 1800 // =30m

	timeout := uint64(900) // =15m
	cfg.Timeout = &timeout

	if utils.UseNetworkProver() {
		cfg.ProposalIntervalInBlocks = 240
	} else {
		cfg.ProposalIntervalInBlocks = 120
	}
	return cfg
}

// LongRunningFastFinalityFPProposerConfig returns configuration for long-running fast finality tests.
// If NETWORK_PRIVATE_KEY is set, uses larger intervals tuned for network proving.
func LongRunningFastFinalityFPProposerConfig() FPProposerConfig {
	cfg := LongRunningFPProposerConfig()
	cfg.FastFinalityMode = true
	cfg.FastFinalityProvingLimit = 8
	return cfg
}

// ProposerOptions returns the proposer options for this configuration.
func (c FPProposerConfig) ProposerOptions() []sysgo.FaultProofProposerOption {
	opts := []sysgo.FaultProofProposerOption{
		sysgo.WithFPProposalIntervalInBlocks(c.ProposalIntervalInBlocks),
		sysgo.WithFPFetchInterval(c.FetchInterval),
		sysgo.WithFPRangeSplitCount(c.RangeSplitCount),
		sysgo.WithFPMaxConcurrentRangeProofs(c.MaxConcurrentRangeProofs),
		sysgo.WithFPFastFinalityMode(c.FastFinalityMode),
		sysgo.WithFPFastFinalityProvingLimit(c.FastFinalityProvingLimit),
	}
	if c.Timeout != nil {
		opts = append(opts, sysgo.WithFPTimeout(*c.Timeout))
	}
	if c.EnvFilePath != "" {
		opts = append(opts, sysgo.WithFPWriteEnvFile(c.EnvFilePath))
	}
	return opts
}

// FPChallengerConfig holds configuration for fault proof challenger tests.
type FPChallengerConfig struct {
	// FetchInterval is the polling interval in seconds for the challenger.
	FetchInterval uint64

	// MaliciousChallengePercentage is the percentage of valid games to challenge
	// maliciously for testing defense mechanisms. Set to 0.0 for honest mode (default).
	MaliciousChallengePercentage float64

	// EnvFilePath is an optional path to write the challenger environment variables.
	EnvFilePath string
}

// DefaultFPChallengerConfig returns the default challenger configuration.
func DefaultFPChallengerConfig() FPChallengerConfig {
	return FPChallengerConfig{
		FetchInterval:                1,
		MaliciousChallengePercentage: 0.0,
	}
}

// WithSuccinctFPProposer creates a fault proof proposer with custom configuration.
func WithSuccinctFPProposer(dest *sysgo.DefaultSingleChainInteropSystemIDs, cfg FPProposerConfig, chain L2ChainConfig) stack.CommonOption {
	// Set batcher's MaxBlocksPerSpanBatch to match the proposal interval
	maxBlocksPerSpanBatch := int(cfg.ProposalIntervalInBlocks)
	return withSuccinctPreset(dest, chain, maxBlocksPerSpanBatch, cfg.AggProofMode, func(opt *stack.CombinedOption[*sysgo.Orchestrator], ids sysgo.DefaultSingleChainInteropSystemIDs, l2ChainID eth.ChainID) {
		if !utils.UseNetworkProver() {
			opt.Add(sysgo.WithSuperDeploySP1MockVerifier(ids.L1EL, l2ChainID))
		}
		opt.Add(sysgo.WithSuperDeployOPSuccinctFaultDisputeGame(ids.L1CL, ids.L1EL, ids.L2ACL, ids.L2AEL,
			sysgo.WithFdgL2StartingBlockNumber(1),
			sysgo.WithFdgMaxChallengeDuration(cfg.MaxChallengeDuration),
			sysgo.WithFdgMaxProveDuration(cfg.MaxProveDuration),
			sysgo.WithFdgDisputeGameFinalityDelaySecs(cfg.DisputeGameFinalityDelaySecs)))
		opt.Add(sysgo.WithSuperSuccinctFaultProofProposer(ids.L2AProposer, ids.L1CL, ids.L1EL, ids.L2ACL, ids.L2AEL, cfg.ProposerOptions()...))
	})
}

// WithDefaultSuccinctFPProposer creates a fault proof proposer with default configuration.
func WithDefaultSuccinctFPProposer(dest *sysgo.DefaultSingleChainInteropSystemIDs) stack.CommonOption {
	return WithSuccinctFPProposer(dest, DefaultFPProposerConfig(), DefaultL2ChainConfig())
}

// WithSuccinctFPChallenger creates a fault proof challenger with custom configuration.
// Use with WithSuccinctFPProposer to create a complete system with both proposer and challenger.
func WithSuccinctFPChallenger(ids *sysgo.DefaultSingleChainInteropSystemIDs, cfg FPChallengerConfig) stack.CommonOption {
	return stack.MakeCommon(stack.Finally(func(orch *sysgo.Orchestrator) {
		challengerOpts := []sysgo.FaultProofChallengerOption{
			sysgo.WithFPChallengerFetchInterval(cfg.FetchInterval),
		}
		if cfg.MaliciousChallengePercentage > 0 {
			challengerOpts = append(challengerOpts, sysgo.WithFPChallengerMaliciousChallengePercentage(cfg.MaliciousChallengePercentage))
		}
		if cfg.EnvFilePath != "" {
			challengerOpts = append(challengerOpts, sysgo.WithFPChallengerWriteEnvFile(cfg.EnvFilePath))
		}
		sysgo.WithSuccinctFaultProofChallengerPostDeploy(orch, ids.L2ChallengerA, ids.L1EL, ids.L2AEL, challengerOpts...)
	}))
}

// WithDefaultSuccinctFPChallenger creates a fault proof challenger with default configuration.
func WithDefaultSuccinctFPChallenger(ids *sysgo.DefaultSingleChainInteropSystemIDs) stack.CommonOption {
	return WithSuccinctFPChallenger(ids, DefaultFPChallengerConfig())
}

// WithSuccinctFPProposerAndChallenger creates both a fault proof proposer and challenger with custom configuration.
func WithSuccinctFPProposerAndChallenger(dest *sysgo.DefaultSingleChainInteropSystemIDs, proposerCfg FPProposerConfig, challengerCfg FPChallengerConfig, chain L2ChainConfig) stack.CommonOption {
	return stack.MakeCommon(stack.Combine(
		WithSuccinctFPProposer(dest, proposerCfg, chain),
		WithSuccinctFPChallenger(dest, challengerCfg),
	))
}

// WithDefaultSuccinctFPProposerAndChallenger creates both a fault proof proposer and challenger with default configuration.
func WithDefaultSuccinctFPProposerAndChallenger(dest *sysgo.DefaultSingleChainInteropSystemIDs) stack.CommonOption {
	return WithSuccinctFPProposerAndChallenger(dest, DefaultFPProposerConfig(), DefaultFPChallengerConfig(), DefaultL2ChainConfig())
}

// FaultProofSystem wraps MinimalWithProposer and provides access to faultproof-specific features.
type FaultProofSystem struct {
	*presets.MinimalWithProposer
	proposer   sysgo.FaultProofProposer
	challenger sysgo.FaultProofChallenger // optional, nil if not configured
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

// HasChallenger returns true if the system was configured with a challenger.
func (s *FaultProofSystem) HasChallenger() bool {
	return s.challenger != nil
}

// StopChallenger stops the faultproof challenger (for restart testing).
// Panics if the system was not configured with a challenger.
func (s *FaultProofSystem) StopChallenger() {
	if s.challenger == nil {
		panic("system has no challenger; use WithChallenger option when creating the system")
	}
	s.challenger.Stop()
}

// StartChallenger starts the faultproof challenger (for restart testing).
// Panics if the system was not configured with a challenger.
func (s *FaultProofSystem) StartChallenger() {
	if s.challenger == nil {
		panic("system has no challenger; use WithChallenger option when creating the system")
	}
	s.challenger.Start()
}

// faultProofSystemOptions holds optional configuration for NewFaultProofSystem.
type faultProofSystemOptions struct {
	challengerCfg *FPChallengerConfig
}

// FaultProofSystemOption configures optional features of the fault proof system.
type FaultProofSystemOption func(*faultProofSystemOptions)

// WithChallenger adds a challenger to the fault proof system.
func WithChallenger(cfg FPChallengerConfig) FaultProofSystemOption {
	return func(o *faultProofSystemOptions) {
		o.challengerCfg = &cfg
	}
}

// WithDefaultChallenger adds a challenger with default configuration.
func WithDefaultChallenger() FaultProofSystemOption {
	return WithChallenger(DefaultFPChallengerConfig())
}

// NewFaultProofSystem creates a new fault proof test system with custom configuration.
// Use WithChallenger option to include a challenger.
func NewFaultProofSystem(t devtest.T, proposerCfg FPProposerConfig, chain L2ChainConfig, opts ...FaultProofSystemOption) *FaultProofSystem {
	var options faultProofSystemOptions
	for _, opt := range opts {
		opt(&options)
	}

	var ids sysgo.DefaultSingleChainInteropSystemIDs

	// Build the system option based on whether challenger is requested
	var stackOpt stack.CommonOption
	if options.challengerCfg != nil {
		stackOpt = WithSuccinctFPProposerAndChallenger(&ids, proposerCfg, *options.challengerCfg, chain)
	} else {
		stackOpt = WithSuccinctFPProposer(&ids, proposerCfg, chain)
	}

	sys, orch, prop := newSystemWithProposer(t, stackOpt, &ids)

	fp, ok := prop.(sysgo.FaultProofProposer)
	t.Require().True(ok, "proposer must implement FaultProofProposer")

	result := &FaultProofSystem{
		MinimalWithProposer: sys,
		proposer:            fp,
	}

	// Add challenger if configured
	if options.challengerCfg != nil {
		chall, ok := orch.GetChallenger(ids.L2ChallengerA)
		t.Require().True(ok, "challenger not found")

		fc, ok := chall.(sysgo.FaultProofChallenger)
		t.Require().True(ok, "challenger must implement FaultProofChallenger")
		result.challenger = fc
	}

	return result
}

// NewDefaultFaultProofSystem creates a fault proof system with default proposer config (no challenger).
func NewDefaultFaultProofSystem(t devtest.T) *FaultProofSystem {
	return NewFaultProofSystem(t, DefaultFPProposerConfig(), DefaultL2ChainConfig())
}

// NewDefaultFaultProofSystemWithChallenger creates a fault proof system with both proposer and challenger using default configs.
func NewDefaultFaultProofSystemWithChallenger(t devtest.T) *FaultProofSystem {
	return NewFaultProofSystem(t, DefaultFPProposerConfig(), DefaultL2ChainConfig(), WithDefaultChallenger())
}
