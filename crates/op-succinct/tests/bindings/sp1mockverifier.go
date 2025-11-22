// Code generated - DO NOT EDIT.
// This file is a generated binding and any manual changes will be lost.

package bindings

import (
	"errors"
	"math/big"
	"strings"

	ethereum "github.com/ethereum/go-ethereum"
	"github.com/ethereum/go-ethereum/accounts/abi"
	"github.com/ethereum/go-ethereum/accounts/abi/bind"
	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/core/types"
	"github.com/ethereum/go-ethereum/event"
)

// Reference imports to suppress errors if they are not otherwise used.
var (
	_ = errors.New
	_ = big.NewInt
	_ = strings.NewReader
	_ = ethereum.NotFound
	_ = bind.Bind
	_ = common.Big1
	_ = types.BloomLookup
	_ = event.NewSubscription
	_ = abi.ConvertType
)

// SP1MockVerifierMetaData contains all meta data concerning the SP1MockVerifier contract.
var SP1MockVerifierMetaData = &bind.MetaData{
	ABI: "[{\"type\":\"function\",\"name\":\"verifyProof\",\"inputs\":[{\"name\":\"\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"\",\"type\":\"bytes\",\"internalType\":\"bytes\"},{\"name\":\"proofBytes\",\"type\":\"bytes\",\"internalType\":\"bytes\"}],\"outputs\":[],\"stateMutability\":\"pure\"}]",
	Bin: "0x608060405234801561001057600080fd5b50610206806100206000396000f3fe608060405234801561001057600080fd5b506004361061002b5760003560e01c806341493c6014610030575b600080fd5b61004a6004803603810190610045919061010c565b61004c565b005b600082829050146100605761005f6101a1565b5b5050505050565b600080fd5b600080fd5b6000819050919050565b61008481610071565b811461008f57600080fd5b50565b6000813590506100a18161007b565b92915050565b600080fd5b600080fd5b600080fd5b60008083601f8401126100cc576100cb6100a7565b5b8235905067ffffffffffffffff8111156100e9576100e86100ac565b5b602083019150836001820283011115610105576101046100b1565b5b9250929050565b60008060008060006060868803121561012857610127610067565b5b600061013688828901610092565b955050602086013567ffffffffffffffff8111156101575761015661006c565b5b610163888289016100b6565b9450945050604086013567ffffffffffffffff8111156101865761018561006c565b5b610192888289016100b6565b92509250509295509295909350565b7f4e487b7100000000000000000000000000000000000000000000000000000000600052600160045260246000fdfea2646970667358221220d550b481a2e81fd1502f90447b70200d364a2a13c43cf70c1956b286628bd5b264736f6c634300080f0033",
}

// SP1MockVerifierABI is the input ABI used to generate the binding from.
// Deprecated: Use SP1MockVerifierMetaData.ABI instead.
var SP1MockVerifierABI = SP1MockVerifierMetaData.ABI

// SP1MockVerifierBin is the compiled bytecode used for deploying new contracts.
// Deprecated: Use SP1MockVerifierMetaData.Bin instead.
var SP1MockVerifierBin = SP1MockVerifierMetaData.Bin

// DeploySP1MockVerifier deploys a new Ethereum contract, binding an instance of SP1MockVerifier to it.
func DeploySP1MockVerifier(auth *bind.TransactOpts, backend bind.ContractBackend) (common.Address, *types.Transaction, *SP1MockVerifier, error) {
	parsed, err := SP1MockVerifierMetaData.GetAbi()
	if err != nil {
		return common.Address{}, nil, nil, err
	}
	if parsed == nil {
		return common.Address{}, nil, nil, errors.New("GetABI returned nil")
	}

	address, tx, contract, err := bind.DeployContract(auth, *parsed, common.FromHex(SP1MockVerifierBin), backend)
	if err != nil {
		return common.Address{}, nil, nil, err
	}
	return address, tx, &SP1MockVerifier{SP1MockVerifierCaller: SP1MockVerifierCaller{contract: contract}, SP1MockVerifierTransactor: SP1MockVerifierTransactor{contract: contract}, SP1MockVerifierFilterer: SP1MockVerifierFilterer{contract: contract}}, nil
}

// SP1MockVerifier is an auto generated Go binding around an Ethereum contract.
type SP1MockVerifier struct {
	SP1MockVerifierCaller     // Read-only binding to the contract
	SP1MockVerifierTransactor // Write-only binding to the contract
	SP1MockVerifierFilterer   // Log filterer for contract events
}

