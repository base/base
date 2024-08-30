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

// TypesOutputProposal is an auto generated low-level Go binding around an user-defined struct.
type TypesOutputProposal struct {
	OutputRoot    [32]byte
	Timestamp     *big.Int
	L2BlockNumber *big.Int
}

// ZKL2OutputOracleZKInitParams is an auto generated low-level Go binding around an user-defined struct.
type ZKL2OutputOracleZKInitParams struct {
	ChainId            *big.Int
	Vkey               [32]byte
	VerifierGateway    common.Address
	StartingOutputRoot [32]byte
	Owner              common.Address
}

// ZKL2OutputOracleMetaData contains all meta data concerning the ZKL2OutputOracle contract.
var ZKL2OutputOracleMetaData = &bind.MetaData{
	ABI: "[{\"type\":\"constructor\",\"inputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"CHALLENGER\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"FINALIZATION_PERIOD_SECONDS\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"L2_BLOCK_TIME\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"PROPOSER\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"SUBMISSION_INTERVAL\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"chainId\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"challenger\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"checkpointBlockHash\",\"inputs\":[{\"name\":\"_blockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"_blockHash\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"computeL2Timestamp\",\"inputs\":[{\"name\":\"_l2BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"deleteL2Outputs\",\"inputs\":[{\"name\":\"_l2OutputIndex\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"finalizationPeriodSeconds\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"getL2Output\",\"inputs\":[{\"name\":\"_l2OutputIndex\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[{\"name\":\"\",\"type\":\"tuple\",\"internalType\":\"structTypes.OutputProposal\",\"components\":[{\"name\":\"outputRoot\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"timestamp\",\"type\":\"uint128\",\"internalType\":\"uint128\"},{\"name\":\"l2BlockNumber\",\"type\":\"uint128\",\"internalType\":\"uint128\"}]}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"getL2OutputAfter\",\"inputs\":[{\"name\":\"_l2BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[{\"name\":\"\",\"type\":\"tuple\",\"internalType\":\"structTypes.OutputProposal\",\"components\":[{\"name\":\"outputRoot\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"timestamp\",\"type\":\"uint128\",\"internalType\":\"uint128\"},{\"name\":\"l2BlockNumber\",\"type\":\"uint128\",\"internalType\":\"uint128\"}]}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"getL2OutputIndexAfter\",\"inputs\":[{\"name\":\"_l2BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"historicBlockHashes\",\"inputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[{\"name\":\"\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"initialize\",\"inputs\":[{\"name\":\"_submissionInterval\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"_l2BlockTime\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"_startingBlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"_startingTimestamp\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"_proposer\",\"type\":\"address\",\"internalType\":\"address\"},{\"name\":\"_challenger\",\"type\":\"address\",\"internalType\":\"address\"},{\"name\":\"_finalizationPeriodSeconds\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"_zkInitParams\",\"type\":\"tuple\",\"internalType\":\"structZKL2OutputOracle.ZKInitParams\",\"components\":[{\"name\":\"chainId\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"vkey\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"verifierGateway\",\"type\":\"address\",\"internalType\":\"address\"},{\"name\":\"startingOutputRoot\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"owner\",\"type\":\"address\",\"internalType\":\"address\"}]}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"l2BlockTime\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"latestBlockNumber\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"latestOutputIndex\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"nextBlockNumber\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"nextOutputIndex\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"owner\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"proposeL2Output\",\"inputs\":[{\"name\":\"_outputRoot\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"_l2BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"_l1BlockHash\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"_l1BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"_proof\",\"type\":\"bytes\",\"internalType\":\"bytes\"}],\"outputs\":[],\"stateMutability\":\"payable\"},{\"type\":\"function\",\"name\":\"proposer\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"startingBlockNumber\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"startingTimestamp\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"submissionInterval\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"transferOwnership\",\"inputs\":[{\"name\":\"_newOwner\",\"type\":\"address\",\"internalType\":\"address\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"updateVKey\",\"inputs\":[{\"name\":\"_vkey\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"updateVerifierGateway\",\"inputs\":[{\"name\":\"_verifierGateway\",\"type\":\"address\",\"internalType\":\"address\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"verifierGateway\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"contractSP1VerifierGateway\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"version\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"string\",\"internalType\":\"string\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"vkey\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"stateMutability\":\"view\"},{\"type\":\"event\",\"name\":\"Initialized\",\"inputs\":[{\"name\":\"version\",\"type\":\"uint8\",\"indexed\":false,\"internalType\":\"uint8\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"OutputProposed\",\"inputs\":[{\"name\":\"outputRoot\",\"type\":\"bytes32\",\"indexed\":true,\"internalType\":\"bytes32\"},{\"name\":\"l2OutputIndex\",\"type\":\"uint256\",\"indexed\":true,\"internalType\":\"uint256\"},{\"name\":\"l2BlockNumber\",\"type\":\"uint256\",\"indexed\":true,\"internalType\":\"uint256\"},{\"name\":\"l1Timestamp\",\"type\":\"uint256\",\"indexed\":false,\"internalType\":\"uint256\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"OutputsDeleted\",\"inputs\":[{\"name\":\"prevNextOutputIndex\",\"type\":\"uint256\",\"indexed\":true,\"internalType\":\"uint256\"},{\"name\":\"newNextOutputIndex\",\"type\":\"uint256\",\"indexed\":true,\"internalType\":\"uint256\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"OwnershipTransferred\",\"inputs\":[{\"name\":\"previousOwner\",\"type\":\"address\",\"indexed\":true,\"internalType\":\"address\"},{\"name\":\"newOwner\",\"type\":\"address\",\"indexed\":true,\"internalType\":\"address\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"UpdatedVKey\",\"inputs\":[{\"name\":\"oldVkey\",\"type\":\"bytes32\",\"indexed\":true,\"internalType\":\"bytes32\"},{\"name\":\"newVkey\",\"type\":\"bytes32\",\"indexed\":true,\"internalType\":\"bytes32\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"UpdatedVerifierGateway\",\"inputs\":[{\"name\":\"oldVerifierGateway\",\"type\":\"address\",\"indexed\":true,\"internalType\":\"address\"},{\"name\":\"newVerifierGateway\",\"type\":\"address\",\"indexed\":true,\"internalType\":\"address\"}],\"anonymous\":false}]",
	Bin: "0x608060405234801561001057600080fd5b5061001961001e565b6100de565b600054610100900460ff161561008a5760405162461bcd60e51b815260206004820152602760248201527f496e697469616c697a61626c653a20636f6e747261637420697320696e697469604482015266616c697a696e6760c81b606482015260840160405180910390fd5b60005460ff90811610156100dc576000805460ff191660ff9081179091556040519081527f7f26b83ff96e1f2b6a682f133852f6798a09c465da95921460cefb38474024989060200160405180910390a15b565b6119bd806100ed6000396000f3fe6080604052600436106101ed5760003560e01c80639a8a05921161010d578063cf8e5cf0116100a0578063dcec33481161006f578063dcec334814610575578063e1a41bcf1461058a578063f2fde38b146105a0578063f4daa291146105c0578063fb3c491c146105d557600080fd5b8063cf8e5cf0146104f5578063d1de856c14610515578063d946a45614610535578063dbf330741461055557600080fd5b8063a9efd6b8116100dc578063a9efd6b814610498578063bffa7f0f146104ab578063c69b0eb1146104c9578063ce5db8d6146104df57600080fd5b80639a8a0592146103e2578063a196b525146103f8578063a25ae55714610425578063a8e4fb901461047857600080fd5b80636abcf563116101855780638878627211610154578063887862721461037657806389c44cbb1461038c5780638da5cb5b146103ac57806393991af3146103cc57600080fd5b80636abcf5631461030d5780636b4d98dd1461032257806370872aa5146103405780637f0064201461035657600080fd5b8063529933df116101c1578063529933df1461026d578063534db0e21461028257806354fd4d50146102ba57806369f16eec146102f857600080fd5b80622134cc146101f25780634418db5e146102165780634599c788146102385780634600df381461024d575b600080fd5b3480156101fe57600080fd5b506005545b6040519081526020015b60405180910390f35b34801561022257600080fd5b506102366102313660046115bf565b6105f5565b005b34801561024457600080fd5b50610203610634565b34801561025957600080fd5b50610236610268366004611651565b610691565b34801561027957600080fd5b50600454610203565b34801561028e57600080fd5b506006546102a2906001600160a01b031681565b6040516001600160a01b03909116815260200161020d565b3480156102c657600080fd5b506102eb604051806040016040528060058152602001640322e302e360dc1b81525081565b60405161020d9190611765565b34801561030457600080fd5b506102036109fe565b34801561031957600080fd5b50600354610203565b34801561032e57600080fd5b506006546001600160a01b03166102a2565b34801561034c57600080fd5b5061020360015481565b34801561036257600080fd5b50610203610371366004611778565b610a10565b34801561038257600080fd5b5061020360025481565b34801561039857600080fd5b506102366103a7366004611778565b610bae565b3480156103b857600080fd5b50600c546102a2906001600160a01b031681565b3480156103d857600080fd5b5061020360055481565b3480156103ee57600080fd5b5061020360095481565b34801561040457600080fd5b50610203610413366004611778565b600d6020526000908152604090205481565b34801561043157600080fd5b50610445610440366004611778565b610db3565b60408051825181526020808401516001600160801b0390811691830191909152928201519092169082015260600161020d565b34801561048457600080fd5b506007546102a2906001600160a01b031681565b6102366104a6366004611791565b610e31565b3480156104b757600080fd5b506007546001600160a01b03166102a2565b3480156104d557600080fd5b50610203600a5481565b3480156104eb57600080fd5b5061020360085481565b34801561050157600080fd5b50610445610510366004611778565b61134a565b34801561052157600080fd5b50610203610530366004611778565b611382565b34801561054157600080fd5b50610236610550366004611778565b6113b2565b34801561056157600080fd5b5061023661057036600461184d565b6113e5565b34801561058157600080fd5b5061020361146d565b34801561059657600080fd5b5061020360045481565b3480156105ac57600080fd5b506102366105bb3660046115bf565b611484565b3480156105cc57600080fd5b50600854610203565b3480156105e157600080fd5b50600b546102a2906001600160a01b031681565b600c546001600160a01b031633146106285760405162461bcd60e51b815260040161061f9061186f565b60405180910390fd5b610631816114b7565b50565b60035460009015610688576003805461064f906001906118cc565b8154811061065f5761065f6118e3565b6000918252602090912060029091020160010154600160801b90046001600160801b0316919050565b6001545b905090565b600054600290610100900460ff161580156106b3575060005460ff8083169116105b6107165760405162461bcd60e51b815260206004820152602e60248201527f496e697469616c697a61626c653a20636f6e747261637420697320616c72656160448201526d191e481a5b9a5d1a585b1a5e995960921b606482015260840161061f565b6000805461ffff191660ff8316176101001790558861079d5760405162461bcd60e51b815260206004820152603a60248201527f4c324f75747075744f7261636c653a207375626d697373696f6e20696e74657260448201527f76616c206d7573742062652067726561746572207468616e2030000000000000606482015260840161061f565b6000881161080a5760405162461bcd60e51b815260206004820152603460248201527f4c324f75747075744f7261636c653a204c3220626c6f636b2074696d65206d75604482015273073742062652067726561746572207468616e20360641b606482015260840161061f565b4286111561088e5760405162461bcd60e51b8152602060048201526044602482018190527f4c324f75747075744f7261636c653a207374617274696e67204c322074696d65908201527f7374616d70206d757374206265206c657373207468616e2063757272656e742060648201526374696d6560e01b608482015260a40161061f565b60048990556005889055600780546001600160a01b038088166001600160a01b0319928316179092556006805492871692909116919091179055600883905560035460000361098557604080516060808201835284015181526001600160801b03808916602083019081528a82169383019384526003805460018181018355600092909252935160029485027fc2575a0e9e593c00f959f8c92f12db2869c3395a3b0502d05e2516446f71f85b810191909155915194518316600160801b0294909216939093177fc2575a0e9e593c00f959f8c92f12db2869c3395a3b0502d05e2516446f71f85c90930192909255908890558690555b8151600955608082015161099890611513565b6109a5826020015161156f565b6109b282604001516114b7565b6000805461ff001916905560405160ff821681527f7f26b83ff96e1f2b6a682f133852f6798a09c465da95921460cefb38474024989060200160405180910390a1505050505050505050565b60035460009061068c906001906118cc565b6000610a1a610634565b821115610aa05760405162461bcd60e51b815260206004820152604860248201527f4c324f75747075744f7261636c653a2063616e6e6f7420676574206f7574707560448201527f7420666f72206120626c6f636b207468617420686173206e6f74206265656e206064820152671c1c9bdc1bdcd95960c21b608482015260a40161061f565b600354610b245760405162461bcd60e51b815260206004820152604660248201527f4c324f75747075744f7261636c653a2063616e6e6f7420676574206f7574707560448201527f74206173206e6f206f7574707574732068617665206265656e2070726f706f736064820152651959081e595d60d21b608482015260a40161061f565b6003546000905b80821015610ba75760006002610b4183856118f9565b610b4b9190611911565b90508460038281548110610b6157610b616118e3565b6000918252602090912060029091020160010154600160801b90046001600160801b03161015610b9d57610b968160016118f9565b9250610ba1565b8091505b50610b2b565b5092915050565b6006546001600160a01b03163314610c2e5760405162461bcd60e51b815260206004820152603e60248201527f4c324f75747075744f7261636c653a206f6e6c7920746865206368616c6c656e60448201527f67657220616464726573732063616e2064656c657465206f7574707574730000606482015260840161061f565b6003548110610cb15760405162461bcd60e51b815260206004820152604360248201527f4c324f75747075744f7261636c653a2063616e6e6f742064656c657465206f7560448201527f747075747320616674657220746865206c6174657374206f757470757420696e6064820152620c8caf60eb1b608482015260a40161061f565b60085460038281548110610cc757610cc76118e3565b6000918252602090912060016002909202010154610cee906001600160801b0316426118cc565b10610d705760405162461bcd60e51b815260206004820152604660248201527f4c324f75747075744f7261636c653a2063616e6e6f742064656c657465206f7560448201527f74707574732074686174206861766520616c7265616479206265656e2066696e606482015265185b1a5e995960d21b608482015260a40161061f565b6000610d7b60035490565b90508160035581817f4ee37ac2c786ec85e87592d3c5c8a1dd66f8496dda3f125d9ea8ca5f657629b660405160405180910390a35050565b604080516060810182526000808252602082018190529181019190915260038281548110610de357610de36118e3565b600091825260209182902060408051606081018252600290930290910180548352600101546001600160801b0380821694840194909452600160801b90049092169181019190915292915050565b6007546001600160a01b0316331480610e5357506007546001600160a01b0316155b610ecf5760405162461bcd60e51b815260206004820152604160248201527f4c324f75747075744f7261636c653a206f6e6c79207468652070726f706f736560448201527f7220616464726573732063616e2070726f706f7365206e6577206f75747075746064820152607360f81b608482015260a40161061f565b610ed761146d565b841015610f725760405162461bcd60e51b815260206004820152605860248201527f4c324f75747075744f7261636c653a20626c6f636b206e756d626572206d757360448201527f742062652067726561746572207468616e206f7220657175616c20746f206e6560648201527f787420657870656374656420626c6f636b206e756d6265720000000000000000608482015260a40161061f565b42610f7c85611382565b10610fe85760405162461bcd60e51b815260206004820152603660248201527f4c324f75747075744f7261636c653a2063616e6e6f742070726f706f7365204c60448201527532206f757470757420696e207468652066757475726560501b606482015260840161061f565b8461105b5760405162461bcd60e51b815260206004820152603a60248201527f4c324f75747075744f7261636c653a204c32206f75747075742070726f706f7360448201527f616c2063616e6e6f7420626520746865207a65726f2068617368000000000000606482015260840161061f565b600a546110d05760405162461bcd60e51b815260206004820152603b60248201527f4c324f75747075744f7261636c653a20766b6579206d7573742062652073657460448201527f206265666f72652070726f706f73696e6720616e206f75747075740000000000606482015260840161061f565b6000828152600d6020526040902054831461115f5760405162461bcd60e51b815260206004820152604360248201527f4c324f75747075744f7261636c653a2070726f706f73656420626c6f636b206860448201527f61736820616e64206e756d62657220617265206e6f7420636865636b706f696e6064820152621d195960ea1b608482015260a40161061f565b60006040518060a00160405280858152602001600361117c6109fe565b8154811061118c5761118c6118e3565b60009182526020918290206002909102015482528181018990526040808301899052600954606093840152600b54600a548251865181860152938601518484015291850151838501529284015160808084019190915284015160a08301529293506001600160a01b03909116916341493c609160c001604051602081830303815290604052856040518463ffffffff1660e01b815260040161123093929190611933565b60006040518083038186803b15801561124857600080fd5b505afa15801561125c573d6000803e3d6000fd5b505050508461126a60035490565b877fa7aaf2512769da4e444e3de247be2564225c2e7a8f74cfe528e46e17d24868e24260405161129c91815260200190565b60405180910390a45050604080516060810182529485526001600160801b034281166020870190815294811691860191825260038054600181018255600091909152955160029096027fc2575a0e9e593c00f959f8c92f12db2869c3395a3b0502d05e2516446f71f85b810196909655935190518416600160801b029316929092177fc2575a0e9e593c00f959f8c92f12db2869c3395a3b0502d05e2516446f71f85c909301929092555050565b6040805160608101825260008082526020820181905291810191909152600361137283610a10565b81548110610de357610de36118e3565b60006005546001548361139591906118cc565b61139f9190611968565b6002546113ac91906118f9565b92915050565b600c546001600160a01b031633146113dc5760405162461bcd60e51b815260040161061f9061186f565b6106318161156f565b8082401461145b5760405162461bcd60e51b815260206004820152603c60248201527f4c324f75747075744f7261636c653a20626c6f636b206861736820616e64206e60448201527f756d6265722063616e6e6f7420626520636865636b706f696e74656400000000606482015260840161061f565b6000918252600d602052604090912055565b600060045461147a610634565b61068c91906118f9565b600c546001600160a01b031633146114ae5760405162461bcd60e51b815260040161061f9061186f565b61063181611513565b600b546040516001600160a01b038084169216907f1379941631ff0ed9178ab16ab67a2e5db3aeada7f87e518f761e79c8e38377e390600090a3600b80546001600160a01b0319166001600160a01b0392909216919091179055565b600c546040516001600160a01b038084169216907f8be0079c531659141344cd1fd0a4f28419497f9722a3daafe3b4186f6b6457e090600090a3600c80546001600160a01b0319166001600160a01b0392909216919091179055565b600a546040518291907f9950c7ae9575688726be5b8ba4f28af88f91a9ad6853bc02658104b62cfcd77d90600090a3600a55565b80356001600160a01b03811681146115ba57600080fd5b919050565b6000602082840312156115d157600080fd5b6115da826115a3565b9392505050565b634e487b7160e01b600052604160045260246000fd5b60405160a0810167ffffffffffffffff8111828210171561161a5761161a6115e1565b60405290565b604051601f8201601f1916810167ffffffffffffffff81118282101715611649576116496115e1565b604052919050565b600080600080600080600080888a0361018081121561166f57600080fd5b8935985060208a0135975060408a0135965060608a0135955061169460808b016115a3565b94506116a260a08b016115a3565b935060c08a0135925060a060df19820112156116bd57600080fd5b506116c66115f7565b60e08a013581526101008a013560208201526116e56101208b016115a3565b60408201526101408a013560608201526117026101608b016115a3565b6080820152809150509295985092959890939650565b6000815180845260005b8181101561173e57602081850181015186830182015201611722565b81811115611750576000602083870101525b50601f01601f19169290920160200192915050565b6020815260006115da6020830184611718565b60006020828403121561178a57600080fd5b5035919050565b600080600080600060a086880312156117a957600080fd5b8535945060208087013594506040870135935060608701359250608087013567ffffffffffffffff808211156117de57600080fd5b818901915089601f8301126117f257600080fd5b813581811115611804576118046115e1565b611816601f8201601f19168501611620565b91508082528a8482850101111561182c57600080fd5b80848401858401376000848284010152508093505050509295509295909350565b6000806040838503121561186057600080fd5b50508035926020909101359150565b60208082526027908201527f4c324f75747075744f7261636c653a2063616c6c6572206973206e6f74207468604082015266329037bbb732b960c91b606082015260800190565b634e487b7160e01b600052601160045260246000fd5b6000828210156118de576118de6118b6565b500390565b634e487b7160e01b600052603260045260246000fd5b6000821982111561190c5761190c6118b6565b500190565b60008261192e57634e487b7160e01b600052601260045260246000fd5b500490565b83815260606020820152600061194c6060830185611718565b828103604084015261195e8185611718565b9695505050505050565b6000816000190483118215151615611982576119826118b6565b50029056fea26469706673582212208dc273e9d7370a4a3a9910063406b0007f50fcc3b19a1675a6eff11e22e4fb8064736f6c634300080f0033",
}

