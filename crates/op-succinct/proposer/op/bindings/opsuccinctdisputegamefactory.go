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

// OPSuccinctDisputeGameFactoryMetaData contains all meta data concerning the OPSuccinctDisputeGameFactory contract.
var OPSuccinctDisputeGameFactoryMetaData = &bind.MetaData{
	ABI: "[{\"type\":\"constructor\",\"inputs\":[{\"name\":\"_impl\",\"type\":\"address\",\"internalType\":\"address\"}],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"create\",\"inputs\":[{\"name\":\"_rootClaim\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"_l2BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"_l1BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"_proof\",\"type\":\"bytes\",\"internalType\":\"bytes\"}],\"outputs\":[],\"stateMutability\":\"payable\"},{\"type\":\"function\",\"name\":\"impl\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"stateMutability\":\"view\"}]",
	Bin: "0x6080604052348015600e575f5ffd5b50604051610452380380610452833981016040819052602b91604e565b5f80546001600160a01b0319166001600160a01b03929092169190911790556079565b5f60208284031215605d575f5ffd5b81516001600160a01b03811681146072575f5ffd5b9392505050565b6103cc806100865f395ff3fe608060405260043610610028575f3560e01c80636dd7af7f1461002c5780638abf607714610041575b5f5ffd5b61003f61003a366004610244565b61007b565b005b34801561004c575f5ffd5b505f5461005f906001600160a01b031681565b6040516001600160a01b03909116815260200160405180910390f35b5f6100da33865f5f1b87878760405160200161009993929190610313565b60408051601f19818403018152908290526100b994939291602001610356565b60408051601f198184030181529190525f546001600160a01b031690610133565b9050806001600160a01b0316638129fc1c346040518263ffffffff1660e01b81526004015f604051808303818588803b158015610115575f5ffd5b505af1158015610127573d5f5f3e3d5ffd5b50505050505050505050565b5f61013f5f8484610146565b9392505050565b5f60608203516040830351602084035184518060208701018051600283016c5af43d3d93803e606057fd5bf3895289600d8a035278593da1005b363d3d373d3d3d3d610000806062363936013d738160481b1760218a03527f9e4ac34f21c619cefc926c8bd93b54bf5a39c7ab2127a895af1cc0691d7e3dff603a8a035272fd6100003d81600a3d39f336602c57343d527f6062820160781b1761ff9e82106059018a03528060f01b8352606c8101604c8a038cf0975050866102105763301164255f526004601cfd5b90528552601f19850152603f19840152605f199092019190915292915050565b634e487b7160e01b5f52604160045260245ffd5b5f5f5f5f60808587031215610257575f5ffd5b843593506020850135925060408501359150606085013567ffffffffffffffff811115610282575f5ffd5b8501601f81018713610292575f5ffd5b803567ffffffffffffffff8111156102ac576102ac610230565b604051601f8201601f19908116603f0116810167ffffffffffffffff811182821017156102db576102db610230565b6040528181528282016020018910156102f2575f5ffd5b816020840160208301375f6020838301015280935050505092959194509250565b838152826020820152606060408201525f82518060608401528060208501608085015e5f608082850101526080601f19601f830116840101915050949350505050565b6bffffffffffffffffffffffff198560601b1681528360148201528260348201525f82518060208501605485015e5f92016054019182525094935050505056fea2646970667358221220ce3719875c7a48c923f77a9f2a7d16403b8314678d8113d140c47bbec7e073eb64736f6c634300081c0033",
}

// OPSuccinctDisputeGameFactoryABI is the input ABI used to generate the binding from.
// Deprecated: Use OPSuccinctDisputeGameFactoryMetaData.ABI instead.
var OPSuccinctDisputeGameFactoryABI = OPSuccinctDisputeGameFactoryMetaData.ABI

// OPSuccinctDisputeGameFactoryBin is the compiled bytecode used for deploying new contracts.
// Deprecated: Use OPSuccinctDisputeGameFactoryMetaData.Bin instead.
var OPSuccinctDisputeGameFactoryBin = OPSuccinctDisputeGameFactoryMetaData.Bin

