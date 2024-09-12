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
	"github.com/ethereum/go-ethereum/common/math"
	"github.com/ethereum/go-ethereum/core/types"
	"github.com/ethereum/go-ethereum/ethclient"
	"github.com/ethereum/go-ethereum/log"

	// Original Optimism Bindings
	opbindings "github.com/ethereum-optimism/optimism/op-proposer/bindings"
	// OP Succinct Contract Bindings
	opsuccinctbindings "github.com/succinctlabs/op-succinct-go/bindings"

	"github.com/ethereum-optimism/optimism/op-proposer/metrics"
	"github.com/ethereum-optimism/optimism/op-service/dial"
	"github.com/ethereum-optimism/optimism/op-service/eth"
	"github.com/ethereum-optimism/optimism/op-service/txmgr"
	"github.com/succinctlabs/op-succinct-go/proposer/db"
)

var (
	supportedL2OutputVersion = eth.Bytes32{}
	ErrProposerNotRunning    = errors.New("proposer is not running")
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
	Metr     metrics.Metricer
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

	dgfContract *opbindings.DisputeGameFactoryCaller
	dgfABI      *abi.ABI

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
	} else if setup.Cfg.DisputeGameFactoryAddr != nil {
		return newDGFSubmitter(ctx, cancel, setup)
	} else {
		return nil, errors.New("neither the `L2OutputOracle` nor `DisputeGameFactory` addresses were provided")
	}
}

