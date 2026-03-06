package presets

import (
	"context"
	"fmt"
	"math/big"

	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	"github.com/ethereum-optimism/optimism/op-devstack/presets"
	"github.com/ethereum-optimism/optimism/op-devstack/stack"
	"github.com/ethereum-optimism/optimism/op-devstack/sysgo"
	"github.com/ethereum-optimism/optimism/op-service/eth"
	"github.com/ethereum/go-ethereum/accounts/abi/bind"
	"github.com/ethereum/go-ethereum/common"
	opsbind "github.com/succinctlabs/op-succinct/bindings"
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
	BackupPath  string

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
	if c.BackupPath != "" {
		opts = append(opts, sysgo.WithFPBackupPath(c.BackupPath))
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

		// Build FDG deployment options
		fdgOpts := []sysgo.FdgOption{
			sysgo.WithFdgL2StartingBlockNumber(1),
			sysgo.WithFdgMaxChallengeDuration(cfg.MaxChallengeDuration),
			sysgo.WithFdgMaxProveDuration(cfg.MaxProveDuration),
			sysgo.WithFdgDisputeGameFinalityDelaySecs(cfg.DisputeGameFinalityDelaySecs),
		}

		opt.Add(sysgo.WithSuperDeployOPSuccinctFaultDisputeGame(ids.L1CL, ids.L1EL, ids.L2ACL, ids.L2AEL, fdgOpts...))
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
	orch       *sysgo.Orchestrator
	ids        sysgo.DefaultSingleChainInteropSystemIDs
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
		orch:                orch,
		ids:                 ids,
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

// GameImplConfig holds the configuration needed to deploy a new game implementation.
// This is extracted from the current deployed game implementation.
type GameImplConfig struct {
	FactoryProxy        common.Address
	VerifierAddress     common.Address
	AnchorStateRegistry common.Address
	AccessManager       common.Address
	RollupConfigHash    [32]byte
	AggregationVkey     [32]byte
	RangeVkeyCommitment [32]byte
	MaxChallengeDuration uint64
	MaxProveDuration     uint64
	ChallengerBondWei    *big.Int
}

// OPSuccinctGameType is the game type used for OP Succinct fault dispute games.
const OPSuccinctGameType = uint32(42)

// GetGameImplConfig queries the current game implementation to get its configuration.
// This is used to deploy a new game implementation with the same configuration but different vkeys.
func (s *FaultProofSystem) GetGameImplConfig(ctx context.Context, t devtest.T) (*GameImplConfig, error) {
	client := s.L1EL.EthClient()
	dgfAddr := s.L2Chain.Escape().Deployment().DisputeGameFactoryProxyAddr()

	// Create DGF binding to get the game implementation address
	dgf, err := opsbind.NewDisputeGameFactoryCaller(dgfAddr, utils.NewEthCaller(client))
	if err != nil {
		return nil, fmt.Errorf("bind DGF: %w", err)
	}

	// Get the game implementation address for our game type
	gameImplAddr, err := dgf.GameImpls(&bind.CallOpts{Context: ctx}, OPSuccinctGameType)
	if err != nil {
		return nil, fmt.Errorf("get game impl address: %w", err)
	}

	// Create game binding to query its configuration
	game, err := opsbind.NewOPSuccinctFaultDisputeGameCaller(gameImplAddr, utils.NewEthCaller(client))
	if err != nil {
		return nil, fmt.Errorf("bind game impl: %w", err)
	}

	opts := &bind.CallOpts{Context: ctx}

	// Query all configuration values
	verifier, err := game.Sp1Verifier(opts)
	if err != nil {
		return nil, fmt.Errorf("get verifier: %w", err)
	}

	asr, err := game.AnchorStateRegistry(opts)
	if err != nil {
		return nil, fmt.Errorf("get anchor state registry: %w", err)
	}

	accessManager, err := game.AccessManager(opts)
	if err != nil {
		return nil, fmt.Errorf("get access manager: %w", err)
	}

	rollupConfigHash, err := game.RollupConfigHash(opts)
	if err != nil {
		return nil, fmt.Errorf("get rollup config hash: %w", err)
	}

	aggVkey, err := game.AggregationVkey(opts)
	if err != nil {
		return nil, fmt.Errorf("get aggregation vkey: %w", err)
	}

	rangeVkey, err := game.RangeVkeyCommitment(opts)
	if err != nil {
		return nil, fmt.Errorf("get range vkey commitment: %w", err)
	}

	maxChallengeDuration, err := game.MaxChallengeDuration(opts)
	if err != nil {
		return nil, fmt.Errorf("get max challenge duration: %w", err)
	}

	maxProveDuration, err := game.MaxProveDuration(opts)
	if err != nil {
		return nil, fmt.Errorf("get max prove duration: %w", err)
	}

	challengerBond, err := game.ChallengerBond(opts)
	if err != nil {
		return nil, fmt.Errorf("get challenger bond: %w", err)
	}

	return &GameImplConfig{
		FactoryProxy:         dgfAddr,
		VerifierAddress:      verifier,
		AnchorStateRegistry:  asr,
		AccessManager:        accessManager,
		RollupConfigHash:     rollupConfigHash,
		AggregationVkey:      aggVkey,
		RangeVkeyCommitment:  rangeVkey,
		MaxChallengeDuration: maxChallengeDuration,
		MaxProveDuration:     maxProveDuration,
		ChallengerBondWei:    challengerBond,
	}, nil
}

// UpgradeGameImplWithFakeVkeys deploys a new game implementation with fake vkeys and
// upgrades the factory to use it. This uses the same Forge script (UpgradeOPSuccinctFDG.s.sol)
// that operators use in production.
//
// This simulates a hardfork scenario where the on-chain vkeys don't match the proposer's
// computed vkeys.
func (s *FaultProofSystem) UpgradeGameImplWithFakeVkeys(
	ctx context.Context,
	t devtest.T,
	fakeAggVkey, fakeRangeVkey string,
) (common.Address, error) {
	implCfg, err := s.GetGameImplConfig(ctx, t)
	if err != nil {
		return common.Address{}, fmt.Errorf("get game impl config: %w", err)
	}

	upgradeCfg := sysgo.UpgradeGameImplConfig{
		FactoryAddress:       implCfg.FactoryProxy,
		GameType:             OPSuccinctGameType,
		MaxChallengeDuration: implCfg.MaxChallengeDuration,
		MaxProveDuration:     implCfg.MaxProveDuration,
		VerifierAddress:      implCfg.VerifierAddress,
		RollupConfigHash:     fmt.Sprintf("0x%x", implCfg.RollupConfigHash),
		AggregationVkey:      fakeAggVkey,
		RangeVkeyCommitment:  fakeRangeVkey,
		ChallengerBondWei:    implCfg.ChallengerBondWei.String(),
		AnchorStateRegistry:  implCfg.AnchorStateRegistry,
		AccessManager:        implCfg.AccessManager,
	}

	newImplAddr, err := s.orch.UpgradeGameImpl(s.ids.L1EL, upgradeCfg)
	if err != nil {
		return common.Address{}, fmt.Errorf("upgrade game impl via Forge script: %w", err)
	}

	t.Logger().Info("Upgraded game implementation with fake vkeys via Forge script",
		"address", newImplAddr,
		"fakeAggVkey", fakeAggVkey[:20]+"...",
		"fakeRangeVkey", fakeRangeVkey[:20]+"...")

	return newImplAddr, nil
}

