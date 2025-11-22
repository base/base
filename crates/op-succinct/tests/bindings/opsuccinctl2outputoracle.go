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

// OPSuccinctL2OutputOracleInitParams is an auto generated low-level Go binding around an user-defined struct.
type OPSuccinctL2OutputOracleInitParams struct {
	Challenger                common.Address
	Proposer                  common.Address
	Owner                     common.Address
	FinalizationPeriodSeconds *big.Int
	L2BlockTime               *big.Int
	AggregationVkey           [32]byte
	RangeVkeyCommitment       [32]byte
	RollupConfigHash          [32]byte
	StartingOutputRoot        [32]byte
	StartingBlockNumber       *big.Int
	StartingTimestamp         *big.Int
	SubmissionInterval        *big.Int
	Verifier                  common.Address
	FallbackTimeout           *big.Int
}

// OPSuccinctL2OutputOracleOpSuccinctConfig is an auto generated low-level Go binding around an user-defined struct.
type OPSuccinctL2OutputOracleOpSuccinctConfig struct {
	AggregationVkey     [32]byte
	RangeVkeyCommitment [32]byte
	RollupConfigHash    [32]byte
}

// TypesOutputProposal is an auto generated low-level Go binding around an user-defined struct.
type TypesOutputProposal struct {
	OutputRoot    [32]byte
	Timestamp     *big.Int
	L2BlockNumber *big.Int
}

