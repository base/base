// Code generated - DO NOT EDIT.
// This file is a generated binding and any manual changes will be lost.

package bindings

import (
	"math/big"

	"github.com/ethereum/go-ethereum/accounts/abi"
	"github.com/ethereum/go-ethereum/accounts/abi/bind"
	"github.com/ethereum/go-ethereum/common"
)

// AnchorStateRegistryMetaData contains all meta data concerning the AnchorStateRegistry contract.
var AnchorStateRegistryMetaData = &bind.MetaData{
	ABI: "[{\"type\":\"function\",\"name\":\"anchors\",\"inputs\":[{\"name\":\"_gameType\",\"type\":\"uint32\",\"internalType\":\"GameType\"}],\"outputs\":[{\"name\":\"root\",\"type\":\"bytes32\",\"internalType\":\"Hash\"},{\"name\":\"l2BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"}]",
}

// AnchorStateRegistryABI is the input ABI used to generate the binding from.
var AnchorStateRegistryABI = AnchorStateRegistryMetaData.ABI

// AnchorStateRegistry is an auto generated Go binding around an Ethereum contract.
type AnchorStateRegistry struct {
	AnchorStateRegistryCaller // Read-only binding to the contract
}

// AnchorStateRegistryCaller is an auto generated read-only Go binding around an Ethereum contract.
type AnchorStateRegistryCaller struct {
	contract *bind.BoundContract // Generic contract wrapper for the low level calls
}

// NewAnchorStateRegistryCaller creates a new read-only instance of AnchorStateRegistry, bound to a specific deployed contract.
func NewAnchorStateRegistryCaller(address common.Address, caller bind.ContractCaller) (*AnchorStateRegistryCaller, error) {
	parsed, err := AnchorStateRegistryMetaData.GetAbi()
	if err != nil {
		return nil, err
	}
	contract := bind.NewBoundContract(address, *parsed, caller, nil, nil)
	return &AnchorStateRegistryCaller{contract: contract}, nil
}

// Anchors is a free data retrieval call binding the contract method 0x7ae251d6.
//
// Solidity: function anchors(uint32 _gameType) view returns(bytes32 root, uint256 l2BlockNumber)
func (_AnchorStateRegistry *AnchorStateRegistryCaller) Anchors(opts *bind.CallOpts, _gameType uint32) (struct {
	Root          [32]byte
	L2BlockNumber *big.Int
}, error) {
	var out []interface{}
	err := _AnchorStateRegistry.contract.Call(opts, &out, "anchors", _gameType)

	outstruct := new(struct {
		Root          [32]byte
		L2BlockNumber *big.Int
	})
	if err != nil {
		return *outstruct, err
	}

	outstruct.Root = *abi.ConvertType(out[0], new([32]byte)).(*[32]byte)
	outstruct.L2BlockNumber = *abi.ConvertType(out[1], new(*big.Int)).(**big.Int)

	return *outstruct, nil
}
