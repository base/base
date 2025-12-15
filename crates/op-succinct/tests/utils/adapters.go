package utils

import (
	"context"
	"database/sql"
	"fmt"
	"math/big"
	"time"

	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	"github.com/ethereum-optimism/optimism/op-service/apis"
	"github.com/ethereum-optimism/optimism/op-service/eth"
	"github.com/ethereum/go-ethereum"
	"github.com/ethereum/go-ethereum/accounts/abi/bind/v2"
	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/common/hexutil"
	"github.com/ethereum/go-ethereum/rpc"
	"github.com/lib/pq"
	"github.com/stretchr/testify/require"
	opsbind "github.com/succinctlabs/op-succinct/bindings"
)

// -------------------------------------------------------------
// L2 OP Succinct Output Oracle Client
// -------------------------------------------------------------

// L2OOClient is a client for interacting with the SuccinctL2OutputOracle contract.
type L2OOClient struct {
	caller *opsbind.OPSuccinctL2OutputOracleCaller
}

// NewL2OOClient creates a new L2OOClient.
func NewL2OOClient(client apis.EthClient, addr common.Address) (*L2OOClient, error) {
	caller := ethCaller{c: client}
	l2ooCaller, err := opsbind.NewOPSuccinctL2OutputOracleCaller(addr, caller)
	if err != nil {
		return nil, fmt.Errorf("bind L2OO: %w", err)
	}

	return &L2OOClient{
		caller: l2ooCaller,
	}, nil
}

// LatestBlockNumber fetches the latest L2 block number from the contract.
func (l2oo *L2OOClient) LatestBlockNumber(ctx context.Context) (uint64, error) {
	latestBlockNumber, err := l2oo.caller.LatestBlockNumber(opts(ctx))
	if err != nil {
		return 0, fmt.Errorf("call latestBlockNumber: %w", err)
	}
	return latestBlockNumber.Uint64(), nil
}

// NextBlockNumber fetches the next L2 block number to be submitted by the proposer.
func (l2oo *L2OOClient) NextBlockNumber(ctx context.Context) (uint64, error) {
	nextBlockNumber, err := l2oo.caller.NextBlockNumber(opts(ctx))
	if err != nil {
		return 0, fmt.Errorf("call nextBlockNumber: %w", err)
	}
	return nextBlockNumber.Uint64(), nil
}

// OutputProposal represents an output proposal from the L2OO contract.
type OutputProposal struct {
	OutputRoot    eth.Bytes32
	Timestamp     uint64
	L2BlockNumber uint64
}

// GetL2OutputAfter fetches the output proposal for a given L2 block number.
func (l2oo *L2OOClient) GetL2OutputAfter(ctx context.Context, l2BlockNumber uint64) (OutputProposal, error) {
	output, err := l2oo.caller.GetL2OutputAfter(opts(ctx), new(big.Int).SetUint64(l2BlockNumber))
	if err != nil {
		return OutputProposal{}, fmt.Errorf("call getL2OutputAfter: %w", err)
	}

	return OutputProposal{
		OutputRoot:    eth.Bytes32(output.OutputRoot),
		Timestamp:     output.Timestamp.Uint64(),
		L2BlockNumber: output.L2BlockNumber.Uint64(),
	}, nil
}

// WaitForLatestBlockNumber waits until the L2OO has submitted an output for at least the target block number.
func WaitForLatestBlockNumber(ctx context.Context, t devtest.T, l2oo *L2OOClient, target uint64) {
	for {
		latestBlockNumber, err := l2oo.LatestBlockNumber(ctx)
		require.NoError(t, err, "failed to get latest block number from L2OO")

		if latestBlockNumber >= target {
			t.Logger().Info("L2OO latest block number reached target", "latest", latestBlockNumber, "target", target)
			return
		}

		t.Logger().Info("Waiting for L2OO latest block number...", "current", latestBlockNumber, "target", target)

		select {
		case <-ctx.Done():
			t.Errorf("timeout waiting for L2OO latest block number to reach %d (current: %d)", target, latestBlockNumber)
			t.FailNow()
		case <-time.After(time.Second):
		}
	}
}

// -------------------------------------------------------------
// Dispute Game Factory Client
// -------------------------------------------------------------

// DgfClient is a client for interacting with the DisputeGameFactory contract.
type DgfClient struct {
	caller *opsbind.DisputeGameFactoryCaller
}

// GameAtIndexResult contains info about a created dispute game.
type GameAtIndexResult struct {
	GameType  uint32
	Timestamp uint64
	Proxy     common.Address
}

// NewDgfClient creates a new DgfClient.
func NewDgfClient(client apis.EthClient, addr common.Address) (*DgfClient, error) {
	caller := ethCaller{c: client}
	dgfCaller, err := opsbind.NewDisputeGameFactoryCaller(addr, caller)
	if err != nil {
		return nil, fmt.Errorf("bind DGF: %w", err)
	}

	return &DgfClient{
		caller: dgfCaller,
	}, nil
}

