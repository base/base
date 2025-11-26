package utils

import (
	"context"
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
	"github.com/stretchr/testify/require"
	opsbind "github.com/succinctlabs/op-succinct/bindings"
)

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

func opts(ctx context.Context) *bind.CallOpts {
	return &bind.CallOpts{Context: ctx}
}