// ZKL2OutputOracleABI is the input ABI used to generate the binding from.
// Deprecated: Use ZKL2OutputOracleMetaData.ABI instead.
var ZKL2OutputOracleABI = ZKL2OutputOracleMetaData.ABI

// ZKL2OutputOracleBin is the compiled bytecode used for deploying new contracts.
// Deprecated: Use ZKL2OutputOracleMetaData.Bin instead.
var ZKL2OutputOracleBin = ZKL2OutputOracleMetaData.Bin

// DeployZKL2OutputOracle deploys a new Ethereum contract, binding an instance of ZKL2OutputOracle to it.
func DeployZKL2OutputOracle(auth *bind.TransactOpts, backend bind.ContractBackend) (common.Address, *types.Transaction, *ZKL2OutputOracle, error) {
	parsed, err := ZKL2OutputOracleMetaData.GetAbi()
	if err != nil {
		return common.Address{}, nil, nil, err
	}
	if parsed == nil {
		return common.Address{}, nil, nil, errors.New("GetABI returned nil")
	}

	address, tx, contract, err := bind.DeployContract(auth, *parsed, common.FromHex(ZKL2OutputOracleBin), backend)
	if err != nil {
		return common.Address{}, nil, nil, err
	}
	return address, tx, &ZKL2OutputOracle{ZKL2OutputOracleCaller: ZKL2OutputOracleCaller{contract: contract}, ZKL2OutputOracleTransactor: ZKL2OutputOracleTransactor{contract: contract}, ZKL2OutputOracleFilterer: ZKL2OutputOracleFilterer{contract: contract}}, nil
}

// ZKL2OutputOracle is an auto generated Go binding around an Ethereum contract.
type ZKL2OutputOracle struct {
	ZKL2OutputOracleCaller     // Read-only binding to the contract
	ZKL2OutputOracleTransactor // Write-only binding to the contract
	ZKL2OutputOracleFilterer   // Log filterer for contract events
}

// ZKL2OutputOracleCaller is an auto generated read-only Go binding around an Ethereum contract.
type ZKL2OutputOracleCaller struct {
	contract *bind.BoundContract // Generic contract wrapper for the low level calls
}

// ZKL2OutputOracleTransactor is an auto generated write-only Go binding around an Ethereum contract.
type ZKL2OutputOracleTransactor struct {
	contract *bind.BoundContract // Generic contract wrapper for the low level calls
}

// ZKL2OutputOracleFilterer is an auto generated log filtering Go binding around an Ethereum contract events.
type ZKL2OutputOracleFilterer struct {
	contract *bind.BoundContract // Generic contract wrapper for the low level calls
}

// ZKL2OutputOracleSession is an auto generated Go binding around an Ethereum contract,
// with pre-set call and transact options.
type ZKL2OutputOracleSession struct {
	Contract     *ZKL2OutputOracle // Generic contract binding to set the session for
	CallOpts     bind.CallOpts     // Call options to use throughout this session
	TransactOpts bind.TransactOpts // Transaction auth options to use throughout this session
}

// ZKL2OutputOracleCallerSession is an auto generated read-only Go binding around an Ethereum contract,
// with pre-set call options.
type ZKL2OutputOracleCallerSession struct {
	Contract *ZKL2OutputOracleCaller // Generic contract caller binding to set the session for
	CallOpts bind.CallOpts           // Call options to use throughout this session
}

// ZKL2OutputOracleTransactorSession is an auto generated write-only Go binding around an Ethereum contract,
// with pre-set transact options.
type ZKL2OutputOracleTransactorSession struct {
	Contract     *ZKL2OutputOracleTransactor // Generic contract transactor binding to set the session for
	TransactOpts bind.TransactOpts           // Transaction auth options to use throughout this session
}

