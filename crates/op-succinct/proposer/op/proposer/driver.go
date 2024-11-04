package proposer

import (
	"context"
	"errors"
	"fmt"
	"math/big"
	_ "net/http/pprof"
	"sync"
	"time"

	"github.com/ethereum/go-ethereum"
	"github.com/ethereum/go-ethereum/accounts/abi"
	"github.com/ethereum/go-ethereum/accounts/abi/bind"
	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/core/types"
	"github.com/ethereum/go-ethereum/ethclient"
	"github.com/ethereum/go-ethereum/log"
	"github.com/slack-go/slack"

	// Original Optimism Bindings

	"github.com/ethereum-optimism/optimism/op-service/dial"
	"github.com/ethereum-optimism/optimism/op-service/eth"
	"github.com/ethereum-optimism/optimism/op-service/txmgr"

	// OP Succinct
	opsuccinctbindings "github.com/succinctlabs/op-succinct-go/bindings"
	"github.com/succinctlabs/op-succinct-go/proposer/db"
	"github.com/succinctlabs/op-succinct-go/proposer/db/ent/proofrequest"
	opsuccinctmetrics "github.com/succinctlabs/op-succinct-go/proposer/metrics"
)

var (
	slackMetricsTickerInterval = 30 * time.Minute
	supportedL2OutputVersion   = eth.Bytes32{}
	ErrProposerNotRunning      = errors.New("proposer is not running")
)

type L1Client interface {
	HeaderByNumber(ctx context.Context, number *big.Int) (*types.Header, error)
	// CodeAt returns the code of the given account. This is needed to differentiate
	// between contract internal errors and the local chain being out of sync.
	CodeAt(ctx context.Context, contract common.Address, blockNumber *big.Int) ([]byte, error)

	// CallContract executes an Ethereum contract call with the specified data as the
	// input.
	CallContract(ctx context.Context, call ethereum.CallMsg, blockNumber *big.Int) ([]byte, error)
}

type L2OOContract interface {
	Version(*bind.CallOpts) (string, error)
	LatestBlockNumber(*bind.CallOpts) (*big.Int, error)
	NextBlockNumber(*bind.CallOpts) (*big.Int, error)
	LatestOutputIndex(*bind.CallOpts) (*big.Int, error)
	NextOutputIndex(*bind.CallOpts) (*big.Int, error)
	StartingTimestamp(*bind.CallOpts) (*big.Int, error)
	L2BLOCKTIME(*bind.CallOpts) (*big.Int, error)
}

type RollupClient interface {
	SyncStatus(ctx context.Context) (*eth.SyncStatus, error)
	OutputAtBlock(ctx context.Context, blockNum uint64) (*eth.OutputResponse, error)
}

type DriverSetup struct {
	Log      log.Logger
	Metr     opsuccinctmetrics.OPSuccinctMetricer
	Cfg      ProposerConfig
	Txmgr    txmgr.TxManager
	L1Client *ethclient.Client

	// RollupProvider's RollupClient() is used to retrieve output roots from
	RollupProvider dial.RollupProvider
}

// L2OutputSubmitter is responsible for proposing outputs
type L2OutputSubmitter struct {
	DriverSetup

	wg   sync.WaitGroup
	done chan struct{}

	ctx    context.Context
	cancel context.CancelFunc

	mutex   sync.Mutex
	running bool

	l2ooContract L2OOContract
	l2ooABI      *abi.ABI

	db db.ProofDB
}

// NewL2OutputSubmitter creates a new L2 Output Submitter
func NewL2OutputSubmitter(setup DriverSetup) (_ *L2OutputSubmitter, err error) {
	ctx, cancel := context.WithCancel(context.Background())
	// The above context is long-lived, and passed to the `L2OutputSubmitter` instance. This context is closed by
	// `StopL2OutputSubmitting`, but if this function returns an error or panics, we want to ensure that the context
	// doesn't leak.
	defer func() {
		if err != nil || recover() != nil {
			cancel()
		}
	}()

	if setup.Cfg.L2OutputOracleAddr != nil {
		return newL2OOSubmitter(ctx, cancel, setup)
	} else {
		return nil, errors.New("the `L2OutputOracle` address was not provided")
	}
}