// SP1MockVerifierCaller is an auto generated read-only Go binding around an Ethereum contract.
type SP1MockVerifierCaller struct {
	contract *bind.BoundContract // Generic contract wrapper for the low level calls
}

// SP1MockVerifierTransactor is an auto generated write-only Go binding around an Ethereum contract.
type SP1MockVerifierTransactor struct {
	contract *bind.BoundContract // Generic contract wrapper for the low level calls
}

// SP1MockVerifierFilterer is an auto generated log filtering Go binding around an Ethereum contract events.
type SP1MockVerifierFilterer struct {
	contract *bind.BoundContract // Generic contract wrapper for the low level calls
}

// SP1MockVerifierSession is an auto generated Go binding around an Ethereum contract,
// with pre-set call and transact options.
type SP1MockVerifierSession struct {
	Contract     *SP1MockVerifier  // Generic contract binding to set the session for
	CallOpts     bind.CallOpts     // Call options to use throughout this session
	TransactOpts bind.TransactOpts // Transaction auth options to use throughout this session
}

// SP1MockVerifierCallerSession is an auto generated read-only Go binding around an Ethereum contract,
// with pre-set call options.
type SP1MockVerifierCallerSession struct {
	Contract *SP1MockVerifierCaller // Generic contract caller binding to set the session for
	CallOpts bind.CallOpts          // Call options to use throughout this session
}

// SP1MockVerifierTransactorSession is an auto generated write-only Go binding around an Ethereum contract,
// with pre-set transact options.
type SP1MockVerifierTransactorSession struct {
	Contract     *SP1MockVerifierTransactor // Generic contract transactor binding to set the session for
	TransactOpts bind.TransactOpts          // Transaction auth options to use throughout this session
}

// SP1MockVerifierRaw is an auto generated low-level Go binding around an Ethereum contract.
type SP1MockVerifierRaw struct {
	Contract *SP1MockVerifier // Generic contract binding to access the raw methods on
}

// SP1MockVerifierCallerRaw is an auto generated low-level read-only Go binding around an Ethereum contract.
type SP1MockVerifierCallerRaw struct {
	Contract *SP1MockVerifierCaller // Generic read-only contract binding to access the raw methods on
}

// SP1MockVerifierTransactorRaw is an auto generated low-level write-only Go binding around an Ethereum contract.
type SP1MockVerifierTransactorRaw struct {
	Contract *SP1MockVerifierTransactor // Generic write-only contract binding to access the raw methods on
}

// NewSP1MockVerifier creates a new instance of SP1MockVerifier, bound to a specific deployed contract.
func NewSP1MockVerifier(address common.Address, backend bind.ContractBackend) (*SP1MockVerifier, error) {
	contract, err := bindSP1MockVerifier(address, backend, backend, backend)
	if err != nil {
		return nil, err
	}
	return &SP1MockVerifier{SP1MockVerifierCaller: SP1MockVerifierCaller{contract: contract}, SP1MockVerifierTransactor: SP1MockVerifierTransactor{contract: contract}, SP1MockVerifierFilterer: SP1MockVerifierFilterer{contract: contract}}, nil
}

// NewSP1MockVerifierCaller creates a new read-only instance of SP1MockVerifier, bound to a specific deployed contract.
func NewSP1MockVerifierCaller(address common.Address, caller bind.ContractCaller) (*SP1MockVerifierCaller, error) {
	contract, err := bindSP1MockVerifier(address, caller, nil, nil)
	if err != nil {
		return nil, err
	}
	return &SP1MockVerifierCaller{contract: contract}, nil
}

// NewSP1MockVerifierTransactor creates a new write-only instance of SP1MockVerifier, bound to a specific deployed contract.
func NewSP1MockVerifierTransactor(address common.Address, transactor bind.ContractTransactor) (*SP1MockVerifierTransactor, error) {
	contract, err := bindSP1MockVerifier(address, nil, transactor, nil)
	if err != nil {
		return nil, err
	}
	return &SP1MockVerifierTransactor{contract: contract}, nil
}

// NewSP1MockVerifierFilterer creates a new log filterer instance of SP1MockVerifier, bound to a specific deployed contract.
func NewSP1MockVerifierFilterer(address common.Address, filterer bind.ContractFilterer) (*SP1MockVerifierFilterer, error) {
	contract, err := bindSP1MockVerifier(address, nil, nil, filterer)
	if err != nil {
		return nil, err
	}
	return &SP1MockVerifierFilterer{contract: contract}, nil
}