// ZKL2OutputOracleRaw is an auto generated low-level Go binding around an Ethereum contract.
type ZKL2OutputOracleRaw struct {
	Contract *ZKL2OutputOracle // Generic contract binding to access the raw methods on
}

// ZKL2OutputOracleCallerRaw is an auto generated low-level read-only Go binding around an Ethereum contract.
type ZKL2OutputOracleCallerRaw struct {
	Contract *ZKL2OutputOracleCaller // Generic read-only contract binding to access the raw methods on
}

// ZKL2OutputOracleTransactorRaw is an auto generated low-level write-only Go binding around an Ethereum contract.
type ZKL2OutputOracleTransactorRaw struct {
	Contract *ZKL2OutputOracleTransactor // Generic write-only contract binding to access the raw methods on
}

// NewZKL2OutputOracle creates a new instance of ZKL2OutputOracle, bound to a specific deployed contract.
func NewZKL2OutputOracle(address common.Address, backend bind.ContractBackend) (*ZKL2OutputOracle, error) {
	contract, err := bindZKL2OutputOracle(address, backend, backend, backend)
	if err != nil {
		return nil, err
	}
	return &ZKL2OutputOracle{ZKL2OutputOracleCaller: ZKL2OutputOracleCaller{contract: contract}, ZKL2OutputOracleTransactor: ZKL2OutputOracleTransactor{contract: contract}, ZKL2OutputOracleFilterer: ZKL2OutputOracleFilterer{contract: contract}}, nil
}

// NewZKL2OutputOracleCaller creates a new read-only instance of ZKL2OutputOracle, bound to a specific deployed contract.
func NewZKL2OutputOracleCaller(address common.Address, caller bind.ContractCaller) (*ZKL2OutputOracleCaller, error) {
	contract, err := bindZKL2OutputOracle(address, caller, nil, nil)
	if err != nil {
		return nil, err
	}
	return &ZKL2OutputOracleCaller{contract: contract}, nil
}

// NewZKL2OutputOracleTransactor creates a new write-only instance of ZKL2OutputOracle, bound to a specific deployed contract.
func NewZKL2OutputOracleTransactor(address common.Address, transactor bind.ContractTransactor) (*ZKL2OutputOracleTransactor, error) {
	contract, err := bindZKL2OutputOracle(address, nil, transactor, nil)
	if err != nil {
		return nil, err
	}
	return &ZKL2OutputOracleTransactor{contract: contract}, nil
}

// NewZKL2OutputOracleFilterer creates a new log filterer instance of ZKL2OutputOracle, bound to a specific deployed contract.
func NewZKL2OutputOracleFilterer(address common.Address, filterer bind.ContractFilterer) (*ZKL2OutputOracleFilterer, error) {
	contract, err := bindZKL2OutputOracle(address, nil, nil, filterer)
	if err != nil {
		return nil, err
	}
	return &ZKL2OutputOracleFilterer{contract: contract}, nil
}

// bindZKL2OutputOracle binds a generic wrapper to an already deployed contract.
func bindZKL2OutputOracle(address common.Address, caller bind.ContractCaller, transactor bind.ContractTransactor, filterer bind.ContractFilterer) (*bind.BoundContract, error) {
	parsed, err := ZKL2OutputOracleMetaData.GetAbi()
	if err != nil {
		return nil, err
	}
	return bind.NewBoundContract(address, *parsed, caller, transactor, filterer), nil
}

// Call invokes the (constant) contract method with params as input values and
// sets the output to result. The result type might be a single field for simple
// returns, a slice of interfaces for anonymous returns and a struct for named
// returns.
func (_ZKL2OutputOracle *ZKL2OutputOracleRaw) Call(opts *bind.CallOpts, result *[]interface{}, method string, params ...interface{}) error {
	return _ZKL2OutputOracle.Contract.ZKL2OutputOracleCaller.contract.Call(opts, result, method, params...)
}

// Transfer initiates a plain transaction to move funds to the contract, calling
// its default method if one is available.
func (_ZKL2OutputOracle *ZKL2OutputOracleRaw) Transfer(opts *bind.TransactOpts) (*types.Transaction, error) {
	return _ZKL2OutputOracle.Contract.ZKL2OutputOracleTransactor.contract.Transfer(opts)
}

// Transact invokes the (paid) contract method with params as input values.
func (_ZKL2OutputOracle *ZKL2OutputOracleRaw) Transact(opts *bind.TransactOpts, method string, params ...interface{}) (*types.Transaction, error) {
	return _ZKL2OutputOracle.Contract.ZKL2OutputOracleTransactor.contract.Transact(opts, method, params...)
}

// Call invokes the (constant) contract method with params as input values and
// sets the output to result. The result type might be a single field for simple
// returns, a slice of interfaces for anonymous returns and a struct for named
// returns.
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerRaw) Call(opts *bind.CallOpts, result *[]interface{}, method string, params ...interface{}) error {
	return _ZKL2OutputOracle.Contract.contract.Call(opts, result, method, params...)
}

// Transfer initiates a plain transaction to move funds to the contract, calling
// its default method if one is available.
func (_ZKL2OutputOracle *ZKL2OutputOracleTransactorRaw) Transfer(opts *bind.TransactOpts) (*types.Transaction, error) {
	return _ZKL2OutputOracle.Contract.contract.Transfer(opts)
}

// Transact invokes the (paid) contract method with params as input values.
func (_ZKL2OutputOracle *ZKL2OutputOracleTransactorRaw) Transact(opts *bind.TransactOpts, method string, params ...interface{}) (*types.Transaction, error) {
	return _ZKL2OutputOracle.Contract.contract.Transact(opts, method, params...)
}

// CHALLENGER is a free data retrieval call binding the contract method 0x6b4d98dd.
//
// Solidity: function CHALLENGER() view returns(address)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) CHALLENGER(opts *bind.CallOpts) (common.Address, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "CHALLENGER")

	if err != nil {
		return *new(common.Address), err
	}

	out0 := *abi.ConvertType(out[0], new(common.Address)).(*common.Address)

	return out0, err

}

// CHALLENGER is a free data retrieval call binding the contract method 0x6b4d98dd.
//
// Solidity: function CHALLENGER() view returns(address)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) CHALLENGER() (common.Address, error) {
	return _ZKL2OutputOracle.Contract.CHALLENGER(&_ZKL2OutputOracle.CallOpts)
}

// CHALLENGER is a free data retrieval call binding the contract method 0x6b4d98dd.
//
// Solidity: function CHALLENGER() view returns(address)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) CHALLENGER() (common.Address, error) {
	return _ZKL2OutputOracle.Contract.CHALLENGER(&_ZKL2OutputOracle.CallOpts)
}

// FINALIZATIONPERIODSECONDS is a free data retrieval call binding the contract method 0xf4daa291.
//
// Solidity: function FINALIZATION_PERIOD_SECONDS() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) FINALIZATIONPERIODSECONDS(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "FINALIZATION_PERIOD_SECONDS")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// FINALIZATIONPERIODSECONDS is a free data retrieval call binding the contract method 0xf4daa291.
//
// Solidity: function FINALIZATION_PERIOD_SECONDS() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) FINALIZATIONPERIODSECONDS() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.FINALIZATIONPERIODSECONDS(&_ZKL2OutputOracle.CallOpts)
}

// FINALIZATIONPERIODSECONDS is a free data retrieval call binding the contract method 0xf4daa291.
//
// Solidity: function FINALIZATION_PERIOD_SECONDS() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) FINALIZATIONPERIODSECONDS() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.FINALIZATIONPERIODSECONDS(&_ZKL2OutputOracle.CallOpts)
}

// L2BLOCKTIME is a free data retrieval call binding the contract method 0x002134cc.
//
// Solidity: function L2_BLOCK_TIME() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) L2BLOCKTIME(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "L2_BLOCK_TIME")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// L2BLOCKTIME is a free data retrieval call binding the contract method 0x002134cc.
//
// Solidity: function L2_BLOCK_TIME() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) L2BLOCKTIME() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.L2BLOCKTIME(&_ZKL2OutputOracle.CallOpts)
}

// L2BLOCKTIME is a free data retrieval call binding the contract method 0x002134cc.
//
// Solidity: function L2_BLOCK_TIME() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) L2BLOCKTIME() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.L2BLOCKTIME(&_ZKL2OutputOracle.CallOpts)
}

// PROPOSER is a free data retrieval call binding the contract method 0xbffa7f0f.
//
// Solidity: function PROPOSER() view returns(address)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) PROPOSER(opts *bind.CallOpts) (common.Address, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "PROPOSER")

	if err != nil {
		return *new(common.Address), err
	}

	out0 := *abi.ConvertType(out[0], new(common.Address)).(*common.Address)

	return out0, err

}

// PROPOSER is a free data retrieval call binding the contract method 0xbffa7f0f.
//
// Solidity: function PROPOSER() view returns(address)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) PROPOSER() (common.Address, error) {
	return _ZKL2OutputOracle.Contract.PROPOSER(&_ZKL2OutputOracle.CallOpts)
}

// PROPOSER is a free data retrieval call binding the contract method 0xbffa7f0f.
//
// Solidity: function PROPOSER() view returns(address)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) PROPOSER() (common.Address, error) {
	return _ZKL2OutputOracle.Contract.PROPOSER(&_ZKL2OutputOracle.CallOpts)
}

// SUBMISSIONINTERVAL is a free data retrieval call binding the contract method 0x529933df.
//
// Solidity: function SUBMISSION_INTERVAL() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) SUBMISSIONINTERVAL(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "SUBMISSION_INTERVAL")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// SUBMISSIONINTERVAL is a free data retrieval call binding the contract method 0x529933df.
//
// Solidity: function SUBMISSION_INTERVAL() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) SUBMISSIONINTERVAL() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.SUBMISSIONINTERVAL(&_ZKL2OutputOracle.CallOpts)
}

// SUBMISSIONINTERVAL is a free data retrieval call binding the contract method 0x529933df.
//
// Solidity: function SUBMISSION_INTERVAL() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) SUBMISSIONINTERVAL() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.SUBMISSIONINTERVAL(&_ZKL2OutputOracle.CallOpts)
}

// ChainId is a free data retrieval call binding the contract method 0x9a8a0592.
//
// Solidity: function chainId() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) ChainId(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "chainId")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// ChainId is a free data retrieval call binding the contract method 0x9a8a0592.
//
// Solidity: function chainId() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) ChainId() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.ChainId(&_ZKL2OutputOracle.CallOpts)
}

// ChainId is a free data retrieval call binding the contract method 0x9a8a0592.
//
// Solidity: function chainId() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) ChainId() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.ChainId(&_ZKL2OutputOracle.CallOpts)
}

// Challenger is a free data retrieval call binding the contract method 0x534db0e2.
//
// Solidity: function challenger() view returns(address)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) Challenger(opts *bind.CallOpts) (common.Address, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "challenger")

	if err != nil {
		return *new(common.Address), err
	}

	out0 := *abi.ConvertType(out[0], new(common.Address)).(*common.Address)

	return out0, err

}

// Challenger is a free data retrieval call binding the contract method 0x534db0e2.
//
// Solidity: function challenger() view returns(address)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) Challenger() (common.Address, error) {
	return _ZKL2OutputOracle.Contract.Challenger(&_ZKL2OutputOracle.CallOpts)
}

// Challenger is a free data retrieval call binding the contract method 0x534db0e2.
//
// Solidity: function challenger() view returns(address)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) Challenger() (common.Address, error) {
	return _ZKL2OutputOracle.Contract.Challenger(&_ZKL2OutputOracle.CallOpts)
}

// ComputeL2Timestamp is a free data retrieval call binding the contract method 0xd1de856c.
//
// Solidity: function computeL2Timestamp(uint256 _l2BlockNumber) view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) ComputeL2Timestamp(opts *bind.CallOpts, _l2BlockNumber *big.Int) (*big.Int, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "computeL2Timestamp", _l2BlockNumber)

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// ComputeL2Timestamp is a free data retrieval call binding the contract method 0xd1de856c.
//
// Solidity: function computeL2Timestamp(uint256 _l2BlockNumber) view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) ComputeL2Timestamp(_l2BlockNumber *big.Int) (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.ComputeL2Timestamp(&_ZKL2OutputOracle.CallOpts, _l2BlockNumber)
}

// ComputeL2Timestamp is a free data retrieval call binding the contract method 0xd1de856c.
//
// Solidity: function computeL2Timestamp(uint256 _l2BlockNumber) view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) ComputeL2Timestamp(_l2BlockNumber *big.Int) (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.ComputeL2Timestamp(&_ZKL2OutputOracle.CallOpts, _l2BlockNumber)
}