// DeployOPSuccinctDisputeGameFactory deploys a new Ethereum contract, binding an instance of OPSuccinctDisputeGameFactory to it.
func DeployOPSuccinctDisputeGameFactory(auth *bind.TransactOpts, backend bind.ContractBackend, _impl common.Address) (common.Address, *types.Transaction, *OPSuccinctDisputeGameFactory, error) {
	parsed, err := OPSuccinctDisputeGameFactoryMetaData.GetAbi()
	if err != nil {
		return common.Address{}, nil, nil, err
	}
	if parsed == nil {
		return common.Address{}, nil, nil, errors.New("GetABI returned nil")
	}

	address, tx, contract, err := bind.DeployContract(auth, *parsed, common.FromHex(OPSuccinctDisputeGameFactoryBin), backend, _impl)
	if err != nil {
		return common.Address{}, nil, nil, err
	}
	return address, tx, &OPSuccinctDisputeGameFactory{OPSuccinctDisputeGameFactoryCaller: OPSuccinctDisputeGameFactoryCaller{contract: contract}, OPSuccinctDisputeGameFactoryTransactor: OPSuccinctDisputeGameFactoryTransactor{contract: contract}, OPSuccinctDisputeGameFactoryFilterer: OPSuccinctDisputeGameFactoryFilterer{contract: contract}}, nil
}

// OPSuccinctDisputeGameFactory is an auto generated Go binding around an Ethereum contract.
type OPSuccinctDisputeGameFactory struct {
	OPSuccinctDisputeGameFactoryCaller     // Read-only binding to the contract
	OPSuccinctDisputeGameFactoryTransactor // Write-only binding to the contract
	OPSuccinctDisputeGameFactoryFilterer   // Log filterer for contract events
}

// OPSuccinctDisputeGameFactoryCaller is an auto generated read-only Go binding around an Ethereum contract.
type OPSuccinctDisputeGameFactoryCaller struct {
	contract *bind.BoundContract // Generic contract wrapper for the low level calls
}

// OPSuccinctDisputeGameFactoryTransactor is an auto generated write-only Go binding around an Ethereum contract.
type OPSuccinctDisputeGameFactoryTransactor struct {
	contract *bind.BoundContract // Generic contract wrapper for the low level calls
}

// OPSuccinctDisputeGameFactoryFilterer is an auto generated log filtering Go binding around an Ethereum contract events.
type OPSuccinctDisputeGameFactoryFilterer struct {
	contract *bind.BoundContract // Generic contract wrapper for the low level calls
}

// OPSuccinctDisputeGameFactorySession is an auto generated Go binding around an Ethereum contract,
// with pre-set call and transact options.
type OPSuccinctDisputeGameFactorySession struct {
	Contract     *OPSuccinctDisputeGameFactory // Generic contract binding to set the session for
	CallOpts     bind.CallOpts                 // Call options to use throughout this session
	TransactOpts bind.TransactOpts             // Transaction auth options to use throughout this session
}

// OPSuccinctDisputeGameFactoryCallerSession is an auto generated read-only Go binding around an Ethereum contract,
// with pre-set call options.
type OPSuccinctDisputeGameFactoryCallerSession struct {
	Contract *OPSuccinctDisputeGameFactoryCaller // Generic contract caller binding to set the session for
	CallOpts bind.CallOpts                       // Call options to use throughout this session
}

// OPSuccinctDisputeGameFactoryTransactorSession is an auto generated write-only Go binding around an Ethereum contract,
// with pre-set transact options.
type OPSuccinctDisputeGameFactoryTransactorSession struct {
	Contract     *OPSuccinctDisputeGameFactoryTransactor // Generic contract transactor binding to set the session for
	TransactOpts bind.TransactOpts                       // Transaction auth options to use throughout this session
}

// OPSuccinctDisputeGameFactoryRaw is an auto generated low-level Go binding around an Ethereum contract.
type OPSuccinctDisputeGameFactoryRaw struct {
	Contract *OPSuccinctDisputeGameFactory // Generic contract binding to access the raw methods on
}

// OPSuccinctDisputeGameFactoryCallerRaw is an auto generated low-level read-only Go binding around an Ethereum contract.
type OPSuccinctDisputeGameFactoryCallerRaw struct {
	Contract *OPSuccinctDisputeGameFactoryCaller // Generic read-only contract binding to access the raw methods on
}

// OPSuccinctDisputeGameFactoryTransactorRaw is an auto generated low-level write-only Go binding around an Ethereum contract.
type OPSuccinctDisputeGameFactoryTransactorRaw struct {
	Contract *OPSuccinctDisputeGameFactoryTransactor // Generic write-only contract binding to access the raw methods on
}