// bindSP1MockVerifier binds a generic wrapper to an already deployed contract.
func bindSP1MockVerifier(address common.Address, caller bind.ContractCaller, transactor bind.ContractTransactor, filterer bind.ContractFilterer) (*bind.BoundContract, error) {
	parsed, err := SP1MockVerifierMetaData.GetAbi()
	if err != nil {
		return nil, err
	}
	return bind.NewBoundContract(address, *parsed, caller, transactor, filterer), nil
}

// Call invokes the (constant) contract method with params as input values and
// sets the output to result. The result type might be a single field for simple
// returns, a slice of interfaces for anonymous returns and a struct for named
// returns.
func (_SP1MockVerifier *SP1MockVerifierRaw) Call(opts *bind.CallOpts, result *[]interface{}, method string, params ...interface{}) error {
	return _SP1MockVerifier.Contract.SP1MockVerifierCaller.contract.Call(opts, result, method, params...)
}

// Transfer initiates a plain transaction to move funds to the contract, calling
// its default method if one is available.
func (_SP1MockVerifier *SP1MockVerifierRaw) Transfer(opts *bind.TransactOpts) (*types.Transaction, error) {
	return _SP1MockVerifier.Contract.SP1MockVerifierTransactor.contract.Transfer(opts)
}

// Transact invokes the (paid) contract method with params as input values.
func (_SP1MockVerifier *SP1MockVerifierRaw) Transact(opts *bind.TransactOpts, method string, params ...interface{}) (*types.Transaction, error) {
	return _SP1MockVerifier.Contract.SP1MockVerifierTransactor.contract.Transact(opts, method, params...)
}

// Call invokes the (constant) contract method with params as input values and
// sets the output to result. The result type might be a single field for simple
// returns, a slice of interfaces for anonymous returns and a struct for named
// returns.
func (_SP1MockVerifier *SP1MockVerifierCallerRaw) Call(opts *bind.CallOpts, result *[]interface{}, method string, params ...interface{}) error {
	return _SP1MockVerifier.Contract.contract.Call(opts, result, method, params...)
}

// Transfer initiates a plain transaction to move funds to the contract, calling
// its default method if one is available.
func (_SP1MockVerifier *SP1MockVerifierTransactorRaw) Transfer(opts *bind.TransactOpts) (*types.Transaction, error) {
	return _SP1MockVerifier.Contract.contract.Transfer(opts)
}

// Transact invokes the (paid) contract method with params as input values.
func (_SP1MockVerifier *SP1MockVerifierTransactorRaw) Transact(opts *bind.TransactOpts, method string, params ...interface{}) (*types.Transaction, error) {
	return _SP1MockVerifier.Contract.contract.Transact(opts, method, params...)
}

// VerifyProof is a free data retrieval call binding the contract method 0x41493c60.
//
// Solidity: function verifyProof(bytes32 , bytes , bytes proofBytes) pure returns()
func (_SP1MockVerifier *SP1MockVerifierCaller) VerifyProof(opts *bind.CallOpts, arg0 [32]byte, arg1 []byte, proofBytes []byte) error {
	var out []interface{}
	err := _SP1MockVerifier.contract.Call(opts, &out, "verifyProof", arg0, arg1, proofBytes)

	if err != nil {
		return err
	}

	return err

}

// VerifyProof is a free data retrieval call binding the contract method 0x41493c60.
//
// Solidity: function verifyProof(bytes32 , bytes , bytes proofBytes) pure returns()
func (_SP1MockVerifier *SP1MockVerifierSession) VerifyProof(arg0 [32]byte, arg1 []byte, proofBytes []byte) error {
	return _SP1MockVerifier.Contract.VerifyProof(&_SP1MockVerifier.CallOpts, arg0, arg1, proofBytes)
}

// VerifyProof is a free data retrieval call binding the contract method 0x41493c60.
//
// Solidity: function verifyProof(bytes32 , bytes , bytes proofBytes) pure returns()
func (_SP1MockVerifier *SP1MockVerifierCallerSession) VerifyProof(arg0 [32]byte, arg1 []byte, proofBytes []byte) error {
	return _SP1MockVerifier.Contract.VerifyProof(&_SP1MockVerifier.CallOpts, arg0, arg1, proofBytes)
}
