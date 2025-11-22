package utils

import (
	"context"
	"errors"
	"fmt"
	"math/big"
	"strings"

	"github.com/ethereum-optimism/optimism/op-service/apis"
	"github.com/ethereum/go-ethereum"
	"github.com/ethereum/go-ethereum/accounts/abi"
	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/rpc"
	opsbind "github.com/succinctlabs/op-succinct/bindings"
)

// L2OOClient is a client for interacting with the SuccinctL2OutputOracle contract.
type L2OOClient struct {
	client   apis.EthClient
	l2ooAddr common.Address
	abi      abi.ABI
}

// NewL2OOClient creates a new L2OOClient.
func NewL2OOClient(client apis.EthClient, l2ooAddr common.Address) (*L2OOClient, error) {
	parsedABI, err := abi.JSON(strings.NewReader(opsbind.OPSuccinctL2OutputOracleMetaData.ABI))
	if err != nil {
		return nil, fmt.Errorf("parse L2OO ABI: %w", err)
	}

	return &L2OOClient{
		client:   client,
		l2ooAddr: l2ooAddr,
		abi:      parsedABI,
	}, nil
}

// LatestBlockNumber fetches the latest L2 block number from the contract.
func (l2oo *L2OOClient) LatestBlockNumber(ctx context.Context) (uint64, error) {
	data, err := l2oo.abi.Pack("latestBlockNumber")
	if err != nil {
		return 0, err
	}

	callMsg := ethereum.CallMsg{
		To:   &l2oo.l2ooAddr,
		Data: data,
	}

	raw, err := l2oo.client.Call(ctx, callMsg, rpc.LatestBlockNumber)
	if err != nil {
		return 0, err
	}

	outs, err := l2oo.abi.Unpack("latestBlockNumber", raw)
	if err != nil {
		return 0, err
	}

	if len(outs) != 1 {
		return 0, errors.New("unexpected number of outputs from latestBlockNumber")
	}

	latestBlock := outs[0].(*big.Int)
	return latestBlock.Uint64(), nil
}

// NextBlockNumber fetches the next L2 block number to be submitted by the proposer.
func (l2oo *L2OOClient) NextBlockNumber(ctx context.Context) (uint64, error) {
	data, err := l2oo.abi.Pack("nextBlockNumber")
	if err != nil {
		return 0, fmt.Errorf("pack nextBlockNumber call: %w", err)
	}

	callMsg := ethereum.CallMsg{
		To:   &l2oo.l2ooAddr,
		Data: data,
	}

	raw, err := l2oo.client.Call(ctx, callMsg, rpc.LatestBlockNumber)
	if err != nil {
		return 0, err
	}

	outs, err := l2oo.abi.Unpack("nextBlockNumber", raw)
	if err != nil {
		return 0, err
	}

	if len(outs) != 1 {
		return 0, err
	}

	latestBlock := outs[0].(*big.Int)
	return latestBlock.Uint64(), nil
}