func (dfg *DgfClient) GameAtIndex(ctx context.Context, index uint64) (GameAtIndexResult, error) {
	out, err := dfg.caller.GameAtIndex(opts(ctx), new(big.Int).SetUint64(index))
	if err != nil {
		return GameAtIndexResult{}, fmt.Errorf("call gameAtIndex: %w", err)
	}
	return GameAtIndexResult{GameType: out.GameType, Timestamp: out.Timestamp, Proxy: out.Proxy}, nil
}

// GameCount fetches the number of dispute games created.
func (dfg *DgfClient) GameCount(ctx context.Context) (uint64, error) {
	count, err := dfg.caller.GameCount(opts(ctx))
	if err != nil {
		return 0, fmt.Errorf("call gameCount: %w", err)
	}
	return count.Uint64(), nil
}

// WaitForGameCount waits until the dispute game factory has at least min games created.
func WaitForGameCount(ctx context.Context, t devtest.T, dgf *DgfClient, min uint64) {
	for {
		gameCount, err := dgf.GameCount(ctx)
		require.NoError(t, err, "failed to get game count from factory")

		if gameCount >= min {
			t.Logger().Info("Dispute game detected", "count", gameCount)
			return
		}

		select {
		case <-ctx.Done():
			t.Errorf("timeout waiting for dispute game to be created")
			t.FailNow()
		case <-time.After(time.Second):
		}
	}
}

// -------------------------------------------------------------
// Fault Dispute Game Client
// -------------------------------------------------------------

// GameStatus represents the status of a dispute game.
type GameStatus uint8

const (
	InProgress     GameStatus = iota // 0
	ChallengerWins                   // 1
	DefenderWins                     // 2
)

// FdgClient is a client for interacting with the OPSuccinctFaultDisputeGame contract.
type FdgClient struct {
	caller *opsbind.OPSuccinctFaultDisputeGameCaller
}

// NewFdgClient creates a new FdgClient.
func NewFdgClient(client apis.EthClient, addr common.Address) (*FdgClient, error) {
	caller := ethCaller{c: client}
	fdgCaller, err := opsbind.NewOPSuccinctFaultDisputeGameCaller(addr, caller)
	if err != nil {
		return nil, fmt.Errorf("bind FDG: %w", err)
	}

	return &FdgClient{
		caller: fdgCaller,
	}, nil
}

func (dfg *FdgClient) Status(ctx context.Context) (uint8, error) {
	status, err := dfg.caller.Status(opts(ctx))
	if err != nil {
		return 0, fmt.Errorf("call status: %w", err)
	}
	return uint8(status), nil
}

// RootClaim fetches the root claim for the fault dispute game.
func (fdg *FdgClient) RootClaim(ctx context.Context) (eth.Bytes32, error) {
	rootClaim, err := fdg.caller.RootClaim(opts(ctx))
	if err != nil {
		return [32]byte{}, fmt.Errorf("call rootClaim: %w", err)
	}
	return eth.Bytes32(rootClaim), nil
}

// L2BlockNumber fetches the L2 block number the fault dispute game targets.
func (fdg *FdgClient) L2BlockNumber(ctx context.Context) (uint64, error) {
	l2BlockNumber, err := fdg.caller.L2BlockNumber(opts(ctx))
	if err != nil {
		return 0, fmt.Errorf("call l2BlockNumber: %w", err)
	}
	return l2BlockNumber.Uint64(), nil
}

// ParentIndex fetches the parent index from the fault dispute game.
func (fdg *FdgClient) ParentIndex(ctx context.Context) (uint32, error) {
	parentIndex, err := fdg.caller.ParentIndex(opts(ctx))
	if err != nil {
		return 0, fmt.Errorf("call parentIndex: %w", err)
	}
	return uint32(parentIndex), nil
}

var _ bind.ContractCaller = ethCaller{}

// implements bind/v2.ContractCaller using apis.EthClient
type ethCaller struct{ c apis.EthClient }

func (w ethCaller) toRPCBlockNumber(blockNumber *big.Int) (rpc.BlockNumber, error) {
	if blockNumber == nil {
		return rpc.LatestBlockNumber, nil
	}
	if !blockNumber.IsInt64() {
		return 0, fmt.Errorf("block number overflow: %s", blockNumber)
	}
	return rpc.BlockNumber(blockNumber.Int64()), nil
}

func (w ethCaller) CallContract(ctx context.Context, call ethereum.CallMsg, blockNumber *big.Int) ([]byte, error) {
	bn, err := w.toRPCBlockNumber(blockNumber)
	if err != nil {
		return nil, err
	}
	return w.c.Call(ctx, call, bn)
}

func (w ethCaller) CodeAt(ctx context.Context, contract common.Address, blockNumber *big.Int) ([]byte, error) {
	bn, err := w.toRPCBlockNumber(blockNumber)
	if err != nil {
		return nil, err
	}
	var code hexutil.Bytes
	if err := w.c.RPC().CallContext(ctx, &code, "eth_getCode", contract, bn); err != nil {
		return nil, err
	}
	return code, nil
}