func newL2OOSubmitter(ctx context.Context, cancel context.CancelFunc, setup DriverSetup) (*L2OutputSubmitter, error) {
	l2ooContract, err := opsuccinctbindings.NewZKL2OutputOracleCaller(*setup.Cfg.L2OutputOracleAddr, setup.L1Client)
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

	parsed, err := opsuccinctbindings.ZKL2OutputOracleMetaData.GetAbi()
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

// Create a new submitter for the DisputeGameFactory. Note: This is unused in OP-Succinct.
func newDGFSubmitter(ctx context.Context, cancel context.CancelFunc, setup DriverSetup) (*L2OutputSubmitter, error) {
	dgfCaller, err := opbindings.NewDisputeGameFactoryCaller(*setup.Cfg.DisputeGameFactoryAddr, setup.L1Client)
	if err != nil {
		cancel()
		return nil, fmt.Errorf("failed to create DGF at address %s: %w", setup.Cfg.DisputeGameFactoryAddr, err)
	}

	cCtx, cCancel := context.WithTimeout(ctx, setup.Cfg.NetworkTimeout)
	defer cCancel()
	version, err := dgfCaller.Version(&bind.CallOpts{Context: cCtx})
	if err != nil {
		cancel()
		return nil, err
	}
	log.Info("Connected to DisputeGameFactory", "address", setup.Cfg.DisputeGameFactoryAddr, "version", version)

	parsed, err := opbindings.DisputeGameFactoryMetaData.GetAbi()
	if err != nil {
		cancel()
		return nil, err
	}

	return &L2OutputSubmitter{
		DriverSetup: setup,
		done:        make(chan struct{}),
		ctx:         ctx,
		cancel:      cancel,

		dgfContract: dgfCaller,
		dgfABI:      parsed,
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

func (l *L2OutputSubmitter) SubmitAggProofs(ctx context.Context) error {
	// Get the latest output index from the L2OutputOracle contract
	latestBlockNumber, err := l.l2ooContract.LatestBlockNumber(&bind.CallOpts{Context: ctx})
	if err != nil {
		return fmt.Errorf("failed to get latest output index: %w", err)
	}

	// Check for a completed AGG proof starting at the next index
	completedAggProofs, err := l.db.GetAllCompletedAggProofs(latestBlockNumber.Uint64() + 1)
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

		l.proposeOutput(ctx, output, aggProof.Proof, aggProof.L1BlockNumber, common.HexToHash(aggProof.L1BlockHash))
		l.Log.Info("AGG proof submitted on-chain", "start", aggProof.StartBlock, "end", aggProof.EndBlock)
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

// FetchDGFOutput gets the next output proposal for the DGF.
// The passed context is expected to be a lifecycle context. A network timeout
// context will be derived from it.
func (l *L2OutputSubmitter) FetchDGFOutput(ctx context.Context) (*eth.OutputResponse, error) {
	ctx, cancel := context.WithTimeout(ctx, l.Cfg.NetworkTimeout)
	defer cancel()

	blockNum, err := l.FetchCurrentBlockNumber(ctx)
	if err != nil {
		return nil, err
	}
	return l.FetchOutput(ctx, blockNum)
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
func (l *L2OutputSubmitter) ProposeL2OutputTxData(output *eth.OutputResponse, proof []byte, l1BlockNum uint64, l1BlockHash common.Hash) ([]byte, error) {
	return proposeL2OutputTxData(l.l2ooABI, output, proof, l1BlockNum, l1BlockHash)
}

// proposeL2OutputTxData creates the transaction data for the ProposeL2Output function
func proposeL2OutputTxData(abi *abi.ABI, output *eth.OutputResponse, proof []byte, l1BlockNum uint64, l1BlockHash common.Hash) ([]byte, error) {
	return abi.Pack(
		"proposeL2Output",
		output.OutputRoot,
		new(big.Int).SetUint64(output.BlockRef.Number),
		l1BlockHash,
		new(big.Int).SetUint64(l1BlockNum),
		proof)
}

func (l *L2OutputSubmitter) ProposeL2OutputDGFTxData(output *eth.OutputResponse) ([]byte, *big.Int, error) {
	bond, err := l.dgfContract.InitBonds(&bind.CallOpts{}, l.Cfg.DisputeGameType)
	if err != nil {
		return nil, nil, err
	}
	data, err := proposeL2OutputDGFTxData(l.dgfABI, l.Cfg.DisputeGameType, output)
	if err != nil {
		return nil, nil, err
	}
	return data, bond, err
}

// proposeL2OutputDGFTxData creates the transaction data for the DisputeGameFactory's `create` function
func proposeL2OutputDGFTxData(abi *abi.ABI, gameType uint32, output *eth.OutputResponse) ([]byte, error) {
	return abi.Pack("create", gameType, output.OutputRoot, math.U256Bytes(new(big.Int).SetUint64(output.BlockRef.Number)))
}

func (l *L2OutputSubmitter) CheckpointBlockHashTxData(blockNumber *big.Int, blockHash common.Hash) ([]byte, error) {
	return l.l2ooABI.Pack("checkpointBlockHash", blockNumber, blockHash)
}

// We wait until l1head advances beyond blocknum. This is used to make sure proposal tx won't
// immediately fail when checking the l1 blockhash. Note that EstimateGas uses "latest" state to
// execute the transaction by default, meaning inside the call, the head block is considered
// "pending" instead of committed. In the case l1blocknum == l1head then, blockhash(l1blocknum)
// will produce a value of 0 within EstimateGas, and the call will fail when the contract checks
// that l1blockhash matches blockhash(l1blocknum).
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
func (l *L2OutputSubmitter) sendTransaction(ctx context.Context, output *eth.OutputResponse, proof []byte, l1BlockNum uint64, l1BlockHash common.Hash) error {
	err := l.waitForL1Head(ctx, output.Status.HeadL1.Number+1)
	if err != nil {
		return err
	}

	l.Log.Info("Proposing output root", "output", output.OutputRoot, "block", output.BlockRef)
	var receipt *types.Receipt
	if l.Cfg.DisputeGameFactoryAddr != nil {
		data, bond, err := l.ProposeL2OutputDGFTxData(output)
		if err != nil {
			return err
		}
		receipt, err = l.Txmgr.Send(ctx, txmgr.TxCandidate{
			TxData:   data,
			To:       l.Cfg.DisputeGameFactoryAddr,
			GasLimit: 0,
			Value:    bond,
		})
		if err != nil {
			return err
		}
	} else {
		data, err := l.ProposeL2OutputTxData(output, proof, l1BlockNum, l1BlockHash)
		if err != nil {
			return err
		}
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
		l.Log.Info("Proposer tx successfully published",
			"tx_hash", receipt.TxHash,
			"l1blocknum", l1BlockNum,
			"l1blockhash", l1BlockHash)
	}
	return nil
}

// sendCheckpointTransaction creates & sends transaction to checkpoint blockhash on L2OO contract.
func (l *L2OutputSubmitter) sendCheckpointTransaction(ctx context.Context, blockNumber *big.Int, blockHash common.Hash) (uint64, common.Hash, error) {
	var receipt *types.Receipt
	data, err := l.CheckpointBlockHashTxData(blockNumber, blockHash)

	if err != nil {
		return 0, common.Hash{}, err
	}
	receipt, err = l.Txmgr.Send(ctx, txmgr.TxCandidate{
		TxData:   data,
		To:       l.Cfg.L2OutputOracleAddr,
		GasLimit: 0,
	})
	if err != nil {
		return 0, common.Hash{}, err
	}

	if receipt.Status == types.ReceiptStatusFailed {
		l.Log.Error("checkpoint blockhash tx successfully published but reverted", "tx_hash", receipt.TxHash)
	} else {
		l.Log.Info("checkpoint blockhash tx successfully published",
			"tx_hash", receipt.TxHash)
	}
	return blockNumber.Uint64(), blockHash, nil
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

	if l.dgfContract == nil {
		l.loopL2OO(ctx)
	} else {
		l.loopDGF(ctx)
	}
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
	defer ticker.Stop()
	for {
		select {
		case <-ticker.C:
			// 1) Queue up any span batches that are ready to prove.
			// This is done by checking the chain for completed channels and pulling span batches out.
			// We break apart span batches if they exceed the max size, and gracefully handle bugs in span batch decoding.
			// We add ranges to be proven to the DB as "UNREQ" so they are queued up to request later.
			l.Log.Info("Stage 1: Deriving Span Batches...")
			err := l.DeriveNewSpanBatches(ctx)
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

			// 3) Determine if any agg proofs are ready to be submitted and queue them up.
			// This is done by checking if we have contiguous span proofs from the last on chain
			// output root through at least the submission interval.
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
		case <-l.done:
			return
		}
	}
}

// The loopDGF proposes a new output every proposal interval. It does _not_ query
// the DGF for when to next propose, as the DGF doesn't have the concept of a
// proposal interval, like in the L2OO case. For this reason, it has to keep track
// of the interval itself, for which it uses an internal ticker.
func (l *L2OutputSubmitter) loopDGF(ctx context.Context) {
	defer l.Log.Info("loopDGF returning")
	ticker := time.NewTicker(l.Cfg.ProposalInterval)
	defer ticker.Stop()
	for {
		select {
		case <-ticker.C:
			var (
				output *eth.OutputResponse
				err    error
			)
			// A note on retrying: because the proposal interval is usually much
			// larger than the interval at which to retry proposing on a failed attempt,
			// we want to keep retrying getting the output proposal until we succeed.
			for output == nil || err != nil {
				select {
				case <-l.done:
					return
				default:
				}

				output, err = l.FetchDGFOutput(ctx)
				if err != nil {
					l.Log.Warn("Error getting DGF output, retrying...", "err", err)
					time.Sleep(l.Cfg.OutputRetryInterval)
				}
			}

			l.proposeOutput(ctx, output, nil, 0, common.Hash{})
		case <-l.done:
			return
		}
	}
}

func (l *L2OutputSubmitter) proposeOutput(ctx context.Context, output *eth.OutputResponse, proof []byte, l1BlockNum uint64, l1BlockHash common.Hash) {
	cCtx, cancel := context.WithTimeout(ctx, 10*time.Minute)
	defer cancel()

	if err := l.sendTransaction(cCtx, output, proof, l1BlockNum, l1BlockHash); err != nil {
		l.Log.Error("Failed to send proposal transaction",
			"err", err,
			"l1blocknum", l1BlockNum,
			"l1blockhash", l1BlockHash,
			"l1head", output.Status.HeadL1.Number,
			"proof", proof)
		return
	}
	l.Metr.RecordL2BlocksProposed(output.BlockRef)
}

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

	return l.sendCheckpointTransaction(cCtx, blockNumber, blockHash)
}