// NewOPSuccinctDisputeGameFactory creates a new instance of OPSuccinctDisputeGameFactory, bound to a specific deployed contract.
func NewOPSuccinctDisputeGameFactory(address common.Address, backend bind.ContractBackend) (*OPSuccinctDisputeGameFactory, error) {
	contract, err := bindOPSuccinctDisputeGameFactory(address, backend, backend, backend)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctDisputeGameFactory{OPSuccinctDisputeGameFactoryCaller: OPSuccinctDisputeGameFactoryCaller{contract: contract}, OPSuccinctDisputeGameFactoryTransactor: OPSuccinctDisputeGameFactoryTransactor{contract: contract}, OPSuccinctDisputeGameFactoryFilterer: OPSuccinctDisputeGameFactoryFilterer{contract: contract}}, nil
}

// NewOPSuccinctDisputeGameFactoryCaller creates a new read-only instance of OPSuccinctDisputeGameFactory, bound to a specific deployed contract.
func NewOPSuccinctDisputeGameFactoryCaller(address common.Address, caller bind.ContractCaller) (*OPSuccinctDisputeGameFactoryCaller, error) {
	contract, err := bindOPSuccinctDisputeGameFactory(address, caller, nil, nil)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctDisputeGameFactoryCaller{contract: contract}, nil
}

// NewOPSuccinctDisputeGameFactoryTransactor creates a new write-only instance of OPSuccinctDisputeGameFactory, bound to a specific deployed contract.
func NewOPSuccinctDisputeGameFactoryTransactor(address common.Address, transactor bind.ContractTransactor) (*OPSuccinctDisputeGameFactoryTransactor, error) {
	contract, err := bindOPSuccinctDisputeGameFactory(address, nil, transactor, nil)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctDisputeGameFactoryTransactor{contract: contract}, nil
}

// NewOPSuccinctDisputeGameFactoryFilterer creates a new log filterer instance of OPSuccinctDisputeGameFactory, bound to a specific deployed contract.
func NewOPSuccinctDisputeGameFactoryFilterer(address common.Address, filterer bind.ContractFilterer) (*OPSuccinctDisputeGameFactoryFilterer, error) {
	contract, err := bindOPSuccinctDisputeGameFactory(address, nil, nil, filterer)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctDisputeGameFactoryFilterer{contract: contract}, nil
}

// bindOPSuccinctDisputeGameFactory binds a generic wrapper to an already deployed contract.
func bindOPSuccinctDisputeGameFactory(address common.Address, caller bind.ContractCaller, transactor bind.ContractTransactor, filterer bind.ContractFilterer) (*bind.BoundContract, error) {
	parsed, err := OPSuccinctDisputeGameFactoryMetaData.GetAbi()
	if err != nil {
		return nil, err
	}
	return bind.NewBoundContract(address, *parsed, caller, transactor, filterer), nil
}

// Call invokes the (constant) contract method with params as input values and
// sets the output to result. The result type might be a single field for simple
// returns, a slice of interfaces for anonymous returns and a struct for named
// returns.
func (_OPSuccinctDisputeGameFactory *OPSuccinctDisputeGameFactoryRaw) Call(opts *bind.CallOpts, result *[]interface{}, method string, params ...interface{}) error {
	return _OPSuccinctDisputeGameFactory.Contract.OPSuccinctDisputeGameFactoryCaller.contract.Call(opts, result, method, params...)
}

// Transfer initiates a plain transaction to move funds to the contract, calling
// its default method if one is available.
func (_OPSuccinctDisputeGameFactory *OPSuccinctDisputeGameFactoryRaw) Transfer(opts *bind.TransactOpts) (*types.Transaction, error) {
	return _OPSuccinctDisputeGameFactory.Contract.OPSuccinctDisputeGameFactoryTransactor.contract.Transfer(opts)
}

// Transact invokes the (paid) contract method with params as input values.
func (_OPSuccinctDisputeGameFactory *OPSuccinctDisputeGameFactoryRaw) Transact(opts *bind.TransactOpts, method string, params ...interface{}) (*types.Transaction, error) {
	return _OPSuccinctDisputeGameFactory.Contract.OPSuccinctDisputeGameFactoryTransactor.contract.Transact(opts, method, params...)
}

// Call invokes the (constant) contract method with params as input values and
// sets the output to result. The result type might be a single field for simple
// returns, a slice of interfaces for anonymous returns and a struct for named
// returns.
func (_OPSuccinctDisputeGameFactory *OPSuccinctDisputeGameFactoryCallerRaw) Call(opts *bind.CallOpts, result *[]interface{}, method string, params ...interface{}) error {
	return _OPSuccinctDisputeGameFactory.Contract.contract.Call(opts, result, method, params...)
}