// OPSuccinctL2OutputOracleMetaData contains all meta data concerning the OPSuccinctL2OutputOracle contract.
var OPSuccinctL2OutputOracleMetaData = &bind.MetaData{
	ABI: "[{\"type\":\"constructor\",\"inputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"GENESIS_CONFIG_NAME\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"addOpSuccinctConfig\",\"inputs\":[{\"name\":\"_configName\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"_rollupConfigHash\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"_aggregationVkey\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"_rangeVkeyCommitment\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"addProposer\",\"inputs\":[{\"name\":\"_proposer\",\"type\":\"address\",\"internalType\":\"address\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"aggregationVkey\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"approvedProposers\",\"inputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"outputs\":[{\"name\":\"\",\"type\":\"bool\",\"internalType\":\"bool\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"challenger\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"checkpointBlockHash\",\"inputs\":[{\"name\":\"_blockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"computeL2Timestamp\",\"inputs\":[{\"name\":\"_l2BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"deleteL2Outputs\",\"inputs\":[{\"name\":\"_l2OutputIndex\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"deleteOpSuccinctConfig\",\"inputs\":[{\"name\":\"_configName\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"dgfProposeL2Output\",\"inputs\":[{\"name\":\"_configName\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"_outputRoot\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"_l2BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"_l1BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"_proof\",\"type\":\"bytes\",\"internalType\":\"bytes\"},{\"name\":\"_proverAddress\",\"type\":\"address\",\"internalType\":\"address\"}],\"outputs\":[{\"name\":\"_game\",\"type\":\"address\",\"internalType\":\"contractIDisputeGame\"}],\"stateMutability\":\"payable\"},{\"type\":\"function\",\"name\":\"disableOptimisticMode\",\"inputs\":[{\"name\":\"_finalizationPeriodSeconds\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"disputeGameFactory\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"enableOptimisticMode\",\"inputs\":[{\"name\":\"_finalizationPeriodSeconds\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"fallbackTimeout\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"finalizationPeriodSeconds\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"getL2Output\",\"inputs\":[{\"name\":\"_l2OutputIndex\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[{\"name\":\"\",\"type\":\"tuple\",\"internalType\":\"structTypes.OutputProposal\",\"components\":[{\"name\":\"outputRoot\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"timestamp\",\"type\":\"uint128\",\"internalType\":\"uint128\"},{\"name\":\"l2BlockNumber\",\"type\":\"uint128\",\"internalType\":\"uint128\"}]}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"getL2OutputAfter\",\"inputs\":[{\"name\":\"_l2BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[{\"name\":\"\",\"type\":\"tuple\",\"internalType\":\"structTypes.OutputProposal\",\"components\":[{\"name\":\"outputRoot\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"timestamp\",\"type\":\"uint128\",\"internalType\":\"uint128\"},{\"name\":\"l2BlockNumber\",\"type\":\"uint128\",\"internalType\":\"uint128\"}]}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"getL2OutputIndexAfter\",\"inputs\":[{\"name\":\"_l2BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"historicBlockHashes\",\"inputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[{\"name\":\"\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"initialize\",\"inputs\":[{\"name\":\"_initParams\",\"type\":\"tuple\",\"internalType\":\"structOPSuccinctL2OutputOracle.InitParams\",\"components\":[{\"name\":\"challenger\",\"type\":\"address\",\"internalType\":\"address\"},{\"name\":\"proposer\",\"type\":\"address\",\"internalType\":\"address\"},{\"name\":\"owner\",\"type\":\"address\",\"internalType\":\"address\"},{\"name\":\"finalizationPeriodSeconds\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"l2BlockTime\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"aggregationVkey\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"rangeVkeyCommitment\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"rollupConfigHash\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"startingOutputRoot\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"startingBlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"startingTimestamp\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"submissionInterval\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"verifier\",\"type\":\"address\",\"internalType\":\"address\"},{\"name\":\"fallbackTimeout\",\"type\":\"uint256\",\"internalType\":\"uint256\"}]}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"initializerVersion\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint8\",\"internalType\":\"uint8\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"isValidOpSuccinctConfig\",\"inputs\":[{\"name\":\"_config\",\"type\":\"tuple\",\"internalType\":\"structOPSuccinctL2OutputOracle.OpSuccinctConfig\",\"components\":[{\"name\":\"aggregationVkey\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"rangeVkeyCommitment\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"rollupConfigHash\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}]}],\"outputs\":[{\"name\":\"\",\"type\":\"bool\",\"internalType\":\"bool\"}],\"stateMutability\":\"pure\"},{\"type\":\"function\",\"name\":\"l2BlockTime\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"lastProposalTimestamp\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"latestBlockNumber\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"latestOutputIndex\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"nextBlockNumber\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"nextOutputIndex\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"opSuccinctConfigs\",\"inputs\":[{\"name\":\"\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"outputs\":[{\"name\":\"aggregationVkey\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"rangeVkeyCommitment\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"rollupConfigHash\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"optimisticMode\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"bool\",\"internalType\":\"bool\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"owner\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"proposeL2Output\",\"inputs\":[{\"name\":\"_outputRoot\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"_l2BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"_l1BlockHash\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"_l1BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[],\"stateMutability\":\"payable\"},{\"type\":\"function\",\"name\":\"proposeL2Output\",\"inputs\":[{\"name\":\"_configName\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"_outputRoot\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"_l2BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"_l1BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"_proof\",\"type\":\"bytes\",\"internalType\":\"bytes\"},{\"name\":\"_proverAddress\",\"type\":\"address\",\"internalType\":\"address\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"proposer\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"rangeVkeyCommitment\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"removeProposer\",\"inputs\":[{\"name\":\"_proposer\",\"type\":\"address\",\"internalType\":\"address\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"rollupConfigHash\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"setDisputeGameFactory\",\"inputs\":[{\"name\":\"_disputeGameFactory\",\"type\":\"address\",\"internalType\":\"address\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"startingBlockNumber\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"startingTimestamp\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"submissionInterval\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"transferOwnership\",\"inputs\":[{\"name\":\"_owner\",\"type\":\"address\",\"internalType\":\"address\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"updateSubmissionInterval\",\"inputs\":[{\"name\":\"_submissionInterval\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"updateVerifier\",\"inputs\":[{\"name\":\"_verifier\",\"type\":\"address\",\"internalType\":\"address\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"verifier\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"version\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"string\",\"internalType\":\"string\"}],\"stateMutability\":\"view\"},{\"type\":\"event\",\"name\":\"DisputeGameFactorySet\",\"inputs\":[{\"name\":\"disputeGameFactory\",\"type\":\"address\",\"indexed\":true,\"internalType\":\"address\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"Initialized\",\"inputs\":[{\"name\":\"version\",\"type\":\"uint8\",\"indexed\":false,\"internalType\":\"uint8\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"OpSuccinctConfigDeleted\",\"inputs\":[{\"name\":\"configName\",\"type\":\"bytes32\",\"indexed\":true,\"internalType\":\"bytes32\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"OpSuccinctConfigUpdated\",\"inputs\":[{\"name\":\"configName\",\"type\":\"bytes32\",\"indexed\":true,\"internalType\":\"bytes32\"},{\"name\":\"aggregationVkey\",\"type\":\"bytes32\",\"indexed\":false,\"internalType\":\"bytes32\"},{\"name\":\"rangeVkeyCommitment\",\"type\":\"bytes32\",\"indexed\":false,\"internalType\":\"bytes32\"},{\"name\":\"rollupConfigHash\",\"type\":\"bytes32\",\"indexed\":false,\"internalType\":\"bytes32\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"OptimisticModeToggled\",\"inputs\":[{\"name\":\"enabled\",\"type\":\"bool\",\"indexed\":true,\"internalType\":\"bool\"},{\"name\":\"finalizationPeriodSeconds\",\"type\":\"uint256\",\"indexed\":false,\"internalType\":\"uint256\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"OutputProposed\",\"inputs\":[{\"name\":\"outputRoot\",\"type\":\"bytes32\",\"indexed\":true,\"internalType\":\"bytes32\"},{\"name\":\"l2OutputIndex\",\"type\":\"uint256\",\"indexed\":true,\"internalType\":\"uint256\"},{\"name\":\"l2BlockNumber\",\"type\":\"uint256\",\"indexed\":true,\"internalType\":\"uint256\"},{\"name\":\"l1Timestamp\",\"type\":\"uint256\",\"indexed\":false,\"internalType\":\"uint256\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"OutputsDeleted\",\"inputs\":[{\"name\":\"prevNextOutputIndex\",\"type\":\"uint256\",\"indexed\":true,\"internalType\":\"uint256\"},{\"name\":\"newNextOutputIndex\",\"type\":\"uint256\",\"indexed\":true,\"internalType\":\"uint256\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"OwnershipTransferred\",\"inputs\":[{\"name\":\"previousOwner\",\"type\":\"address\",\"indexed\":true,\"internalType\":\"address\"},{\"name\":\"newOwner\",\"type\":\"address\",\"indexed\":true,\"internalType\":\"address\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"ProposerUpdated\",\"inputs\":[{\"name\":\"proposer\",\"type\":\"address\",\"indexed\":true,\"internalType\":\"address\"},{\"name\":\"added\",\"type\":\"bool\",\"indexed\":false,\"internalType\":\"bool\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"SubmissionIntervalUpdated\",\"inputs\":[{\"name\":\"oldSubmissionInterval\",\"type\":\"uint256\",\"indexed\":false,\"internalType\":\"uint256\"},{\"name\":\"newSubmissionInterval\",\"type\":\"uint256\",\"indexed\":false,\"internalType\":\"uint256\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"VerifierUpdated\",\"inputs\":[{\"name\":\"oldVerifier\",\"type\":\"address\",\"indexed\":true,\"internalType\":\"address\"},{\"name\":\"newVerifier\",\"type\":\"address\",\"indexed\":true,\"internalType\":\"address\"}],\"anonymous\":false},{\"type\":\"error\",\"name\":\"L1BlockHashNotAvailable\",\"inputs\":[]},{\"type\":\"error\",\"name\":\"L1BlockHashNotCheckpointed\",\"inputs\":[]}]",
	Bin: "0x60806040523480156200001157600080fd5b50620000226200002860201b60201c565b620001d3565b600060019054906101000a900460ff16156200007b576040517f08c379a0000000000000000000000000000000000000000000000000000000008152600401620000729062000176565b60405180910390fd5b60ff801660008054906101000a900460ff1660ff161015620000ed5760ff6000806101000a81548160ff021916908360ff1602179055507f7f26b83ff96e1f2b6a682f133852f6798a09c465da95921460cefb384740249860ff604051620000e49190620001b6565b60405180910390a15b565b600082825260208201905092915050565b7f496e697469616c697a61626c653a20636f6e747261637420697320696e69746960008201527f616c697a696e6700000000000000000000000000000000000000000000000000602082015250565b60006200015e602783620000ef565b91506200016b8262000100565b604082019050919050565b6000602082019050818103600083015262000191816200014f565b9050919050565b600060ff82169050919050565b620001b08162000198565b82525050565b6000602082019050620001cd6000830184620001a5565b92915050565b61502780620001e36000396000f3fe6080604052600436106102885760003560e01c8063887862721161015a578063ce5db8d6116100c1578063e1a41bcf1161007a578063e1a41bcf146109e2578063e40b7a1214610a0d578063ec5b2e3a14610a36578063f2b4e61714610a5f578063f2fde38b14610a8a578063f72f606d14610ab357610288565b8063ce5db8d6146108aa578063cf8e5cf0146108d5578063d1de856c14610912578063d46512761461094f578063dcec33481461098c578063e0c2f935146109b757610288565b8063a196b52511610113578063a196b52514610788578063a25ae557146107c5578063a4ee9d7b14610802578063a8e4fb901461082b578063b03cd41814610856578063c32e4e3e1461087f57610288565b8063887862721461069957806389c44cbb146106c45780638da5cb5b146106ed57806393991af31461071857806397fc007c146107435780639aaab6481461076c57610288565b80634ab309ac116101fe5780636abcf563116101b75780636abcf563146105805780636d9a1c8b146105ab57806370872aa5146105d65780637a41a035146106015780637f006420146106315780637f01ea681461066e57610288565b80634ab309ac1461046c578063534db0e21461049557806354fd4d50146104c057806360caf7a0146104eb57806369f16eec146105165780636a56620b1461054157610288565b8063336c9e8111610250578063336c9e811461035e5780633419d2c2146103875780634277bc06146103b05780634599c788146103db57806347c37e9c1461040657806349185e061461042f57610288565b806309d632d31461028d5780631e856800146102b65780632b31841e146102df5780632b7ac3f31461030a5780632c69796114610335575b600080fd5b34801561029957600080fd5b506102b460048036038101906102af91906131f0565b610ade565b005b3480156102c257600080fd5b506102dd60048036038101906102d89190613253565b610c18565b005b3480156102eb57600080fd5b506102f4610c76565b6040516103019190613299565b60405180910390f35b34801561031657600080fd5b5061031f610c7c565b60405161032c91906132c3565b60405180910390f35b34801561034157600080fd5b5061035c60048036038101906103579190613253565b610ca2565b005b34801561036a57600080fd5b5061038560048036038101906103809190613253565b610de2565b005b34801561039357600080fd5b506103ae60048036038101906103a991906131f0565b610eb7565b005b3480156103bc57600080fd5b506103c5610fce565b6040516103d291906132ed565b60405180910390f35b3480156103e757600080fd5b506103f0610fd4565b6040516103fd91906132ed565b60405180910390f35b34801561041257600080fd5b5061042d60048036038101906104289190613334565b611055565b005b34801561043b57600080fd5b5061045660048036038101906104519190613490565b61128d565b60405161046391906134d8565b60405180910390f35b34801561047857600080fd5b50610493600480360381019061048e9190613253565b6112c7565b005b3480156104a157600080fd5b506104aa611406565b6040516104b791906132c3565b60405180910390f35b3480156104cc57600080fd5b506104d561142c565b6040516104e2919061357b565b60405180910390f35b3480156104f757600080fd5b50610500611465565b60405161050d91906134d8565b60405180910390f35b34801561052257600080fd5b5061052b611478565b60405161053891906132ed565b60405180910390f35b34801561054d57600080fd5b506105686004803603810190610563919061359d565b611491565b604051610577939291906135ca565b60405180910390f35b34801561058c57600080fd5b506105956114bb565b6040516105a291906132ed565b60405180910390f35b3480156105b757600080fd5b506105c06114c8565b6040516105cd9190613299565b60405180910390f35b3480156105e257600080fd5b506105eb6114ce565b6040516105f891906132ed565b60405180910390f35b61061b600480360381019061061691906136bb565b6114d4565b60405161062891906137c3565b60405180910390f35b34801561063d57600080fd5b5061065860048036038101906106539190613253565b6116c4565b60405161066591906132ed565b60405180910390f35b34801561067a57600080fd5b5061068361180b565b60405161069091906137fa565b60405180910390f35b3480156106a557600080fd5b506106ae611810565b6040516106bb91906132ed565b60405180910390f35b3480156106d057600080fd5b506106eb60048036038101906106e69190613253565b611816565b005b3480156106f957600080fd5b506107026119d1565b60405161070f91906132c3565b60405180910390f35b34801561072457600080fd5b5061072d6119f7565b60405161073a91906132ed565b60405180910390f35b34801561074f57600080fd5b5061076a600480360381019061076591906131f0565b6119fd565b005b61078660048036038101906107819190613815565b611b4d565b005b34801561079457600080fd5b506107af60048036038101906107aa9190613253565b611edd565b6040516107bc9190613299565b60405180910390f35b3480156107d157600080fd5b506107ec60048036038101906107e79190613253565b611ef5565b6040516107f991906138f8565b60405180910390f35b34801561080e57600080fd5b50610829600480360381019061082491906136bb565b611fcf565b005b34801561083757600080fd5b50610840612640565b60405161084d91906132c3565b60405180910390f35b34801561086257600080fd5b5061087d600480360381019061087891906131f0565b612666565b005b34801561088b57600080fd5b506108946127a0565b6040516108a19190613299565b60405180910390f35b3480156108b657600080fd5b506108bf6127a6565b6040516108cc91906132ed565b60405180910390f35b3480156108e157600080fd5b506108fc60048036038101906108f79190613253565b6127ac565b60405161090991906138f8565b60405180910390f35b34801561091e57600080fd5b5061093960048036038101906109349190613253565b61288e565b60405161094691906132ed565b60405180910390f35b34801561095b57600080fd5b50610976600480360381019061097191906131f0565b6128bf565b60405161098391906134d8565b60405180910390f35b34801561099857600080fd5b506109a16128df565b6040516109ae91906132ed565b60405180910390f35b3480156109c357600080fd5b506109cc6128fb565b6040516109d991906132ed565b60405180910390f35b3480156109ee57600080fd5b506109f761297c565b604051610a0491906132ed565b60405180910390f35b348015610a1957600080fd5b50610a346004803603810190610a2f9190613a61565b612982565b005b348015610a4257600080fd5b50610a5d6004803603810190610a58919061359d565b612eae565b005b348015610a6b57600080fd5b50610a74612f9c565b604051610a8191906132c3565b60405180910390f35b348015610a9657600080fd5b50610ab16004803603810190610aac91906131f0565b612fc2565b005b348015610abf57600080fd5b50610ac8613112565b604051610ad59190613299565b60405180910390f35b600d60009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff1614610b6e576040517f08c379a0000000000000000000000000000000000000000000000000000000008152600401610b6590613b01565b60405180910390fd5b6000600e60008373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060006101000a81548160ff0219169083151502179055508073ffffffffffffffffffffffffffffffffffffffff167f5df38d395edc15b669d646569bd015513395070b5b4deb8a16300abb060d1b5a6000604051610c0d91906134d8565b60405180910390a250565b6000814090506000801b8103610c5a576040517f84c0686400000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b80600f6000848152602001908152602001600020819055505050565b600a5481565b600b60009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1681565b600d60009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff1614610d32576040517f08c379a0000000000000000000000000000000000000000000000000000000008152600401610d2990613b01565b60405180910390fd5b601060009054906101000a900460ff1615610d82576040517f08c379a0000000000000000000000000000000000000000000000000000000008152600401610d7990613b93565b60405180910390fd5b806008819055506001601060006101000a81548160ff021916908315150217905550600115157f1f5c872f1ea93c57e43112ea449ee19ef5754488b87627b4c52456b0e5a4109a82604051610dd791906132ed565b60405180910390a250565b600d60009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff1614610e72576040517f08c379a0000000000000000000000000000000000000000000000000000000008152600401610e6990613b01565b60405180910390fd5b7fc1bf9abfb57ea01ed9ecb4f45e9cefa7ba44b2e6778c3ce7281409999f1af1b260045482604051610ea5929190613bb3565b60405180910390a18060048190555050565b600d60009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff1614610f47576040517f08c379a0000000000000000000000000000000000000000000000000000000008152600401610f3e90613b01565b60405180910390fd5b80601360006101000a81548173ffffffffffffffffffffffffffffffffffffffff021916908373ffffffffffffffffffffffffffffffffffffffff1602179055508073ffffffffffffffffffffffffffffffffffffffff167f73702180ce348e07b058846d1745c99987ae6c741ff97ec28d4539530ef1e8f160405160405180910390a250565b60115481565b6000806003805490501461104c5760036001600380549050610ff69190613c0b565b8154811061100757611006613c3f565b5b906000526020600020906002020160010160109054906101000a90046fffffffffffffffffffffffffffffffff166fffffffffffffffffffffffffffffffff16611050565b6001545b905090565b600d60009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff16146110e5576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016110dc90613b01565b60405180910390fd5b6000801b840361112a576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161112190613ce0565b60405180910390fd5b61116e60126000868152602001908152602001600020604051806060016040529081600082015481526020016001820154815260200160028201548152505061128d565b156111ae576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016111a590613d72565b60405180910390fd5b600060405180606001604052808481526020018381526020018581525090506111d68161128d565b611215576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161120c90613e04565b60405180910390fd5b8060126000878152602001908152602001600020600082015181600001556020820151816001015560408201518160020155905050847fea0123c726a665cb0ab5691444f929a7056c7a7709c60c0587829e8046b8d51484848760405161127e939291906135ca565b60405180910390a25050505050565b60008060001b8260000151141580156112ad57506000801b826020015114155b80156112c057506000801b826040015114155b9050919050565b600d60009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff1614611357576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161134e90613b01565b60405180910390fd5b601060009054906101000a900460ff166113a6576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161139d90613e96565b60405180910390fd5b806008819055506000601060006101000a81548160ff021916908315150217905550600015157f1f5c872f1ea93c57e43112ea449ee19ef5754488b87627b4c52456b0e5a4109a826040516113fb91906132ed565b60405180910390a250565b600660009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1681565b6040518060400160405280600681526020017f76332e302e30000000000000000000000000000000000000000000000000000081525081565b601060009054906101000a900460ff1681565b6000600160038054905061148c9190613c0b565b905090565b60126020528060005260406000206000915090508060000154908060010154908060020154905083565b6000600380549050905090565b600c5481565b60015481565b6000601060009054906101000a900460ff1615611526576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161151d90613b93565b60405180910390fd5b600073ffffffffffffffffffffffffffffffffffffffff16601360009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16036115b7576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016115ae90613f28565b60405180910390fd5b6001601360146101000a81548160ff021916908315150217905550601360009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff166382ecf2f6346006898989888e8b60405160200161162d959493929190614019565b6040516020818303038152906040526040518563ffffffff1660e01b815260040161165a93929190614120565b60206040518083038185885af1158015611678573d6000803e3d6000fd5b50505050506040513d601f19601f8201168201806040525081019061169d919061419c565b90506000601360146101000a81548160ff0219169083151502179055509695505050505050565b60006116ce610fd4565b821115611710576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161170790614261565b60405180910390fd5b600060038054905011611758576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161174f90614319565b60405180910390fd5b60008060038054905090505b808210156118015760006002828461177c9190614339565b61178691906143be565b9050846003828154811061179d5761179c613c3f565b5b906000526020600020906002020160010160109054906101000a90046fffffffffffffffffffffffffffffffff166fffffffffffffffffffffffffffffffff1610156117f7576001816117f09190614339565b92506117fb565b8091505b50611764565b8192505050919050565b600381565b60025481565b600660009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff16146118a6576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161189d90614461565b60405180910390fd5b60038054905081106118ed576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016118e490614519565b60405180910390fd5b6008546003828154811061190457611903613c3f565b5b906000526020600020906002020160010160009054906101000a90046fffffffffffffffffffffffffffffffff166fffffffffffffffffffffffffffffffff164261194f9190613c0b565b1061198f576040517f08c379a0000000000000000000000000000000000000000000000000000000008152600401611986906145d1565b60405180910390fd5b60006119996114bb565b90508160035581817f4ee37ac2c786ec85e87592d3c5c8a1dd66f8496dda3f125d9ea8ca5f657629b660405160405180910390a35050565b600d60009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1681565b60055481565b600d60009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff1614611a8d576040517f08c379a0000000000000000000000000000000000000000000000000000000008152600401611a8490613b01565b60405180910390fd5b8073ffffffffffffffffffffffffffffffffffffffff16600b60009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff167f0243549a92b2412f7a3caf7a2e56d65b8821b91345363faa5f57195384065fcc60405160405180910390a380600b60006101000a81548173ffffffffffffffffffffffffffffffffffffffff021916908373ffffffffffffffffffffffffffffffffffffffff16021790555050565b601060009054906101000a900460ff16611b9c576040517f08c379a0000000000000000000000000000000000000000000000000000000008152600401611b9390613e96565b60405180910390fd5b600e60003373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060009054906101000a900460ff1680611c3d5750600e60008073ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060009054906101000a900460ff165b611c7c576040517f08c379a0000000000000000000000000000000000000000000000000000000008152600401611c7390614663565b60405180910390fd5b611c846128df565b8314611cc5576040517f08c379a0000000000000000000000000000000000000000000000000000000008152600401611cbc9061471b565b60405180910390fd5b42611ccf8461288e565b10611d0f576040517f08c379a0000000000000000000000000000000000000000000000000000000008152600401611d06906147ad565b60405180910390fd5b6000801b8403611d54576040517f08c379a0000000000000000000000000000000000000000000000000000000008152600401611d4b9061483f565b60405180910390fd5b6000801b8214611da25781814014611da1576040517f08c379a0000000000000000000000000000000000000000000000000000000008152600401611d98906148f7565b60405180910390fd5b5b82611dab6114bb565b857fa7aaf2512769da4e444e3de247be2564225c2e7a8f74cfe528e46e17d24868e242604051611ddb91906132ed565b60405180910390a460036040518060600160405280868152602001426fffffffffffffffffffffffffffffffff168152602001856fffffffffffffffffffffffffffffffff1681525090806001815401808255809150506001900390600052602060002090600202016000909190919091506000820151816000015560208201518160010160006101000a8154816fffffffffffffffffffffffffffffffff02191690836fffffffffffffffffffffffffffffffff16021790555060408201518160010160106101000a8154816fffffffffffffffffffffffffffffffff02191690836fffffffffffffffffffffffffffffffff160217905550505050505050565b600f6020528060005260406000206000915090505481565b611efd613136565b60038281548110611f1157611f10613c3f565b5b9060005260206000209060020201604051806060016040529081600082015481526020016001820160009054906101000a90046fffffffffffffffffffffffffffffffff166fffffffffffffffffffffffffffffffff166fffffffffffffffffffffffffffffffff1681526020016001820160109054906101000a90046fffffffffffffffffffffffffffffffff166fffffffffffffffffffffffffffffffff166fffffffffffffffffffffffffffffffff16815250509050919050565b601060009054906101000a900460ff161561201f576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161201690613b93565b60405180910390fd5b600e60003273ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060009054906101000a900460ff16806120c05750600e60008073ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060009054906101000a900460ff165b806120de57506011546120d16128fb565b426120dc9190613c0b565b115b61211d576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161211490614663565b60405180910390fd5b6121256128df565b841015612167576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161215e906149af565b60405180910390fd5b426121718561288e565b106121b1576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016121a8906147ad565b60405180910390fd5b600073ffffffffffffffffffffffffffffffffffffffff16601360009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff161461225b57601360149054906101000a900460ff16612256576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161224d90614a8d565b60405180910390fd5b6122ac565b601360149054906101000a900460ff16156122ab576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016122a290614b6b565b60405180910390fd5b5b6000801b85036122f1576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016122e89061483f565b60405180910390fd5b6000601260008881526020019081526020016000206040518060600160405290816000820154815260200160018201548152602001600282015481525050905061233a8161128d565b612379576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161237090614bfd565b60405180910390fd5b6000600f60008681526020019081526020016000205490506000801b81036123cd576040517f22aa3a9800000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b60006040518060e0016040528083815260200160036123ea611478565b815481106123fb576123fa613c3f565b5b906000526020600020906002020160000154815260200189815260200188815260200184604001518152602001846020015181526020018573ffffffffffffffffffffffffffffffffffffffff168152509050600b60009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff166341493c608460000151836040516020016124a29190614cc9565b604051602081830303815290604052886040518463ffffffff1660e01b81526004016124d093929190614ce4565b60006040518083038186803b1580156124e857600080fd5b505afa1580156124fc573d6000803e3d6000fd5b50505050866125096114bb565b897fa7aaf2512769da4e444e3de247be2564225c2e7a8f74cfe528e46e17d24868e24260405161253991906132ed565b60405180910390a4600360405180606001604052808a8152602001426fffffffffffffffffffffffffffffffff168152602001896fffffffffffffffffffffffffffffffff1681525090806001815401808255809150506001900390600052602060002090600202016000909190919091506000820151816000015560208201518160010160006101000a8154816fffffffffffffffffffffffffffffffff02191690836fffffffffffffffffffffffffffffffff16021790555060408201518160010160106101000a8154816fffffffffffffffffffffffffffffffff02191690836fffffffffffffffffffffffffffffffff1602179055505050505050505050505050565b600760009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1681565b600d60009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff16146126f6576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016126ed90613b01565b60405180910390fd5b6001600e60008373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060006101000a81548160ff0219169083151502179055508073ffffffffffffffffffffffffffffffffffffffff167f5df38d395edc15b669d646569bd015513395070b5b4deb8a16300abb060d1b5a600160405161279591906134d8565b60405180910390a250565b60095481565b60085481565b6127b4613136565b60036127bf836116c4565b815481106127d0576127cf613c3f565b5b9060005260206000209060020201604051806060016040529081600082015481526020016001820160009054906101000a90046fffffffffffffffffffffffffffffffff166fffffffffffffffffffffffffffffffff166fffffffffffffffffffffffffffffffff1681526020016001820160109054906101000a90046fffffffffffffffffffffffffffffffff166fffffffffffffffffffffffffffffffff166fffffffffffffffffffffffffffffffff16815250509050919050565b6000600554600154836128a19190613c0b565b6128ab9190614d29565b6002546128b89190614339565b9050919050565b600e6020528060005260406000206000915054906101000a900460ff1681565b60006004546128ec610fd4565b6128f69190614339565b905090565b60008060038054905014612973576003600160038054905061291d9190613c0b565b8154811061292e5761292d613c3f565b5b906000526020600020906002020160010160009054906101000a90046fffffffffffffffffffffffffffffffff166fffffffffffffffffffffffffffffffff16612977565b6002545b905090565b60045481565b6003600060019054906101000a900460ff161580156129b357508060ff1660008054906101000a900460ff1660ff16105b6129f2576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016129e990614df5565b60405180910390fd5b806000806101000a81548160ff021916908360ff1602179055506001600060016101000a81548160ff021916908315150217905550600082610160015111612a6f576040517f08c379a0000000000000000000000000000000000000000000000000000000008152600401612a6690614e87565b60405180910390fd5b6000826080015111612ab6576040517f08c379a0000000000000000000000000000000000000000000000000000000008152600401612aad90614f19565b60405180910390fd5b428261014001511115612afe576040517f08c379a0000000000000000000000000000000000000000000000000000000008152600401612af590614fd1565b60405180910390fd5b8161016001516004819055508160800151600581905550600060038054905003612c3e576003604051806060016040528084610100015181526020018461014001516fffffffffffffffffffffffffffffffff1681526020018461012001516fffffffffffffffffffffffffffffffff1681525090806001815401808255809150506001900390600052602060002090600202016000909190919091506000820151816000015560208201518160010160006101000a8154816fffffffffffffffffffffffffffffffff02191690836fffffffffffffffffffffffffffffffff16021790555060408201518160010160106101000a8154816fffffffffffffffffffffffffffffffff02191690836fffffffffffffffffffffffffffffffff16021790555050508161012001516001819055508161014001516002819055505b8160000151600660006101000a81548173ffffffffffffffffffffffffffffffffffffffff021916908373ffffffffffffffffffffffffffffffffffffffff16021790555081606001516008819055506001600e6000846020015173ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060006101000a81548160ff021916908315150217905550816101a0015160118190555060405180606001604052808360a0015181526020018360c0015181526020018360e00151815250601260007fae8304f40f7123e0c87b97f8a600e94ff3a3a25be588fc66b8a3717c8959ce778152602001908152602001600020600082015181600001556020820151816001015560408201518160020155905050816101800151600b60006101000a81548173ffffffffffffffffffffffffffffffffffffffff021916908373ffffffffffffffffffffffffffffffffffffffff1602179055508160400151600d60006101000a81548173ffffffffffffffffffffffffffffffffffffffff021916908373ffffffffffffffffffffffffffffffffffffffff1602179055506000601360146101000a81548160ff0219169083151502179055506000601360006101000a81548173ffffffffffffffffffffffffffffffffffffffff021916908373ffffffffffffffffffffffffffffffffffffffff16021790555060008060016101000a81548160ff0219169083151502179055507f7f26b83ff96e1f2b6a682f133852f6798a09c465da95921460cefb384740249881604051612ea291906137fa565b60405180910390a15050565b600d60009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff1614612f3e576040517f08c379a0000000000000000000000000000000000000000000000000000000008152600401612f3590613b01565b60405180910390fd5b60126000828152602001908152602001600020600080820160009055600182016000905560028201600090555050807f4432b02a2fcbed48d94e8d72723e155c6690e4b7f39afa41a2a8ff8c0aa425da60405160405180910390a250565b601360009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1681565b600d60009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff1614613052576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161304990613b01565b60405180910390fd5b8073ffffffffffffffffffffffffffffffffffffffff16600d60009054906101000a900473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff167f8be0079c531659141344cd1fd0a4f28419497f9722a3daafe3b4186f6b6457e060405160405180910390a380600d60006101000a81548173ffffffffffffffffffffffffffffffffffffffff021916908373ffffffffffffffffffffffffffffffffffffffff16021790555050565b7fae8304f40f7123e0c87b97f8a600e94ff3a3a25be588fc66b8a3717c8959ce7781565b60405180606001604052806000801916815260200160006fffffffffffffffffffffffffffffffff16815260200160006fffffffffffffffffffffffffffffffff1681525090565b6000604051905090565b600080fd5b600080fd5b600073ffffffffffffffffffffffffffffffffffffffff82169050919050565b60006131bd82613192565b9050919050565b6131cd816131b2565b81146131d857600080fd5b50565b6000813590506131ea816131c4565b92915050565b60006020828403121561320657613205613188565b5b6000613214848285016131db565b91505092915050565b6000819050919050565b6132308161321d565b811461323b57600080fd5b50565b60008135905061324d81613227565b92915050565b60006020828403121561326957613268613188565b5b60006132778482850161323e565b91505092915050565b6000819050919050565b61329381613280565b82525050565b60006020820190506132ae600083018461328a565b92915050565b6132bd816131b2565b82525050565b60006020820190506132d860008301846132b4565b92915050565b6132e78161321d565b82525050565b600060208201905061330260008301846132de565b92915050565b61331181613280565b811461331c57600080fd5b50565b60008135905061332e81613308565b92915050565b6000806000806080858703121561334e5761334d613188565b5b600061335c8782880161331f565b945050602061336d8782880161331f565b935050604061337e8782880161331f565b925050606061338f8782880161331f565b91505092959194509250565b600080fd5b6000601f19601f8301169050919050565b7f4e487b7100000000000000000000000000000000000000000000000000000000600052604160045260246000fd5b6133e9826133a0565b810181811067ffffffffffffffff82111715613408576134076133b1565b5b80604052505050565b600061341b61317e565b905061342782826133e0565b919050565b6000606082840312156134425761344161339b565b5b61344c6060613411565b9050600061345c8482850161331f565b60008301525060206134708482850161331f565b60208301525060406134848482850161331f565b60408301525092915050565b6000606082840312156134a6576134a5613188565b5b60006134b48482850161342c565b91505092915050565b60008115159050919050565b6134d2816134bd565b82525050565b60006020820190506134ed60008301846134c9565b92915050565b600081519050919050565b600082825260208201905092915050565b60005b8381101561352d578082015181840152602081019050613512565b8381111561353c576000848401525b50505050565b600061354d826134f3565b61355781856134fe565b935061356781856020860161350f565b613570816133a0565b840191505092915050565b600060208201905081810360008301526135958184613542565b905092915050565b6000602082840312156135b3576135b2613188565b5b60006135c18482850161331f565b91505092915050565b60006060820190506135df600083018661328a565b6135ec602083018561328a565b6135f9604083018461328a565b949350505050565b600080fd5b600080fd5b600067ffffffffffffffff821115613626576136256133b1565b5b61362f826133a0565b9050602081019050919050565b82818337600083830152505050565b600061365e6136598461360b565b613411565b90508281526020810184848401111561367a57613679613606565b5b61368584828561363c565b509392505050565b600082601f8301126136a2576136a1613601565b5b81356136b284826020860161364b565b91505092915050565b60008060008060008060c087890312156136d8576136d7613188565b5b60006136e689828a0161331f565b96505060206136f789828a0161331f565b955050604061370889828a0161323e565b945050606061371989828a0161323e565b935050608087013567ffffffffffffffff81111561373a5761373961318d565b5b61374689828a0161368d565b92505060a061375789828a016131db565b9150509295509295509295565b6000819050919050565b600061378961378461377f84613192565b613764565b613192565b9050919050565b600061379b8261376e565b9050919050565b60006137ad82613790565b9050919050565b6137bd816137a2565b82525050565b60006020820190506137d860008301846137b4565b92915050565b600060ff82169050919050565b6137f4816137de565b82525050565b600060208201905061380f60008301846137eb565b92915050565b6000806000806080858703121561382f5761382e613188565b5b600061383d8782880161331f565b945050602061384e8782880161323e565b935050604061385f8782880161331f565b92505060606138708782880161323e565b91505092959194509250565b61388581613280565b82525050565b60006fffffffffffffffffffffffffffffffff82169050919050565b6138b08161388b565b82525050565b6060820160008201516138cc600085018261387c565b5060208201516138df60208501826138a7565b5060408201516138f260408501826138a7565b50505050565b600060608201905061390d60008301846138b6565b92915050565b60006101c0828403121561392a5761392961339b565b5b6139356101c0613411565b90506000613945848285016131db565b6000830152506020613959848285016131db565b602083015250604061396d848285016131db565b60408301525060606139818482850161323e565b60608301525060806139958482850161323e565b60808301525060a06139a98482850161331f565b60a08301525060c06139bd8482850161331f565b60c08301525060e06139d18482850161331f565b60e0830152506101006139e68482850161331f565b610100830152506101206139fc8482850161323e565b61012083015250610140613a128482850161323e565b61014083015250610160613a288482850161323e565b61016083015250610180613a3e848285016131db565b610180830152506101a0613a548482850161323e565b6101a08301525092915050565b60006101c08284031215613a7857613a77613188565b5b6000613a8684828501613913565b91505092915050565b7f4c324f75747075744f7261636c653a2063616c6c6572206973206e6f7420746860008201527f65206f776e657200000000000000000000000000000000000000000000000000602082015250565b6000613aeb6027836134fe565b9150613af682613a8f565b604082019050919050565b60006020820190508181036000830152613b1a81613ade565b9050919050565b7f4c324f75747075744f7261636c653a206f7074696d6973746963206d6f64652060008201527f697320656e61626c656400000000000000000000000000000000000000000000602082015250565b6000613b7d602a836134fe565b9150613b8882613b21565b604082019050919050565b60006020820190508181036000830152613bac81613b70565b9050919050565b6000604082019050613bc860008301856132de565b613bd560208301846132de565b9392505050565b7f4e487b7100000000000000000000000000000000000000000000000000000000600052601160045260246000fd5b6000613c168261321d565b9150613c218361321d565b925082821015613c3457613c33613bdc565b5b828203905092915050565b7f4e487b7100000000000000000000000000000000000000000000000000000000600052603260045260246000fd5b7f4c324f75747075744f7261636c653a20636f6e666967206e616d652063616e6e60008201527f6f7420626520656d707479000000000000000000000000000000000000000000602082015250565b6000613cca602b836134fe565b9150613cd582613c6e565b604082019050919050565b60006020820190508181036000830152613cf981613cbd565b9050919050565b7f4c324f75747075744f7261636c653a20636f6e66696720616c7265616479206560008201527f7869737473000000000000000000000000000000000000000000000000000000602082015250565b6000613d5c6025836134fe565b9150613d6782613d00565b604082019050919050565b60006020820190508181036000830152613d8b81613d4f565b9050919050565b7f4c324f75747075744f7261636c653a20696e76616c6964204f5020537563636960008201527f6e637420636f6e66696775726174696f6e20706172616d657465727300000000602082015250565b6000613dee603c836134fe565b9150613df982613d92565b604082019050919050565b60006020820190508181036000830152613e1d81613de1565b9050919050565b7f4c324f75747075744f7261636c653a206f7074696d6973746963206d6f64652060008201527f6973206e6f7420656e61626c6564000000000000000000000000000000000000602082015250565b6000613e80602e836134fe565b9150613e8b82613e24565b604082019050919050565b60006020820190508181036000830152613eaf81613e73565b9050919050565b7f4c324f75747075744f7261636c653a20646973707574652067616d652066616360008201527f746f7279206973206e6f74207365740000000000000000000000000000000000602082015250565b6000613f12602f836134fe565b9150613f1d82613eb6565b604082019050919050565b60006020820190508181036000830152613f4181613f05565b9050919050565b6000819050919050565b613f63613f5e8261321d565b613f48565b82525050565b60008160601b9050919050565b6000613f8182613f69565b9050919050565b6000613f9382613f76565b9050919050565b613fab613fa6826131b2565b613f88565b82525050565b6000819050919050565b613fcc613fc782613280565b613fb1565b82525050565b600081519050919050565b600081905092915050565b6000613ff382613fd2565b613ffd8185613fdd565b935061400d81856020860161350f565b80840191505092915050565b60006140258288613f52565b6020820191506140358287613f52565b6020820191506140458286613f9a565b6014820191506140558285613fbb565b6020820191506140658284613fe8565b91508190509695505050505050565b600063ffffffff82169050919050565b600061409f61409a61409584614074565b613764565b614074565b9050919050565b6140af81614084565b82525050565b60006140c082613280565b9050919050565b6140d0816140b5565b82525050565b600082825260208201905092915050565b60006140f282613fd2565b6140fc81856140d6565b935061410c81856020860161350f565b614115816133a0565b840191505092915050565b600060608201905061413560008301866140a6565b61414260208301856140c7565b818103604083015261415481846140e7565b9050949350505050565b6000614169826131b2565b9050919050565b6141798161415e565b811461418457600080fd5b50565b60008151905061419681614170565b92915050565b6000602082840312156141b2576141b1613188565b5b60006141c084828501614187565b91505092915050565b7f4c324f75747075744f7261636c653a2063616e6e6f7420676574206f7574707560008201527f7420666f72206120626c6f636b207468617420686173206e6f74206265656e2060208201527f70726f706f736564000000000000000000000000000000000000000000000000604082015250565b600061424b6048836134fe565b9150614256826141c9565b606082019050919050565b6000602082019050818103600083015261427a8161423e565b9050919050565b7f4c324f75747075744f7261636c653a2063616e6e6f7420676574206f7574707560008201527f74206173206e6f206f7574707574732068617665206265656e2070726f706f7360208201527f6564207965740000000000000000000000000000000000000000000000000000604082015250565b60006143036046836134fe565b915061430e82614281565b606082019050919050565b60006020820190508181036000830152614332816142f6565b9050919050565b60006143448261321d565b915061434f8361321d565b9250827fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff0382111561438457614383613bdc565b5b828201905092915050565b7f4e487b7100000000000000000000000000000000000000000000000000000000600052601260045260246000fd5b60006143c98261321d565b91506143d48361321d565b9250826143e4576143e361438f565b5b828204905092915050565b7f4c324f75747075744f7261636c653a206f6e6c7920746865206368616c6c656e60008201527f67657220616464726573732063616e2064656c657465206f7574707574730000602082015250565b600061444b603e836134fe565b9150614456826143ef565b604082019050919050565b6000602082019050818103600083015261447a8161443e565b9050919050565b7f4c324f75747075744f7261636c653a2063616e6e6f742064656c657465206f7560008201527f747075747320616674657220746865206c6174657374206f757470757420696e60208201527f6465780000000000000000000000000000000000000000000000000000000000604082015250565b60006145036043836134fe565b915061450e82614481565b606082019050919050565b60006020820190508181036000830152614532816144f6565b9050919050565b7f4c324f75747075744f7261636c653a2063616e6e6f742064656c657465206f7560008201527f74707574732074686174206861766520616c7265616479206265656e2066696e60208201527f616c697a65640000000000000000000000000000000000000000000000000000604082015250565b60006145bb6046836134fe565b91506145c682614539565b606082019050919050565b600060208201905081810360008301526145ea816145ae565b9050919050565b7f4c324f75747075744f7261636c653a206f6e6c7920617070726f76656420707260008201527f6f706f736572732063616e2070726f706f7365206e6577206f75747075747300602082015250565b600061464d603f836134fe565b9150614658826145f1565b604082019050919050565b6000602082019050818103600083015261467c81614640565b9050919050565b7f4c324f75747075744f7261636c653a20626c6f636b206e756d626572206d757360008201527f7420626520657175616c20746f206e65787420657870656374656420626c6f6360208201527f6b206e756d626572000000000000000000000000000000000000000000000000604082015250565b60006147056048836134fe565b915061471082614683565b606082019050919050565b60006020820190508181036000830152614734816146f8565b9050919050565b7f4c324f75747075744f7261636c653a2063616e6e6f742070726f706f7365204c60008201527f32206f757470757420696e207468652066757475726500000000000000000000602082015250565b60006147976036836134fe565b91506147a28261473b565b604082019050919050565b600060208201905081810360008301526147c68161478a565b9050919050565b7f4c324f75747075744f7261636c653a204c32206f75747075742070726f706f7360008201527f616c2063616e6e6f7420626520746865207a65726f2068617368000000000000602082015250565b6000614829603a836134fe565b9150614834826147cd565b604082019050919050565b600060208201905081810360008301526148588161481c565b9050919050565b7f4c324f75747075744f7261636c653a20626c6f636b206861736820646f65732060008201527f6e6f74206d61746368207468652068617368206174207468652065787065637460208201527f6564206865696768740000000000000000000000000000000000000000000000604082015250565b60006148e16049836134fe565b91506148ec8261485f565b606082019050919050565b60006020820190508181036000830152614910816148d4565b9050919050565b7f4c324f75747075744f7261636c653a20626c6f636b206e756d626572206d757360008201527f742062652067726561746572207468616e206f7220657175616c20746f206e6560208201527f787420657870656374656420626c6f636b206e756d6265720000000000000000604082015250565b60006149996058836134fe565b91506149a482614917565b606082019050919050565b600060208201905081810360008301526149c88161498c565b9050919050565b7f4c324f75747075744f7261636c653a2063616e6e6f742070726f706f7365204c60008201527f32206f75747075742066726f6d206f757473696465204469737075746547616d60208201527f65466163746f72792e637265617465207768696c65206469737075746547616d60408201527f65466163746f7279206973207365740000000000000000000000000000000000606082015250565b6000614a77606f836134fe565b9150614a82826149cf565b608082019050919050565b60006020820190508181036000830152614aa681614a6a565b9050919050565b7f4c324f75747075744f7261636c653a2063616e6e6f742070726f706f7365204c60008201527f32206f75747075742066726f6d20696e73696465204469737075746547616d6560208201527f466163746f72792e63726561746520776974686f75742073657474696e67206460408201527f69737075746547616d65466163746f7279000000000000000000000000000000606082015250565b6000614b556071836134fe565b9150614b6082614aad565b608082019050919050565b60006020820190508181036000830152614b8481614b48565b9050919050565b7f4c324f75747075744f7261636c653a20696e76616c6964204f5020537563636960008201527f6e637420636f6e66696775726174696f6e000000000000000000000000000000602082015250565b6000614be76031836134fe565b9150614bf282614b8b565b604082019050919050565b60006020820190508181036000830152614c1681614bda565b9050919050565b614c268161321d565b82525050565b614c35816131b2565b82525050565b60e082016000820151614c51600085018261387c565b506020820151614c64602085018261387c565b506040820151614c77604085018261387c565b506060820151614c8a6060850182614c1d565b506080820151614c9d608085018261387c565b5060a0820151614cb060a085018261387c565b5060c0820151614cc360c0850182614c2c565b50505050565b600060e082019050614cde6000830184614c3b565b92915050565b6000606082019050614cf9600083018661328a565b8181036020830152614d0b81856140e7565b90508181036040830152614d1f81846140e7565b9050949350505050565b6000614d348261321d565b9150614d3f8361321d565b9250817fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff0483118215151615614d7857614d77613bdc565b5b828202905092915050565b7f496e697469616c697a61626c653a20636f6e747261637420697320616c72656160008201527f647920696e697469616c697a6564000000000000000000000000000000000000602082015250565b6000614ddf602e836134fe565b9150614dea82614d83565b604082019050919050565b60006020820190508181036000830152614e0e81614dd2565b9050919050565b7f4c324f75747075744f7261636c653a207375626d697373696f6e20696e74657260008201527f76616c206d7573742062652067726561746572207468616e2030000000000000602082015250565b6000614e71603a836134fe565b9150614e7c82614e15565b604082019050919050565b60006020820190508181036000830152614ea081614e64565b9050919050565b7f4c324f75747075744f7261636c653a204c3220626c6f636b2074696d65206d7560008201527f73742062652067726561746572207468616e2030000000000000000000000000602082015250565b6000614f036034836134fe565b9150614f0e82614ea7565b604082019050919050565b60006020820190508181036000830152614f3281614ef6565b9050919050565b7f4c324f75747075744f7261636c653a207374617274696e67204c322074696d6560008201527f7374616d70206d757374206265206c657373207468616e2063757272656e742060208201527f74696d6500000000000000000000000000000000000000000000000000000000604082015250565b6000614fbb6044836134fe565b9150614fc682614f39565b606082019050919050565b60006020820190508181036000830152614fea81614fae565b905091905056fea2646970667358221220c8fce71cb2285819efbc3932c6dda6bf3cd5fd6de428d9085743d40c908872e264736f6c634300080f0033",
}