func WaitForDefenderWins(ctx context.Context, t devtest.T, dgf *FdgClient) {
	for {
		status, err := dgf.Status(ctx)
		require.NoError(t, err, "failed to get game status")

		if GameStatus(status) == DefenderWins {
			return
		}

		select {
		case <-ctx.Done():
			t.Errorf("timeout waiting for dispute game to be resolved")
			t.FailNow()
		case <-time.After(time.Second):
		}
	}
}

func opts(ctx context.Context) *bind.CallOpts {
	return &bind.CallOpts{Context: ctx}
}

// -------------------------------------------------------------
// Proposer Database Client
// -------------------------------------------------------------

// RangeProof represents a range proof request from the proposer database.
type RangeProof struct {
	StartBlock int64
	EndBlock   int64
}

// FetchRangeProofs queries completed range proofs from the proposer database.
func FetchRangeProofs(ctx context.Context, dbURL string, startBlock, endBlock uint64) ([]RangeProof, error) {
	db, err := sql.Open("postgres", dbURL)
	if err != nil {
		return nil, fmt.Errorf("open db: %w", err)
	}
	defer db.Close()

	// req_type=0 is Range, status=4 is Complete
	rows, err := db.QueryContext(ctx, `
		SELECT start_block, end_block
		FROM requests
		WHERE req_type = 0 AND status = 4 AND start_block >= $1 AND end_block <= $2
		ORDER BY start_block
	`, startBlock, endBlock)
	if err != nil {
		return nil, fmt.Errorf("query: %w", err)
	}
	defer rows.Close()

	var proofs []RangeProof
	for rows.Next() {
		var p RangeProof
		if err := rows.Scan(&p.StartBlock, &p.EndBlock); err != nil {
			return nil, fmt.Errorf("scan: %w", err)
		}
		proofs = append(proofs, p)
	}
	return proofs, rows.Err()
}

// CountRangeProofRequests counts all range proof requests in the database (any status).
// Returns 0 if the requests table doesn't exist yet (proposer still initializing).
func CountRangeProofRequests(ctx context.Context, dbURL string) (int, error) {
	db, err := sql.Open("postgres", dbURL)
	if err != nil {
		return 0, err
	}
	defer db.Close()

	var count int
	err = db.QueryRowContext(ctx, `SELECT COUNT(*) FROM requests WHERE req_type = 0`).Scan(&count)
	if err != nil {
		// 42P01 = undefined_table (table doesn't exist yet during proposer init)
		if pqErr, ok := err.(*pq.Error); ok && pqErr.Code == "42P01" {
			return 0, nil
		}
		return 0, err
	}
	return count, nil
}

// WaitForRangeProofProgress waits until at least minCount range proof requests exist in the database.
func WaitForRangeProofProgress(ctx context.Context, t devtest.T, dbURL string, minCount int) {
	for {
		count, err := CountRangeProofRequests(ctx, dbURL)
		require.NoError(t, err, "failed to count range proof requests")

		if count >= minCount {
			t.Logger().Info("Range proof progress detected", "count", count, "minRequired", minCount)
			return
		}

		t.Logger().Info("Waiting for range proof progress...", "current", count, "minRequired", minCount)

		select {
		case <-ctx.Done():
			t.Errorf("timeout waiting for range proof progress (current: %d, required: %d)", count, minCount)
			t.FailNow()
		case <-time.After(time.Second):
		}
	}
}

// VerifyRangeProofsWithExpected fetches and verifies range proofs from the database.
func VerifyRangeProofsWithExpected(ctx context.Context, t devtest.T, dbURL string, startingBlock, outputBlock uint64, expectedCount int) {
	require := t.Require()
	logger := t.Logger()

	ranges, err := FetchRangeProofs(ctx, dbURL, startingBlock, outputBlock)
	require.NoError(err, "failed to fetch range proofs")

	for i, r := range ranges {
		logger.Info("Range proof", "index", i, "start", r.StartBlock, "end", r.EndBlock)
	}

	err = VerifyRanges(ranges, int64(startingBlock), int64(outputBlock), expectedCount)
	require.NoError(err, "range verification failed")

	logger.Info("Range proofs verified", "count", len(ranges), "expected", expectedCount)
}

// VerifyRanges checks that ranges have correct count, are contiguous, and cover the expected interval.
func VerifyRanges(ranges []RangeProof, expectedStart, expectedEnd int64, expectedCount int) error {
	if len(ranges) != expectedCount {
		return fmt.Errorf("expected %d ranges, got %d", expectedCount, len(ranges))
	}
	if len(ranges) == 0 {
		return nil
	}
	if ranges[0].StartBlock != expectedStart {
		return fmt.Errorf("first range starts at %d, expected %d", ranges[0].StartBlock, expectedStart)
	}
	for i := 1; i < len(ranges); i++ {
		if ranges[i-1].EndBlock != ranges[i].StartBlock {
			return fmt.Errorf("gap between range %d (end=%d) and %d (start=%d)",
				i-1, ranges[i-1].EndBlock, i, ranges[i].StartBlock)
		}
	}
	if ranges[len(ranges)-1].EndBlock != expectedEnd {
		return fmt.Errorf("last range ends at %d, expected %d", ranges[len(ranges)-1].EndBlock, expectedEnd)
	}
	return nil
}