// FinalizationPeriodSeconds is a free data retrieval call binding the contract method 0xce5db8d6.
//
// Solidity: function finalizationPeriodSeconds() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) FinalizationPeriodSeconds(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "finalizationPeriodSeconds")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// FinalizationPeriodSeconds is a free data retrieval call binding the contract method 0xce5db8d6.
//
// Solidity: function finalizationPeriodSeconds() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) FinalizationPeriodSeconds() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.FinalizationPeriodSeconds(&_ZKL2OutputOracle.CallOpts)
}

// FinalizationPeriodSeconds is a free data retrieval call binding the contract method 0xce5db8d6.
//
// Solidity: function finalizationPeriodSeconds() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) FinalizationPeriodSeconds() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.FinalizationPeriodSeconds(&_ZKL2OutputOracle.CallOpts)
}

// GetL2Output is a free data retrieval call binding the contract method 0xa25ae557.
//
// Solidity: function getL2Output(uint256 _l2OutputIndex) view returns((bytes32,uint128,uint128))
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) GetL2Output(opts *bind.CallOpts, _l2OutputIndex *big.Int) (TypesOutputProposal, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "getL2Output", _l2OutputIndex)

	if err != nil {
		return *new(TypesOutputProposal), err
	}

	out0 := *abi.ConvertType(out[0], new(TypesOutputProposal)).(*TypesOutputProposal)

	return out0, err

}

// GetL2Output is a free data retrieval call binding the contract method 0xa25ae557.
//
// Solidity: function getL2Output(uint256 _l2OutputIndex) view returns((bytes32,uint128,uint128))
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) GetL2Output(_l2OutputIndex *big.Int) (TypesOutputProposal, error) {
	return _ZKL2OutputOracle.Contract.GetL2Output(&_ZKL2OutputOracle.CallOpts, _l2OutputIndex)
}

// GetL2Output is a free data retrieval call binding the contract method 0xa25ae557.
//
// Solidity: function getL2Output(uint256 _l2OutputIndex) view returns((bytes32,uint128,uint128))
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) GetL2Output(_l2OutputIndex *big.Int) (TypesOutputProposal, error) {
	return _ZKL2OutputOracle.Contract.GetL2Output(&_ZKL2OutputOracle.CallOpts, _l2OutputIndex)
}

// GetL2OutputAfter is a free data retrieval call binding the contract method 0xcf8e5cf0.
//
// Solidity: function getL2OutputAfter(uint256 _l2BlockNumber) view returns((bytes32,uint128,uint128))
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) GetL2OutputAfter(opts *bind.CallOpts, _l2BlockNumber *big.Int) (TypesOutputProposal, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "getL2OutputAfter", _l2BlockNumber)

	if err != nil {
		return *new(TypesOutputProposal), err
	}

	out0 := *abi.ConvertType(out[0], new(TypesOutputProposal)).(*TypesOutputProposal)

	return out0, err

}

// GetL2OutputAfter is a free data retrieval call binding the contract method 0xcf8e5cf0.
//
// Solidity: function getL2OutputAfter(uint256 _l2BlockNumber) view returns((bytes32,uint128,uint128))
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) GetL2OutputAfter(_l2BlockNumber *big.Int) (TypesOutputProposal, error) {
	return _ZKL2OutputOracle.Contract.GetL2OutputAfter(&_ZKL2OutputOracle.CallOpts, _l2BlockNumber)
}

// GetL2OutputAfter is a free data retrieval call binding the contract method 0xcf8e5cf0.
//
// Solidity: function getL2OutputAfter(uint256 _l2BlockNumber) view returns((bytes32,uint128,uint128))
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) GetL2OutputAfter(_l2BlockNumber *big.Int) (TypesOutputProposal, error) {
	return _ZKL2OutputOracle.Contract.GetL2OutputAfter(&_ZKL2OutputOracle.CallOpts, _l2BlockNumber)
}

// GetL2OutputIndexAfter is a free data retrieval call binding the contract method 0x7f006420.
//
// Solidity: function getL2OutputIndexAfter(uint256 _l2BlockNumber) view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) GetL2OutputIndexAfter(opts *bind.CallOpts, _l2BlockNumber *big.Int) (*big.Int, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "getL2OutputIndexAfter", _l2BlockNumber)

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// GetL2OutputIndexAfter is a free data retrieval call binding the contract method 0x7f006420.
//
// Solidity: function getL2OutputIndexAfter(uint256 _l2BlockNumber) view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) GetL2OutputIndexAfter(_l2BlockNumber *big.Int) (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.GetL2OutputIndexAfter(&_ZKL2OutputOracle.CallOpts, _l2BlockNumber)
}

// GetL2OutputIndexAfter is a free data retrieval call binding the contract method 0x7f006420.
//
// Solidity: function getL2OutputIndexAfter(uint256 _l2BlockNumber) view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) GetL2OutputIndexAfter(_l2BlockNumber *big.Int) (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.GetL2OutputIndexAfter(&_ZKL2OutputOracle.CallOpts, _l2BlockNumber)
}

// HistoricBlockHashes is a free data retrieval call binding the contract method 0xa196b525.
//
// Solidity: function historicBlockHashes(uint256 ) view returns(bytes32)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) HistoricBlockHashes(opts *bind.CallOpts, arg0 *big.Int) ([32]byte, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "historicBlockHashes", arg0)

	if err != nil {
		return *new([32]byte), err
	}

	out0 := *abi.ConvertType(out[0], new([32]byte)).(*[32]byte)

	return out0, err

}

// HistoricBlockHashes is a free data retrieval call binding the contract method 0xa196b525.
//
// Solidity: function historicBlockHashes(uint256 ) view returns(bytes32)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) HistoricBlockHashes(arg0 *big.Int) ([32]byte, error) {
	return _ZKL2OutputOracle.Contract.HistoricBlockHashes(&_ZKL2OutputOracle.CallOpts, arg0)
}

// HistoricBlockHashes is a free data retrieval call binding the contract method 0xa196b525.
//
// Solidity: function historicBlockHashes(uint256 ) view returns(bytes32)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) HistoricBlockHashes(arg0 *big.Int) ([32]byte, error) {
	return _ZKL2OutputOracle.Contract.HistoricBlockHashes(&_ZKL2OutputOracle.CallOpts, arg0)
}

// L2BlockTime is a free data retrieval call binding the contract method 0x93991af3.
//
// Solidity: function l2BlockTime() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) L2BlockTime(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "l2BlockTime")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// L2BlockTime is a free data retrieval call binding the contract method 0x93991af3.
//
// Solidity: function l2BlockTime() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) L2BlockTime() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.L2BlockTime(&_ZKL2OutputOracle.CallOpts)
}

// L2BlockTime is a free data retrieval call binding the contract method 0x93991af3.
//
// Solidity: function l2BlockTime() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) L2BlockTime() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.L2BlockTime(&_ZKL2OutputOracle.CallOpts)
}

// LatestBlockNumber is a free data retrieval call binding the contract method 0x4599c788.
//
// Solidity: function latestBlockNumber() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) LatestBlockNumber(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "latestBlockNumber")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// LatestBlockNumber is a free data retrieval call binding the contract method 0x4599c788.
//
// Solidity: function latestBlockNumber() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) LatestBlockNumber() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.LatestBlockNumber(&_ZKL2OutputOracle.CallOpts)
}

// LatestBlockNumber is a free data retrieval call binding the contract method 0x4599c788.
//
// Solidity: function latestBlockNumber() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) LatestBlockNumber() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.LatestBlockNumber(&_ZKL2OutputOracle.CallOpts)
}

// LatestOutputIndex is a free data retrieval call binding the contract method 0x69f16eec.
//
// Solidity: function latestOutputIndex() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) LatestOutputIndex(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "latestOutputIndex")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// LatestOutputIndex is a free data retrieval call binding the contract method 0x69f16eec.
//
// Solidity: function latestOutputIndex() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) LatestOutputIndex() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.LatestOutputIndex(&_ZKL2OutputOracle.CallOpts)
}

// LatestOutputIndex is a free data retrieval call binding the contract method 0x69f16eec.
//
// Solidity: function latestOutputIndex() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) LatestOutputIndex() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.LatestOutputIndex(&_ZKL2OutputOracle.CallOpts)
}

// NextBlockNumber is a free data retrieval call binding the contract method 0xdcec3348.
//
// Solidity: function nextBlockNumber() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) NextBlockNumber(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "nextBlockNumber")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// NextBlockNumber is a free data retrieval call binding the contract method 0xdcec3348.
//
// Solidity: function nextBlockNumber() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) NextBlockNumber() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.NextBlockNumber(&_ZKL2OutputOracle.CallOpts)
}

// NextBlockNumber is a free data retrieval call binding the contract method 0xdcec3348.
//
// Solidity: function nextBlockNumber() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) NextBlockNumber() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.NextBlockNumber(&_ZKL2OutputOracle.CallOpts)
}

// NextOutputIndex is a free data retrieval call binding the contract method 0x6abcf563.
//
// Solidity: function nextOutputIndex() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) NextOutputIndex(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "nextOutputIndex")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// NextOutputIndex is a free data retrieval call binding the contract method 0x6abcf563.
//
// Solidity: function nextOutputIndex() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) NextOutputIndex() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.NextOutputIndex(&_ZKL2OutputOracle.CallOpts)
}

// NextOutputIndex is a free data retrieval call binding the contract method 0x6abcf563.
//
// Solidity: function nextOutputIndex() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) NextOutputIndex() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.NextOutputIndex(&_ZKL2OutputOracle.CallOpts)
}

// Owner is a free data retrieval call binding the contract method 0x8da5cb5b.
//
// Solidity: function owner() view returns(address)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) Owner(opts *bind.CallOpts) (common.Address, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "owner")

	if err != nil {
		return *new(common.Address), err
	}

	out0 := *abi.ConvertType(out[0], new(common.Address)).(*common.Address)

	return out0, err

}

// Owner is a free data retrieval call binding the contract method 0x8da5cb5b.
//
// Solidity: function owner() view returns(address)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) Owner() (common.Address, error) {
	return _ZKL2OutputOracle.Contract.Owner(&_ZKL2OutputOracle.CallOpts)
}

// Owner is a free data retrieval call binding the contract method 0x8da5cb5b.
//
// Solidity: function owner() view returns(address)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) Owner() (common.Address, error) {
	return _ZKL2OutputOracle.Contract.Owner(&_ZKL2OutputOracle.CallOpts)
}

// Proposer is a free data retrieval call binding the contract method 0xa8e4fb90.
//
// Solidity: function proposer() view returns(address)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) Proposer(opts *bind.CallOpts) (common.Address, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "proposer")

	if err != nil {
		return *new(common.Address), err
	}

	out0 := *abi.ConvertType(out[0], new(common.Address)).(*common.Address)

	return out0, err

}

// Proposer is a free data retrieval call binding the contract method 0xa8e4fb90.
//
// Solidity: function proposer() view returns(address)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) Proposer() (common.Address, error) {
	return _ZKL2OutputOracle.Contract.Proposer(&_ZKL2OutputOracle.CallOpts)
}

// Proposer is a free data retrieval call binding the contract method 0xa8e4fb90.
//
// Solidity: function proposer() view returns(address)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) Proposer() (common.Address, error) {
	return _ZKL2OutputOracle.Contract.Proposer(&_ZKL2OutputOracle.CallOpts)
}

// StartingBlockNumber is a free data retrieval call binding the contract method 0x70872aa5.
//
// Solidity: function startingBlockNumber() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) StartingBlockNumber(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "startingBlockNumber")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// StartingBlockNumber is a free data retrieval call binding the contract method 0x70872aa5.
//
// Solidity: function startingBlockNumber() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) StartingBlockNumber() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.StartingBlockNumber(&_ZKL2OutputOracle.CallOpts)
}

// StartingBlockNumber is a free data retrieval call binding the contract method 0x70872aa5.
//
// Solidity: function startingBlockNumber() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) StartingBlockNumber() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.StartingBlockNumber(&_ZKL2OutputOracle.CallOpts)
}

// StartingTimestamp is a free data retrieval call binding the contract method 0x88786272.
//
// Solidity: function startingTimestamp() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) StartingTimestamp(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "startingTimestamp")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// StartingTimestamp is a free data retrieval call binding the contract method 0x88786272.
//
// Solidity: function startingTimestamp() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) StartingTimestamp() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.StartingTimestamp(&_ZKL2OutputOracle.CallOpts)
}