// OPSuccinctL2OutputOracleABI is the input ABI used to generate the binding from.
// Deprecated: Use OPSuccinctL2OutputOracleMetaData.ABI instead.
var OPSuccinctL2OutputOracleABI = OPSuccinctL2OutputOracleMetaData.ABI

// OPSuccinctL2OutputOracleBin is the compiled bytecode used for deploying new contracts.
// Deprecated: Use OPSuccinctL2OutputOracleMetaData.Bin instead.
var OPSuccinctL2OutputOracleBin = OPSuccinctL2OutputOracleMetaData.Bin

// DeployOPSuccinctL2OutputOracle deploys a new Ethereum contract, binding an instance of OPSuccinctL2OutputOracle to it.
func DeployOPSuccinctL2OutputOracle(auth *bind.TransactOpts, backend bind.ContractBackend) (common.Address, *types.Transaction, *OPSuccinctL2OutputOracle, error) {
	parsed, err := OPSuccinctL2OutputOracleMetaData.GetAbi()
	if err != nil {
		return common.Address{}, nil, nil, err
	}
	if parsed == nil {
		return common.Address{}, nil, nil, errors.New("GetABI returned nil")
	}

	address, tx, contract, err := bind.DeployContract(auth, *parsed, common.FromHex(OPSuccinctL2OutputOracleBin), backend)
	if err != nil {
		return common.Address{}, nil, nil, err
	}
	return address, tx, &OPSuccinctL2OutputOracle{OPSuccinctL2OutputOracleCaller: OPSuccinctL2OutputOracleCaller{contract: contract}, OPSuccinctL2OutputOracleTransactor: OPSuccinctL2OutputOracleTransactor{contract: contract}, OPSuccinctL2OutputOracleFilterer: OPSuccinctL2OutputOracleFilterer{contract: contract}}, nil
}

// OPSuccinctL2OutputOracle is an auto generated Go binding around an Ethereum contract.
type OPSuccinctL2OutputOracle struct {
	OPSuccinctL2OutputOracleCaller     // Read-only binding to the contract
	OPSuccinctL2OutputOracleTransactor // Write-only binding to the contract
	OPSuccinctL2OutputOracleFilterer   // Log filterer for contract events
}

// OPSuccinctL2OutputOracleCaller is an auto generated read-only Go binding around an Ethereum contract.
type OPSuccinctL2OutputOracleCaller struct {
	contract *bind.BoundContract // Generic contract wrapper for the low level calls
}

// OPSuccinctL2OutputOracleTransactor is an auto generated write-only Go binding around an Ethereum contract.
type OPSuccinctL2OutputOracleTransactor struct {
	contract *bind.BoundContract // Generic contract wrapper for the low level calls
}

// OPSuccinctL2OutputOracleFilterer is an auto generated log filtering Go binding around an Ethereum contract events.
type OPSuccinctL2OutputOracleFilterer struct {
	contract *bind.BoundContract // Generic contract wrapper for the low level calls
}

// OPSuccinctL2OutputOracleSession is an auto generated Go binding around an Ethereum contract,
// with pre-set call and transact options.
type OPSuccinctL2OutputOracleSession struct {
	Contract     *OPSuccinctL2OutputOracle // Generic contract binding to set the session for
	CallOpts     bind.CallOpts             // Call options to use throughout this session
	TransactOpts bind.TransactOpts         // Transaction auth options to use throughout this session
}

// OPSuccinctL2OutputOracleCallerSession is an auto generated read-only Go binding around an Ethereum contract,
// with pre-set call options.
type OPSuccinctL2OutputOracleCallerSession struct {
	Contract *OPSuccinctL2OutputOracleCaller // Generic contract caller binding to set the session for
	CallOpts bind.CallOpts                   // Call options to use throughout this session
}

// OPSuccinctL2OutputOracleTransactorSession is an auto generated write-only Go binding around an Ethereum contract,
// with pre-set transact options.
type OPSuccinctL2OutputOracleTransactorSession struct {
	Contract     *OPSuccinctL2OutputOracleTransactor // Generic contract transactor binding to set the session for
	TransactOpts bind.TransactOpts                   // Transaction auth options to use throughout this session
}

// OPSuccinctL2OutputOracleRaw is an auto generated low-level Go binding around an Ethereum contract.
type OPSuccinctL2OutputOracleRaw struct {
	Contract *OPSuccinctL2OutputOracle // Generic contract binding to access the raw methods on
}

// OPSuccinctL2OutputOracleCallerRaw is an auto generated low-level read-only Go binding around an Ethereum contract.
type OPSuccinctL2OutputOracleCallerRaw struct {
	Contract *OPSuccinctL2OutputOracleCaller // Generic read-only contract binding to access the raw methods on
}

// OPSuccinctL2OutputOracleTransactorRaw is an auto generated low-level write-only Go binding around an Ethereum contract.
type OPSuccinctL2OutputOracleTransactorRaw struct {
	Contract *OPSuccinctL2OutputOracleTransactor // Generic write-only contract binding to access the raw methods on
}