// Transfer initiates a plain transaction to move funds to the contract, calling
// its default method if one is available.
func (_OPSuccinctDisputeGameFactory *OPSuccinctDisputeGameFactoryTransactorRaw) Transfer(opts *bind.TransactOpts) (*types.Transaction, error) {
	return _OPSuccinctDisputeGameFactory.Contract.contract.Transfer(opts)
}

// Transact invokes the (paid) contract method with params as input values.
func (_OPSuccinctDisputeGameFactory *OPSuccinctDisputeGameFactoryTransactorRaw) Transact(opts *bind.TransactOpts, method string, params ...interface{}) (*types.Transaction, error) {
	return _OPSuccinctDisputeGameFactory.Contract.contract.Transact(opts, method, params...)
}

// Impl is a free data retrieval call binding the contract method 0x8abf6077.
//
// Solidity: function impl() view returns(address)
func (_OPSuccinctDisputeGameFactory *OPSuccinctDisputeGameFactoryCaller) Impl(opts *bind.CallOpts) (common.Address, error) {
	var out []interface{}
	err := _OPSuccinctDisputeGameFactory.contract.Call(opts, &out, "impl")

	if err != nil {
		return *new(common.Address), err
	}

	out0 := *abi.ConvertType(out[0], new(common.Address)).(*common.Address)

	return out0, err

}

// Impl is a free data retrieval call binding the contract method 0x8abf6077.
//
// Solidity: function impl() view returns(address)
func (_OPSuccinctDisputeGameFactory *OPSuccinctDisputeGameFactorySession) Impl() (common.Address, error) {
	return _OPSuccinctDisputeGameFactory.Contract.Impl(&_OPSuccinctDisputeGameFactory.CallOpts)
}

// Impl is a free data retrieval call binding the contract method 0x8abf6077.
//
// Solidity: function impl() view returns(address)
func (_OPSuccinctDisputeGameFactory *OPSuccinctDisputeGameFactoryCallerSession) Impl() (common.Address, error) {
	return _OPSuccinctDisputeGameFactory.Contract.Impl(&_OPSuccinctDisputeGameFactory.CallOpts)
}

// Create is a paid mutator transaction binding the contract method 0x6dd7af7f.
//
// Solidity: function create(bytes32 _rootClaim, uint256 _l2BlockNumber, uint256 _l1BlockNumber, bytes _proof) payable returns()
func (_OPSuccinctDisputeGameFactory *OPSuccinctDisputeGameFactoryTransactor) Create(opts *bind.TransactOpts, _rootClaim [32]byte, _l2BlockNumber *big.Int, _l1BlockNumber *big.Int, _proof []byte) (*types.Transaction, error) {
	return _OPSuccinctDisputeGameFactory.contract.Transact(opts, "create", _rootClaim, _l2BlockNumber, _l1BlockNumber, _proof)
}

// Create is a paid mutator transaction binding the contract method 0x6dd7af7f.
//
// Solidity: function create(bytes32 _rootClaim, uint256 _l2BlockNumber, uint256 _l1BlockNumber, bytes _proof) payable returns()
func (_OPSuccinctDisputeGameFactory *OPSuccinctDisputeGameFactorySession) Create(_rootClaim [32]byte, _l2BlockNumber *big.Int, _l1BlockNumber *big.Int, _proof []byte) (*types.Transaction, error) {
	return _OPSuccinctDisputeGameFactory.Contract.Create(&_OPSuccinctDisputeGameFactory.TransactOpts, _rootClaim, _l2BlockNumber, _l1BlockNumber, _proof)
}

// Create is a paid mutator transaction binding the contract method 0x6dd7af7f.
//
// Solidity: function create(bytes32 _rootClaim, uint256 _l2BlockNumber, uint256 _l1BlockNumber, bytes _proof) payable returns()
func (_OPSuccinctDisputeGameFactory *OPSuccinctDisputeGameFactoryTransactorSession) Create(_rootClaim [32]byte, _l2BlockNumber *big.Int, _l1BlockNumber *big.Int, _proof []byte) (*types.Transaction, error) {
	return _OPSuccinctDisputeGameFactory.Contract.Create(&_OPSuccinctDisputeGameFactory.TransactOpts, _rootClaim, _l2BlockNumber, _l1BlockNumber, _proof)
}