// StartingTimestamp is a free data retrieval call binding the contract method 0x88786272.
//
// Solidity: function startingTimestamp() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) StartingTimestamp() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.StartingTimestamp(&_ZKL2OutputOracle.CallOpts)
}

// SubmissionInterval is a free data retrieval call binding the contract method 0xe1a41bcf.
//
// Solidity: function submissionInterval() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) SubmissionInterval(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "submissionInterval")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// SubmissionInterval is a free data retrieval call binding the contract method 0xe1a41bcf.
//
// Solidity: function submissionInterval() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) SubmissionInterval() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.SubmissionInterval(&_ZKL2OutputOracle.CallOpts)
}

// SubmissionInterval is a free data retrieval call binding the contract method 0xe1a41bcf.
//
// Solidity: function submissionInterval() view returns(uint256)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) SubmissionInterval() (*big.Int, error) {
	return _ZKL2OutputOracle.Contract.SubmissionInterval(&_ZKL2OutputOracle.CallOpts)
}

// VerifierGateway is a free data retrieval call binding the contract method 0xfb3c491c.
//
// Solidity: function verifierGateway() view returns(address)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) VerifierGateway(opts *bind.CallOpts) (common.Address, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "verifierGateway")

	if err != nil {
		return *new(common.Address), err
	}

	out0 := *abi.ConvertType(out[0], new(common.Address)).(*common.Address)

	return out0, err

}

// VerifierGateway is a free data retrieval call binding the contract method 0xfb3c491c.
//
// Solidity: function verifierGateway() view returns(address)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) VerifierGateway() (common.Address, error) {
	return _ZKL2OutputOracle.Contract.VerifierGateway(&_ZKL2OutputOracle.CallOpts)
}

// VerifierGateway is a free data retrieval call binding the contract method 0xfb3c491c.
//
// Solidity: function verifierGateway() view returns(address)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) VerifierGateway() (common.Address, error) {
	return _ZKL2OutputOracle.Contract.VerifierGateway(&_ZKL2OutputOracle.CallOpts)
}

// Version is a free data retrieval call binding the contract method 0x54fd4d50.
//
// Solidity: function version() view returns(string)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) Version(opts *bind.CallOpts) (string, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "version")

	if err != nil {
		return *new(string), err
	}

	out0 := *abi.ConvertType(out[0], new(string)).(*string)

	return out0, err

}

// Version is a free data retrieval call binding the contract method 0x54fd4d50.
//
// Solidity: function version() view returns(string)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) Version() (string, error) {
	return _ZKL2OutputOracle.Contract.Version(&_ZKL2OutputOracle.CallOpts)
}

// Version is a free data retrieval call binding the contract method 0x54fd4d50.
//
// Solidity: function version() view returns(string)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) Version() (string, error) {
	return _ZKL2OutputOracle.Contract.Version(&_ZKL2OutputOracle.CallOpts)
}

// Vkey is a free data retrieval call binding the contract method 0xc69b0eb1.
//
// Solidity: function vkey() view returns(bytes32)
func (_ZKL2OutputOracle *ZKL2OutputOracleCaller) Vkey(opts *bind.CallOpts) ([32]byte, error) {
	var out []interface{}
	err := _ZKL2OutputOracle.contract.Call(opts, &out, "vkey")

	if err != nil {
		return *new([32]byte), err
	}

	out0 := *abi.ConvertType(out[0], new([32]byte)).(*[32]byte)

	return out0, err

}

// Vkey is a free data retrieval call binding the contract method 0xc69b0eb1.
//
// Solidity: function vkey() view returns(bytes32)
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) Vkey() ([32]byte, error) {
	return _ZKL2OutputOracle.Contract.Vkey(&_ZKL2OutputOracle.CallOpts)
}

// Vkey is a free data retrieval call binding the contract method 0xc69b0eb1.
//
// Solidity: function vkey() view returns(bytes32)
func (_ZKL2OutputOracle *ZKL2OutputOracleCallerSession) Vkey() ([32]byte, error) {
	return _ZKL2OutputOracle.Contract.Vkey(&_ZKL2OutputOracle.CallOpts)
}

// CheckpointBlockHash is a paid mutator transaction binding the contract method 0xdbf33074.
//
// Solidity: function checkpointBlockHash(uint256 _blockNumber, bytes32 _blockHash) returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleTransactor) CheckpointBlockHash(opts *bind.TransactOpts, _blockNumber *big.Int, _blockHash [32]byte) (*types.Transaction, error) {
	return _ZKL2OutputOracle.contract.Transact(opts, "checkpointBlockHash", _blockNumber, _blockHash)
}

// CheckpointBlockHash is a paid mutator transaction binding the contract method 0xdbf33074.
//
// Solidity: function checkpointBlockHash(uint256 _blockNumber, bytes32 _blockHash) returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) CheckpointBlockHash(_blockNumber *big.Int, _blockHash [32]byte) (*types.Transaction, error) {
	return _ZKL2OutputOracle.Contract.CheckpointBlockHash(&_ZKL2OutputOracle.TransactOpts, _blockNumber, _blockHash)
}

// CheckpointBlockHash is a paid mutator transaction binding the contract method 0xdbf33074.
//
// Solidity: function checkpointBlockHash(uint256 _blockNumber, bytes32 _blockHash) returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleTransactorSession) CheckpointBlockHash(_blockNumber *big.Int, _blockHash [32]byte) (*types.Transaction, error) {
	return _ZKL2OutputOracle.Contract.CheckpointBlockHash(&_ZKL2OutputOracle.TransactOpts, _blockNumber, _blockHash)
}

// DeleteL2Outputs is a paid mutator transaction binding the contract method 0x89c44cbb.
//
// Solidity: function deleteL2Outputs(uint256 _l2OutputIndex) returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleTransactor) DeleteL2Outputs(opts *bind.TransactOpts, _l2OutputIndex *big.Int) (*types.Transaction, error) {
	return _ZKL2OutputOracle.contract.Transact(opts, "deleteL2Outputs", _l2OutputIndex)
}

// DeleteL2Outputs is a paid mutator transaction binding the contract method 0x89c44cbb.
//
// Solidity: function deleteL2Outputs(uint256 _l2OutputIndex) returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) DeleteL2Outputs(_l2OutputIndex *big.Int) (*types.Transaction, error) {
	return _ZKL2OutputOracle.Contract.DeleteL2Outputs(&_ZKL2OutputOracle.TransactOpts, _l2OutputIndex)
}

// DeleteL2Outputs is a paid mutator transaction binding the contract method 0x89c44cbb.
//
// Solidity: function deleteL2Outputs(uint256 _l2OutputIndex) returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleTransactorSession) DeleteL2Outputs(_l2OutputIndex *big.Int) (*types.Transaction, error) {
	return _ZKL2OutputOracle.Contract.DeleteL2Outputs(&_ZKL2OutputOracle.TransactOpts, _l2OutputIndex)
}

// Initialize is a paid mutator transaction binding the contract method 0x4600df38.
//
// Solidity: function initialize(uint256 _submissionInterval, uint256 _l2BlockTime, uint256 _startingBlockNumber, uint256 _startingTimestamp, address _proposer, address _challenger, uint256 _finalizationPeriodSeconds, (uint256,bytes32,address,bytes32,address) _zkInitParams) returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleTransactor) Initialize(opts *bind.TransactOpts, _submissionInterval *big.Int, _l2BlockTime *big.Int, _startingBlockNumber *big.Int, _startingTimestamp *big.Int, _proposer common.Address, _challenger common.Address, _finalizationPeriodSeconds *big.Int, _zkInitParams ZKL2OutputOracleZKInitParams) (*types.Transaction, error) {
	return _ZKL2OutputOracle.contract.Transact(opts, "initialize", _submissionInterval, _l2BlockTime, _startingBlockNumber, _startingTimestamp, _proposer, _challenger, _finalizationPeriodSeconds, _zkInitParams)
}

// Initialize is a paid mutator transaction binding the contract method 0x4600df38.
//
// Solidity: function initialize(uint256 _submissionInterval, uint256 _l2BlockTime, uint256 _startingBlockNumber, uint256 _startingTimestamp, address _proposer, address _challenger, uint256 _finalizationPeriodSeconds, (uint256,bytes32,address,bytes32,address) _zkInitParams) returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) Initialize(_submissionInterval *big.Int, _l2BlockTime *big.Int, _startingBlockNumber *big.Int, _startingTimestamp *big.Int, _proposer common.Address, _challenger common.Address, _finalizationPeriodSeconds *big.Int, _zkInitParams ZKL2OutputOracleZKInitParams) (*types.Transaction, error) {
	return _ZKL2OutputOracle.Contract.Initialize(&_ZKL2OutputOracle.TransactOpts, _submissionInterval, _l2BlockTime, _startingBlockNumber, _startingTimestamp, _proposer, _challenger, _finalizationPeriodSeconds, _zkInitParams)
}

// Initialize is a paid mutator transaction binding the contract method 0x4600df38.
//
// Solidity: function initialize(uint256 _submissionInterval, uint256 _l2BlockTime, uint256 _startingBlockNumber, uint256 _startingTimestamp, address _proposer, address _challenger, uint256 _finalizationPeriodSeconds, (uint256,bytes32,address,bytes32,address) _zkInitParams) returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleTransactorSession) Initialize(_submissionInterval *big.Int, _l2BlockTime *big.Int, _startingBlockNumber *big.Int, _startingTimestamp *big.Int, _proposer common.Address, _challenger common.Address, _finalizationPeriodSeconds *big.Int, _zkInitParams ZKL2OutputOracleZKInitParams) (*types.Transaction, error) {
	return _ZKL2OutputOracle.Contract.Initialize(&_ZKL2OutputOracle.TransactOpts, _submissionInterval, _l2BlockTime, _startingBlockNumber, _startingTimestamp, _proposer, _challenger, _finalizationPeriodSeconds, _zkInitParams)
}

// ProposeL2Output is a paid mutator transaction binding the contract method 0xa9efd6b8.
//
// Solidity: function proposeL2Output(bytes32 _outputRoot, uint256 _l2BlockNumber, bytes32 _l1BlockHash, uint256 _l1BlockNumber, bytes _proof) payable returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleTransactor) ProposeL2Output(opts *bind.TransactOpts, _outputRoot [32]byte, _l2BlockNumber *big.Int, _l1BlockHash [32]byte, _l1BlockNumber *big.Int, _proof []byte) (*types.Transaction, error) {
	return _ZKL2OutputOracle.contract.Transact(opts, "proposeL2Output", _outputRoot, _l2BlockNumber, _l1BlockHash, _l1BlockNumber, _proof)
}

// ProposeL2Output is a paid mutator transaction binding the contract method 0xa9efd6b8.
//
// Solidity: function proposeL2Output(bytes32 _outputRoot, uint256 _l2BlockNumber, bytes32 _l1BlockHash, uint256 _l1BlockNumber, bytes _proof) payable returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) ProposeL2Output(_outputRoot [32]byte, _l2BlockNumber *big.Int, _l1BlockHash [32]byte, _l1BlockNumber *big.Int, _proof []byte) (*types.Transaction, error) {
	return _ZKL2OutputOracle.Contract.ProposeL2Output(&_ZKL2OutputOracle.TransactOpts, _outputRoot, _l2BlockNumber, _l1BlockHash, _l1BlockNumber, _proof)
}

// ProposeL2Output is a paid mutator transaction binding the contract method 0xa9efd6b8.
//
// Solidity: function proposeL2Output(bytes32 _outputRoot, uint256 _l2BlockNumber, bytes32 _l1BlockHash, uint256 _l1BlockNumber, bytes _proof) payable returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleTransactorSession) ProposeL2Output(_outputRoot [32]byte, _l2BlockNumber *big.Int, _l1BlockHash [32]byte, _l1BlockNumber *big.Int, _proof []byte) (*types.Transaction, error) {
	return _ZKL2OutputOracle.Contract.ProposeL2Output(&_ZKL2OutputOracle.TransactOpts, _outputRoot, _l2BlockNumber, _l1BlockHash, _l1BlockNumber, _proof)
}

// TransferOwnership is a paid mutator transaction binding the contract method 0xf2fde38b.
//
// Solidity: function transferOwnership(address _newOwner) returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleTransactor) TransferOwnership(opts *bind.TransactOpts, _newOwner common.Address) (*types.Transaction, error) {
	return _ZKL2OutputOracle.contract.Transact(opts, "transferOwnership", _newOwner)
}