// NewOPSuccinctL2OutputOracle creates a new instance of OPSuccinctL2OutputOracle, bound to a specific deployed contract.
func NewOPSuccinctL2OutputOracle(address common.Address, backend bind.ContractBackend) (*OPSuccinctL2OutputOracle, error) {
	contract, err := bindOPSuccinctL2OutputOracle(address, backend, backend, backend)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctL2OutputOracle{OPSuccinctL2OutputOracleCaller: OPSuccinctL2OutputOracleCaller{contract: contract}, OPSuccinctL2OutputOracleTransactor: OPSuccinctL2OutputOracleTransactor{contract: contract}, OPSuccinctL2OutputOracleFilterer: OPSuccinctL2OutputOracleFilterer{contract: contract}}, nil
}

// NewOPSuccinctL2OutputOracleCaller creates a new read-only instance of OPSuccinctL2OutputOracle, bound to a specific deployed contract.
func NewOPSuccinctL2OutputOracleCaller(address common.Address, caller bind.ContractCaller) (*OPSuccinctL2OutputOracleCaller, error) {
	contract, err := bindOPSuccinctL2OutputOracle(address, caller, nil, nil)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctL2OutputOracleCaller{contract: contract}, nil
}

// NewOPSuccinctL2OutputOracleTransactor creates a new write-only instance of OPSuccinctL2OutputOracle, bound to a specific deployed contract.
func NewOPSuccinctL2OutputOracleTransactor(address common.Address, transactor bind.ContractTransactor) (*OPSuccinctL2OutputOracleTransactor, error) {
	contract, err := bindOPSuccinctL2OutputOracle(address, nil, transactor, nil)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctL2OutputOracleTransactor{contract: contract}, nil
}

// NewOPSuccinctL2OutputOracleFilterer creates a new log filterer instance of OPSuccinctL2OutputOracle, bound to a specific deployed contract.
func NewOPSuccinctL2OutputOracleFilterer(address common.Address, filterer bind.ContractFilterer) (*OPSuccinctL2OutputOracleFilterer, error) {
	contract, err := bindOPSuccinctL2OutputOracle(address, nil, nil, filterer)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctL2OutputOracleFilterer{contract: contract}, nil
}

// bindOPSuccinctL2OutputOracle binds a generic wrapper to an already deployed contract.
func bindOPSuccinctL2OutputOracle(address common.Address, caller bind.ContractCaller, transactor bind.ContractTransactor, filterer bind.ContractFilterer) (*bind.BoundContract, error) {
	parsed, err := OPSuccinctL2OutputOracleMetaData.GetAbi()
	if err != nil {
		return nil, err
	}
	return bind.NewBoundContract(address, *parsed, caller, transactor, filterer), nil
}

// Call invokes the (constant) contract method with params as input values and
// sets the output to result. The result type might be a single field for simple
// returns, a slice of interfaces for anonymous returns and a struct for named
// returns.
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleRaw) Call(opts *bind.CallOpts, result *[]interface{}, method string, params ...interface{}) error {
	return _OPSuccinctL2OutputOracle.Contract.OPSuccinctL2OutputOracleCaller.contract.Call(opts, result, method, params...)
}

// Transfer initiates a plain transaction to move funds to the contract, calling
// its default method if one is available.
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleRaw) Transfer(opts *bind.TransactOpts) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.OPSuccinctL2OutputOracleTransactor.contract.Transfer(opts)
}

// Transact invokes the (paid) contract method with params as input values.
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleRaw) Transact(opts *bind.TransactOpts, method string, params ...interface{}) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.OPSuccinctL2OutputOracleTransactor.contract.Transact(opts, method, params...)
}

// Call invokes the (constant) contract method with params as input values and
// sets the output to result. The result type might be a single field for simple
// returns, a slice of interfaces for anonymous returns and a struct for named
// returns.
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerRaw) Call(opts *bind.CallOpts, result *[]interface{}, method string, params ...interface{}) error {
	return _OPSuccinctL2OutputOracle.Contract.contract.Call(opts, result, method, params...)
}

// Transfer initiates a plain transaction to move funds to the contract, calling
// its default method if one is available.
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorRaw) Transfer(opts *bind.TransactOpts) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.contract.Transfer(opts)
}

// Transact invokes the (paid) contract method with params as input values.
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorRaw) Transact(opts *bind.TransactOpts, method string, params ...interface{}) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.contract.Transact(opts, method, params...)
}

// GENESISCONFIGNAME is a free data retrieval call binding the contract method 0xf72f606d.
//
// Solidity: function GENESIS_CONFIG_NAME() view returns(bytes32)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) GENESISCONFIGNAME(opts *bind.CallOpts) ([32]byte, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "GENESIS_CONFIG_NAME")

	if err != nil {
		return *new([32]byte), err
	}

	out0 := *abi.ConvertType(out[0], new([32]byte)).(*[32]byte)

	return out0, err

}

// GENESISCONFIGNAME is a free data retrieval call binding the contract method 0xf72f606d.
//
// Solidity: function GENESIS_CONFIG_NAME() view returns(bytes32)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) GENESISCONFIGNAME() ([32]byte, error) {
	return _OPSuccinctL2OutputOracle.Contract.GENESISCONFIGNAME(&_OPSuccinctL2OutputOracle.CallOpts)
}

// GENESISCONFIGNAME is a free data retrieval call binding the contract method 0xf72f606d.
//
// Solidity: function GENESIS_CONFIG_NAME() view returns(bytes32)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) GENESISCONFIGNAME() ([32]byte, error) {
	return _OPSuccinctL2OutputOracle.Contract.GENESISCONFIGNAME(&_OPSuccinctL2OutputOracle.CallOpts)
}

// AggregationVkey is a free data retrieval call binding the contract method 0xc32e4e3e.
//
// Solidity: function aggregationVkey() view returns(bytes32)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) AggregationVkey(opts *bind.CallOpts) ([32]byte, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "aggregationVkey")

	if err != nil {
		return *new([32]byte), err
	}

	out0 := *abi.ConvertType(out[0], new([32]byte)).(*[32]byte)

	return out0, err

}

// AggregationVkey is a free data retrieval call binding the contract method 0xc32e4e3e.
//
// Solidity: function aggregationVkey() view returns(bytes32)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) AggregationVkey() ([32]byte, error) {
	return _OPSuccinctL2OutputOracle.Contract.AggregationVkey(&_OPSuccinctL2OutputOracle.CallOpts)
}

// AggregationVkey is a free data retrieval call binding the contract method 0xc32e4e3e.
//
// Solidity: function aggregationVkey() view returns(bytes32)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) AggregationVkey() ([32]byte, error) {
	return _OPSuccinctL2OutputOracle.Contract.AggregationVkey(&_OPSuccinctL2OutputOracle.CallOpts)
}

// ApprovedProposers is a free data retrieval call binding the contract method 0xd4651276.
//
// Solidity: function approvedProposers(address ) view returns(bool)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) ApprovedProposers(opts *bind.CallOpts, arg0 common.Address) (bool, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "approvedProposers", arg0)

	if err != nil {
		return *new(bool), err
	}

	out0 := *abi.ConvertType(out[0], new(bool)).(*bool)

	return out0, err

}

// ApprovedProposers is a free data retrieval call binding the contract method 0xd4651276.
//
// Solidity: function approvedProposers(address ) view returns(bool)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) ApprovedProposers(arg0 common.Address) (bool, error) {
	return _OPSuccinctL2OutputOracle.Contract.ApprovedProposers(&_OPSuccinctL2OutputOracle.CallOpts, arg0)
}

// ApprovedProposers is a free data retrieval call binding the contract method 0xd4651276.
//
// Solidity: function approvedProposers(address ) view returns(bool)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) ApprovedProposers(arg0 common.Address) (bool, error) {
	return _OPSuccinctL2OutputOracle.Contract.ApprovedProposers(&_OPSuccinctL2OutputOracle.CallOpts, arg0)
}

// Challenger is a free data retrieval call binding the contract method 0x534db0e2.
//
// Solidity: function challenger() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) Challenger(opts *bind.CallOpts) (common.Address, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "challenger")

	if err != nil {
		return *new(common.Address), err
	}

	out0 := *abi.ConvertType(out[0], new(common.Address)).(*common.Address)

	return out0, err

}

// Challenger is a free data retrieval call binding the contract method 0x534db0e2.
//
// Solidity: function challenger() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) Challenger() (common.Address, error) {
	return _OPSuccinctL2OutputOracle.Contract.Challenger(&_OPSuccinctL2OutputOracle.CallOpts)
}

// Challenger is a free data retrieval call binding the contract method 0x534db0e2.
//
// Solidity: function challenger() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) Challenger() (common.Address, error) {
	return _OPSuccinctL2OutputOracle.Contract.Challenger(&_OPSuccinctL2OutputOracle.CallOpts)
}

// ComputeL2Timestamp is a free data retrieval call binding the contract method 0xd1de856c.
//
// Solidity: function computeL2Timestamp(uint256 _l2BlockNumber) view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) ComputeL2Timestamp(opts *bind.CallOpts, _l2BlockNumber *big.Int) (*big.Int, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "computeL2Timestamp", _l2BlockNumber)

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// ComputeL2Timestamp is a free data retrieval call binding the contract method 0xd1de856c.
//
// Solidity: function computeL2Timestamp(uint256 _l2BlockNumber) view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) ComputeL2Timestamp(_l2BlockNumber *big.Int) (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.ComputeL2Timestamp(&_OPSuccinctL2OutputOracle.CallOpts, _l2BlockNumber)
}

// ComputeL2Timestamp is a free data retrieval call binding the contract method 0xd1de856c.
//
// Solidity: function computeL2Timestamp(uint256 _l2BlockNumber) view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) ComputeL2Timestamp(_l2BlockNumber *big.Int) (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.ComputeL2Timestamp(&_OPSuccinctL2OutputOracle.CallOpts, _l2BlockNumber)
}

// DisputeGameFactory is a free data retrieval call binding the contract method 0xf2b4e617.
//
// Solidity: function disputeGameFactory() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) DisputeGameFactory(opts *bind.CallOpts) (common.Address, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "disputeGameFactory")

	if err != nil {
		return *new(common.Address), err
	}

	out0 := *abi.ConvertType(out[0], new(common.Address)).(*common.Address)

	return out0, err

}

// DisputeGameFactory is a free data retrieval call binding the contract method 0xf2b4e617.
//
// Solidity: function disputeGameFactory() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) DisputeGameFactory() (common.Address, error) {
	return _OPSuccinctL2OutputOracle.Contract.DisputeGameFactory(&_OPSuccinctL2OutputOracle.CallOpts)
}

// DisputeGameFactory is a free data retrieval call binding the contract method 0xf2b4e617.
//
// Solidity: function disputeGameFactory() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) DisputeGameFactory() (common.Address, error) {
	return _OPSuccinctL2OutputOracle.Contract.DisputeGameFactory(&_OPSuccinctL2OutputOracle.CallOpts)
}

// FallbackTimeout is a free data retrieval call binding the contract method 0x4277bc06.
//
// Solidity: function fallbackTimeout() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) FallbackTimeout(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "fallbackTimeout")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// FallbackTimeout is a free data retrieval call binding the contract method 0x4277bc06.
//
// Solidity: function fallbackTimeout() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) FallbackTimeout() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.FallbackTimeout(&_OPSuccinctL2OutputOracle.CallOpts)
}

// FallbackTimeout is a free data retrieval call binding the contract method 0x4277bc06.
//
// Solidity: function fallbackTimeout() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) FallbackTimeout() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.FallbackTimeout(&_OPSuccinctL2OutputOracle.CallOpts)
}

// FinalizationPeriodSeconds is a free data retrieval call binding the contract method 0xce5db8d6.
//
// Solidity: function finalizationPeriodSeconds() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) FinalizationPeriodSeconds(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "finalizationPeriodSeconds")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// FinalizationPeriodSeconds is a free data retrieval call binding the contract method 0xce5db8d6.
//
// Solidity: function finalizationPeriodSeconds() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) FinalizationPeriodSeconds() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.FinalizationPeriodSeconds(&_OPSuccinctL2OutputOracle.CallOpts)
}

// FinalizationPeriodSeconds is a free data retrieval call binding the contract method 0xce5db8d6.
//
// Solidity: function finalizationPeriodSeconds() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) FinalizationPeriodSeconds() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.FinalizationPeriodSeconds(&_OPSuccinctL2OutputOracle.CallOpts)
}

// GetL2Output is a free data retrieval call binding the contract method 0xa25ae557.
//
// Solidity: function getL2Output(uint256 _l2OutputIndex) view returns((bytes32,uint128,uint128))
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) GetL2Output(opts *bind.CallOpts, _l2OutputIndex *big.Int) (TypesOutputProposal, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "getL2Output", _l2OutputIndex)

	if err != nil {
		return *new(TypesOutputProposal), err
	}

	out0 := *abi.ConvertType(out[0], new(TypesOutputProposal)).(*TypesOutputProposal)

	return out0, err

}

// GetL2Output is a free data retrieval call binding the contract method 0xa25ae557.
//
// Solidity: function getL2Output(uint256 _l2OutputIndex) view returns((bytes32,uint128,uint128))
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) GetL2Output(_l2OutputIndex *big.Int) (TypesOutputProposal, error) {
	return _OPSuccinctL2OutputOracle.Contract.GetL2Output(&_OPSuccinctL2OutputOracle.CallOpts, _l2OutputIndex)
}

// GetL2Output is a free data retrieval call binding the contract method 0xa25ae557.
//
// Solidity: function getL2Output(uint256 _l2OutputIndex) view returns((bytes32,uint128,uint128))
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) GetL2Output(_l2OutputIndex *big.Int) (TypesOutputProposal, error) {
	return _OPSuccinctL2OutputOracle.Contract.GetL2Output(&_OPSuccinctL2OutputOracle.CallOpts, _l2OutputIndex)
}

// GetL2OutputAfter is a free data retrieval call binding the contract method 0xcf8e5cf0.
//
// Solidity: function getL2OutputAfter(uint256 _l2BlockNumber) view returns((bytes32,uint128,uint128))
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) GetL2OutputAfter(opts *bind.CallOpts, _l2BlockNumber *big.Int) (TypesOutputProposal, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "getL2OutputAfter", _l2BlockNumber)

	if err != nil {
		return *new(TypesOutputProposal), err
	}

	out0 := *abi.ConvertType(out[0], new(TypesOutputProposal)).(*TypesOutputProposal)

	return out0, err

}

// GetL2OutputAfter is a free data retrieval call binding the contract method 0xcf8e5cf0.
//
// Solidity: function getL2OutputAfter(uint256 _l2BlockNumber) view returns((bytes32,uint128,uint128))
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) GetL2OutputAfter(_l2BlockNumber *big.Int) (TypesOutputProposal, error) {
	return _OPSuccinctL2OutputOracle.Contract.GetL2OutputAfter(&_OPSuccinctL2OutputOracle.CallOpts, _l2BlockNumber)
}

// GetL2OutputAfter is a free data retrieval call binding the contract method 0xcf8e5cf0.
//
// Solidity: function getL2OutputAfter(uint256 _l2BlockNumber) view returns((bytes32,uint128,uint128))
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) GetL2OutputAfter(_l2BlockNumber *big.Int) (TypesOutputProposal, error) {
	return _OPSuccinctL2OutputOracle.Contract.GetL2OutputAfter(&_OPSuccinctL2OutputOracle.CallOpts, _l2BlockNumber)
}

// GetL2OutputIndexAfter is a free data retrieval call binding the contract method 0x7f006420.
//
// Solidity: function getL2OutputIndexAfter(uint256 _l2BlockNumber) view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) GetL2OutputIndexAfter(opts *bind.CallOpts, _l2BlockNumber *big.Int) (*big.Int, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "getL2OutputIndexAfter", _l2BlockNumber)

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// GetL2OutputIndexAfter is a free data retrieval call binding the contract method 0x7f006420.
//
// Solidity: function getL2OutputIndexAfter(uint256 _l2BlockNumber) view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) GetL2OutputIndexAfter(_l2BlockNumber *big.Int) (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.GetL2OutputIndexAfter(&_OPSuccinctL2OutputOracle.CallOpts, _l2BlockNumber)
}

// GetL2OutputIndexAfter is a free data retrieval call binding the contract method 0x7f006420.
//
// Solidity: function getL2OutputIndexAfter(uint256 _l2BlockNumber) view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) GetL2OutputIndexAfter(_l2BlockNumber *big.Int) (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.GetL2OutputIndexAfter(&_OPSuccinctL2OutputOracle.CallOpts, _l2BlockNumber)
}

// HistoricBlockHashes is a free data retrieval call binding the contract method 0xa196b525.
//
// Solidity: function historicBlockHashes(uint256 ) view returns(bytes32)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) HistoricBlockHashes(opts *bind.CallOpts, arg0 *big.Int) ([32]byte, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "historicBlockHashes", arg0)

	if err != nil {
		return *new([32]byte), err
	}

	out0 := *abi.ConvertType(out[0], new([32]byte)).(*[32]byte)

	return out0, err

}

// HistoricBlockHashes is a free data retrieval call binding the contract method 0xa196b525.
//
// Solidity: function historicBlockHashes(uint256 ) view returns(bytes32)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) HistoricBlockHashes(arg0 *big.Int) ([32]byte, error) {
	return _OPSuccinctL2OutputOracle.Contract.HistoricBlockHashes(&_OPSuccinctL2OutputOracle.CallOpts, arg0)
}

// HistoricBlockHashes is a free data retrieval call binding the contract method 0xa196b525.
//
// Solidity: function historicBlockHashes(uint256 ) view returns(bytes32)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) HistoricBlockHashes(arg0 *big.Int) ([32]byte, error) {
	return _OPSuccinctL2OutputOracle.Contract.HistoricBlockHashes(&_OPSuccinctL2OutputOracle.CallOpts, arg0)
}

// InitializerVersion is a free data retrieval call binding the contract method 0x7f01ea68.
//
// Solidity: function initializerVersion() view returns(uint8)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) InitializerVersion(opts *bind.CallOpts) (uint8, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "initializerVersion")

	if err != nil {
		return *new(uint8), err
	}

	out0 := *abi.ConvertType(out[0], new(uint8)).(*uint8)

	return out0, err

}