func newL2OOSubmitter(ctx context.Context, cancel context.CancelFunc, setup DriverSetup) (*L2OutputSubmitter, error) {
	l2ooContract, err := opsuccinctbindings.NewOPSuccinctL2OutputOracleCaller(*setup.Cfg.L2OutputOracleAddr, setup.L1Client)
	if err != nil {
		cancel()
		return nil, fmt.Errorf("failed to create L2OO at address %s: %w", setup.Cfg.L2OutputOracleAddr, err)
	}

	cCtx, cCancel := context.WithTimeout(ctx, setup.Cfg.NetworkTimeout)
	defer cCancel()
	version, err := l2ooContract.Version(&bind.CallOpts{Context: cCtx})
	if err != nil {
		cancel()
		return nil, err
	}
	log.Info("Connected to L2OutputOracle", "address", setup.Cfg.L2OutputOracleAddr, "version", version)

	parsed, err := opsuccinctbindings.OPSuccinctL2OutputOracleMetaData.GetAbi()
	if err != nil {
		cancel()
		return nil, err
	}

	db, err := db.InitDB(setup.Cfg.DbPath, setup.Cfg.UseCachedDb)
	if err != nil {
		cancel()
		return nil, err
	}

	return &L2OutputSubmitter{
		DriverSetup: setup,
		done:        make(chan struct{}),
		ctx:         ctx,
		cancel:      cancel,

		l2ooContract: l2ooContract,
		l2ooABI:      parsed,
		db:           *db,
	}, nil
}

func (l *L2OutputSubmitter) StartL2OutputSubmitting() error {
	l.Log.Info("Starting Proposer")

	l.mutex.Lock()
	defer l.mutex.Unlock()

	if l.running {
		return errors.New("proposer is already running")
	}
	l.running = true

	// When restarting the proposer using a cached database, we need to mark all proofs that are in witness generation state as failed, and retry them.
	witnessGenReqs, err := l.db.GetAllProofsWithStatus(proofrequest.StatusWITNESSGEN)
	if err != nil {
		return fmt.Errorf("failed to get witness generation pending proofs: %w", err)
	}
	for _, req := range witnessGenReqs {
		err = l.RetryRequest(req, ProofStatusResponse{})
		if err != nil {
			return fmt.Errorf("failed to retry request: %w", err)
		}
	}

	// Validate the contract's configuration of the aggregation and range verification keys as well
	// as the rollup config hash.
	err = l.ValidateConfig(l.Cfg.L2OutputOracleAddr.Hex())
	if err != nil {
		return fmt.Errorf("failed to validate config: %w", err)
	}

	l.wg.Add(1)
	go l.loop()

	l.Log.Info("Proposer started")
	return nil
}

func (l *L2OutputSubmitter) StopL2OutputSubmittingIfRunning() error {
	err := l.StopL2OutputSubmitting()
	if errors.Is(err, ErrProposerNotRunning) {
		return nil
	}
	return err
}

func (l *L2OutputSubmitter) StopL2OutputSubmitting() error {
	l.Log.Info("Stopping Proposer")

	l.mutex.Lock()
	defer l.mutex.Unlock()

	if !l.running {
		return ErrProposerNotRunning
	}
	l.running = false

	l.cancel()
	close(l.done)
	l.wg.Wait()

	if l.db != (db.ProofDB{}) {
		if err := l.db.CloseDB(); err != nil {
			return fmt.Errorf("error closing database: %w", err)
		}
	}

	l.Log.Info("Proposer stopped")
	return nil
}