// TransferOwnership is a paid mutator transaction binding the contract method 0xf2fde38b.
//
// Solidity: function transferOwnership(address _newOwner) returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) TransferOwnership(_newOwner common.Address) (*types.Transaction, error) {
	return _ZKL2OutputOracle.Contract.TransferOwnership(&_ZKL2OutputOracle.TransactOpts, _newOwner)
}

// TransferOwnership is a paid mutator transaction binding the contract method 0xf2fde38b.
//
// Solidity: function transferOwnership(address _newOwner) returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleTransactorSession) TransferOwnership(_newOwner common.Address) (*types.Transaction, error) {
	return _ZKL2OutputOracle.Contract.TransferOwnership(&_ZKL2OutputOracle.TransactOpts, _newOwner)
}

// UpdateVKey is a paid mutator transaction binding the contract method 0xd946a456.
//
// Solidity: function updateVKey(bytes32 _vkey) returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleTransactor) UpdateVKey(opts *bind.TransactOpts, _vkey [32]byte) (*types.Transaction, error) {
	return _ZKL2OutputOracle.contract.Transact(opts, "updateVKey", _vkey)
}

// UpdateVKey is a paid mutator transaction binding the contract method 0xd946a456.
//
// Solidity: function updateVKey(bytes32 _vkey) returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) UpdateVKey(_vkey [32]byte) (*types.Transaction, error) {
	return _ZKL2OutputOracle.Contract.UpdateVKey(&_ZKL2OutputOracle.TransactOpts, _vkey)
}

// UpdateVKey is a paid mutator transaction binding the contract method 0xd946a456.
//
// Solidity: function updateVKey(bytes32 _vkey) returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleTransactorSession) UpdateVKey(_vkey [32]byte) (*types.Transaction, error) {
	return _ZKL2OutputOracle.Contract.UpdateVKey(&_ZKL2OutputOracle.TransactOpts, _vkey)
}

// UpdateVerifierGateway is a paid mutator transaction binding the contract method 0x4418db5e.
//
// Solidity: function updateVerifierGateway(address _verifierGateway) returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleTransactor) UpdateVerifierGateway(opts *bind.TransactOpts, _verifierGateway common.Address) (*types.Transaction, error) {
	return _ZKL2OutputOracle.contract.Transact(opts, "updateVerifierGateway", _verifierGateway)
}

// UpdateVerifierGateway is a paid mutator transaction binding the contract method 0x4418db5e.
//
// Solidity: function updateVerifierGateway(address _verifierGateway) returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleSession) UpdateVerifierGateway(_verifierGateway common.Address) (*types.Transaction, error) {
	return _ZKL2OutputOracle.Contract.UpdateVerifierGateway(&_ZKL2OutputOracle.TransactOpts, _verifierGateway)
}

// UpdateVerifierGateway is a paid mutator transaction binding the contract method 0x4418db5e.
//
// Solidity: function updateVerifierGateway(address _verifierGateway) returns()
func (_ZKL2OutputOracle *ZKL2OutputOracleTransactorSession) UpdateVerifierGateway(_verifierGateway common.Address) (*types.Transaction, error) {
	return _ZKL2OutputOracle.Contract.UpdateVerifierGateway(&_ZKL2OutputOracle.TransactOpts, _verifierGateway)
}

// ZKL2OutputOracleInitializedIterator is returned from FilterInitialized and is used to iterate over the raw logs and unpacked data for Initialized events raised by the ZKL2OutputOracle contract.
type ZKL2OutputOracleInitializedIterator struct {
	Event *ZKL2OutputOracleInitialized // Event containing the contract specifics and raw log

	contract *bind.BoundContract // Generic contract to use for unpacking event data
	event    string              // Event name to use for unpacking event data

	logs chan types.Log        // Log channel receiving the found contract events
	sub  ethereum.Subscription // Subscription for errors, completion and termination
	done bool                  // Whether the subscription completed delivering logs
	fail error                 // Occurred error to stop iteration
}

// Next advances the iterator to the subsequent event, returning whether there
// are any more events found. In case of a retrieval or parsing error, false is
// returned and Error() can be queried for the exact failure.
func (it *ZKL2OutputOracleInitializedIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(ZKL2OutputOracleInitialized)
			if err := it.contract.UnpackLog(it.Event, it.event, log); err != nil {
				it.fail = err
				return false
			}
			it.Event.Raw = log
			return true

		default:
			return false
		}
	}
	// Iterator still in progress, wait for either a data or an error event
	select {
	case log := <-it.logs:
		it.Event = new(ZKL2OutputOracleInitialized)
		if err := it.contract.UnpackLog(it.Event, it.event, log); err != nil {
			it.fail = err
			return false
		}
		it.Event.Raw = log
		return true

	case err := <-it.sub.Err():
		it.done = true
		it.fail = err
		return it.Next()
	}
}

// Error returns any retrieval or parsing error occurred during filtering.
func (it *ZKL2OutputOracleInitializedIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *ZKL2OutputOracleInitializedIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// ZKL2OutputOracleInitialized represents a Initialized event raised by the ZKL2OutputOracle contract.
type ZKL2OutputOracleInitialized struct {
	Version uint8
	Raw     types.Log // Blockchain specific contextual infos
}

// FilterInitialized is a free log retrieval operation binding the contract event 0x7f26b83ff96e1f2b6a682f133852f6798a09c465da95921460cefb3847402498.
//
// Solidity: event Initialized(uint8 version)
func (_ZKL2OutputOracle *ZKL2OutputOracleFilterer) FilterInitialized(opts *bind.FilterOpts) (*ZKL2OutputOracleInitializedIterator, error) {

	logs, sub, err := _ZKL2OutputOracle.contract.FilterLogs(opts, "Initialized")
	if err != nil {
		return nil, err
	}
	return &ZKL2OutputOracleInitializedIterator{contract: _ZKL2OutputOracle.contract, event: "Initialized", logs: logs, sub: sub}, nil
}

// WatchInitialized is a free log subscription operation binding the contract event 0x7f26b83ff96e1f2b6a682f133852f6798a09c465da95921460cefb3847402498.
//
// Solidity: event Initialized(uint8 version)
func (_ZKL2OutputOracle *ZKL2OutputOracleFilterer) WatchInitialized(opts *bind.WatchOpts, sink chan<- *ZKL2OutputOracleInitialized) (event.Subscription, error) {

	logs, sub, err := _ZKL2OutputOracle.contract.WatchLogs(opts, "Initialized")
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(ZKL2OutputOracleInitialized)
				if err := _ZKL2OutputOracle.contract.UnpackLog(event, "Initialized", log); err != nil {
					return err
				}
				event.Raw = log

				select {
				case sink <- event:
				case err := <-sub.Err():
					return err
				case <-quit:
					return nil
				}
			case err := <-sub.Err():
				return err
			case <-quit:
				return nil
			}
		}
	}), nil
}

// ParseInitialized is a log parse operation binding the contract event 0x7f26b83ff96e1f2b6a682f133852f6798a09c465da95921460cefb3847402498.
//
// Solidity: event Initialized(uint8 version)
func (_ZKL2OutputOracle *ZKL2OutputOracleFilterer) ParseInitialized(log types.Log) (*ZKL2OutputOracleInitialized, error) {
	event := new(ZKL2OutputOracleInitialized)
	if err := _ZKL2OutputOracle.contract.UnpackLog(event, "Initialized", log); err != nil {
		return nil, err
	}
	event.Raw = log
	return event, nil
}

// ZKL2OutputOracleOutputProposedIterator is returned from FilterOutputProposed and is used to iterate over the raw logs and unpacked data for OutputProposed events raised by the ZKL2OutputOracle contract.
type ZKL2OutputOracleOutputProposedIterator struct {
	Event *ZKL2OutputOracleOutputProposed // Event containing the contract specifics and raw log

	contract *bind.BoundContract // Generic contract to use for unpacking event data
	event    string              // Event name to use for unpacking event data

	logs chan types.Log        // Log channel receiving the found contract events
	sub  ethereum.Subscription // Subscription for errors, completion and termination
	done bool                  // Whether the subscription completed delivering logs
	fail error                 // Occurred error to stop iteration
}

// Next advances the iterator to the subsequent event, returning whether there
// are any more events found. In case of a retrieval or parsing error, false is
// returned and Error() can be queried for the exact failure.
func (it *ZKL2OutputOracleOutputProposedIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(ZKL2OutputOracleOutputProposed)
			if err := it.contract.UnpackLog(it.Event, it.event, log); err != nil {
				it.fail = err
				return false
			}
			it.Event.Raw = log
			return true

		default:
			return false
		}
	}
	// Iterator still in progress, wait for either a data or an error event
	select {
	case log := <-it.logs:
		it.Event = new(ZKL2OutputOracleOutputProposed)
		if err := it.contract.UnpackLog(it.Event, it.event, log); err != nil {
			it.fail = err
			return false
		}
		it.Event.Raw = log
		return true

	case err := <-it.sub.Err():
		it.done = true
		it.fail = err
		return it.Next()
	}
}

// Error returns any retrieval or parsing error occurred during filtering.
func (it *ZKL2OutputOracleOutputProposedIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *ZKL2OutputOracleOutputProposedIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// ZKL2OutputOracleOutputProposed represents a OutputProposed event raised by the ZKL2OutputOracle contract.
type ZKL2OutputOracleOutputProposed struct {
	OutputRoot    [32]byte
	L2OutputIndex *big.Int
	L2BlockNumber *big.Int
	L1Timestamp   *big.Int
	Raw           types.Log // Blockchain specific contextual infos
}

// FilterOutputProposed is a free log retrieval operation binding the contract event 0xa7aaf2512769da4e444e3de247be2564225c2e7a8f74cfe528e46e17d24868e2.
//
// Solidity: event OutputProposed(bytes32 indexed outputRoot, uint256 indexed l2OutputIndex, uint256 indexed l2BlockNumber, uint256 l1Timestamp)
func (_ZKL2OutputOracle *ZKL2OutputOracleFilterer) FilterOutputProposed(opts *bind.FilterOpts, outputRoot [][32]byte, l2OutputIndex []*big.Int, l2BlockNumber []*big.Int) (*ZKL2OutputOracleOutputProposedIterator, error) {

	var outputRootRule []interface{}
	for _, outputRootItem := range outputRoot {
		outputRootRule = append(outputRootRule, outputRootItem)
	}
	var l2OutputIndexRule []interface{}
	for _, l2OutputIndexItem := range l2OutputIndex {
		l2OutputIndexRule = append(l2OutputIndexRule, l2OutputIndexItem)
	}
	var l2BlockNumberRule []interface{}
	for _, l2BlockNumberItem := range l2BlockNumber {
		l2BlockNumberRule = append(l2BlockNumberRule, l2BlockNumberItem)
	}

	logs, sub, err := _ZKL2OutputOracle.contract.FilterLogs(opts, "OutputProposed", outputRootRule, l2OutputIndexRule, l2BlockNumberRule)
	if err != nil {
		return nil, err
	}
	return &ZKL2OutputOracleOutputProposedIterator{contract: _ZKL2OutputOracle.contract, event: "OutputProposed", logs: logs, sub: sub}, nil
}

// WatchOutputProposed is a free log subscription operation binding the contract event 0xa7aaf2512769da4e444e3de247be2564225c2e7a8f74cfe528e46e17d24868e2.
//
// Solidity: event OutputProposed(bytes32 indexed outputRoot, uint256 indexed l2OutputIndex, uint256 indexed l2BlockNumber, uint256 l1Timestamp)
func (_ZKL2OutputOracle *ZKL2OutputOracleFilterer) WatchOutputProposed(opts *bind.WatchOpts, sink chan<- *ZKL2OutputOracleOutputProposed, outputRoot [][32]byte, l2OutputIndex []*big.Int, l2BlockNumber []*big.Int) (event.Subscription, error) {

	var outputRootRule []interface{}
	for _, outputRootItem := range outputRoot {
		outputRootRule = append(outputRootRule, outputRootItem)
	}
	var l2OutputIndexRule []interface{}
	for _, l2OutputIndexItem := range l2OutputIndex {
		l2OutputIndexRule = append(l2OutputIndexRule, l2OutputIndexItem)
	}
	var l2BlockNumberRule []interface{}
	for _, l2BlockNumberItem := range l2BlockNumber {
		l2BlockNumberRule = append(l2BlockNumberRule, l2BlockNumberItem)
	}

	logs, sub, err := _ZKL2OutputOracle.contract.WatchLogs(opts, "OutputProposed", outputRootRule, l2OutputIndexRule, l2BlockNumberRule)
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(ZKL2OutputOracleOutputProposed)
				if err := _ZKL2OutputOracle.contract.UnpackLog(event, "OutputProposed", log); err != nil {
					return err
				}
				event.Raw = log

				select {
				case sink <- event:
				case err := <-sub.Err():
					return err
				case <-quit:
					return nil
				}
			case err := <-sub.Err():
				return err
			case <-quit:
				return nil
			}
		}
	}), nil
}