// InitializerVersion is a free data retrieval call binding the contract method 0x7f01ea68.
//
// Solidity: function initializerVersion() view returns(uint8)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) InitializerVersion() (uint8, error) {
	return _OPSuccinctL2OutputOracle.Contract.InitializerVersion(&_OPSuccinctL2OutputOracle.CallOpts)
}

// InitializerVersion is a free data retrieval call binding the contract method 0x7f01ea68.
//
// Solidity: function initializerVersion() view returns(uint8)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) InitializerVersion() (uint8, error) {
	return _OPSuccinctL2OutputOracle.Contract.InitializerVersion(&_OPSuccinctL2OutputOracle.CallOpts)
}

// IsValidOpSuccinctConfig is a free data retrieval call binding the contract method 0x49185e06.
//
// Solidity: function isValidOpSuccinctConfig((bytes32,bytes32,bytes32) _config) pure returns(bool)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) IsValidOpSuccinctConfig(opts *bind.CallOpts, _config OPSuccinctL2OutputOracleOpSuccinctConfig) (bool, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "isValidOpSuccinctConfig", _config)

	if err != nil {
		return *new(bool), err
	}

	out0 := *abi.ConvertType(out[0], new(bool)).(*bool)

	return out0, err

}

// IsValidOpSuccinctConfig is a free data retrieval call binding the contract method 0x49185e06.
//
// Solidity: function isValidOpSuccinctConfig((bytes32,bytes32,bytes32) _config) pure returns(bool)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) IsValidOpSuccinctConfig(_config OPSuccinctL2OutputOracleOpSuccinctConfig) (bool, error) {
	return _OPSuccinctL2OutputOracle.Contract.IsValidOpSuccinctConfig(&_OPSuccinctL2OutputOracle.CallOpts, _config)
}

// IsValidOpSuccinctConfig is a free data retrieval call binding the contract method 0x49185e06.
//
// Solidity: function isValidOpSuccinctConfig((bytes32,bytes32,bytes32) _config) pure returns(bool)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) IsValidOpSuccinctConfig(_config OPSuccinctL2OutputOracleOpSuccinctConfig) (bool, error) {
	return _OPSuccinctL2OutputOracle.Contract.IsValidOpSuccinctConfig(&_OPSuccinctL2OutputOracle.CallOpts, _config)
}

// L2BlockTime is a free data retrieval call binding the contract method 0x93991af3.
//
// Solidity: function l2BlockTime() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) L2BlockTime(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "l2BlockTime")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// L2BlockTime is a free data retrieval call binding the contract method 0x93991af3.
//
// Solidity: function l2BlockTime() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) L2BlockTime() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.L2BlockTime(&_OPSuccinctL2OutputOracle.CallOpts)
}

// L2BlockTime is a free data retrieval call binding the contract method 0x93991af3.
//
// Solidity: function l2BlockTime() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) L2BlockTime() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.L2BlockTime(&_OPSuccinctL2OutputOracle.CallOpts)
}

// LastProposalTimestamp is a free data retrieval call binding the contract method 0xe0c2f935.
//
// Solidity: function lastProposalTimestamp() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) LastProposalTimestamp(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "lastProposalTimestamp")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// LastProposalTimestamp is a free data retrieval call binding the contract method 0xe0c2f935.
//
// Solidity: function lastProposalTimestamp() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) LastProposalTimestamp() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.LastProposalTimestamp(&_OPSuccinctL2OutputOracle.CallOpts)
}

// LastProposalTimestamp is a free data retrieval call binding the contract method 0xe0c2f935.
//
// Solidity: function lastProposalTimestamp() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) LastProposalTimestamp() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.LastProposalTimestamp(&_OPSuccinctL2OutputOracle.CallOpts)
}

// LatestBlockNumber is a free data retrieval call binding the contract method 0x4599c788.
//
// Solidity: function latestBlockNumber() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) LatestBlockNumber(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "latestBlockNumber")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// LatestBlockNumber is a free data retrieval call binding the contract method 0x4599c788.
//
// Solidity: function latestBlockNumber() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) LatestBlockNumber() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.LatestBlockNumber(&_OPSuccinctL2OutputOracle.CallOpts)
}

// LatestBlockNumber is a free data retrieval call binding the contract method 0x4599c788.
//
// Solidity: function latestBlockNumber() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) LatestBlockNumber() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.LatestBlockNumber(&_OPSuccinctL2OutputOracle.CallOpts)
}

// LatestOutputIndex is a free data retrieval call binding the contract method 0x69f16eec.
//
// Solidity: function latestOutputIndex() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) LatestOutputIndex(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "latestOutputIndex")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// LatestOutputIndex is a free data retrieval call binding the contract method 0x69f16eec.
//
// Solidity: function latestOutputIndex() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) LatestOutputIndex() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.LatestOutputIndex(&_OPSuccinctL2OutputOracle.CallOpts)
}

// LatestOutputIndex is a free data retrieval call binding the contract method 0x69f16eec.
//
// Solidity: function latestOutputIndex() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) LatestOutputIndex() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.LatestOutputIndex(&_OPSuccinctL2OutputOracle.CallOpts)
}

// NextBlockNumber is a free data retrieval call binding the contract method 0xdcec3348.
//
// Solidity: function nextBlockNumber() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) NextBlockNumber(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "nextBlockNumber")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// NextBlockNumber is a free data retrieval call binding the contract method 0xdcec3348.
//
// Solidity: function nextBlockNumber() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) NextBlockNumber() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.NextBlockNumber(&_OPSuccinctL2OutputOracle.CallOpts)
}

// NextBlockNumber is a free data retrieval call binding the contract method 0xdcec3348.
//
// Solidity: function nextBlockNumber() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) NextBlockNumber() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.NextBlockNumber(&_OPSuccinctL2OutputOracle.CallOpts)
}

// NextOutputIndex is a free data retrieval call binding the contract method 0x6abcf563.
//
// Solidity: function nextOutputIndex() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) NextOutputIndex(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "nextOutputIndex")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// NextOutputIndex is a free data retrieval call binding the contract method 0x6abcf563.
//
// Solidity: function nextOutputIndex() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) NextOutputIndex() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.NextOutputIndex(&_OPSuccinctL2OutputOracle.CallOpts)
}

// NextOutputIndex is a free data retrieval call binding the contract method 0x6abcf563.
//
// Solidity: function nextOutputIndex() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) NextOutputIndex() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.NextOutputIndex(&_OPSuccinctL2OutputOracle.CallOpts)
}

// OpSuccinctConfigs is a free data retrieval call binding the contract method 0x6a56620b.
//
// Solidity: function opSuccinctConfigs(bytes32 ) view returns(bytes32 aggregationVkey, bytes32 rangeVkeyCommitment, bytes32 rollupConfigHash)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) OpSuccinctConfigs(opts *bind.CallOpts, arg0 [32]byte) (struct {
	AggregationVkey     [32]byte
	RangeVkeyCommitment [32]byte
	RollupConfigHash    [32]byte
}, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "opSuccinctConfigs", arg0)

	outstruct := new(struct {
		AggregationVkey     [32]byte
		RangeVkeyCommitment [32]byte
		RollupConfigHash    [32]byte
	})
	if err != nil {
		return *outstruct, err
	}

	outstruct.AggregationVkey = *abi.ConvertType(out[0], new([32]byte)).(*[32]byte)
	outstruct.RangeVkeyCommitment = *abi.ConvertType(out[1], new([32]byte)).(*[32]byte)
	outstruct.RollupConfigHash = *abi.ConvertType(out[2], new([32]byte)).(*[32]byte)

	return *outstruct, err

}

// OpSuccinctConfigs is a free data retrieval call binding the contract method 0x6a56620b.
//
// Solidity: function opSuccinctConfigs(bytes32 ) view returns(bytes32 aggregationVkey, bytes32 rangeVkeyCommitment, bytes32 rollupConfigHash)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) OpSuccinctConfigs(arg0 [32]byte) (struct {
	AggregationVkey     [32]byte
	RangeVkeyCommitment [32]byte
	RollupConfigHash    [32]byte
}, error) {
	return _OPSuccinctL2OutputOracle.Contract.OpSuccinctConfigs(&_OPSuccinctL2OutputOracle.CallOpts, arg0)
}

// OpSuccinctConfigs is a free data retrieval call binding the contract method 0x6a56620b.
//
// Solidity: function opSuccinctConfigs(bytes32 ) view returns(bytes32 aggregationVkey, bytes32 rangeVkeyCommitment, bytes32 rollupConfigHash)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) OpSuccinctConfigs(arg0 [32]byte) (struct {
	AggregationVkey     [32]byte
	RangeVkeyCommitment [32]byte
	RollupConfigHash    [32]byte
}, error) {
	return _OPSuccinctL2OutputOracle.Contract.OpSuccinctConfigs(&_OPSuccinctL2OutputOracle.CallOpts, arg0)
}

// OptimisticMode is a free data retrieval call binding the contract method 0x60caf7a0.
//
// Solidity: function optimisticMode() view returns(bool)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) OptimisticMode(opts *bind.CallOpts) (bool, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "optimisticMode")

	if err != nil {
		return *new(bool), err
	}

	out0 := *abi.ConvertType(out[0], new(bool)).(*bool)

	return out0, err

}

// OptimisticMode is a free data retrieval call binding the contract method 0x60caf7a0.
//
// Solidity: function optimisticMode() view returns(bool)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) OptimisticMode() (bool, error) {
	return _OPSuccinctL2OutputOracle.Contract.OptimisticMode(&_OPSuccinctL2OutputOracle.CallOpts)
}

// OptimisticMode is a free data retrieval call binding the contract method 0x60caf7a0.
//
// Solidity: function optimisticMode() view returns(bool)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) OptimisticMode() (bool, error) {
	return _OPSuccinctL2OutputOracle.Contract.OptimisticMode(&_OPSuccinctL2OutputOracle.CallOpts)
}

// Owner is a free data retrieval call binding the contract method 0x8da5cb5b.
//
// Solidity: function owner() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) Owner(opts *bind.CallOpts) (common.Address, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "owner")

	if err != nil {
		return *new(common.Address), err
	}

	out0 := *abi.ConvertType(out[0], new(common.Address)).(*common.Address)

	return out0, err

}

// Owner is a free data retrieval call binding the contract method 0x8da5cb5b.
//
// Solidity: function owner() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) Owner() (common.Address, error) {
	return _OPSuccinctL2OutputOracle.Contract.Owner(&_OPSuccinctL2OutputOracle.CallOpts)
}

// Owner is a free data retrieval call binding the contract method 0x8da5cb5b.
//
// Solidity: function owner() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) Owner() (common.Address, error) {
	return _OPSuccinctL2OutputOracle.Contract.Owner(&_OPSuccinctL2OutputOracle.CallOpts)
}

// Proposer is a free data retrieval call binding the contract method 0xa8e4fb90.
//
// Solidity: function proposer() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) Proposer(opts *bind.CallOpts) (common.Address, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "proposer")

	if err != nil {
		return *new(common.Address), err
	}

	out0 := *abi.ConvertType(out[0], new(common.Address)).(*common.Address)

	return out0, err

}

// Proposer is a free data retrieval call binding the contract method 0xa8e4fb90.
//
// Solidity: function proposer() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) Proposer() (common.Address, error) {
	return _OPSuccinctL2OutputOracle.Contract.Proposer(&_OPSuccinctL2OutputOracle.CallOpts)
}

// Proposer is a free data retrieval call binding the contract method 0xa8e4fb90.
//
// Solidity: function proposer() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) Proposer() (common.Address, error) {
	return _OPSuccinctL2OutputOracle.Contract.Proposer(&_OPSuccinctL2OutputOracle.CallOpts)
}

// RangeVkeyCommitment is a free data retrieval call binding the contract method 0x2b31841e.
//
// Solidity: function rangeVkeyCommitment() view returns(bytes32)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) RangeVkeyCommitment(opts *bind.CallOpts) ([32]byte, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "rangeVkeyCommitment")

	if err != nil {
		return *new([32]byte), err
	}

	out0 := *abi.ConvertType(out[0], new([32]byte)).(*[32]byte)

	return out0, err

}

// RangeVkeyCommitment is a free data retrieval call binding the contract method 0x2b31841e.
//
// Solidity: function rangeVkeyCommitment() view returns(bytes32)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) RangeVkeyCommitment() ([32]byte, error) {
	return _OPSuccinctL2OutputOracle.Contract.RangeVkeyCommitment(&_OPSuccinctL2OutputOracle.CallOpts)
}

// RangeVkeyCommitment is a free data retrieval call binding the contract method 0x2b31841e.
//
// Solidity: function rangeVkeyCommitment() view returns(bytes32)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) RangeVkeyCommitment() ([32]byte, error) {
	return _OPSuccinctL2OutputOracle.Contract.RangeVkeyCommitment(&_OPSuccinctL2OutputOracle.CallOpts)
}

// RollupConfigHash is a free data retrieval call binding the contract method 0x6d9a1c8b.
//
// Solidity: function rollupConfigHash() view returns(bytes32)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) RollupConfigHash(opts *bind.CallOpts) ([32]byte, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "rollupConfigHash")

	if err != nil {
		return *new([32]byte), err
	}

	out0 := *abi.ConvertType(out[0], new([32]byte)).(*[32]byte)

	return out0, err

}

// RollupConfigHash is a free data retrieval call binding the contract method 0x6d9a1c8b.
//
// Solidity: function rollupConfigHash() view returns(bytes32)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) RollupConfigHash() ([32]byte, error) {
	return _OPSuccinctL2OutputOracle.Contract.RollupConfigHash(&_OPSuccinctL2OutputOracle.CallOpts)
}

// RollupConfigHash is a free data retrieval call binding the contract method 0x6d9a1c8b.
//
// Solidity: function rollupConfigHash() view returns(bytes32)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) RollupConfigHash() ([32]byte, error) {
	return _OPSuccinctL2OutputOracle.Contract.RollupConfigHash(&_OPSuccinctL2OutputOracle.CallOpts)
}

// StartingBlockNumber is a free data retrieval call binding the contract method 0x70872aa5.
//
// Solidity: function startingBlockNumber() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) StartingBlockNumber(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "startingBlockNumber")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// StartingBlockNumber is a free data retrieval call binding the contract method 0x70872aa5.
//
// Solidity: function startingBlockNumber() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) StartingBlockNumber() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.StartingBlockNumber(&_OPSuccinctL2OutputOracle.CallOpts)
}

// StartingBlockNumber is a free data retrieval call binding the contract method 0x70872aa5.
//
// Solidity: function startingBlockNumber() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) StartingBlockNumber() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.StartingBlockNumber(&_OPSuccinctL2OutputOracle.CallOpts)
}

// StartingTimestamp is a free data retrieval call binding the contract method 0x88786272.
//
// Solidity: function startingTimestamp() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) StartingTimestamp(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "startingTimestamp")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// StartingTimestamp is a free data retrieval call binding the contract method 0x88786272.
//
// Solidity: function startingTimestamp() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) StartingTimestamp() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.StartingTimestamp(&_OPSuccinctL2OutputOracle.CallOpts)
}

// StartingTimestamp is a free data retrieval call binding the contract method 0x88786272.
//
// Solidity: function startingTimestamp() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) StartingTimestamp() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.StartingTimestamp(&_OPSuccinctL2OutputOracle.CallOpts)
}

// SubmissionInterval is a free data retrieval call binding the contract method 0xe1a41bcf.
//
// Solidity: function submissionInterval() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) SubmissionInterval(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "submissionInterval")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// SubmissionInterval is a free data retrieval call binding the contract method 0xe1a41bcf.
//
// Solidity: function submissionInterval() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) SubmissionInterval() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.SubmissionInterval(&_OPSuccinctL2OutputOracle.CallOpts)
}

// SubmissionInterval is a free data retrieval call binding the contract method 0xe1a41bcf.
//
// Solidity: function submissionInterval() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) SubmissionInterval() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.SubmissionInterval(&_OPSuccinctL2OutputOracle.CallOpts)
}

// Verifier is a free data retrieval call binding the contract method 0x2b7ac3f3.
//
// Solidity: function verifier() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) Verifier(opts *bind.CallOpts) (common.Address, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "verifier")

	if err != nil {
		return *new(common.Address), err
	}

	out0 := *abi.ConvertType(out[0], new(common.Address)).(*common.Address)

	return out0, err

}

// Verifier is a free data retrieval call binding the contract method 0x2b7ac3f3.
//
// Solidity: function verifier() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) Verifier() (common.Address, error) {
	return _OPSuccinctL2OutputOracle.Contract.Verifier(&_OPSuccinctL2OutputOracle.CallOpts)
}

// Verifier is a free data retrieval call binding the contract method 0x2b7ac3f3.
//
// Solidity: function verifier() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) Verifier() (common.Address, error) {
	return _OPSuccinctL2OutputOracle.Contract.Verifier(&_OPSuccinctL2OutputOracle.CallOpts)
}

// Version is a free data retrieval call binding the contract method 0x54fd4d50.
//
// Solidity: function version() view returns(string)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) Version(opts *bind.CallOpts) (string, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "version")

	if err != nil {
		return *new(string), err
	}

	out0 := *abi.ConvertType(out[0], new(string)).(*string)

	return out0, err

}

// Version is a free data retrieval call binding the contract method 0x54fd4d50.
//
// Solidity: function version() view returns(string)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) Version() (string, error) {
	return _OPSuccinctL2OutputOracle.Contract.Version(&_OPSuccinctL2OutputOracle.CallOpts)
}

// Version is a free data retrieval call binding the contract method 0x54fd4d50.
//
// Solidity: function version() view returns(string)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) Version() (string, error) {
	return _OPSuccinctL2OutputOracle.Contract.Version(&_OPSuccinctL2OutputOracle.CallOpts)
}

// AddOpSuccinctConfig is a paid mutator transaction binding the contract method 0x47c37e9c.
//
// Solidity: function addOpSuccinctConfig(bytes32 _configName, bytes32 _rollupConfigHash, bytes32 _aggregationVkey, bytes32 _rangeVkeyCommitment) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) AddOpSuccinctConfig(opts *bind.TransactOpts, _configName [32]byte, _rollupConfigHash [32]byte, _aggregationVkey [32]byte, _rangeVkeyCommitment [32]byte) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "addOpSuccinctConfig", _configName, _rollupConfigHash, _aggregationVkey, _rangeVkeyCommitment)
}