// GetProposerMetrics gets the performance metrics for the proposer.
// TODO: Add a metric for the latest proven transaction.
func (l *L2OutputSubmitter) GetProposerMetrics(ctx context.Context) (opsuccinctmetrics.ProposerMetrics, error) {
	rollupClient, err := l.RollupProvider.RollupClient(ctx)
	if err != nil {
		return opsuccinctmetrics.ProposerMetrics{}, fmt.Errorf("getting rollup client: %w", err)
	}

	status, err := rollupClient.SyncStatus(ctx)
	if err != nil {
		return opsuccinctmetrics.ProposerMetrics{}, fmt.Errorf("getting sync status: %w", err)
	}

	// The unsafe head block on L2.
	l2UnsafeHeadBlock := status.UnsafeL2.Number
	l2FinalizedBlock := status.FinalizedL2.Number

	// The latest block number on the L2OO contract.
	latestContractL2Block, err := l.l2ooContract.LatestBlockNumber(&bind.CallOpts{Context: ctx})
	if err != nil {
		return opsuccinctmetrics.ProposerMetrics{}, fmt.Errorf("failed to get latest output index: %w", err)
	}

	// Get the highest proven L2 block contiguous with the contract's latest block.
	highestProvenContiguousL2Block, err := l.db.GetMaxContiguousSpanProofRange(latestContractL2Block.Uint64())
	if err != nil {
		return opsuccinctmetrics.ProposerMetrics{}, fmt.Errorf("failed to get max contiguous span proof range: %w", err)
	}

	numProving, err := l.db.GetNumberOfRequestsWithStatuses(proofrequest.StatusPROVING)
	if err != nil {
		return opsuccinctmetrics.ProposerMetrics{}, fmt.Errorf("failed to get number of proofs proving: %w", err)
	}

	numWitnessgen, err := l.db.GetNumberOfRequestsWithStatuses(proofrequest.StatusWITNESSGEN)
	if err != nil {
		return opsuccinctmetrics.ProposerMetrics{}, fmt.Errorf("failed to get number of proofs witnessgen: %w", err)
	}

	numUnrequested, err := l.db.GetNumberOfRequestsWithStatuses(proofrequest.StatusUNREQ)
	if err != nil {
		return opsuccinctmetrics.ProposerMetrics{}, fmt.Errorf("failed to get number of unrequested proofs: %w", err)
	}

	metrics := opsuccinctmetrics.ProposerMetrics{
		L2UnsafeHeadBlock:              l2UnsafeHeadBlock,
		L2FinalizedBlock:               l2FinalizedBlock,
			LatestContractL2Block:          latestContractL2Block.Uint64(),
			HighestProvenContiguousL2Block: highestProvenContiguousL2Block,
				NumProving:                     uint64(numProving),
				NumWitnessgen:                  uint64(numWitnessgen),
				NumUnrequested:                 uint64(numUnrequested),
	}

	// Record the metrics
	if m, ok := l.Metr.(*opsuccinctmetrics.OPSuccinctMetrics); ok {
		m.RecordProposerStatus(metrics)
	}

	return metrics, nil
}

// SendSlackNotification sends a Slack notification with the proposer metrics.
func (l *L2OutputSubmitter) SendSlackNotification(proposerMetrics opsuccinctmetrics.ProposerMetrics) error {
	if l.Cfg.SlackToken == "" {
		l.Log.Info("Slack notifications disabled, token not set")
		return nil // Slack notifications disabled if token not set
	}

	api := slack.New(l.Cfg.SlackToken)
	channelID := "op-succinct-tests"

	ctx, cancel := context.WithTimeout(l.ctx, l.Cfg.NetworkTimeout)
	defer cancel()

	rollupClient, err := l.RollupProvider.RollupClient(ctx)
	if err != nil {
		return fmt.Errorf("getting rollup client: %w", err)
	}
	cfg, err := rollupClient.RollupConfig(ctx)
	if err != nil {
		return fmt.Errorf("getting rollup config: %w", err)
	}
	l2BlockTime := cfg.BlockTime

	// Get the number of minutes behind the L2 finalized block the contract is.
	minutesBehind := (proposerMetrics.L2FinalizedBlock - proposerMetrics.LatestContractL2Block) * l2BlockTime / 60

	message := fmt.Sprintf("*Chain %d Proposer Metrics*:\n"+
		"Contract is %d minutes behind L2 Finalized\n"+
		"| L2 Unsafe | L2 Finalized | Contract L2 | Proven L2 |\n"+
		"| %-9d | %-12d | %-11d | %-9d |\n"+
		"| Proving   | Witness Gen | Unrequested |\n"+
		"| %-9d | %-11d | %-11d |",
		l.Cfg.L2ChainID,
		minutesBehind,
		proposerMetrics.L2UnsafeHeadBlock,
		proposerMetrics.L2FinalizedBlock,
		proposerMetrics.LatestContractL2Block,
		proposerMetrics.HighestProvenContiguousL2Block,
		proposerMetrics.NumProving,
		proposerMetrics.NumWitnessgen,
		proposerMetrics.NumUnrequested)

	_, _, err = api.PostMessage(
		channelID,
		slack.MsgOptionText(message, false),
	)
	if err != nil {
		return fmt.Errorf("error sending Slack notification: %w", err)
	}

	return nil
}