// ParseOutputProposed is a log parse operation binding the contract event 0xa7aaf2512769da4e444e3de247be2564225c2e7a8f74cfe528e46e17d24868e2.
//
// Solidity: event OutputProposed(bytes32 indexed outputRoot, uint256 indexed l2OutputIndex, uint256 indexed l2BlockNumber, uint256 l1Timestamp)
func (_ZKL2OutputOracle *ZKL2OutputOracleFilterer) ParseOutputProposed(log types.Log) (*ZKL2OutputOracleOutputProposed, error) {
	event := new(ZKL2OutputOracleOutputProposed)
	if err := _ZKL2OutputOracle.contract.UnpackLog(event, "OutputProposed", log); err != nil {
		return nil, err
	}
	event.Raw = log
	return event, nil
}

// ZKL2OutputOracleOutputsDeletedIterator is returned from FilterOutputsDeleted and is used to iterate over the raw logs and unpacked data for OutputsDeleted events raised by the ZKL2OutputOracle contract.
type ZKL2OutputOracleOutputsDeletedIterator struct {
	Event *ZKL2OutputOracleOutputsDeleted // Event containing the contract specifics and raw log

	contract *bind.BoundContract // Generic contract to use for unpacking event data
	event    string              // Event name to use for unpacking event data

	logs chan types.Log        // Log channel receiving the found contract events
	sub  ethereum.Subscription // Subscription for errors, completion and termination
	done bool                  // Whether the subscription completed delivering logs
	fail error                 // Occurred error to stop iteration
}

// Next advances the iterator to the subsequent event, returning whether there
// are any more events found. In case of a retrieval or parsing error, false is
// returned and Error() can be queried for the exact failure.
func (it *ZKL2OutputOracleOutputsDeletedIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(ZKL2OutputOracleOutputsDeleted)
			if err := it.contract.UnpackLog(it.Event, it.event, log); err != nil {
				it.fail = err
				return false
			}
			it.Event.Raw = log
			return true

		default:
			return false
		}
	}
	// Iterator still in progress, wait for either a data or an error event
	select {
	case log := <-it.logs:
		it.Event = new(ZKL2OutputOracleOutputsDeleted)
		if err := it.contract.UnpackLog(it.Event, it.event, log); err != nil {
			it.fail = err
			return false
		}
		it.Event.Raw = log
		return true

	case err := <-it.sub.Err():
		it.done = true
		it.fail = err
		return it.Next()
	}
}

// Error returns any retrieval or parsing error occurred during filtering.
func (it *ZKL2OutputOracleOutputsDeletedIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *ZKL2OutputOracleOutputsDeletedIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// ZKL2OutputOracleOutputsDeleted represents a OutputsDeleted event raised by the ZKL2OutputOracle contract.
type ZKL2OutputOracleOutputsDeleted struct {
	PrevNextOutputIndex *big.Int
	NewNextOutputIndex  *big.Int
	Raw                 types.Log // Blockchain specific contextual infos
}

// FilterOutputsDeleted is a free log retrieval operation binding the contract event 0x4ee37ac2c786ec85e87592d3c5c8a1dd66f8496dda3f125d9ea8ca5f657629b6.
//
// Solidity: event OutputsDeleted(uint256 indexed prevNextOutputIndex, uint256 indexed newNextOutputIndex)
func (_ZKL2OutputOracle *ZKL2OutputOracleFilterer) FilterOutputsDeleted(opts *bind.FilterOpts, prevNextOutputIndex []*big.Int, newNextOutputIndex []*big.Int) (*ZKL2OutputOracleOutputsDeletedIterator, error) {

	var prevNextOutputIndexRule []interface{}
	for _, prevNextOutputIndexItem := range prevNextOutputIndex {
		prevNextOutputIndexRule = append(prevNextOutputIndexRule, prevNextOutputIndexItem)
	}
	var newNextOutputIndexRule []interface{}
	for _, newNextOutputIndexItem := range newNextOutputIndex {
		newNextOutputIndexRule = append(newNextOutputIndexRule, newNextOutputIndexItem)
	}

	logs, sub, err := _ZKL2OutputOracle.contract.FilterLogs(opts, "OutputsDeleted", prevNextOutputIndexRule, newNextOutputIndexRule)
	if err != nil {
		return nil, err
	}
	return &ZKL2OutputOracleOutputsDeletedIterator{contract: _ZKL2OutputOracle.contract, event: "OutputsDeleted", logs: logs, sub: sub}, nil
}

// WatchOutputsDeleted is a free log subscription operation binding the contract event 0x4ee37ac2c786ec85e87592d3c5c8a1dd66f8496dda3f125d9ea8ca5f657629b6.
//
// Solidity: event OutputsDeleted(uint256 indexed prevNextOutputIndex, uint256 indexed newNextOutputIndex)
func (_ZKL2OutputOracle *ZKL2OutputOracleFilterer) WatchOutputsDeleted(opts *bind.WatchOpts, sink chan<- *ZKL2OutputOracleOutputsDeleted, prevNextOutputIndex []*big.Int, newNextOutputIndex []*big.Int) (event.Subscription, error) {

	var prevNextOutputIndexRule []interface{}
	for _, prevNextOutputIndexItem := range prevNextOutputIndex {
		prevNextOutputIndexRule = append(prevNextOutputIndexRule, prevNextOutputIndexItem)
	}
	var newNextOutputIndexRule []interface{}
	for _, newNextOutputIndexItem := range newNextOutputIndex {
		newNextOutputIndexRule = append(newNextOutputIndexRule, newNextOutputIndexItem)
	}

	logs, sub, err := _ZKL2OutputOracle.contract.WatchLogs(opts, "OutputsDeleted", prevNextOutputIndexRule, newNextOutputIndexRule)
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(ZKL2OutputOracleOutputsDeleted)
				if err := _ZKL2OutputOracle.contract.UnpackLog(event, "OutputsDeleted", log); err != nil {
					return err
				}
				event.Raw = log

				select {
				case sink <- event:
				case err := <-sub.Err():
					return err
				case <-quit:
					return nil
				}
			case err := <-sub.Err():
				return err
			case <-quit:
				return nil
			}
		}
	}), nil
}

// ParseOutputsDeleted is a log parse operation binding the contract event 0x4ee37ac2c786ec85e87592d3c5c8a1dd66f8496dda3f125d9ea8ca5f657629b6.
//
// Solidity: event OutputsDeleted(uint256 indexed prevNextOutputIndex, uint256 indexed newNextOutputIndex)
func (_ZKL2OutputOracle *ZKL2OutputOracleFilterer) ParseOutputsDeleted(log types.Log) (*ZKL2OutputOracleOutputsDeleted, error) {
	event := new(ZKL2OutputOracleOutputsDeleted)
	if err := _ZKL2OutputOracle.contract.UnpackLog(event, "OutputsDeleted", log); err != nil {
		return nil, err
	}
	event.Raw = log
	return event, nil
}

// ZKL2OutputOracleOwnershipTransferredIterator is returned from FilterOwnershipTransferred and is used to iterate over the raw logs and unpacked data for OwnershipTransferred events raised by the ZKL2OutputOracle contract.
type ZKL2OutputOracleOwnershipTransferredIterator struct {
	Event *ZKL2OutputOracleOwnershipTransferred // Event containing the contract specifics and raw log

	contract *bind.BoundContract // Generic contract to use for unpacking event data
	event    string              // Event name to use for unpacking event data

	logs chan types.Log        // Log channel receiving the found contract events
	sub  ethereum.Subscription // Subscription for errors, completion and termination
	done bool                  // Whether the subscription completed delivering logs
	fail error                 // Occurred error to stop iteration
}

// Next advances the iterator to the subsequent event, returning whether there
// are any more events found. In case of a retrieval or parsing error, false is
// returned and Error() can be queried for the exact failure.
func (it *ZKL2OutputOracleOwnershipTransferredIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(ZKL2OutputOracleOwnershipTransferred)
			if err := it.contract.UnpackLog(it.Event, it.event, log); err != nil {
				it.fail = err
				return false
			}
			it.Event.Raw = log
			return true

		default:
			return false
		}
	}
	// Iterator still in progress, wait for either a data or an error event
	select {
	case log := <-it.logs:
		it.Event = new(ZKL2OutputOracleOwnershipTransferred)
		if err := it.contract.UnpackLog(it.Event, it.event, log); err != nil {
			it.fail = err
			return false
		}
		it.Event.Raw = log
		return true

	case err := <-it.sub.Err():
		it.done = true
		it.fail = err
		return it.Next()
	}
}

// Error returns any retrieval or parsing error occurred during filtering.
func (it *ZKL2OutputOracleOwnershipTransferredIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *ZKL2OutputOracleOwnershipTransferredIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// ZKL2OutputOracleOwnershipTransferred represents a OwnershipTransferred event raised by the ZKL2OutputOracle contract.
type ZKL2OutputOracleOwnershipTransferred struct {
	PreviousOwner common.Address
	NewOwner      common.Address
	Raw           types.Log // Blockchain specific contextual infos
}

// FilterOwnershipTransferred is a free log retrieval operation binding the contract event 0x8be0079c531659141344cd1fd0a4f28419497f9722a3daafe3b4186f6b6457e0.
//
// Solidity: event OwnershipTransferred(address indexed previousOwner, address indexed newOwner)
func (_ZKL2OutputOracle *ZKL2OutputOracleFilterer) FilterOwnershipTransferred(opts *bind.FilterOpts, previousOwner []common.Address, newOwner []common.Address) (*ZKL2OutputOracleOwnershipTransferredIterator, error) {

	var previousOwnerRule []interface{}
	for _, previousOwnerItem := range previousOwner {
		previousOwnerRule = append(previousOwnerRule, previousOwnerItem)
	}
	var newOwnerRule []interface{}
	for _, newOwnerItem := range newOwner {
		newOwnerRule = append(newOwnerRule, newOwnerItem)
	}

	logs, sub, err := _ZKL2OutputOracle.contract.FilterLogs(opts, "OwnershipTransferred", previousOwnerRule, newOwnerRule)
	if err != nil {
		return nil, err
	}
	return &ZKL2OutputOracleOwnershipTransferredIterator{contract: _ZKL2OutputOracle.contract, event: "OwnershipTransferred", logs: logs, sub: sub}, nil
}

// WatchOwnershipTransferred is a free log subscription operation binding the contract event 0x8be0079c531659141344cd1fd0a4f28419497f9722a3daafe3b4186f6b6457e0.
//
// Solidity: event OwnershipTransferred(address indexed previousOwner, address indexed newOwner)
func (_ZKL2OutputOracle *ZKL2OutputOracleFilterer) WatchOwnershipTransferred(opts *bind.WatchOpts, sink chan<- *ZKL2OutputOracleOwnershipTransferred, previousOwner []common.Address, newOwner []common.Address) (event.Subscription, error) {

	var previousOwnerRule []interface{}
	for _, previousOwnerItem := range previousOwner {
		previousOwnerRule = append(previousOwnerRule, previousOwnerItem)
	}
	var newOwnerRule []interface{}
	for _, newOwnerItem := range newOwner {
		newOwnerRule = append(newOwnerRule, newOwnerItem)
	}

	logs, sub, err := _ZKL2OutputOracle.contract.WatchLogs(opts, "OwnershipTransferred", previousOwnerRule, newOwnerRule)
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(ZKL2OutputOracleOwnershipTransferred)
				if err := _ZKL2OutputOracle.contract.UnpackLog(event, "OwnershipTransferred", log); err != nil {
					return err
				}
				event.Raw = log

				select {
				case sink <- event:
				case err := <-sub.Err():
					return err
				case <-quit:
					return nil
				}
			case err := <-sub.Err():
				return err
			case <-quit:
				return nil
			}
		}
	}), nil
}