// AddOpSuccinctConfig is a paid mutator transaction binding the contract method 0x47c37e9c.
//
// Solidity: function addOpSuccinctConfig(bytes32 _configName, bytes32 _rollupConfigHash, bytes32 _aggregationVkey, bytes32 _rangeVkeyCommitment) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) AddOpSuccinctConfig(_configName [32]byte, _rollupConfigHash [32]byte, _aggregationVkey [32]byte, _rangeVkeyCommitment [32]byte) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.AddOpSuccinctConfig(&_OPSuccinctL2OutputOracle.TransactOpts, _configName, _rollupConfigHash, _aggregationVkey, _rangeVkeyCommitment)
}

// AddOpSuccinctConfig is a paid mutator transaction binding the contract method 0x47c37e9c.
//
// Solidity: function addOpSuccinctConfig(bytes32 _configName, bytes32 _rollupConfigHash, bytes32 _aggregationVkey, bytes32 _rangeVkeyCommitment) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) AddOpSuccinctConfig(_configName [32]byte, _rollupConfigHash [32]byte, _aggregationVkey [32]byte, _rangeVkeyCommitment [32]byte) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.AddOpSuccinctConfig(&_OPSuccinctL2OutputOracle.TransactOpts, _configName, _rollupConfigHash, _aggregationVkey, _rangeVkeyCommitment)
}

// AddProposer is a paid mutator transaction binding the contract method 0xb03cd418.
//
// Solidity: function addProposer(address _proposer) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) AddProposer(opts *bind.TransactOpts, _proposer common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "addProposer", _proposer)
}

// AddProposer is a paid mutator transaction binding the contract method 0xb03cd418.
//
// Solidity: function addProposer(address _proposer) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) AddProposer(_proposer common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.AddProposer(&_OPSuccinctL2OutputOracle.TransactOpts, _proposer)
}

// AddProposer is a paid mutator transaction binding the contract method 0xb03cd418.
//
// Solidity: function addProposer(address _proposer) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) AddProposer(_proposer common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.AddProposer(&_OPSuccinctL2OutputOracle.TransactOpts, _proposer)
}

// CheckpointBlockHash is a paid mutator transaction binding the contract method 0x1e856800.
//
// Solidity: function checkpointBlockHash(uint256 _blockNumber) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) CheckpointBlockHash(opts *bind.TransactOpts, _blockNumber *big.Int) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "checkpointBlockHash", _blockNumber)
}

// CheckpointBlockHash is a paid mutator transaction binding the contract method 0x1e856800.
//
// Solidity: function checkpointBlockHash(uint256 _blockNumber) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) CheckpointBlockHash(_blockNumber *big.Int) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.CheckpointBlockHash(&_OPSuccinctL2OutputOracle.TransactOpts, _blockNumber)
}

// CheckpointBlockHash is a paid mutator transaction binding the contract method 0x1e856800.
//
// Solidity: function checkpointBlockHash(uint256 _blockNumber) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) CheckpointBlockHash(_blockNumber *big.Int) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.CheckpointBlockHash(&_OPSuccinctL2OutputOracle.TransactOpts, _blockNumber)
}

// DeleteL2Outputs is a paid mutator transaction binding the contract method 0x89c44cbb.
//
// Solidity: function deleteL2Outputs(uint256 _l2OutputIndex) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) DeleteL2Outputs(opts *bind.TransactOpts, _l2OutputIndex *big.Int) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "deleteL2Outputs", _l2OutputIndex)
}

// DeleteL2Outputs is a paid mutator transaction binding the contract method 0x89c44cbb.
//
// Solidity: function deleteL2Outputs(uint256 _l2OutputIndex) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) DeleteL2Outputs(_l2OutputIndex *big.Int) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.DeleteL2Outputs(&_OPSuccinctL2OutputOracle.TransactOpts, _l2OutputIndex)
}

// DeleteL2Outputs is a paid mutator transaction binding the contract method 0x89c44cbb.
//
// Solidity: function deleteL2Outputs(uint256 _l2OutputIndex) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) DeleteL2Outputs(_l2OutputIndex *big.Int) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.DeleteL2Outputs(&_OPSuccinctL2OutputOracle.TransactOpts, _l2OutputIndex)
}

// DeleteOpSuccinctConfig is a paid mutator transaction binding the contract method 0xec5b2e3a.
//
// Solidity: function deleteOpSuccinctConfig(bytes32 _configName) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) DeleteOpSuccinctConfig(opts *bind.TransactOpts, _configName [32]byte) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "deleteOpSuccinctConfig", _configName)
}

// DeleteOpSuccinctConfig is a paid mutator transaction binding the contract method 0xec5b2e3a.
//
// Solidity: function deleteOpSuccinctConfig(bytes32 _configName) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) DeleteOpSuccinctConfig(_configName [32]byte) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.DeleteOpSuccinctConfig(&_OPSuccinctL2OutputOracle.TransactOpts, _configName)
}

// DeleteOpSuccinctConfig is a paid mutator transaction binding the contract method 0xec5b2e3a.
//
// Solidity: function deleteOpSuccinctConfig(bytes32 _configName) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) DeleteOpSuccinctConfig(_configName [32]byte) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.DeleteOpSuccinctConfig(&_OPSuccinctL2OutputOracle.TransactOpts, _configName)
}

// DgfProposeL2Output is a paid mutator transaction binding the contract method 0x7a41a035.
//
// Solidity: function dgfProposeL2Output(bytes32 _configName, bytes32 _outputRoot, uint256 _l2BlockNumber, uint256 _l1BlockNumber, bytes _proof, address _proverAddress) payable returns(address _game)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) DgfProposeL2Output(opts *bind.TransactOpts, _configName [32]byte, _outputRoot [32]byte, _l2BlockNumber *big.Int, _l1BlockNumber *big.Int, _proof []byte, _proverAddress common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "dgfProposeL2Output", _configName, _outputRoot, _l2BlockNumber, _l1BlockNumber, _proof, _proverAddress)
}

// DgfProposeL2Output is a paid mutator transaction binding the contract method 0x7a41a035.
//
// Solidity: function dgfProposeL2Output(bytes32 _configName, bytes32 _outputRoot, uint256 _l2BlockNumber, uint256 _l1BlockNumber, bytes _proof, address _proverAddress) payable returns(address _game)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) DgfProposeL2Output(_configName [32]byte, _outputRoot [32]byte, _l2BlockNumber *big.Int, _l1BlockNumber *big.Int, _proof []byte, _proverAddress common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.DgfProposeL2Output(&_OPSuccinctL2OutputOracle.TransactOpts, _configName, _outputRoot, _l2BlockNumber, _l1BlockNumber, _proof, _proverAddress)
}

// DgfProposeL2Output is a paid mutator transaction binding the contract method 0x7a41a035.
//
// Solidity: function dgfProposeL2Output(bytes32 _configName, bytes32 _outputRoot, uint256 _l2BlockNumber, uint256 _l1BlockNumber, bytes _proof, address _proverAddress) payable returns(address _game)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) DgfProposeL2Output(_configName [32]byte, _outputRoot [32]byte, _l2BlockNumber *big.Int, _l1BlockNumber *big.Int, _proof []byte, _proverAddress common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.DgfProposeL2Output(&_OPSuccinctL2OutputOracle.TransactOpts, _configName, _outputRoot, _l2BlockNumber, _l1BlockNumber, _proof, _proverAddress)
}

// DisableOptimisticMode is a paid mutator transaction binding the contract method 0x4ab309ac.
//
// Solidity: function disableOptimisticMode(uint256 _finalizationPeriodSeconds) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) DisableOptimisticMode(opts *bind.TransactOpts, _finalizationPeriodSeconds *big.Int) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "disableOptimisticMode", _finalizationPeriodSeconds)
}

// DisableOptimisticMode is a paid mutator transaction binding the contract method 0x4ab309ac.
//
// Solidity: function disableOptimisticMode(uint256 _finalizationPeriodSeconds) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) DisableOptimisticMode(_finalizationPeriodSeconds *big.Int) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.DisableOptimisticMode(&_OPSuccinctL2OutputOracle.TransactOpts, _finalizationPeriodSeconds)
}

// DisableOptimisticMode is a paid mutator transaction binding the contract method 0x4ab309ac.
//
// Solidity: function disableOptimisticMode(uint256 _finalizationPeriodSeconds) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) DisableOptimisticMode(_finalizationPeriodSeconds *big.Int) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.DisableOptimisticMode(&_OPSuccinctL2OutputOracle.TransactOpts, _finalizationPeriodSeconds)
}

// EnableOptimisticMode is a paid mutator transaction binding the contract method 0x2c697961.
//
// Solidity: function enableOptimisticMode(uint256 _finalizationPeriodSeconds) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) EnableOptimisticMode(opts *bind.TransactOpts, _finalizationPeriodSeconds *big.Int) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "enableOptimisticMode", _finalizationPeriodSeconds)
}

// EnableOptimisticMode is a paid mutator transaction binding the contract method 0x2c697961.
//
// Solidity: function enableOptimisticMode(uint256 _finalizationPeriodSeconds) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) EnableOptimisticMode(_finalizationPeriodSeconds *big.Int) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.EnableOptimisticMode(&_OPSuccinctL2OutputOracle.TransactOpts, _finalizationPeriodSeconds)
}

// EnableOptimisticMode is a paid mutator transaction binding the contract method 0x2c697961.
//
// Solidity: function enableOptimisticMode(uint256 _finalizationPeriodSeconds) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) EnableOptimisticMode(_finalizationPeriodSeconds *big.Int) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.EnableOptimisticMode(&_OPSuccinctL2OutputOracle.TransactOpts, _finalizationPeriodSeconds)
}

// Initialize is a paid mutator transaction binding the contract method 0xe40b7a12.
//
// Solidity: function initialize((address,address,address,uint256,uint256,bytes32,bytes32,bytes32,bytes32,uint256,uint256,uint256,address,uint256) _initParams) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) Initialize(opts *bind.TransactOpts, _initParams OPSuccinctL2OutputOracleInitParams) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "initialize", _initParams)
}

// Initialize is a paid mutator transaction binding the contract method 0xe40b7a12.
//
// Solidity: function initialize((address,address,address,uint256,uint256,bytes32,bytes32,bytes32,bytes32,uint256,uint256,uint256,address,uint256) _initParams) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) Initialize(_initParams OPSuccinctL2OutputOracleInitParams) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.Initialize(&_OPSuccinctL2OutputOracle.TransactOpts, _initParams)
}

// Initialize is a paid mutator transaction binding the contract method 0xe40b7a12.
//
// Solidity: function initialize((address,address,address,uint256,uint256,bytes32,bytes32,bytes32,bytes32,uint256,uint256,uint256,address,uint256) _initParams) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) Initialize(_initParams OPSuccinctL2OutputOracleInitParams) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.Initialize(&_OPSuccinctL2OutputOracle.TransactOpts, _initParams)
}

// ProposeL2Output is a paid mutator transaction binding the contract method 0x9aaab648.
//
// Solidity: function proposeL2Output(bytes32 _outputRoot, uint256 _l2BlockNumber, bytes32 _l1BlockHash, uint256 _l1BlockNumber) payable returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) ProposeL2Output(opts *bind.TransactOpts, _outputRoot [32]byte, _l2BlockNumber *big.Int, _l1BlockHash [32]byte, _l1BlockNumber *big.Int) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "proposeL2Output", _outputRoot, _l2BlockNumber, _l1BlockHash, _l1BlockNumber)
}

// ProposeL2Output is a paid mutator transaction binding the contract method 0x9aaab648.
//
// Solidity: function proposeL2Output(bytes32 _outputRoot, uint256 _l2BlockNumber, bytes32 _l1BlockHash, uint256 _l1BlockNumber) payable returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) ProposeL2Output(_outputRoot [32]byte, _l2BlockNumber *big.Int, _l1BlockHash [32]byte, _l1BlockNumber *big.Int) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.ProposeL2Output(&_OPSuccinctL2OutputOracle.TransactOpts, _outputRoot, _l2BlockNumber, _l1BlockHash, _l1BlockNumber)
}

// ProposeL2Output is a paid mutator transaction binding the contract method 0x9aaab648.
//
// Solidity: function proposeL2Output(bytes32 _outputRoot, uint256 _l2BlockNumber, bytes32 _l1BlockHash, uint256 _l1BlockNumber) payable returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) ProposeL2Output(_outputRoot [32]byte, _l2BlockNumber *big.Int, _l1BlockHash [32]byte, _l1BlockNumber *big.Int) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.ProposeL2Output(&_OPSuccinctL2OutputOracle.TransactOpts, _outputRoot, _l2BlockNumber, _l1BlockHash, _l1BlockNumber)
}

// ProposeL2Output0 is a paid mutator transaction binding the contract method 0xa4ee9d7b.
//
// Solidity: function proposeL2Output(bytes32 _configName, bytes32 _outputRoot, uint256 _l2BlockNumber, uint256 _l1BlockNumber, bytes _proof, address _proverAddress) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) ProposeL2Output0(opts *bind.TransactOpts, _configName [32]byte, _outputRoot [32]byte, _l2BlockNumber *big.Int, _l1BlockNumber *big.Int, _proof []byte, _proverAddress common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "proposeL2Output0", _configName, _outputRoot, _l2BlockNumber, _l1BlockNumber, _proof, _proverAddress)
}

// ProposeL2Output0 is a paid mutator transaction binding the contract method 0xa4ee9d7b.
//
// Solidity: function proposeL2Output(bytes32 _configName, bytes32 _outputRoot, uint256 _l2BlockNumber, uint256 _l1BlockNumber, bytes _proof, address _proverAddress) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) ProposeL2Output0(_configName [32]byte, _outputRoot [32]byte, _l2BlockNumber *big.Int, _l1BlockNumber *big.Int, _proof []byte, _proverAddress common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.ProposeL2Output0(&_OPSuccinctL2OutputOracle.TransactOpts, _configName, _outputRoot, _l2BlockNumber, _l1BlockNumber, _proof, _proverAddress)
}

// ProposeL2Output0 is a paid mutator transaction binding the contract method 0xa4ee9d7b.
//
// Solidity: function proposeL2Output(bytes32 _configName, bytes32 _outputRoot, uint256 _l2BlockNumber, uint256 _l1BlockNumber, bytes _proof, address _proverAddress) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) ProposeL2Output0(_configName [32]byte, _outputRoot [32]byte, _l2BlockNumber *big.Int, _l1BlockNumber *big.Int, _proof []byte, _proverAddress common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.ProposeL2Output0(&_OPSuccinctL2OutputOracle.TransactOpts, _configName, _outputRoot, _l2BlockNumber, _l1BlockNumber, _proof, _proverAddress)
}

// RemoveProposer is a paid mutator transaction binding the contract method 0x09d632d3.
//
// Solidity: function removeProposer(address _proposer) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) RemoveProposer(opts *bind.TransactOpts, _proposer common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "removeProposer", _proposer)
}

// RemoveProposer is a paid mutator transaction binding the contract method 0x09d632d3.
//
// Solidity: function removeProposer(address _proposer) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) RemoveProposer(_proposer common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.RemoveProposer(&_OPSuccinctL2OutputOracle.TransactOpts, _proposer)
}

// RemoveProposer is a paid mutator transaction binding the contract method 0x09d632d3.
//
// Solidity: function removeProposer(address _proposer) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) RemoveProposer(_proposer common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.RemoveProposer(&_OPSuccinctL2OutputOracle.TransactOpts, _proposer)
}

// SetDisputeGameFactory is a paid mutator transaction binding the contract method 0x3419d2c2.
//
// Solidity: function setDisputeGameFactory(address _disputeGameFactory) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) SetDisputeGameFactory(opts *bind.TransactOpts, _disputeGameFactory common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "setDisputeGameFactory", _disputeGameFactory)
}

// SetDisputeGameFactory is a paid mutator transaction binding the contract method 0x3419d2c2.
//
// Solidity: function setDisputeGameFactory(address _disputeGameFactory) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) SetDisputeGameFactory(_disputeGameFactory common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.SetDisputeGameFactory(&_OPSuccinctL2OutputOracle.TransactOpts, _disputeGameFactory)
}

// SetDisputeGameFactory is a paid mutator transaction binding the contract method 0x3419d2c2.
//
// Solidity: function setDisputeGameFactory(address _disputeGameFactory) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) SetDisputeGameFactory(_disputeGameFactory common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.SetDisputeGameFactory(&_OPSuccinctL2OutputOracle.TransactOpts, _disputeGameFactory)
}

// TransferOwnership is a paid mutator transaction binding the contract method 0xf2fde38b.
//
// Solidity: function transferOwnership(address _owner) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) TransferOwnership(opts *bind.TransactOpts, _owner common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "transferOwnership", _owner)
}

// TransferOwnership is a paid mutator transaction binding the contract method 0xf2fde38b.
//
// Solidity: function transferOwnership(address _owner) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) TransferOwnership(_owner common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.TransferOwnership(&_OPSuccinctL2OutputOracle.TransactOpts, _owner)
}

// TransferOwnership is a paid mutator transaction binding the contract method 0xf2fde38b.
//
// Solidity: function transferOwnership(address _owner) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) TransferOwnership(_owner common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.TransferOwnership(&_OPSuccinctL2OutputOracle.TransactOpts, _owner)
}

// UpdateSubmissionInterval is a paid mutator transaction binding the contract method 0x336c9e81.
//
// Solidity: function updateSubmissionInterval(uint256 _submissionInterval) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) UpdateSubmissionInterval(opts *bind.TransactOpts, _submissionInterval *big.Int) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "updateSubmissionInterval", _submissionInterval)
}

// UpdateSubmissionInterval is a paid mutator transaction binding the contract method 0x336c9e81.
//
// Solidity: function updateSubmissionInterval(uint256 _submissionInterval) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) UpdateSubmissionInterval(_submissionInterval *big.Int) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.UpdateSubmissionInterval(&_OPSuccinctL2OutputOracle.TransactOpts, _submissionInterval)
}

// UpdateSubmissionInterval is a paid mutator transaction binding the contract method 0x336c9e81.
//
// Solidity: function updateSubmissionInterval(uint256 _submissionInterval) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) UpdateSubmissionInterval(_submissionInterval *big.Int) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.UpdateSubmissionInterval(&_OPSuccinctL2OutputOracle.TransactOpts, _submissionInterval)
}

// UpdateVerifier is a paid mutator transaction binding the contract method 0x97fc007c.
//
// Solidity: function updateVerifier(address _verifier) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) UpdateVerifier(opts *bind.TransactOpts, _verifier common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "updateVerifier", _verifier)
}

// UpdateVerifier is a paid mutator transaction binding the contract method 0x97fc007c.
//
// Solidity: function updateVerifier(address _verifier) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) UpdateVerifier(_verifier common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.UpdateVerifier(&_OPSuccinctL2OutputOracle.TransactOpts, _verifier)
}