func (l *L2OutputSubmitter) SubmitAggProofs(ctx context.Context) error {
	// Get the latest output index from the L2OutputOracle contract
	latestBlockNumber, err := l.l2ooContract.LatestBlockNumber(&bind.CallOpts{Context: ctx})
	if err != nil {
		return fmt.Errorf("failed to get latest output index: %w", err)
	}

	// Check for a completed AGG proof starting at the next index
	completedAggProofs, err := l.db.GetAllCompletedAggProofs(latestBlockNumber.Uint64())
	if err != nil {
		return fmt.Errorf("failed to query for completed AGG proof: %w", err)
	}

	if len(completedAggProofs) == 0 {
		return nil
	}

	for _, aggProof := range completedAggProofs {
		output, err := l.FetchOutput(ctx, aggProof.EndBlock)
		if err != nil {
			return fmt.Errorf("failed to fetch output at block %d: %w", aggProof.EndBlock, err)
		}

		l.proposeOutput(ctx, output, aggProof.Proof, aggProof.L1BlockNumber)
	}

	return nil
}

// FetchL2OOOutput gets the next output proposal for the L2OO.
// It queries the L2OO for the earliest next block number that should be proposed.
// It returns the output to propose, and whether the proposal should be submitted at all.
// The passed context is expected to be a lifecycle context. A network timeout
// context will be derived from it.
func (l *L2OutputSubmitter) FetchL2OOOutput(ctx context.Context) (*eth.OutputResponse, bool, error) {
	if l.l2ooContract == nil {
		return nil, false, fmt.Errorf("L2OutputOracle contract not set, cannot fetch next output info")
	}

	cCtx, cancel := context.WithTimeout(ctx, l.Cfg.NetworkTimeout)
	defer cancel()
	callOpts := &bind.CallOpts{
		From:    l.Txmgr.From(),
		Context: cCtx,
	}
	nextCheckpointBlockBig, err := l.l2ooContract.NextBlockNumber(callOpts)
	if err != nil {
		return nil, false, fmt.Errorf("querying next block number: %w", err)
	}
	nextCheckpointBlock := nextCheckpointBlockBig.Uint64()
	// Fetch the current L2 heads
	currentBlockNumber, err := l.FetchCurrentBlockNumber(ctx)
	if err != nil {
		return nil, false, err
	}

	// Ensure that we do not submit a block in the future
	if currentBlockNumber < nextCheckpointBlock {
		l.Log.Debug("Proposer submission interval has not elapsed", "currentBlockNumber", currentBlockNumber, "nextBlockNumber", nextCheckpointBlock)
		return nil, false, nil
	}

	output, err := l.FetchOutput(ctx, nextCheckpointBlock)
	if err != nil {
		return nil, false, fmt.Errorf("fetching output: %w", err)
	}

	// Always propose if it's part of the Finalized L2 chain. Or if allowed, if it's part of the safe L2 chain.
	if output.BlockRef.Number > output.Status.FinalizedL2.Number && (!l.Cfg.AllowNonFinalized || output.BlockRef.Number > output.Status.SafeL2.Number) {
		l.Log.Debug("Not proposing yet, L2 block is not ready for proposal",
			"l2_proposal", output.BlockRef,
			"l2_safe", output.Status.SafeL2,
			"l2_finalized", output.Status.FinalizedL2,
			"allow_non_finalized", l.Cfg.AllowNonFinalized)
		return output, false, nil
	}
	return output, true, nil
}