// ParseOwnershipTransferred is a log parse operation binding the contract event 0x8be0079c531659141344cd1fd0a4f28419497f9722a3daafe3b4186f6b6457e0.
//
// Solidity: event OwnershipTransferred(address indexed previousOwner, address indexed newOwner)
func (_ZKL2OutputOracle *ZKL2OutputOracleFilterer) ParseOwnershipTransferred(log types.Log) (*ZKL2OutputOracleOwnershipTransferred, error) {
	event := new(ZKL2OutputOracleOwnershipTransferred)
	if err := _ZKL2OutputOracle.contract.UnpackLog(event, "OwnershipTransferred", log); err != nil {
		return nil, err
	}
	event.Raw = log
	return event, nil
}

// ZKL2OutputOracleUpdatedVKeyIterator is returned from FilterUpdatedVKey and is used to iterate over the raw logs and unpacked data for UpdatedVKey events raised by the ZKL2OutputOracle contract.
type ZKL2OutputOracleUpdatedVKeyIterator struct {
	Event *ZKL2OutputOracleUpdatedVKey // Event containing the contract specifics and raw log

	contract *bind.BoundContract // Generic contract to use for unpacking event data
	event    string              // Event name to use for unpacking event data

	logs chan types.Log        // Log channel receiving the found contract events
	sub  ethereum.Subscription // Subscription for errors, completion and termination
	done bool                  // Whether the subscription completed delivering logs
	fail error                 // Occurred error to stop iteration
}

// Next advances the iterator to the subsequent event, returning whether there
// are any more events found. In case of a retrieval or parsing error, false is
// returned and Error() can be queried for the exact failure.
func (it *ZKL2OutputOracleUpdatedVKeyIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(ZKL2OutputOracleUpdatedVKey)
			if err := it.contract.UnpackLog(it.Event, it.event, log); err != nil {
				it.fail = err
				return false
			}
			it.Event.Raw = log
			return true

		default:
			return false
		}
	}
	// Iterator still in progress, wait for either a data or an error event
	select {
	case log := <-it.logs:
		it.Event = new(ZKL2OutputOracleUpdatedVKey)
		if err := it.contract.UnpackLog(it.Event, it.event, log); err != nil {
			it.fail = err
			return false
		}
		it.Event.Raw = log
		return true

	case err := <-it.sub.Err():
		it.done = true
		it.fail = err
		return it.Next()
	}
}

// Error returns any retrieval or parsing error occurred during filtering.
func (it *ZKL2OutputOracleUpdatedVKeyIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *ZKL2OutputOracleUpdatedVKeyIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// ZKL2OutputOracleUpdatedVKey represents a UpdatedVKey event raised by the ZKL2OutputOracle contract.
type ZKL2OutputOracleUpdatedVKey struct {
	OldVkey [32]byte
	NewVkey [32]byte
	Raw     types.Log // Blockchain specific contextual infos
}

// FilterUpdatedVKey is a free log retrieval operation binding the contract event 0x9950c7ae9575688726be5b8ba4f28af88f91a9ad6853bc02658104b62cfcd77d.
//
// Solidity: event UpdatedVKey(bytes32 indexed oldVkey, bytes32 indexed newVkey)
func (_ZKL2OutputOracle *ZKL2OutputOracleFilterer) FilterUpdatedVKey(opts *bind.FilterOpts, oldVkey [][32]byte, newVkey [][32]byte) (*ZKL2OutputOracleUpdatedVKeyIterator, error) {

	var oldVkeyRule []interface{}
	for _, oldVkeyItem := range oldVkey {
		oldVkeyRule = append(oldVkeyRule, oldVkeyItem)
	}
	var newVkeyRule []interface{}
	for _, newVkeyItem := range newVkey {
		newVkeyRule = append(newVkeyRule, newVkeyItem)
	}

	logs, sub, err := _ZKL2OutputOracle.contract.FilterLogs(opts, "UpdatedVKey", oldVkeyRule, newVkeyRule)
	if err != nil {
		return nil, err
	}
	return &ZKL2OutputOracleUpdatedVKeyIterator{contract: _ZKL2OutputOracle.contract, event: "UpdatedVKey", logs: logs, sub: sub}, nil
}

// WatchUpdatedVKey is a free log subscription operation binding the contract event 0x9950c7ae9575688726be5b8ba4f28af88f91a9ad6853bc02658104b62cfcd77d.
//
// Solidity: event UpdatedVKey(bytes32 indexed oldVkey, bytes32 indexed newVkey)
func (_ZKL2OutputOracle *ZKL2OutputOracleFilterer) WatchUpdatedVKey(opts *bind.WatchOpts, sink chan<- *ZKL2OutputOracleUpdatedVKey, oldVkey [][32]byte, newVkey [][32]byte) (event.Subscription, error) {

	var oldVkeyRule []interface{}
	for _, oldVkeyItem := range oldVkey {
		oldVkeyRule = append(oldVkeyRule, oldVkeyItem)
	}
	var newVkeyRule []interface{}
	for _, newVkeyItem := range newVkey {
		newVkeyRule = append(newVkeyRule, newVkeyItem)
	}

	logs, sub, err := _ZKL2OutputOracle.contract.WatchLogs(opts, "UpdatedVKey", oldVkeyRule, newVkeyRule)
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(ZKL2OutputOracleUpdatedVKey)
				if err := _ZKL2OutputOracle.contract.UnpackLog(event, "UpdatedVKey", log); err != nil {
					return err
				}
				event.Raw = log

				select {
				case sink <- event:
				case err := <-sub.Err():
					return err
				case <-quit:
					return nil
				}
			case err := <-sub.Err():
				return err
			case <-quit:
				return nil
			}
		}
	}), nil
}

// ParseUpdatedVKey is a log parse operation binding the contract event 0x9950c7ae9575688726be5b8ba4f28af88f91a9ad6853bc02658104b62cfcd77d.
//
// Solidity: event UpdatedVKey(bytes32 indexed oldVkey, bytes32 indexed newVkey)
func (_ZKL2OutputOracle *ZKL2OutputOracleFilterer) ParseUpdatedVKey(log types.Log) (*ZKL2OutputOracleUpdatedVKey, error) {
	event := new(ZKL2OutputOracleUpdatedVKey)
	if err := _ZKL2OutputOracle.contract.UnpackLog(event, "UpdatedVKey", log); err != nil {
		return nil, err
	}
	event.Raw = log
	return event, nil
}

// ZKL2OutputOracleUpdatedVerifierGatewayIterator is returned from FilterUpdatedVerifierGateway and is used to iterate over the raw logs and unpacked data for UpdatedVerifierGateway events raised by the ZKL2OutputOracle contract.
type ZKL2OutputOracleUpdatedVerifierGatewayIterator struct {
	Event *ZKL2OutputOracleUpdatedVerifierGateway // Event containing the contract specifics and raw log

	contract *bind.BoundContract // Generic contract to use for unpacking event data
	event    string              // Event name to use for unpacking event data

	logs chan types.Log        // Log channel receiving the found contract events
	sub  ethereum.Subscription // Subscription for errors, completion and termination
	done bool                  // Whether the subscription completed delivering logs
	fail error                 // Occurred error to stop iteration
}

// Next advances the iterator to the subsequent event, returning whether there
// are any more events found. In case of a retrieval or parsing error, false is
// returned and Error() can be queried for the exact failure.
func (it *ZKL2OutputOracleUpdatedVerifierGatewayIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(ZKL2OutputOracleUpdatedVerifierGateway)
			if err := it.contract.UnpackLog(it.Event, it.event, log); err != nil {
				it.fail = err
				return false
			}
			it.Event.Raw = log
			return true

		default:
			return false
		}
	}
	// Iterator still in progress, wait for either a data or an error event
	select {
	case log := <-it.logs:
		it.Event = new(ZKL2OutputOracleUpdatedVerifierGateway)
		if err := it.contract.UnpackLog(it.Event, it.event, log); err != nil {
			it.fail = err
			return false
		}
		it.Event.Raw = log
		return true

	case err := <-it.sub.Err():
		it.done = true
		it.fail = err
		return it.Next()
	}
}

// Error returns any retrieval or parsing error occurred during filtering.
func (it *ZKL2OutputOracleUpdatedVerifierGatewayIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *ZKL2OutputOracleUpdatedVerifierGatewayIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// ZKL2OutputOracleUpdatedVerifierGateway represents a UpdatedVerifierGateway event raised by the ZKL2OutputOracle contract.
type ZKL2OutputOracleUpdatedVerifierGateway struct {
	OldVerifierGateway common.Address
	NewVerifierGateway common.Address
	Raw                types.Log // Blockchain specific contextual infos
}

// FilterUpdatedVerifierGateway is a free log retrieval operation binding the contract event 0x1379941631ff0ed9178ab16ab67a2e5db3aeada7f87e518f761e79c8e38377e3.
//
// Solidity: event UpdatedVerifierGateway(address indexed oldVerifierGateway, address indexed newVerifierGateway)
func (_ZKL2OutputOracle *ZKL2OutputOracleFilterer) FilterUpdatedVerifierGateway(opts *bind.FilterOpts, oldVerifierGateway []common.Address, newVerifierGateway []common.Address) (*ZKL2OutputOracleUpdatedVerifierGatewayIterator, error) {

	var oldVerifierGatewayRule []interface{}
	for _, oldVerifierGatewayItem := range oldVerifierGateway {
		oldVerifierGatewayRule = append(oldVerifierGatewayRule, oldVerifierGatewayItem)
	}
	var newVerifierGatewayRule []interface{}
	for _, newVerifierGatewayItem := range newVerifierGateway {
		newVerifierGatewayRule = append(newVerifierGatewayRule, newVerifierGatewayItem)
	}

	logs, sub, err := _ZKL2OutputOracle.contract.FilterLogs(opts, "UpdatedVerifierGateway", oldVerifierGatewayRule, newVerifierGatewayRule)
	if err != nil {
		return nil, err
	}
	return &ZKL2OutputOracleUpdatedVerifierGatewayIterator{contract: _ZKL2OutputOracle.contract, event: "UpdatedVerifierGateway", logs: logs, sub: sub}, nil
}

// WatchUpdatedVerifierGateway is a free log subscription operation binding the contract event 0x1379941631ff0ed9178ab16ab67a2e5db3aeada7f87e518f761e79c8e38377e3.
//
// Solidity: event UpdatedVerifierGateway(address indexed oldVerifierGateway, address indexed newVerifierGateway)
func (_ZKL2OutputOracle *ZKL2OutputOracleFilterer) WatchUpdatedVerifierGateway(opts *bind.WatchOpts, sink chan<- *ZKL2OutputOracleUpdatedVerifierGateway, oldVerifierGateway []common.Address, newVerifierGateway []common.Address) (event.Subscription, error) {

	var oldVerifierGatewayRule []interface{}
	for _, oldVerifierGatewayItem := range oldVerifierGateway {
		oldVerifierGatewayRule = append(oldVerifierGatewayRule, oldVerifierGatewayItem)
	}
	var newVerifierGatewayRule []interface{}
	for _, newVerifierGatewayItem := range newVerifierGateway {
		newVerifierGatewayRule = append(newVerifierGatewayRule, newVerifierGatewayItem)
	}

	logs, sub, err := _ZKL2OutputOracle.contract.WatchLogs(opts, "UpdatedVerifierGateway", oldVerifierGatewayRule, newVerifierGatewayRule)
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(ZKL2OutputOracleUpdatedVerifierGateway)
				if err := _ZKL2OutputOracle.contract.UnpackLog(event, "UpdatedVerifierGateway", log); err != nil {
					return err
				}
				event.Raw = log

				select {
				case sink <- event:
				case err := <-sub.Err():
					return err
				case <-quit:
					return nil
				}
			case err := <-sub.Err():
				return err
			case <-quit:
				return nil
			}
		}
	}), nil
}

// ParseUpdatedVerifierGateway is a log parse operation binding the contract event 0x1379941631ff0ed9178ab16ab67a2e5db3aeada7f87e518f761e79c8e38377e3.
//
// Solidity: event UpdatedVerifierGateway(address indexed oldVerifierGateway, address indexed newVerifierGateway)
func (_ZKL2OutputOracle *ZKL2OutputOracleFilterer) ParseUpdatedVerifierGateway(log types.Log) (*ZKL2OutputOracleUpdatedVerifierGateway, error) {
	event := new(ZKL2OutputOracleUpdatedVerifierGateway)
	if err := _ZKL2OutputOracle.contract.UnpackLog(event, "UpdatedVerifierGateway", log); err != nil {
		return nil, err
	}
	event.Raw = log
	return event, nil
}