// UpdateVerifier is a paid mutator transaction binding the contract method 0x97fc007c.
//
// Solidity: function updateVerifier(address _verifier) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) UpdateVerifier(_verifier common.Address) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.UpdateVerifier(&_OPSuccinctL2OutputOracle.TransactOpts, _verifier)
}

// OPSuccinctL2OutputOracleDisputeGameFactorySetIterator is returned from FilterDisputeGameFactorySet and is used to iterate over the raw logs and unpacked data for DisputeGameFactorySet events raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleDisputeGameFactorySetIterator struct {
	Event *OPSuccinctL2OutputOracleDisputeGameFactorySet // Event containing the contract specifics and raw log

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
func (it *OPSuccinctL2OutputOracleDisputeGameFactorySetIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(OPSuccinctL2OutputOracleDisputeGameFactorySet)
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
		it.Event = new(OPSuccinctL2OutputOracleDisputeGameFactorySet)
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
func (it *OPSuccinctL2OutputOracleDisputeGameFactorySetIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *OPSuccinctL2OutputOracleDisputeGameFactorySetIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// OPSuccinctL2OutputOracleDisputeGameFactorySet represents a DisputeGameFactorySet event raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleDisputeGameFactorySet struct {
	DisputeGameFactory common.Address
	Raw                types.Log // Blockchain specific contextual infos
}

// FilterDisputeGameFactorySet is a free log retrieval operation binding the contract event 0x73702180ce348e07b058846d1745c99987ae6c741ff97ec28d4539530ef1e8f1.
//
// Solidity: event DisputeGameFactorySet(address indexed disputeGameFactory)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) FilterDisputeGameFactorySet(opts *bind.FilterOpts, disputeGameFactory []common.Address) (*OPSuccinctL2OutputOracleDisputeGameFactorySetIterator, error) {

	var disputeGameFactoryRule []interface{}
	for _, disputeGameFactoryItem := range disputeGameFactory {
		disputeGameFactoryRule = append(disputeGameFactoryRule, disputeGameFactoryItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.FilterLogs(opts, "DisputeGameFactorySet", disputeGameFactoryRule)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctL2OutputOracleDisputeGameFactorySetIterator{contract: _OPSuccinctL2OutputOracle.contract, event: "DisputeGameFactorySet", logs: logs, sub: sub}, nil
}

// WatchDisputeGameFactorySet is a free log subscription operation binding the contract event 0x73702180ce348e07b058846d1745c99987ae6c741ff97ec28d4539530ef1e8f1.
//
// Solidity: event DisputeGameFactorySet(address indexed disputeGameFactory)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) WatchDisputeGameFactorySet(opts *bind.WatchOpts, sink chan<- *OPSuccinctL2OutputOracleDisputeGameFactorySet, disputeGameFactory []common.Address) (event.Subscription, error) {

	var disputeGameFactoryRule []interface{}
	for _, disputeGameFactoryItem := range disputeGameFactory {
		disputeGameFactoryRule = append(disputeGameFactoryRule, disputeGameFactoryItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.WatchLogs(opts, "DisputeGameFactorySet", disputeGameFactoryRule)
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(OPSuccinctL2OutputOracleDisputeGameFactorySet)
				if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "DisputeGameFactorySet", log); err != nil {
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

// ParseDisputeGameFactorySet is a log parse operation binding the contract event 0x73702180ce348e07b058846d1745c99987ae6c741ff97ec28d4539530ef1e8f1.
//
// Solidity: event DisputeGameFactorySet(address indexed disputeGameFactory)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) ParseDisputeGameFactorySet(log types.Log) (*OPSuccinctL2OutputOracleDisputeGameFactorySet, error) {
	event := new(OPSuccinctL2OutputOracleDisputeGameFactorySet)
	if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "DisputeGameFactorySet", log); err != nil {
		return nil, err
	}
	event.Raw = log
	return event, nil
}

// OPSuccinctL2OutputOracleInitializedIterator is returned from FilterInitialized and is used to iterate over the raw logs and unpacked data for Initialized events raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleInitializedIterator struct {
	Event *OPSuccinctL2OutputOracleInitialized // Event containing the contract specifics and raw log

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
func (it *OPSuccinctL2OutputOracleInitializedIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(OPSuccinctL2OutputOracleInitialized)
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
		it.Event = new(OPSuccinctL2OutputOracleInitialized)
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
func (it *OPSuccinctL2OutputOracleInitializedIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *OPSuccinctL2OutputOracleInitializedIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// OPSuccinctL2OutputOracleInitialized represents a Initialized event raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleInitialized struct {
	Version uint8
	Raw     types.Log // Blockchain specific contextual infos
}

// FilterInitialized is a free log retrieval operation binding the contract event 0x7f26b83ff96e1f2b6a682f133852f6798a09c465da95921460cefb3847402498.
//
// Solidity: event Initialized(uint8 version)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) FilterInitialized(opts *bind.FilterOpts) (*OPSuccinctL2OutputOracleInitializedIterator, error) {

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.FilterLogs(opts, "Initialized")
	if err != nil {
		return nil, err
	}
	return &OPSuccinctL2OutputOracleInitializedIterator{contract: _OPSuccinctL2OutputOracle.contract, event: "Initialized", logs: logs, sub: sub}, nil
}

// WatchInitialized is a free log subscription operation binding the contract event 0x7f26b83ff96e1f2b6a682f133852f6798a09c465da95921460cefb3847402498.
//
// Solidity: event Initialized(uint8 version)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) WatchInitialized(opts *bind.WatchOpts, sink chan<- *OPSuccinctL2OutputOracleInitialized) (event.Subscription, error) {

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.WatchLogs(opts, "Initialized")
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(OPSuccinctL2OutputOracleInitialized)
				if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "Initialized", log); err != nil {
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
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) ParseInitialized(log types.Log) (*OPSuccinctL2OutputOracleInitialized, error) {
	event := new(OPSuccinctL2OutputOracleInitialized)
	if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "Initialized", log); err != nil {
		return nil, err
	}
	event.Raw = log
	return event, nil
}