// FetchCurrentBlockNumber gets the current block number from the [L2OutputSubmitter]'s [RollupClient]. If the `AllowNonFinalized` configuration
// option is set, it will return the safe head block number, and if not, it will return the finalized head block number.
func (l *L2OutputSubmitter) FetchCurrentBlockNumber(ctx context.Context) (uint64, error) {
	rollupClient, err := l.RollupProvider.RollupClient(ctx)
	if err != nil {
		return 0, fmt.Errorf("getting rollup client: %w", err)
	}

	status, err := rollupClient.SyncStatus(ctx)
	if err != nil {
		return 0, fmt.Errorf("getting sync status: %w", err)
	}

	// Use either the finalized or safe head depending on the config. Finalized head is default & safer.
	if l.Cfg.AllowNonFinalized {
		return status.SafeL2.Number, nil
	}
	return status.FinalizedL2.Number, nil
}

func (l *L2OutputSubmitter) FetchOutput(ctx context.Context, block uint64) (*eth.OutputResponse, error) {
	rollupClient, err := l.RollupProvider.RollupClient(ctx)
	if err != nil {
		return nil, fmt.Errorf("getting rollup client: %w", err)
	}

	output, err := rollupClient.OutputAtBlock(ctx, block)
	if err != nil {
		return nil, fmt.Errorf("fetching output at block %d: %w", block, err)
	}
	if output.Version != supportedL2OutputVersion {
		return nil, fmt.Errorf("unsupported l2 output version: %v, supported: %v", output.Version, supportedL2OutputVersion)
	}
	if onum := output.BlockRef.Number; onum != block { // sanity check, e.g. in case of bad RPC caching
		return nil, fmt.Errorf("output block number %d mismatches requested %d", output.BlockRef.Number, block)
	}
	return output, nil
}

// ProposeL2OutputTxData creates the transaction data for the ProposeL2Output function
func (l *L2OutputSubmitter) ProposeL2OutputTxData(output *eth.OutputResponse, proof []byte, l1BlockNum uint64) ([]byte, error) {
	return proposeL2OutputTxData(l.l2ooABI, output, proof, l1BlockNum)
}

// proposeL2OutputTxData creates the transaction data for the ProposeL2Output function
func proposeL2OutputTxData(abi *abi.ABI, output *eth.OutputResponse, proof []byte, l1BlockNum uint64) ([]byte, error) {
	return abi.Pack(
		"proposeL2Output",
		output.OutputRoot,
		new(big.Int).SetUint64(output.BlockRef.Number),
		new(big.Int).SetUint64(l1BlockNum),
		proof)
}

func (l *L2OutputSubmitter) CheckpointBlockHashTxData(blockNumber *big.Int) ([]byte, error) {
	return l.l2ooABI.Pack("checkpointBlockHash", blockNumber)
}

// Wait for L1 head to advance beyond blocknum to ensure proposal transaction validity.
// EstimateGas uses "latest" state, treating head block as "pending".
// If l1blocknum == l1head, blockhash(l1blocknum) returns 0 in EstimateGas,
// causing failure when contract verifies l1blockhash against blockhash(l1blocknum).
func (l *L2OutputSubmitter) waitForL1Head(ctx context.Context, blockNum uint64) error {
	ticker := time.NewTicker(l.Cfg.PollInterval)
	defer ticker.Stop()
	l1head, err := l.Txmgr.BlockNumber(ctx)
	if err != nil {
		return err
	}
	for l1head <= blockNum {
		l.Log.Debug("Waiting for l1 head > l1blocknum1+1", "l1head", l1head, "l1blocknum", blockNum)
		select {
		case <-ticker.C:
			l1head, err = l.Txmgr.BlockNumber(ctx)
			if err != nil {
				return err
			}
		case <-l.done:
			return fmt.Errorf("L2OutputSubmitter is done()")
		}
	}
	return nil
}

// sendTransaction creates & sends transactions through the underlying transaction manager.
func (l *L2OutputSubmitter) sendTransaction(ctx context.Context, output *eth.OutputResponse, proof []byte, l1BlockNum uint64) error {
	err := l.waitForL1Head(ctx, output.Status.HeadL1.Number+1)
	if err != nil {
		return err
	}

	l.Log.Info("Proposing output root", "output", output.OutputRoot, "block", output.BlockRef)
	var receipt *types.Receipt
	if l.Cfg.DisputeGameFactoryAddr != nil {
		return errors.New("not implemented")
	} else {
		data, err := l.ProposeL2OutputTxData(output, proof, l1BlockNum)
		if err != nil {
			return err
		}
		// TODO: This currently blocks the loop while it waits for the transaction to be confirmed. Up to 3 minutes.
		receipt, err = l.Txmgr.Send(ctx, txmgr.TxCandidate{
			TxData:   data,
			To:       l.Cfg.L2OutputOracleAddr,
			GasLimit: 0,
		})
		if err != nil {
			return err
		}
	}

	if receipt.Status == types.ReceiptStatusFailed {
		l.Log.Error("Proposer tx successfully published but reverted", "tx_hash", receipt.TxHash)
	} else {
		l.Log.Info("Proposer tx successfully published", "tx_hash", receipt.TxHash)
	}
	return nil
}

// sendCheckpointTransaction sends a transaction to checkpoint the blockhash corresponding to `blockNumber` on the L2OO contract.
func (l *L2OutputSubmitter) sendCheckpointTransaction(ctx context.Context, blockNumber *big.Int) error {
	var receipt *types.Receipt
	data, err := l.CheckpointBlockHashTxData(blockNumber)
	if err != nil {
		return err
	}
	// TODO: This currently blocks the loop while it waits for the transaction to be confirmed. Up to 3 minutes.
	receipt, err = l.Txmgr.Send(ctx, txmgr.TxCandidate{
		TxData:   data,
		To:       l.Cfg.L2OutputOracleAddr,
		GasLimit: 0,
	})
	if err != nil {
		return err
	}

	if receipt.Status == types.ReceiptStatusFailed {
		l.Log.Error("checkpoint blockhash tx successfully published but reverted", "tx_hash", receipt.TxHash)
	} else {
		l.Log.Info("checkpoint blockhash tx successfully published",
			"tx_hash", receipt.TxHash)
	}
	return nil
}

// loop is responsible for creating & submitting the next outputs
// TODO: Look into adding a transaction cache so the loop isn't waiting for the transaction to confirm. This sometimes takes up to 30s.
func (l *L2OutputSubmitter) loop() {
	defer l.wg.Done()
	ctx := l.ctx

	if l.Cfg.WaitNodeSync {
		err := l.waitNodeSync()
		if err != nil {
			l.Log.Error("Error waiting for node sync", "err", err)
			return
		}
	}

	l.loopL2OO(ctx)
}

func (l *L2OutputSubmitter) waitNodeSync() error {
	cCtx, cancel := context.WithTimeout(l.ctx, l.Cfg.NetworkTimeout)
	defer cancel()

	l1head, err := l.Txmgr.BlockNumber(cCtx)
	if err != nil {
		return fmt.Errorf("failed to retrieve current L1 block number: %w", err)
	}

	rollupClient, err := l.RollupProvider.RollupClient(l.ctx)
	if err != nil {
		return fmt.Errorf("failed to get rollup client: %w", err)
	}

	return dial.WaitRollupSync(l.ctx, l.Log, rollupClient, l1head, time.Second*12)
}