// OPSuccinctL2OutputOracleOpSuccinctConfigDeletedIterator is returned from FilterOpSuccinctConfigDeleted and is used to iterate over the raw logs and unpacked data for OpSuccinctConfigDeleted events raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleOpSuccinctConfigDeletedIterator struct {
	Event *OPSuccinctL2OutputOracleOpSuccinctConfigDeleted // Event containing the contract specifics and raw log

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
func (it *OPSuccinctL2OutputOracleOpSuccinctConfigDeletedIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(OPSuccinctL2OutputOracleOpSuccinctConfigDeleted)
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
		it.Event = new(OPSuccinctL2OutputOracleOpSuccinctConfigDeleted)
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
func (it *OPSuccinctL2OutputOracleOpSuccinctConfigDeletedIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *OPSuccinctL2OutputOracleOpSuccinctConfigDeletedIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// OPSuccinctL2OutputOracleOpSuccinctConfigDeleted represents a OpSuccinctConfigDeleted event raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleOpSuccinctConfigDeleted struct {
	ConfigName [32]byte
	Raw        types.Log // Blockchain specific contextual infos
}

// FilterOpSuccinctConfigDeleted is a free log retrieval operation binding the contract event 0x4432b02a2fcbed48d94e8d72723e155c6690e4b7f39afa41a2a8ff8c0aa425da.
//
// Solidity: event OpSuccinctConfigDeleted(bytes32 indexed configName)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) FilterOpSuccinctConfigDeleted(opts *bind.FilterOpts, configName [][32]byte) (*OPSuccinctL2OutputOracleOpSuccinctConfigDeletedIterator, error) {

	var configNameRule []interface{}
	for _, configNameItem := range configName {
		configNameRule = append(configNameRule, configNameItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.FilterLogs(opts, "OpSuccinctConfigDeleted", configNameRule)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctL2OutputOracleOpSuccinctConfigDeletedIterator{contract: _OPSuccinctL2OutputOracle.contract, event: "OpSuccinctConfigDeleted", logs: logs, sub: sub}, nil
}

// WatchOpSuccinctConfigDeleted is a free log subscription operation binding the contract event 0x4432b02a2fcbed48d94e8d72723e155c6690e4b7f39afa41a2a8ff8c0aa425da.
//
// Solidity: event OpSuccinctConfigDeleted(bytes32 indexed configName)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) WatchOpSuccinctConfigDeleted(opts *bind.WatchOpts, sink chan<- *OPSuccinctL2OutputOracleOpSuccinctConfigDeleted, configName [][32]byte) (event.Subscription, error) {

	var configNameRule []interface{}
	for _, configNameItem := range configName {
		configNameRule = append(configNameRule, configNameItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.WatchLogs(opts, "OpSuccinctConfigDeleted", configNameRule)
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(OPSuccinctL2OutputOracleOpSuccinctConfigDeleted)
				if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "OpSuccinctConfigDeleted", log); err != nil {
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

// ParseOpSuccinctConfigDeleted is a log parse operation binding the contract event 0x4432b02a2fcbed48d94e8d72723e155c6690e4b7f39afa41a2a8ff8c0aa425da.
//
// Solidity: event OpSuccinctConfigDeleted(bytes32 indexed configName)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) ParseOpSuccinctConfigDeleted(log types.Log) (*OPSuccinctL2OutputOracleOpSuccinctConfigDeleted, error) {
	event := new(OPSuccinctL2OutputOracleOpSuccinctConfigDeleted)
	if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "OpSuccinctConfigDeleted", log); err != nil {
		return nil, err
	}
	event.Raw = log
	return event, nil
}

// OPSuccinctL2OutputOracleOpSuccinctConfigUpdatedIterator is returned from FilterOpSuccinctConfigUpdated and is used to iterate over the raw logs and unpacked data for OpSuccinctConfigUpdated events raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleOpSuccinctConfigUpdatedIterator struct {
	Event *OPSuccinctL2OutputOracleOpSuccinctConfigUpdated // Event containing the contract specifics and raw log

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
func (it *OPSuccinctL2OutputOracleOpSuccinctConfigUpdatedIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(OPSuccinctL2OutputOracleOpSuccinctConfigUpdated)
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
		it.Event = new(OPSuccinctL2OutputOracleOpSuccinctConfigUpdated)
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
func (it *OPSuccinctL2OutputOracleOpSuccinctConfigUpdatedIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *OPSuccinctL2OutputOracleOpSuccinctConfigUpdatedIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// OPSuccinctL2OutputOracleOpSuccinctConfigUpdated represents a OpSuccinctConfigUpdated event raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleOpSuccinctConfigUpdated struct {
	ConfigName          [32]byte
	AggregationVkey     [32]byte
	RangeVkeyCommitment [32]byte
	RollupConfigHash    [32]byte
	Raw                 types.Log // Blockchain specific contextual infos
}

// FilterOpSuccinctConfigUpdated is a free log retrieval operation binding the contract event 0xea0123c726a665cb0ab5691444f929a7056c7a7709c60c0587829e8046b8d514.
//
// Solidity: event OpSuccinctConfigUpdated(bytes32 indexed configName, bytes32 aggregationVkey, bytes32 rangeVkeyCommitment, bytes32 rollupConfigHash)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) FilterOpSuccinctConfigUpdated(opts *bind.FilterOpts, configName [][32]byte) (*OPSuccinctL2OutputOracleOpSuccinctConfigUpdatedIterator, error) {

	var configNameRule []interface{}
	for _, configNameItem := range configName {
		configNameRule = append(configNameRule, configNameItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.FilterLogs(opts, "OpSuccinctConfigUpdated", configNameRule)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctL2OutputOracleOpSuccinctConfigUpdatedIterator{contract: _OPSuccinctL2OutputOracle.contract, event: "OpSuccinctConfigUpdated", logs: logs, sub: sub}, nil
}

// WatchOpSuccinctConfigUpdated is a free log subscription operation binding the contract event 0xea0123c726a665cb0ab5691444f929a7056c7a7709c60c0587829e8046b8d514.
//
// Solidity: event OpSuccinctConfigUpdated(bytes32 indexed configName, bytes32 aggregationVkey, bytes32 rangeVkeyCommitment, bytes32 rollupConfigHash)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) WatchOpSuccinctConfigUpdated(opts *bind.WatchOpts, sink chan<- *OPSuccinctL2OutputOracleOpSuccinctConfigUpdated, configName [][32]byte) (event.Subscription, error) {

	var configNameRule []interface{}
	for _, configNameItem := range configName {
		configNameRule = append(configNameRule, configNameItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.WatchLogs(opts, "OpSuccinctConfigUpdated", configNameRule)
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(OPSuccinctL2OutputOracleOpSuccinctConfigUpdated)
				if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "OpSuccinctConfigUpdated", log); err != nil {
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

// ParseOpSuccinctConfigUpdated is a log parse operation binding the contract event 0xea0123c726a665cb0ab5691444f929a7056c7a7709c60c0587829e8046b8d514.
//
// Solidity: event OpSuccinctConfigUpdated(bytes32 indexed configName, bytes32 aggregationVkey, bytes32 rangeVkeyCommitment, bytes32 rollupConfigHash)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) ParseOpSuccinctConfigUpdated(log types.Log) (*OPSuccinctL2OutputOracleOpSuccinctConfigUpdated, error) {
	event := new(OPSuccinctL2OutputOracleOpSuccinctConfigUpdated)
	if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "OpSuccinctConfigUpdated", log); err != nil {
		return nil, err
	}
	event.Raw = log
	return event, nil
}

// OPSuccinctL2OutputOracleOptimisticModeToggledIterator is returned from FilterOptimisticModeToggled and is used to iterate over the raw logs and unpacked data for OptimisticModeToggled events raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleOptimisticModeToggledIterator struct {
	Event *OPSuccinctL2OutputOracleOptimisticModeToggled // Event containing the contract specifics and raw log

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
func (it *OPSuccinctL2OutputOracleOptimisticModeToggledIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(OPSuccinctL2OutputOracleOptimisticModeToggled)
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
		it.Event = new(OPSuccinctL2OutputOracleOptimisticModeToggled)
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
func (it *OPSuccinctL2OutputOracleOptimisticModeToggledIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *OPSuccinctL2OutputOracleOptimisticModeToggledIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// OPSuccinctL2OutputOracleOptimisticModeToggled represents a OptimisticModeToggled event raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleOptimisticModeToggled struct {
	Enabled                   bool
	FinalizationPeriodSeconds *big.Int
	Raw                       types.Log // Blockchain specific contextual infos
}

// FilterOptimisticModeToggled is a free log retrieval operation binding the contract event 0x1f5c872f1ea93c57e43112ea449ee19ef5754488b87627b4c52456b0e5a4109a.
//
// Solidity: event OptimisticModeToggled(bool indexed enabled, uint256 finalizationPeriodSeconds)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) FilterOptimisticModeToggled(opts *bind.FilterOpts, enabled []bool) (*OPSuccinctL2OutputOracleOptimisticModeToggledIterator, error) {

	var enabledRule []interface{}
	for _, enabledItem := range enabled {
		enabledRule = append(enabledRule, enabledItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.FilterLogs(opts, "OptimisticModeToggled", enabledRule)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctL2OutputOracleOptimisticModeToggledIterator{contract: _OPSuccinctL2OutputOracle.contract, event: "OptimisticModeToggled", logs: logs, sub: sub}, nil
}

// WatchOptimisticModeToggled is a free log subscription operation binding the contract event 0x1f5c872f1ea93c57e43112ea449ee19ef5754488b87627b4c52456b0e5a4109a.
//
// Solidity: event OptimisticModeToggled(bool indexed enabled, uint256 finalizationPeriodSeconds)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) WatchOptimisticModeToggled(opts *bind.WatchOpts, sink chan<- *OPSuccinctL2OutputOracleOptimisticModeToggled, enabled []bool) (event.Subscription, error) {

	var enabledRule []interface{}
	for _, enabledItem := range enabled {
		enabledRule = append(enabledRule, enabledItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.WatchLogs(opts, "OptimisticModeToggled", enabledRule)
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(OPSuccinctL2OutputOracleOptimisticModeToggled)
				if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "OptimisticModeToggled", log); err != nil {
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

// ParseOptimisticModeToggled is a log parse operation binding the contract event 0x1f5c872f1ea93c57e43112ea449ee19ef5754488b87627b4c52456b0e5a4109a.
//
// Solidity: event OptimisticModeToggled(bool indexed enabled, uint256 finalizationPeriodSeconds)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) ParseOptimisticModeToggled(log types.Log) (*OPSuccinctL2OutputOracleOptimisticModeToggled, error) {
	event := new(OPSuccinctL2OutputOracleOptimisticModeToggled)
	if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "OptimisticModeToggled", log); err != nil {
		return nil, err
	}
	event.Raw = log
	return event, nil
}

// OPSuccinctL2OutputOracleOutputProposedIterator is returned from FilterOutputProposed and is used to iterate over the raw logs and unpacked data for OutputProposed events raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleOutputProposedIterator struct {
	Event *OPSuccinctL2OutputOracleOutputProposed // Event containing the contract specifics and raw log

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
func (it *OPSuccinctL2OutputOracleOutputProposedIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(OPSuccinctL2OutputOracleOutputProposed)
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
		it.Event = new(OPSuccinctL2OutputOracleOutputProposed)
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
func (it *OPSuccinctL2OutputOracleOutputProposedIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *OPSuccinctL2OutputOracleOutputProposedIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// OPSuccinctL2OutputOracleOutputProposed represents a OutputProposed event raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleOutputProposed struct {
	OutputRoot    [32]byte
	L2OutputIndex *big.Int
	L2BlockNumber *big.Int
	L1Timestamp   *big.Int
	Raw           types.Log // Blockchain specific contextual infos
}

// FilterOutputProposed is a free log retrieval operation binding the contract event 0xa7aaf2512769da4e444e3de247be2564225c2e7a8f74cfe528e46e17d24868e2.
//
// Solidity: event OutputProposed(bytes32 indexed outputRoot, uint256 indexed l2OutputIndex, uint256 indexed l2BlockNumber, uint256 l1Timestamp)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) FilterOutputProposed(opts *bind.FilterOpts, outputRoot [][32]byte, l2OutputIndex []*big.Int, l2BlockNumber []*big.Int) (*OPSuccinctL2OutputOracleOutputProposedIterator, error) {

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

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.FilterLogs(opts, "OutputProposed", outputRootRule, l2OutputIndexRule, l2BlockNumberRule)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctL2OutputOracleOutputProposedIterator{contract: _OPSuccinctL2OutputOracle.contract, event: "OutputProposed", logs: logs, sub: sub}, nil
}

// WatchOutputProposed is a free log subscription operation binding the contract event 0xa7aaf2512769da4e444e3de247be2564225c2e7a8f74cfe528e46e17d24868e2.
//
// Solidity: event OutputProposed(bytes32 indexed outputRoot, uint256 indexed l2OutputIndex, uint256 indexed l2BlockNumber, uint256 l1Timestamp)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) WatchOutputProposed(opts *bind.WatchOpts, sink chan<- *OPSuccinctL2OutputOracleOutputProposed, outputRoot [][32]byte, l2OutputIndex []*big.Int, l2BlockNumber []*big.Int) (event.Subscription, error) {

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

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.WatchLogs(opts, "OutputProposed", outputRootRule, l2OutputIndexRule, l2BlockNumberRule)
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(OPSuccinctL2OutputOracleOutputProposed)
				if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "OutputProposed", log); err != nil {
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
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) ParseOutputProposed(log types.Log) (*OPSuccinctL2OutputOracleOutputProposed, error) {
	event := new(OPSuccinctL2OutputOracleOutputProposed)
	if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "OutputProposed", log); err != nil {
		return nil, err
	}
	event.Raw = log
	return event, nil
}

// OPSuccinctL2OutputOracleOutputsDeletedIterator is returned from FilterOutputsDeleted and is used to iterate over the raw logs and unpacked data for OutputsDeleted events raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleOutputsDeletedIterator struct {
	Event *OPSuccinctL2OutputOracleOutputsDeleted // Event containing the contract specifics and raw log

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
func (it *OPSuccinctL2OutputOracleOutputsDeletedIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(OPSuccinctL2OutputOracleOutputsDeleted)
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
		it.Event = new(OPSuccinctL2OutputOracleOutputsDeleted)
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
func (it *OPSuccinctL2OutputOracleOutputsDeletedIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *OPSuccinctL2OutputOracleOutputsDeletedIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// OPSuccinctL2OutputOracleOutputsDeleted represents a OutputsDeleted event raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleOutputsDeleted struct {
	PrevNextOutputIndex *big.Int
	NewNextOutputIndex  *big.Int
	Raw                 types.Log // Blockchain specific contextual infos
}

// FilterOutputsDeleted is a free log retrieval operation binding the contract event 0x4ee37ac2c786ec85e87592d3c5c8a1dd66f8496dda3f125d9ea8ca5f657629b6.
//
// Solidity: event OutputsDeleted(uint256 indexed prevNextOutputIndex, uint256 indexed newNextOutputIndex)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) FilterOutputsDeleted(opts *bind.FilterOpts, prevNextOutputIndex []*big.Int, newNextOutputIndex []*big.Int) (*OPSuccinctL2OutputOracleOutputsDeletedIterator, error) {

	var prevNextOutputIndexRule []interface{}
	for _, prevNextOutputIndexItem := range prevNextOutputIndex {
		prevNextOutputIndexRule = append(prevNextOutputIndexRule, prevNextOutputIndexItem)
	}
	var newNextOutputIndexRule []interface{}
	for _, newNextOutputIndexItem := range newNextOutputIndex {
		newNextOutputIndexRule = append(newNextOutputIndexRule, newNextOutputIndexItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.FilterLogs(opts, "OutputsDeleted", prevNextOutputIndexRule, newNextOutputIndexRule)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctL2OutputOracleOutputsDeletedIterator{contract: _OPSuccinctL2OutputOracle.contract, event: "OutputsDeleted", logs: logs, sub: sub}, nil
}

// WatchOutputsDeleted is a free log subscription operation binding the contract event 0x4ee37ac2c786ec85e87592d3c5c8a1dd66f8496dda3f125d9ea8ca5f657629b6.
//
// Solidity: event OutputsDeleted(uint256 indexed prevNextOutputIndex, uint256 indexed newNextOutputIndex)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) WatchOutputsDeleted(opts *bind.WatchOpts, sink chan<- *OPSuccinctL2OutputOracleOutputsDeleted, prevNextOutputIndex []*big.Int, newNextOutputIndex []*big.Int) (event.Subscription, error) {

	var prevNextOutputIndexRule []interface{}
	for _, prevNextOutputIndexItem := range prevNextOutputIndex {
		prevNextOutputIndexRule = append(prevNextOutputIndexRule, prevNextOutputIndexItem)
	}
	var newNextOutputIndexRule []interface{}
	for _, newNextOutputIndexItem := range newNextOutputIndex {
		newNextOutputIndexRule = append(newNextOutputIndexRule, newNextOutputIndexItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.WatchLogs(opts, "OutputsDeleted", prevNextOutputIndexRule, newNextOutputIndexRule)
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(OPSuccinctL2OutputOracleOutputsDeleted)
				if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "OutputsDeleted", log); err != nil {
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
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) ParseOutputsDeleted(log types.Log) (*OPSuccinctL2OutputOracleOutputsDeleted, error) {
	event := new(OPSuccinctL2OutputOracleOutputsDeleted)
	if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "OutputsDeleted", log); err != nil {
		return nil, err
	}
	event.Raw = log
	return event, nil
}

// OPSuccinctL2OutputOracleOwnershipTransferredIterator is returned from FilterOwnershipTransferred and is used to iterate over the raw logs and unpacked data for OwnershipTransferred events raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleOwnershipTransferredIterator struct {
	Event *OPSuccinctL2OutputOracleOwnershipTransferred // Event containing the contract specifics and raw log

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
func (it *OPSuccinctL2OutputOracleOwnershipTransferredIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(OPSuccinctL2OutputOracleOwnershipTransferred)
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
		it.Event = new(OPSuccinctL2OutputOracleOwnershipTransferred)
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
func (it *OPSuccinctL2OutputOracleOwnershipTransferredIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *OPSuccinctL2OutputOracleOwnershipTransferredIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// OPSuccinctL2OutputOracleOwnershipTransferred represents a OwnershipTransferred event raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleOwnershipTransferred struct {
	PreviousOwner common.Address
	NewOwner      common.Address
	Raw           types.Log // Blockchain specific contextual infos
}

// FilterOwnershipTransferred is a free log retrieval operation binding the contract event 0x8be0079c531659141344cd1fd0a4f28419497f9722a3daafe3b4186f6b6457e0.
//
// Solidity: event OwnershipTransferred(address indexed previousOwner, address indexed newOwner)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) FilterOwnershipTransferred(opts *bind.FilterOpts, previousOwner []common.Address, newOwner []common.Address) (*OPSuccinctL2OutputOracleOwnershipTransferredIterator, error) {

	var previousOwnerRule []interface{}
	for _, previousOwnerItem := range previousOwner {
		previousOwnerRule = append(previousOwnerRule, previousOwnerItem)
	}
	var newOwnerRule []interface{}
	for _, newOwnerItem := range newOwner {
		newOwnerRule = append(newOwnerRule, newOwnerItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.FilterLogs(opts, "OwnershipTransferred", previousOwnerRule, newOwnerRule)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctL2OutputOracleOwnershipTransferredIterator{contract: _OPSuccinctL2OutputOracle.contract, event: "OwnershipTransferred", logs: logs, sub: sub}, nil
}

// WatchOwnershipTransferred is a free log subscription operation binding the contract event 0x8be0079c531659141344cd1fd0a4f28419497f9722a3daafe3b4186f6b6457e0.
//
// Solidity: event OwnershipTransferred(address indexed previousOwner, address indexed newOwner)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) WatchOwnershipTransferred(opts *bind.WatchOpts, sink chan<- *OPSuccinctL2OutputOracleOwnershipTransferred, previousOwner []common.Address, newOwner []common.Address) (event.Subscription, error) {

	var previousOwnerRule []interface{}
	for _, previousOwnerItem := range previousOwner {
		previousOwnerRule = append(previousOwnerRule, previousOwnerItem)
	}
	var newOwnerRule []interface{}
	for _, newOwnerItem := range newOwner {
		newOwnerRule = append(newOwnerRule, newOwnerItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.WatchLogs(opts, "OwnershipTransferred", previousOwnerRule, newOwnerRule)
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(OPSuccinctL2OutputOracleOwnershipTransferred)
				if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "OwnershipTransferred", log); err != nil {
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
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) ParseOwnershipTransferred(log types.Log) (*OPSuccinctL2OutputOracleOwnershipTransferred, error) {
	event := new(OPSuccinctL2OutputOracleOwnershipTransferred)
	if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "OwnershipTransferred", log); err != nil {
		return nil, err
	}
	event.Raw = log
	return event, nil
}

// OPSuccinctL2OutputOracleProposerUpdatedIterator is returned from FilterProposerUpdated and is used to iterate over the raw logs and unpacked data for ProposerUpdated events raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleProposerUpdatedIterator struct {
	Event *OPSuccinctL2OutputOracleProposerUpdated // Event containing the contract specifics and raw log

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
func (it *OPSuccinctL2OutputOracleProposerUpdatedIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(OPSuccinctL2OutputOracleProposerUpdated)
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
		it.Event = new(OPSuccinctL2OutputOracleProposerUpdated)
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
func (it *OPSuccinctL2OutputOracleProposerUpdatedIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *OPSuccinctL2OutputOracleProposerUpdatedIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// OPSuccinctL2OutputOracleProposerUpdated represents a ProposerUpdated event raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleProposerUpdated struct {
	Proposer common.Address
	Added    bool
	Raw      types.Log // Blockchain specific contextual infos
}

// FilterProposerUpdated is a free log retrieval operation binding the contract event 0x5df38d395edc15b669d646569bd015513395070b5b4deb8a16300abb060d1b5a.
//
// Solidity: event ProposerUpdated(address indexed proposer, bool added)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) FilterProposerUpdated(opts *bind.FilterOpts, proposer []common.Address) (*OPSuccinctL2OutputOracleProposerUpdatedIterator, error) {

	var proposerRule []interface{}
	for _, proposerItem := range proposer {
		proposerRule = append(proposerRule, proposerItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.FilterLogs(opts, "ProposerUpdated", proposerRule)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctL2OutputOracleProposerUpdatedIterator{contract: _OPSuccinctL2OutputOracle.contract, event: "ProposerUpdated", logs: logs, sub: sub}, nil
}

// WatchProposerUpdated is a free log subscription operation binding the contract event 0x5df38d395edc15b669d646569bd015513395070b5b4deb8a16300abb060d1b5a.
//
// Solidity: event ProposerUpdated(address indexed proposer, bool added)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) WatchProposerUpdated(opts *bind.WatchOpts, sink chan<- *OPSuccinctL2OutputOracleProposerUpdated, proposer []common.Address) (event.Subscription, error) {

	var proposerRule []interface{}
	for _, proposerItem := range proposer {
		proposerRule = append(proposerRule, proposerItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.WatchLogs(opts, "ProposerUpdated", proposerRule)
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(OPSuccinctL2OutputOracleProposerUpdated)
				if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "ProposerUpdated", log); err != nil {
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

// ParseProposerUpdated is a log parse operation binding the contract event 0x5df38d395edc15b669d646569bd015513395070b5b4deb8a16300abb060d1b5a.
//
// Solidity: event ProposerUpdated(address indexed proposer, bool added)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) ParseProposerUpdated(log types.Log) (*OPSuccinctL2OutputOracleProposerUpdated, error) {
	event := new(OPSuccinctL2OutputOracleProposerUpdated)
	if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "ProposerUpdated", log); err != nil {
		return nil, err
	}
	event.Raw = log
	return event, nil
}

// OPSuccinctL2OutputOracleSubmissionIntervalUpdatedIterator is returned from FilterSubmissionIntervalUpdated and is used to iterate over the raw logs and unpacked data for SubmissionIntervalUpdated events raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleSubmissionIntervalUpdatedIterator struct {
	Event *OPSuccinctL2OutputOracleSubmissionIntervalUpdated // Event containing the contract specifics and raw log

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
func (it *OPSuccinctL2OutputOracleSubmissionIntervalUpdatedIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(OPSuccinctL2OutputOracleSubmissionIntervalUpdated)
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
		it.Event = new(OPSuccinctL2OutputOracleSubmissionIntervalUpdated)
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
func (it *OPSuccinctL2OutputOracleSubmissionIntervalUpdatedIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *OPSuccinctL2OutputOracleSubmissionIntervalUpdatedIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// OPSuccinctL2OutputOracleSubmissionIntervalUpdated represents a SubmissionIntervalUpdated event raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleSubmissionIntervalUpdated struct {
	OldSubmissionInterval *big.Int
	NewSubmissionInterval *big.Int
	Raw                   types.Log // Blockchain specific contextual infos
}

// FilterSubmissionIntervalUpdated is a free log retrieval operation binding the contract event 0xc1bf9abfb57ea01ed9ecb4f45e9cefa7ba44b2e6778c3ce7281409999f1af1b2.
//
// Solidity: event SubmissionIntervalUpdated(uint256 oldSubmissionInterval, uint256 newSubmissionInterval)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) FilterSubmissionIntervalUpdated(opts *bind.FilterOpts) (*OPSuccinctL2OutputOracleSubmissionIntervalUpdatedIterator, error) {

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.FilterLogs(opts, "SubmissionIntervalUpdated")
	if err != nil {
		return nil, err
	}
	return &OPSuccinctL2OutputOracleSubmissionIntervalUpdatedIterator{contract: _OPSuccinctL2OutputOracle.contract, event: "SubmissionIntervalUpdated", logs: logs, sub: sub}, nil
}

// WatchSubmissionIntervalUpdated is a free log subscription operation binding the contract event 0xc1bf9abfb57ea01ed9ecb4f45e9cefa7ba44b2e6778c3ce7281409999f1af1b2.
//
// Solidity: event SubmissionIntervalUpdated(uint256 oldSubmissionInterval, uint256 newSubmissionInterval)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) WatchSubmissionIntervalUpdated(opts *bind.WatchOpts, sink chan<- *OPSuccinctL2OutputOracleSubmissionIntervalUpdated) (event.Subscription, error) {

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.WatchLogs(opts, "SubmissionIntervalUpdated")
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(OPSuccinctL2OutputOracleSubmissionIntervalUpdated)
				if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "SubmissionIntervalUpdated", log); err != nil {
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

// ParseSubmissionIntervalUpdated is a log parse operation binding the contract event 0xc1bf9abfb57ea01ed9ecb4f45e9cefa7ba44b2e6778c3ce7281409999f1af1b2.
//
// Solidity: event SubmissionIntervalUpdated(uint256 oldSubmissionInterval, uint256 newSubmissionInterval)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) ParseSubmissionIntervalUpdated(log types.Log) (*OPSuccinctL2OutputOracleSubmissionIntervalUpdated, error) {
	event := new(OPSuccinctL2OutputOracleSubmissionIntervalUpdated)
	if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "SubmissionIntervalUpdated", log); err != nil {
		return nil, err
	}
	event.Raw = log
	return event, nil
}

// OPSuccinctL2OutputOracleVerifierUpdatedIterator is returned from FilterVerifierUpdated and is used to iterate over the raw logs and unpacked data for VerifierUpdated events raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleVerifierUpdatedIterator struct {
	Event *OPSuccinctL2OutputOracleVerifierUpdated // Event containing the contract specifics and raw log

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
func (it *OPSuccinctL2OutputOracleVerifierUpdatedIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(OPSuccinctL2OutputOracleVerifierUpdated)
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
		it.Event = new(OPSuccinctL2OutputOracleVerifierUpdated)
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
func (it *OPSuccinctL2OutputOracleVerifierUpdatedIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *OPSuccinctL2OutputOracleVerifierUpdatedIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// OPSuccinctL2OutputOracleVerifierUpdated represents a VerifierUpdated event raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleVerifierUpdated struct {
	OldVerifier common.Address
	NewVerifier common.Address
	Raw         types.Log // Blockchain specific contextual infos
}

// FilterVerifierUpdated is a free log retrieval operation binding the contract event 0x0243549a92b2412f7a3caf7a2e56d65b8821b91345363faa5f57195384065fcc.
//
// Solidity: event VerifierUpdated(address indexed oldVerifier, address indexed newVerifier)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) FilterVerifierUpdated(opts *bind.FilterOpts, oldVerifier []common.Address, newVerifier []common.Address) (*OPSuccinctL2OutputOracleVerifierUpdatedIterator, error) {

	var oldVerifierRule []interface{}
	for _, oldVerifierItem := range oldVerifier {
		oldVerifierRule = append(oldVerifierRule, oldVerifierItem)
	}
	var newVerifierRule []interface{}
	for _, newVerifierItem := range newVerifier {
		newVerifierRule = append(newVerifierRule, newVerifierItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.FilterLogs(opts, "VerifierUpdated", oldVerifierRule, newVerifierRule)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctL2OutputOracleVerifierUpdatedIterator{contract: _OPSuccinctL2OutputOracle.contract, event: "VerifierUpdated", logs: logs, sub: sub}, nil
}

// WatchVerifierUpdated is a free log subscription operation binding the contract event 0x0243549a92b2412f7a3caf7a2e56d65b8821b91345363faa5f57195384065fcc.
//
// Solidity: event VerifierUpdated(address indexed oldVerifier, address indexed newVerifier)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) WatchVerifierUpdated(opts *bind.WatchOpts, sink chan<- *OPSuccinctL2OutputOracleVerifierUpdated, oldVerifier []common.Address, newVerifier []common.Address) (event.Subscription, error) {

	var oldVerifierRule []interface{}
	for _, oldVerifierItem := range oldVerifier {
		oldVerifierRule = append(oldVerifierRule, oldVerifierItem)
	}
	var newVerifierRule []interface{}
	for _, newVerifierItem := range newVerifier {
		newVerifierRule = append(newVerifierRule, newVerifierItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.WatchLogs(opts, "VerifierUpdated", oldVerifierRule, newVerifierRule)
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(OPSuccinctL2OutputOracleVerifierUpdated)
				if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "VerifierUpdated", log); err != nil {
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

// ParseVerifierUpdated is a log parse operation binding the contract event 0x0243549a92b2412f7a3caf7a2e56d65b8821b91345363faa5f57195384065fcc.
//
// Solidity: event VerifierUpdated(address indexed oldVerifier, address indexed newVerifier)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) ParseVerifierUpdated(log types.Log) (*OPSuccinctL2OutputOracleVerifierUpdated, error) {
	event := new(OPSuccinctL2OutputOracleVerifierUpdated)
	if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "VerifierUpdated", log); err != nil {
		return nil, err
	}
	event.Raw = log
	return event, nil
}