// The loopL2OO regularly polls the L2OO for the next block to propose,
// and if the current finalized (or safe) block is past that next block, it
// proposes it.
func (l *L2OutputSubmitter) loopL2OO(ctx context.Context) {
	ticker := time.NewTicker(l.Cfg.PollInterval)
	slackMetricsTicker := time.NewTicker(slackMetricsTickerInterval)
	defer ticker.Stop()
	defer slackMetricsTicker.Stop()
	for {
		select {
		case <-ticker.C:
			// Get the current metrics for the proposer.
			metrics, err := l.GetProposerMetrics(ctx)
			if err != nil {
				l.Log.Error("failed to get metrics", "err", err)
				continue
			}
			l.Log.Info("Proposer status", "metrics", metrics)

			// 1) Queue up the span proofs that are ready to prove. Determine these range proofs based on the latest L2 finalized block,
			// and the current L2 unsafe head.
			l.Log.Info("Stage 1: Deriving Span Batches...")
			err = l.DeriveNewSpanBatches(ctx)
			if err != nil {
				l.Log.Error("failed to add next span batches to db", "err", err)
				continue
			}

			// 2) Check the statuses of all requested proofs.
			// If it's successfully returned, we validate that we have it on disk and set status = "COMPLETE".
			// If it fails or times out, we set status = "FAILED" (and, if it's a span proof, split the request in half to try again).
			l.Log.Info("Stage 2: Processing Pending Proofs...")
			err = l.ProcessPendingProofs()
			if err != nil {
				l.Log.Error("failed to update requested proofs", "err", err)
				continue
			}

			// 3) Determine if there is a continguous chain of span proofs starting from the latest block on the L2OO contract.
			// If there is, queue an aggregate proof for all of the span proofs.
			l.Log.Info("Stage 3: Deriving Agg Proofs...")
			err = l.DeriveAggProofs(ctx)
			if err != nil {
				l.Log.Error("failed to generate pending agg proofs", "err", err)
				continue
			}

			// 4) Request all unrequested proofs from the prover network.
			// Any DB entry with status = "UNREQ" means it's queued up and ready.
			// We request all of these (both span and agg) from the prover network.
			// For agg proofs, we also checkpoint the blockhash in advance.
			l.Log.Info("Stage 4: Requesting Queued Proofs...")
			err = l.RequestQueuedProofs(ctx)
			if err != nil {
				l.Log.Error("failed to request unrequested proofs", "err", err)
				continue
			}

			// 5) Submit agg proofs on chain.
			// If we have a completed agg proof waiting in the DB, we submit them on chain.
			l.Log.Info("Stage 5: Submitting Agg Proofs...")
			err = l.SubmitAggProofs(ctx)
			if err != nil {
				l.Log.Error("failed to submit agg proofs", "err", err)
			}
		case <-slackMetricsTicker.C:
			metrics, err := l.GetProposerMetrics(ctx)
			if err != nil {
				l.Log.Error("failed to get metrics for Slack notification", "err", err)
				continue
			}
			err = l.SendSlackNotification(metrics)
			if err != nil {
				l.Log.Error("failed to send Slack notification", "err", err)
			}
		case <-l.done:
			return
		}
	}
}

func (l *L2OutputSubmitter) proposeOutput(ctx context.Context, output *eth.OutputResponse, proof []byte, l1BlockNum uint64) {
	cCtx, cancel := context.WithTimeout(ctx, 10*time.Minute)
	defer cancel()

	if err := l.sendTransaction(cCtx, output, proof, l1BlockNum); err != nil {
		l.Log.Error("Failed to send proposal transaction",
			"err", err,
			"l1blocknum", l1BlockNum,
			"l1head", output.Status.HeadL1.Number,
			"proof", proof)
		return
	}
	l.Log.Info("AGG proof submitted on-chain", "end", output.BlockRef.Number)
	l.Metr.RecordL2BlocksProposed(output.BlockRef)
}

// checkpointBlockHash gets the current L1 head, and then sends a transaction to checkpoint the blockhash on
// the L2OO contract for the aggregation proof.
func (l *L2OutputSubmitter) checkpointBlockHash(ctx context.Context) (uint64, common.Hash, error) {
	cCtx, cancel := context.WithTimeout(ctx, 10*time.Minute)
	defer cancel()

	currBlockNum, err := l.L1Client.BlockNumber(cCtx)
	if err != nil {
		return 0, common.Hash{}, err
	}
	header, err := l.L1Client.HeaderByNumber(cCtx, new(big.Int).SetUint64(currBlockNum-1))
	if err != nil {
		return 0, common.Hash{}, err
	}
	blockHash := header.Hash()
	blockNumber := header.Number

	err = l.sendCheckpointTransaction(cCtx, blockNumber)
	if err != nil {
		return 0, common.Hash{}, err
	}
	return blockNumber.Uint64(), blockHash, nil
}
