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
}

// TypesOutputProposal is an auto generated low-level Go binding around an user-defined struct.
type TypesOutputProposal struct {
	OutputRoot    [32]byte
	Timestamp     *big.Int
	L2BlockNumber *big.Int
}

// OPSuccinctL2OutputOracleMetaData contains all meta data concerning the OPSuccinctL2OutputOracle contract.
var OPSuccinctL2OutputOracleMetaData = &bind.MetaData{
	ABI: "[{\"type\":\"constructor\",\"inputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"CHALLENGER\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"FINALIZATION_PERIOD_SECONDS\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"L2_BLOCK_TIME\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"PROPOSER\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"SUBMISSION_INTERVAL\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"addProposer\",\"inputs\":[{\"name\":\"_proposer\",\"type\":\"address\",\"internalType\":\"address\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"aggregationVkey\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"approvedProposers\",\"inputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"outputs\":[{\"name\":\"\",\"type\":\"bool\",\"internalType\":\"bool\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"challenger\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"checkpointBlockHash\",\"inputs\":[{\"name\":\"_blockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"computeL2Timestamp\",\"inputs\":[{\"name\":\"_l2BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"deleteL2Outputs\",\"inputs\":[{\"name\":\"_l2OutputIndex\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"finalizationPeriodSeconds\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"getL2Output\",\"inputs\":[{\"name\":\"_l2OutputIndex\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[{\"name\":\"\",\"type\":\"tuple\",\"internalType\":\"structTypes.OutputProposal\",\"components\":[{\"name\":\"outputRoot\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"timestamp\",\"type\":\"uint128\",\"internalType\":\"uint128\"},{\"name\":\"l2BlockNumber\",\"type\":\"uint128\",\"internalType\":\"uint128\"}]}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"getL2OutputAfter\",\"inputs\":[{\"name\":\"_l2BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[{\"name\":\"\",\"type\":\"tuple\",\"internalType\":\"structTypes.OutputProposal\",\"components\":[{\"name\":\"outputRoot\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"timestamp\",\"type\":\"uint128\",\"internalType\":\"uint128\"},{\"name\":\"l2BlockNumber\",\"type\":\"uint128\",\"internalType\":\"uint128\"}]}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"getL2OutputIndexAfter\",\"inputs\":[{\"name\":\"_l2BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"historicBlockHashes\",\"inputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[{\"name\":\"\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"initialize\",\"inputs\":[{\"name\":\"_initParams\",\"type\":\"tuple\",\"internalType\":\"structOPSuccinctL2OutputOracle.InitParams\",\"components\":[{\"name\":\"challenger\",\"type\":\"address\",\"internalType\":\"address\"},{\"name\":\"proposer\",\"type\":\"address\",\"internalType\":\"address\"},{\"name\":\"owner\",\"type\":\"address\",\"internalType\":\"address\"},{\"name\":\"finalizationPeriodSeconds\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"l2BlockTime\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"aggregationVkey\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"rangeVkeyCommitment\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"rollupConfigHash\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"startingOutputRoot\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"startingBlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"startingTimestamp\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"submissionInterval\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"verifier\",\"type\":\"address\",\"internalType\":\"address\"}]}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"l2BlockTime\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"latestBlockNumber\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"latestOutputIndex\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"nextBlockNumber\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"nextOutputIndex\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"owner\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"proposeL2Output\",\"inputs\":[{\"name\":\"_outputRoot\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"},{\"name\":\"_l2BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"_l1BlockNumber\",\"type\":\"uint256\",\"internalType\":\"uint256\"},{\"name\":\"_proof\",\"type\":\"bytes\",\"internalType\":\"bytes\"}],\"outputs\":[],\"stateMutability\":\"payable\"},{\"type\":\"function\",\"name\":\"proposer\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"rangeVkeyCommitment\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"removeProposer\",\"inputs\":[{\"name\":\"_proposer\",\"type\":\"address\",\"internalType\":\"address\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"rollupConfigHash\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"startingBlockNumber\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"startingTimestamp\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"submissionInterval\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"transferOwnership\",\"inputs\":[{\"name\":\"_owner\",\"type\":\"address\",\"internalType\":\"address\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"updateAggregationVkey\",\"inputs\":[{\"name\":\"_aggregationVkey\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"updateRangeVkeyCommitment\",\"inputs\":[{\"name\":\"_rangeVkeyCommitment\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"updateRollupConfigHash\",\"inputs\":[{\"name\":\"_rollupConfigHash\",\"type\":\"bytes32\",\"internalType\":\"bytes32\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"updateSubmissionInterval\",\"inputs\":[{\"name\":\"_submissionInterval\",\"type\":\"uint256\",\"internalType\":\"uint256\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"updateVerifier\",\"inputs\":[{\"name\":\"_verifier\",\"type\":\"address\",\"internalType\":\"address\"}],\"outputs\":[],\"stateMutability\":\"nonpayable\"},{\"type\":\"function\",\"name\":\"verifier\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"address\",\"internalType\":\"address\"}],\"stateMutability\":\"view\"},{\"type\":\"function\",\"name\":\"version\",\"inputs\":[],\"outputs\":[{\"name\":\"\",\"type\":\"string\",\"internalType\":\"string\"}],\"stateMutability\":\"view\"},{\"type\":\"event\",\"name\":\"AggregationVkeyUpdated\",\"inputs\":[{\"name\":\"oldAggregationVkey\",\"type\":\"bytes32\",\"indexed\":true,\"internalType\":\"bytes32\"},{\"name\":\"newAggregationVkey\",\"type\":\"bytes32\",\"indexed\":true,\"internalType\":\"bytes32\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"Initialized\",\"inputs\":[{\"name\":\"version\",\"type\":\"uint8\",\"indexed\":false,\"internalType\":\"uint8\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"OutputProposed\",\"inputs\":[{\"name\":\"outputRoot\",\"type\":\"bytes32\",\"indexed\":true,\"internalType\":\"bytes32\"},{\"name\":\"l2OutputIndex\",\"type\":\"uint256\",\"indexed\":true,\"internalType\":\"uint256\"},{\"name\":\"l2BlockNumber\",\"type\":\"uint256\",\"indexed\":true,\"internalType\":\"uint256\"},{\"name\":\"l1Timestamp\",\"type\":\"uint256\",\"indexed\":false,\"internalType\":\"uint256\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"OutputsDeleted\",\"inputs\":[{\"name\":\"prevNextOutputIndex\",\"type\":\"uint256\",\"indexed\":true,\"internalType\":\"uint256\"},{\"name\":\"newNextOutputIndex\",\"type\":\"uint256\",\"indexed\":true,\"internalType\":\"uint256\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"OwnershipTransferred\",\"inputs\":[{\"name\":\"previousOwner\",\"type\":\"address\",\"indexed\":true,\"internalType\":\"address\"},{\"name\":\"newOwner\",\"type\":\"address\",\"indexed\":true,\"internalType\":\"address\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"ProposerUpdated\",\"inputs\":[{\"name\":\"proposer\",\"type\":\"address\",\"indexed\":true,\"internalType\":\"address\"},{\"name\":\"added\",\"type\":\"bool\",\"indexed\":false,\"internalType\":\"bool\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"RangeVkeyCommitmentUpdated\",\"inputs\":[{\"name\":\"oldRangeVkeyCommitment\",\"type\":\"bytes32\",\"indexed\":true,\"internalType\":\"bytes32\"},{\"name\":\"newRangeVkeyCommitment\",\"type\":\"bytes32\",\"indexed\":true,\"internalType\":\"bytes32\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"RollupConfigHashUpdated\",\"inputs\":[{\"name\":\"oldRollupConfigHash\",\"type\":\"bytes32\",\"indexed\":true,\"internalType\":\"bytes32\"},{\"name\":\"newRollupConfigHash\",\"type\":\"bytes32\",\"indexed\":true,\"internalType\":\"bytes32\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"SubmissionIntervalUpdated\",\"inputs\":[{\"name\":\"oldSubmissionInterval\",\"type\":\"uint256\",\"indexed\":false,\"internalType\":\"uint256\"},{\"name\":\"newSubmissionInterval\",\"type\":\"uint256\",\"indexed\":false,\"internalType\":\"uint256\"}],\"anonymous\":false},{\"type\":\"event\",\"name\":\"VerifierUpdated\",\"inputs\":[{\"name\":\"oldVerifier\",\"type\":\"address\",\"indexed\":true,\"internalType\":\"address\"},{\"name\":\"newVerifier\",\"type\":\"address\",\"indexed\":true,\"internalType\":\"address\"}],\"anonymous\":false},{\"type\":\"error\",\"name\":\"L1BlockHashNotAvailable\",\"inputs\":[]},{\"type\":\"error\",\"name\":\"L1BlockHashNotCheckpointed\",\"inputs\":[]}]",
	Bin: "0x608060405234801561001057600080fd5b5061001961001e565b6100de565b600054610100900460ff161561008a5760405162461bcd60e51b815260206004820152602760248201527f496e697469616c697a61626c653a20636f6e747261637420697320696e697469604482015266616c697a696e6760c81b606482015260840160405180910390fd5b60005460ff90811610156100dc576000805460ff191660ff9081179091556040519081527f7f26b83ff96e1f2b6a682f133852f6798a09c465da95921460cefb38474024989060200160405180910390a15b565b611c57806100ed6000396000f3fe60806040526004361061023a5760003560e01c806393991af31161012e578063c4cb03ec116100ab578063db1470f51161006f578063db1470f5146106be578063dcec3348146106de578063e1a41bcf146106f3578063f2fde38b14610709578063f4daa2911461072957600080fd5b8063c4cb03ec14610608578063ce5db8d614610628578063cf8e5cf01461063e578063d1de856c1461065e578063d46512761461067e57600080fd5b8063a8e4fb90116100f2578063a8e4fb9014610574578063b03cd41814610594578063bc91ce33146105b4578063bffa7f0f146105d4578063c32e4e3e146105f257600080fd5b806393991af3146104ab57806397fc007c146104c15780639ad84880146104e1578063a196b525146104f4578063a25ae5571461052157600080fd5b806354fd4d50116101bc57806370872aa51161018057806370872aa51461041f5780637f00642014610435578063887862721461045557806389c44cbb1461046b5780638da5cb5b1461048b57600080fd5b806354fd4d501461037d57806369f16eec146103c15780636abcf563146103d65780636b4d98dd146103eb5780636d9a1c8b1461040957600080fd5b80632b7ac3f3116102035780632b7ac3f3146102db578063336c9e81146103135780634599c78814610333578063529933df14610348578063534db0e21461035d57600080fd5b80622134cc1461023f57806309d632d3146102635780631bdd450c146102855780631e856800146102a55780632b31841e146102c5575b600080fd5b34801561024b57600080fd5b506005545b6040519081526020015b60405180910390f35b34801561026f57600080fd5b5061028361027e366004611888565b61073e565b005b34801561029157600080fd5b506102836102a03660046118aa565b6107c7565b3480156102b157600080fd5b506102836102c03660046118aa565b610825565b3480156102d157600080fd5b50610250600a5481565b3480156102e757600080fd5b50600b546102fb906001600160a01b031681565b6040516001600160a01b03909116815260200161025a565b34801561031f57600080fd5b5061028361032e3660046118aa565b610857565b34801561033f57600080fd5b506102506108c2565b34801561035457600080fd5b50600454610250565b34801561036957600080fd5b506006546102fb906001600160a01b031681565b34801561038957600080fd5b506103b46040518060400160405280600b81526020016a0626574612d76302e322e360ac1b81525081565b60405161025a9190611910565b3480156103cd57600080fd5b5061025061091f565b3480156103e257600080fd5b50600354610250565b3480156103f757600080fd5b506006546001600160a01b03166102fb565b34801561041557600080fd5b50610250600c5481565b34801561042b57600080fd5b5061025060015481565b34801561044157600080fd5b506102506104503660046118aa565b610931565b34801561046157600080fd5b5061025060025481565b34801561047757600080fd5b506102836104863660046118aa565b610acf565b34801561049757600080fd5b50600d546102fb906001600160a01b031681565b3480156104b757600080fd5b5061025060055481565b3480156104cd57600080fd5b506102836104dc366004611888565b610cd4565b6102836104ef366004611994565b610d5a565b34801561050057600080fd5b5061025061050f3660046118aa565b600f6020526000908152604090205481565b34801561052d57600080fd5b5061054161053c3660046118aa565b6111cc565b60408051825181526020808401516001600160801b0390811691830191909152928201519092169082015260600161025a565b34801561058057600080fd5b506007546102fb906001600160a01b031681565b3480156105a057600080fd5b506102836105af366004611888565b61124a565b3480156105c057600080fd5b506102836105cf3660046118aa565b6112cb565b3480156105e057600080fd5b506007546001600160a01b03166102fb565b3480156105fe57600080fd5b5061025060095481565b34801561061457600080fd5b506102836106233660046118aa565b611329565b34801561063457600080fd5b5061025060085481565b34801561064a57600080fd5b506105416106593660046118aa565b611387565b34801561066a57600080fd5b506102506106793660046118aa565b6113bf565b34801561068a57600080fd5b506106ae610699366004611888565b600e6020526000908152604090205460ff1681565b604051901515815260200161025a565b3480156106ca57600080fd5b506102836106d9366004611a46565b6113ef565b3480156106ea57600080fd5b506102506117cf565b3480156106ff57600080fd5b5061025060045481565b34801561071557600080fd5b50610283610724366004611888565b6117e6565b34801561073557600080fd5b50600854610250565b600d546001600160a01b031633146107715760405162461bcd60e51b815260040161076890611b09565b60405180910390fd5b6001600160a01b0381166000818152600e60209081526040808320805460ff19169055519182527f5df38d395edc15b669d646569bd015513395070b5b4deb8a16300abb060d1b5a91015b60405180910390a250565b600d546001600160a01b031633146107f15760405162461bcd60e51b815260040161076890611b09565b600c546040518291907f5d9ebe9f09b0810b3546b30781ba9a51092b37dd6abada4b830ce54a41ac6a4b90600090a3600c55565b804080610845576040516321301a1960e21b815260040160405180910390fd5b6000918252600f602052604090912055565b600d546001600160a01b031633146108815760405162461bcd60e51b815260040161076890611b09565b60045460408051918252602082018390527fc1bf9abfb57ea01ed9ecb4f45e9cefa7ba44b2e6778c3ce7281409999f1af1b2910160405180910390a1600455565b6003546000901561091657600380546108dd90600190611b66565b815481106108ed576108ed611b7d565b6000918252602090912060029091020160010154600160801b90046001600160801b0316919050565b6001545b905090565b60035460009061091a90600190611b66565b600061093b6108c2565b8211156109c15760405162461bcd60e51b815260206004820152604860248201527f4c324f75747075744f7261636c653a2063616e6e6f7420676574206f7574707560448201527f7420666f72206120626c6f636b207468617420686173206e6f74206265656e206064820152671c1c9bdc1bdcd95960c21b608482015260a401610768565b600354610a455760405162461bcd60e51b815260206004820152604660248201527f4c324f75747075744f7261636c653a2063616e6e6f7420676574206f7574707560448201527f74206173206e6f206f7574707574732068617665206265656e2070726f706f736064820152651959081e595d60d21b608482015260a401610768565b6003546000905b80821015610ac85760006002610a628385611b93565b610a6c9190611bab565b90508460038281548110610a8257610a82611b7d565b6000918252602090912060029091020160010154600160801b90046001600160801b03161015610abe57610ab7816001611b93565b9250610ac2565b8091505b50610a4c565b5092915050565b6006546001600160a01b03163314610b4f5760405162461bcd60e51b815260206004820152603e60248201527f4c324f75747075744f7261636c653a206f6e6c7920746865206368616c6c656e60448201527f67657220616464726573732063616e2064656c657465206f75747075747300006064820152608401610768565b6003548110610bd25760405162461bcd60e51b815260206004820152604360248201527f4c324f75747075744f7261636c653a2063616e6e6f742064656c657465206f7560448201527f747075747320616674657220746865206c6174657374206f757470757420696e6064820152620c8caf60eb1b608482015260a401610768565b60085460038281548110610be857610be8611b7d565b6000918252602090912060016002909202010154610c0f906001600160801b031642611b66565b10610c915760405162461bcd60e51b815260206004820152604660248201527f4c324f75747075744f7261636c653a2063616e6e6f742064656c657465206f7560448201527f74707574732074686174206861766520616c7265616479206265656e2066696e606482015265185b1a5e995960d21b608482015260a401610768565b6000610c9c60035490565b90508160035581817f4ee37ac2c786ec85e87592d3c5c8a1dd66f8496dda3f125d9ea8ca5f657629b660405160405180910390a35050565b600d546001600160a01b03163314610cfe5760405162461bcd60e51b815260040161076890611b09565b600b546040516001600160a01b038084169216907f0243549a92b2412f7a3caf7a2e56d65b8821b91345363faa5f57195384065fcc90600090a3600b80546001600160a01b0319166001600160a01b0392909216919091179055565b336000908152600e602052604090205460ff1680610da2575060008052600e6020527fe710864318d4a32f37d6ce54cb3fadbef648dd12d8dbdf53973564d56b7f881c5460ff165b610e145760405162461bcd60e51b815260206004820152603f60248201527f4c324f75747075744f7261636c653a206f6e6c7920617070726f76656420707260448201527f6f706f736572732063616e2070726f706f7365206e6577206f757470757473006064820152608401610768565b610e1c6117cf565b831015610eb75760405162461bcd60e51b815260206004820152605860248201527f4c324f75747075744f7261636c653a20626c6f636b206e756d626572206d757360448201527f742062652067726561746572207468616e206f7220657175616c20746f206e6560648201527f787420657870656374656420626c6f636b206e756d6265720000000000000000608482015260a401610768565b42610ec1846113bf565b10610f2d5760405162461bcd60e51b815260206004820152603660248201527f4c324f75747075744f7261636c653a2063616e6e6f742070726f706f7365204c60448201527532206f757470757420696e207468652066757475726560501b6064820152608401610768565b83610fa05760405162461bcd60e51b815260206004820152603a60248201527f4c324f75747075744f7261636c653a204c32206f75747075742070726f706f7360448201527f616c2063616e6e6f7420626520746865207a65726f20686173680000000000006064820152608401610768565b6000828152600f602052604090205480610fcd57604051630455475360e31b815260040160405180910390fd5b60006040518060c001604052808381526020016003610fea61091f565b81548110610ffa57610ffa611b7d565b60009182526020918290206002909102015482528181018990526040808301899052600c54606080850191909152600a54608094850152600b5460095483518751818701529487015185850152928601518483015290850151838501529284015160a08084019190915284015160c08301529293506001600160a01b03909116916341493c609160e001604051602081830303815290604052866040518463ffffffff1660e01b81526004016110b293929190611bcd565b60006040518083038186803b1580156110ca57600080fd5b505afa1580156110de573d6000803e3d6000fd5b50505050846110ec60035490565b877fa7aaf2512769da4e444e3de247be2564225c2e7a8f74cfe528e46e17d24868e24260405161111e91815260200190565b60405180910390a45050604080516060810182529485526001600160801b034281166020870190815294811691860191825260038054600181018255600091909152955160029096027fc2575a0e9e593c00f959f8c92f12db2869c3395a3b0502d05e2516446f71f85b810196909655935190518416600160801b029316929092177fc2575a0e9e593c00f959f8c92f12db2869c3395a3b0502d05e2516446f71f85c909301929092555050565b6040805160608101825260008082526020820181905291810191909152600382815481106111fc576111fc611b7d565b600091825260209182902060408051606081018252600290930290910180548352600101546001600160801b0380821694840194909452600160801b90049092169181019190915292915050565b600d546001600160a01b031633146112745760405162461bcd60e51b815260040161076890611b09565b6001600160a01b0381166000818152600e6020908152604091829020805460ff1916600190811790915591519182527f5df38d395edc15b669d646569bd015513395070b5b4deb8a16300abb060d1b5a91016107bc565b600d546001600160a01b031633146112f55760405162461bcd60e51b815260040161076890611b09565b600a546040518291907fbf8cab6317796bfa97fea82b6d27c9542a08fa0821813cf2a57e7cff7fdc815690600090a3600a55565b600d546001600160a01b031633146113535760405162461bcd60e51b815260040161076890611b09565b6009546040518291907f390b73b2b067afcef04d30b573e4590c6e565519e370927dd777ca0ce8a55db090600090a3600955565b604080516060810182526000808252602082018190529181019190915260036113af83610931565b815481106111fc576111fc611b7d565b6000600554600154836113d29190611b66565b6113dc9190611c02565b6002546113e99190611b93565b92915050565b600054610100900460ff161580801561140f5750600054600160ff909116105b806114295750303b158015611429575060005460ff166001145b61148c5760405162461bcd60e51b815260206004820152602e60248201527f496e697469616c697a61626c653a20636f6e747261637420697320616c72656160448201526d191e481a5b9a5d1a585b1a5e995960921b6064820152608401610768565b6000805460ff1916600117905580156114af576000805461ff0019166101001790555b60008261016001511161152a5760405162461bcd60e51b815260206004820152603a60248201527f4c324f75747075744f7261636c653a207375626d697373696f6e20696e74657260448201527f76616c206d7573742062652067726561746572207468616e20300000000000006064820152608401610768565b600082608001511161159b5760405162461bcd60e51b815260206004820152603460248201527f4c324f75747075744f7261636c653a204c3220626c6f636b2074696d65206d75604482015273073742062652067726561746572207468616e20360641b6064820152608401610768565b4282610140015111156116245760405162461bcd60e51b8152602060048201526044602482018190527f4c324f75747075744f7261636c653a207374617274696e67204c322074696d65908201527f7374616d70206d757374206265206c657373207468616e2063757272656e742060648201526374696d6560e01b608482015260a401610768565b61016082015160045560808201516005556003546000036116fb57604080516060810182526101008401518152610140840180516001600160801b03908116602084019081526101208701805183169585019586526003805460018181018355600092909252955160029687027fc2575a0e9e593c00f959f8c92f12db2869c3395a3b0502d05e2516446f71f85b810191909155925196518416600160801b0296909316959095177fc2575a0e9e593c00f959f8c92f12db2869c3395a3b0502d05e2516446f71f85c909101559251909255905190555b8151600680546001600160a01b039283166001600160a01b031991821617909155606084015160085560208085015183166000908152600e909152604090819020805460ff1916600117905560a085015160095560c0850151600a55610180850151600b805491851691841691909117905560e0850151600c55840151600d805491909316911617905580156117cb576000805461ff0019169055604051600181527f7f26b83ff96e1f2b6a682f133852f6798a09c465da95921460cefb38474024989060200160405180910390a15b5050565b60006004546117dc6108c2565b61091a9190611b93565b600d546001600160a01b031633146118105760405162461bcd60e51b815260040161076890611b09565b600d546040516001600160a01b038084169216907f8be0079c531659141344cd1fd0a4f28419497f9722a3daafe3b4186f6b6457e090600090a3600d80546001600160a01b0319166001600160a01b0392909216919091179055565b80356001600160a01b038116811461188357600080fd5b919050565b60006020828403121561189a57600080fd5b6118a38261186c565b9392505050565b6000602082840312156118bc57600080fd5b5035919050565b6000815180845260005b818110156118e9576020818501810151868301820152016118cd565b818111156118fb576000602083870101525b50601f01601f19169290920160200192915050565b6020815260006118a360208301846118c3565b634e487b7160e01b600052604160045260246000fd5b6040516101a0810167ffffffffffffffff8111828210171561195d5761195d611923565b60405290565b604051601f8201601f1916810167ffffffffffffffff8111828210171561198c5761198c611923565b604052919050565b600080600080608085870312156119aa57600080fd5b84359350602080860135935060408601359250606086013567ffffffffffffffff808211156119d857600080fd5b818801915088601f8301126119ec57600080fd5b8135818111156119fe576119fe611923565b611a10601f8201601f19168501611963565b91508082528984828501011115611a2657600080fd5b808484018584013760008482840101525080935050505092959194509250565b60006101a08284031215611a5957600080fd5b611a61611939565b611a6a8361186c565b8152611a786020840161186c565b6020820152611a896040840161186c565b6040820152606083013560608201526080830135608082015260a083013560a082015260c083013560c082015260e083013560e0820152610100808401358183015250610120808401358183015250610140808401358183015250610160808401358183015250610180611afe81850161186c565b908201529392505050565b60208082526027908201527f4c324f75747075744f7261636c653a2063616c6c6572206973206e6f74207468604082015266329037bbb732b960c91b606082015260800190565b634e487b7160e01b600052601160045260246000fd5b600082821015611b7857611b78611b50565b500390565b634e487b7160e01b600052603260045260246000fd5b60008219821115611ba657611ba6611b50565b500190565b600082611bc857634e487b7160e01b600052601260045260246000fd5b500490565b838152606060208201526000611be660608301856118c3565b8281036040840152611bf881856118c3565b9695505050505050565b6000816000190483118215151615611c1c57611c1c611b50565b50029056fea264697066735822122023138f5c218ba36ca3229d9d1186e3a32a72aeebd857796d4bda7374247683e764736f6c634300080f0033",
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

// CHALLENGER is a free data retrieval call binding the contract method 0x6b4d98dd.
//
// Solidity: function CHALLENGER() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) CHALLENGER(opts *bind.CallOpts) (common.Address, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "CHALLENGER")

	if err != nil {
		return *new(common.Address), err
	}

	out0 := *abi.ConvertType(out[0], new(common.Address)).(*common.Address)

	return out0, err

}

// CHALLENGER is a free data retrieval call binding the contract method 0x6b4d98dd.
//
// Solidity: function CHALLENGER() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) CHALLENGER() (common.Address, error) {
	return _OPSuccinctL2OutputOracle.Contract.CHALLENGER(&_OPSuccinctL2OutputOracle.CallOpts)
}

// CHALLENGER is a free data retrieval call binding the contract method 0x6b4d98dd.
//
// Solidity: function CHALLENGER() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) CHALLENGER() (common.Address, error) {
	return _OPSuccinctL2OutputOracle.Contract.CHALLENGER(&_OPSuccinctL2OutputOracle.CallOpts)
}

// FINALIZATIONPERIODSECONDS is a free data retrieval call binding the contract method 0xf4daa291.
//
// Solidity: function FINALIZATION_PERIOD_SECONDS() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) FINALIZATIONPERIODSECONDS(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "FINALIZATION_PERIOD_SECONDS")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// FINALIZATIONPERIODSECONDS is a free data retrieval call binding the contract method 0xf4daa291.
//
// Solidity: function FINALIZATION_PERIOD_SECONDS() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) FINALIZATIONPERIODSECONDS() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.FINALIZATIONPERIODSECONDS(&_OPSuccinctL2OutputOracle.CallOpts)
}

// FINALIZATIONPERIODSECONDS is a free data retrieval call binding the contract method 0xf4daa291.
//
// Solidity: function FINALIZATION_PERIOD_SECONDS() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) FINALIZATIONPERIODSECONDS() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.FINALIZATIONPERIODSECONDS(&_OPSuccinctL2OutputOracle.CallOpts)
}

// L2BLOCKTIME is a free data retrieval call binding the contract method 0x002134cc.
//
// Solidity: function L2_BLOCK_TIME() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) L2BLOCKTIME(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "L2_BLOCK_TIME")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// L2BLOCKTIME is a free data retrieval call binding the contract method 0x002134cc.
//
// Solidity: function L2_BLOCK_TIME() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) L2BLOCKTIME() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.L2BLOCKTIME(&_OPSuccinctL2OutputOracle.CallOpts)
}

// L2BLOCKTIME is a free data retrieval call binding the contract method 0x002134cc.
//
// Solidity: function L2_BLOCK_TIME() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) L2BLOCKTIME() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.L2BLOCKTIME(&_OPSuccinctL2OutputOracle.CallOpts)
}

// PROPOSER is a free data retrieval call binding the contract method 0xbffa7f0f.
//
// Solidity: function PROPOSER() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) PROPOSER(opts *bind.CallOpts) (common.Address, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "PROPOSER")

	if err != nil {
		return *new(common.Address), err
	}

	out0 := *abi.ConvertType(out[0], new(common.Address)).(*common.Address)

	return out0, err

}

// PROPOSER is a free data retrieval call binding the contract method 0xbffa7f0f.
//
// Solidity: function PROPOSER() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) PROPOSER() (common.Address, error) {
	return _OPSuccinctL2OutputOracle.Contract.PROPOSER(&_OPSuccinctL2OutputOracle.CallOpts)
}

// PROPOSER is a free data retrieval call binding the contract method 0xbffa7f0f.
//
// Solidity: function PROPOSER() view returns(address)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) PROPOSER() (common.Address, error) {
	return _OPSuccinctL2OutputOracle.Contract.PROPOSER(&_OPSuccinctL2OutputOracle.CallOpts)
}

// SUBMISSIONINTERVAL is a free data retrieval call binding the contract method 0x529933df.
//
// Solidity: function SUBMISSION_INTERVAL() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCaller) SUBMISSIONINTERVAL(opts *bind.CallOpts) (*big.Int, error) {
	var out []interface{}
	err := _OPSuccinctL2OutputOracle.contract.Call(opts, &out, "SUBMISSION_INTERVAL")

	if err != nil {
		return *new(*big.Int), err
	}

	out0 := *abi.ConvertType(out[0], new(*big.Int)).(**big.Int)

	return out0, err

}

// SUBMISSIONINTERVAL is a free data retrieval call binding the contract method 0x529933df.
//
// Solidity: function SUBMISSION_INTERVAL() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) SUBMISSIONINTERVAL() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.SUBMISSIONINTERVAL(&_OPSuccinctL2OutputOracle.CallOpts)
}

// SUBMISSIONINTERVAL is a free data retrieval call binding the contract method 0x529933df.
//
// Solidity: function SUBMISSION_INTERVAL() view returns(uint256)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleCallerSession) SUBMISSIONINTERVAL() (*big.Int, error) {
	return _OPSuccinctL2OutputOracle.Contract.SUBMISSIONINTERVAL(&_OPSuccinctL2OutputOracle.CallOpts)
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

// Initialize is a paid mutator transaction binding the contract method 0xdb1470f5.
//
// Solidity: function initialize((address,address,address,uint256,uint256,bytes32,bytes32,bytes32,bytes32,uint256,uint256,uint256,address) _initParams) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) Initialize(opts *bind.TransactOpts, _initParams OPSuccinctL2OutputOracleInitParams) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "initialize", _initParams)
}

// Initialize is a paid mutator transaction binding the contract method 0xdb1470f5.
//
// Solidity: function initialize((address,address,address,uint256,uint256,bytes32,bytes32,bytes32,bytes32,uint256,uint256,uint256,address) _initParams) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) Initialize(_initParams OPSuccinctL2OutputOracleInitParams) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.Initialize(&_OPSuccinctL2OutputOracle.TransactOpts, _initParams)
}

// Initialize is a paid mutator transaction binding the contract method 0xdb1470f5.
//
// Solidity: function initialize((address,address,address,uint256,uint256,bytes32,bytes32,bytes32,bytes32,uint256,uint256,uint256,address) _initParams) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) Initialize(_initParams OPSuccinctL2OutputOracleInitParams) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.Initialize(&_OPSuccinctL2OutputOracle.TransactOpts, _initParams)
}

// ProposeL2Output is a paid mutator transaction binding the contract method 0x9ad84880.
//
// Solidity: function proposeL2Output(bytes32 _outputRoot, uint256 _l2BlockNumber, uint256 _l1BlockNumber, bytes _proof) payable returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) ProposeL2Output(opts *bind.TransactOpts, _outputRoot [32]byte, _l2BlockNumber *big.Int, _l1BlockNumber *big.Int, _proof []byte) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "proposeL2Output", _outputRoot, _l2BlockNumber, _l1BlockNumber, _proof)
}

// ProposeL2Output is a paid mutator transaction binding the contract method 0x9ad84880.
//
// Solidity: function proposeL2Output(bytes32 _outputRoot, uint256 _l2BlockNumber, uint256 _l1BlockNumber, bytes _proof) payable returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) ProposeL2Output(_outputRoot [32]byte, _l2BlockNumber *big.Int, _l1BlockNumber *big.Int, _proof []byte) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.ProposeL2Output(&_OPSuccinctL2OutputOracle.TransactOpts, _outputRoot, _l2BlockNumber, _l1BlockNumber, _proof)
}

// ProposeL2Output is a paid mutator transaction binding the contract method 0x9ad84880.
//
// Solidity: function proposeL2Output(bytes32 _outputRoot, uint256 _l2BlockNumber, uint256 _l1BlockNumber, bytes _proof) payable returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) ProposeL2Output(_outputRoot [32]byte, _l2BlockNumber *big.Int, _l1BlockNumber *big.Int, _proof []byte) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.ProposeL2Output(&_OPSuccinctL2OutputOracle.TransactOpts, _outputRoot, _l2BlockNumber, _l1BlockNumber, _proof)
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

// UpdateAggregationVkey is a paid mutator transaction binding the contract method 0xc4cb03ec.
//
// Solidity: function updateAggregationVkey(bytes32 _aggregationVkey) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) UpdateAggregationVkey(opts *bind.TransactOpts, _aggregationVkey [32]byte) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "updateAggregationVkey", _aggregationVkey)
}

// UpdateAggregationVkey is a paid mutator transaction binding the contract method 0xc4cb03ec.
//
// Solidity: function updateAggregationVkey(bytes32 _aggregationVkey) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) UpdateAggregationVkey(_aggregationVkey [32]byte) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.UpdateAggregationVkey(&_OPSuccinctL2OutputOracle.TransactOpts, _aggregationVkey)
}

// UpdateAggregationVkey is a paid mutator transaction binding the contract method 0xc4cb03ec.
//
// Solidity: function updateAggregationVkey(bytes32 _aggregationVkey) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) UpdateAggregationVkey(_aggregationVkey [32]byte) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.UpdateAggregationVkey(&_OPSuccinctL2OutputOracle.TransactOpts, _aggregationVkey)
}

// UpdateRangeVkeyCommitment is a paid mutator transaction binding the contract method 0xbc91ce33.
//
// Solidity: function updateRangeVkeyCommitment(bytes32 _rangeVkeyCommitment) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) UpdateRangeVkeyCommitment(opts *bind.TransactOpts, _rangeVkeyCommitment [32]byte) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "updateRangeVkeyCommitment", _rangeVkeyCommitment)
}

// UpdateRangeVkeyCommitment is a paid mutator transaction binding the contract method 0xbc91ce33.
//
// Solidity: function updateRangeVkeyCommitment(bytes32 _rangeVkeyCommitment) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) UpdateRangeVkeyCommitment(_rangeVkeyCommitment [32]byte) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.UpdateRangeVkeyCommitment(&_OPSuccinctL2OutputOracle.TransactOpts, _rangeVkeyCommitment)
}

// UpdateRangeVkeyCommitment is a paid mutator transaction binding the contract method 0xbc91ce33.
//
// Solidity: function updateRangeVkeyCommitment(bytes32 _rangeVkeyCommitment) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) UpdateRangeVkeyCommitment(_rangeVkeyCommitment [32]byte) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.UpdateRangeVkeyCommitment(&_OPSuccinctL2OutputOracle.TransactOpts, _rangeVkeyCommitment)
}

// UpdateRollupConfigHash is a paid mutator transaction binding the contract method 0x1bdd450c.
//
// Solidity: function updateRollupConfigHash(bytes32 _rollupConfigHash) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactor) UpdateRollupConfigHash(opts *bind.TransactOpts, _rollupConfigHash [32]byte) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.contract.Transact(opts, "updateRollupConfigHash", _rollupConfigHash)
}

// UpdateRollupConfigHash is a paid mutator transaction binding the contract method 0x1bdd450c.
//
// Solidity: function updateRollupConfigHash(bytes32 _rollupConfigHash) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleSession) UpdateRollupConfigHash(_rollupConfigHash [32]byte) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.UpdateRollupConfigHash(&_OPSuccinctL2OutputOracle.TransactOpts, _rollupConfigHash)
}

// UpdateRollupConfigHash is a paid mutator transaction binding the contract method 0x1bdd450c.
//
// Solidity: function updateRollupConfigHash(bytes32 _rollupConfigHash) returns()
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleTransactorSession) UpdateRollupConfigHash(_rollupConfigHash [32]byte) (*types.Transaction, error) {
	return _OPSuccinctL2OutputOracle.Contract.UpdateRollupConfigHash(&_OPSuccinctL2OutputOracle.TransactOpts, _rollupConfigHash)
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

// OPSuccinctL2OutputOracleAggregationVkeyUpdatedIterator is returned from FilterAggregationVkeyUpdated and is used to iterate over the raw logs and unpacked data for AggregationVkeyUpdated events raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleAggregationVkeyUpdatedIterator struct {
	Event *OPSuccinctL2OutputOracleAggregationVkeyUpdated // Event containing the contract specifics and raw log

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
func (it *OPSuccinctL2OutputOracleAggregationVkeyUpdatedIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(OPSuccinctL2OutputOracleAggregationVkeyUpdated)
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
		it.Event = new(OPSuccinctL2OutputOracleAggregationVkeyUpdated)
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
func (it *OPSuccinctL2OutputOracleAggregationVkeyUpdatedIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *OPSuccinctL2OutputOracleAggregationVkeyUpdatedIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// OPSuccinctL2OutputOracleAggregationVkeyUpdated represents a AggregationVkeyUpdated event raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleAggregationVkeyUpdated struct {
	OldAggregationVkey [32]byte
	NewAggregationVkey [32]byte
	Raw                types.Log // Blockchain specific contextual infos
}

// FilterAggregationVkeyUpdated is a free log retrieval operation binding the contract event 0x390b73b2b067afcef04d30b573e4590c6e565519e370927dd777ca0ce8a55db0.
//
// Solidity: event AggregationVkeyUpdated(bytes32 indexed oldAggregationVkey, bytes32 indexed newAggregationVkey)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) FilterAggregationVkeyUpdated(opts *bind.FilterOpts, oldAggregationVkey [][32]byte, newAggregationVkey [][32]byte) (*OPSuccinctL2OutputOracleAggregationVkeyUpdatedIterator, error) {

	var oldAggregationVkeyRule []interface{}
	for _, oldAggregationVkeyItem := range oldAggregationVkey {
		oldAggregationVkeyRule = append(oldAggregationVkeyRule, oldAggregationVkeyItem)
	}
	var newAggregationVkeyRule []interface{}
	for _, newAggregationVkeyItem := range newAggregationVkey {
		newAggregationVkeyRule = append(newAggregationVkeyRule, newAggregationVkeyItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.FilterLogs(opts, "AggregationVkeyUpdated", oldAggregationVkeyRule, newAggregationVkeyRule)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctL2OutputOracleAggregationVkeyUpdatedIterator{contract: _OPSuccinctL2OutputOracle.contract, event: "AggregationVkeyUpdated", logs: logs, sub: sub}, nil
}

// WatchAggregationVkeyUpdated is a free log subscription operation binding the contract event 0x390b73b2b067afcef04d30b573e4590c6e565519e370927dd777ca0ce8a55db0.
//
// Solidity: event AggregationVkeyUpdated(bytes32 indexed oldAggregationVkey, bytes32 indexed newAggregationVkey)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) WatchAggregationVkeyUpdated(opts *bind.WatchOpts, sink chan<- *OPSuccinctL2OutputOracleAggregationVkeyUpdated, oldAggregationVkey [][32]byte, newAggregationVkey [][32]byte) (event.Subscription, error) {

	var oldAggregationVkeyRule []interface{}
	for _, oldAggregationVkeyItem := range oldAggregationVkey {
		oldAggregationVkeyRule = append(oldAggregationVkeyRule, oldAggregationVkeyItem)
	}
	var newAggregationVkeyRule []interface{}
	for _, newAggregationVkeyItem := range newAggregationVkey {
		newAggregationVkeyRule = append(newAggregationVkeyRule, newAggregationVkeyItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.WatchLogs(opts, "AggregationVkeyUpdated", oldAggregationVkeyRule, newAggregationVkeyRule)
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(OPSuccinctL2OutputOracleAggregationVkeyUpdated)
				if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "AggregationVkeyUpdated", log); err != nil {
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

// ParseAggregationVkeyUpdated is a log parse operation binding the contract event 0x390b73b2b067afcef04d30b573e4590c6e565519e370927dd777ca0ce8a55db0.
//
// Solidity: event AggregationVkeyUpdated(bytes32 indexed oldAggregationVkey, bytes32 indexed newAggregationVkey)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) ParseAggregationVkeyUpdated(log types.Log) (*OPSuccinctL2OutputOracleAggregationVkeyUpdated, error) {
	event := new(OPSuccinctL2OutputOracleAggregationVkeyUpdated)
	if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "AggregationVkeyUpdated", log); err != nil {
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

// OPSuccinctL2OutputOracleRangeVkeyCommitmentUpdatedIterator is returned from FilterRangeVkeyCommitmentUpdated and is used to iterate over the raw logs and unpacked data for RangeVkeyCommitmentUpdated events raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleRangeVkeyCommitmentUpdatedIterator struct {
	Event *OPSuccinctL2OutputOracleRangeVkeyCommitmentUpdated // Event containing the contract specifics and raw log

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
func (it *OPSuccinctL2OutputOracleRangeVkeyCommitmentUpdatedIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(OPSuccinctL2OutputOracleRangeVkeyCommitmentUpdated)
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
		it.Event = new(OPSuccinctL2OutputOracleRangeVkeyCommitmentUpdated)
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
func (it *OPSuccinctL2OutputOracleRangeVkeyCommitmentUpdatedIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *OPSuccinctL2OutputOracleRangeVkeyCommitmentUpdatedIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// OPSuccinctL2OutputOracleRangeVkeyCommitmentUpdated represents a RangeVkeyCommitmentUpdated event raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleRangeVkeyCommitmentUpdated struct {
	OldRangeVkeyCommitment [32]byte
	NewRangeVkeyCommitment [32]byte
	Raw                    types.Log // Blockchain specific contextual infos
}

// FilterRangeVkeyCommitmentUpdated is a free log retrieval operation binding the contract event 0xbf8cab6317796bfa97fea82b6d27c9542a08fa0821813cf2a57e7cff7fdc8156.
//
// Solidity: event RangeVkeyCommitmentUpdated(bytes32 indexed oldRangeVkeyCommitment, bytes32 indexed newRangeVkeyCommitment)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) FilterRangeVkeyCommitmentUpdated(opts *bind.FilterOpts, oldRangeVkeyCommitment [][32]byte, newRangeVkeyCommitment [][32]byte) (*OPSuccinctL2OutputOracleRangeVkeyCommitmentUpdatedIterator, error) {

	var oldRangeVkeyCommitmentRule []interface{}
	for _, oldRangeVkeyCommitmentItem := range oldRangeVkeyCommitment {
		oldRangeVkeyCommitmentRule = append(oldRangeVkeyCommitmentRule, oldRangeVkeyCommitmentItem)
	}
	var newRangeVkeyCommitmentRule []interface{}
	for _, newRangeVkeyCommitmentItem := range newRangeVkeyCommitment {
		newRangeVkeyCommitmentRule = append(newRangeVkeyCommitmentRule, newRangeVkeyCommitmentItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.FilterLogs(opts, "RangeVkeyCommitmentUpdated", oldRangeVkeyCommitmentRule, newRangeVkeyCommitmentRule)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctL2OutputOracleRangeVkeyCommitmentUpdatedIterator{contract: _OPSuccinctL2OutputOracle.contract, event: "RangeVkeyCommitmentUpdated", logs: logs, sub: sub}, nil
}

// WatchRangeVkeyCommitmentUpdated is a free log subscription operation binding the contract event 0xbf8cab6317796bfa97fea82b6d27c9542a08fa0821813cf2a57e7cff7fdc8156.
//
// Solidity: event RangeVkeyCommitmentUpdated(bytes32 indexed oldRangeVkeyCommitment, bytes32 indexed newRangeVkeyCommitment)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) WatchRangeVkeyCommitmentUpdated(opts *bind.WatchOpts, sink chan<- *OPSuccinctL2OutputOracleRangeVkeyCommitmentUpdated, oldRangeVkeyCommitment [][32]byte, newRangeVkeyCommitment [][32]byte) (event.Subscription, error) {

	var oldRangeVkeyCommitmentRule []interface{}
	for _, oldRangeVkeyCommitmentItem := range oldRangeVkeyCommitment {
		oldRangeVkeyCommitmentRule = append(oldRangeVkeyCommitmentRule, oldRangeVkeyCommitmentItem)
	}
	var newRangeVkeyCommitmentRule []interface{}
	for _, newRangeVkeyCommitmentItem := range newRangeVkeyCommitment {
		newRangeVkeyCommitmentRule = append(newRangeVkeyCommitmentRule, newRangeVkeyCommitmentItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.WatchLogs(opts, "RangeVkeyCommitmentUpdated", oldRangeVkeyCommitmentRule, newRangeVkeyCommitmentRule)
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(OPSuccinctL2OutputOracleRangeVkeyCommitmentUpdated)
				if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "RangeVkeyCommitmentUpdated", log); err != nil {
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

// ParseRangeVkeyCommitmentUpdated is a log parse operation binding the contract event 0xbf8cab6317796bfa97fea82b6d27c9542a08fa0821813cf2a57e7cff7fdc8156.
//
// Solidity: event RangeVkeyCommitmentUpdated(bytes32 indexed oldRangeVkeyCommitment, bytes32 indexed newRangeVkeyCommitment)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) ParseRangeVkeyCommitmentUpdated(log types.Log) (*OPSuccinctL2OutputOracleRangeVkeyCommitmentUpdated, error) {
	event := new(OPSuccinctL2OutputOracleRangeVkeyCommitmentUpdated)
	if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "RangeVkeyCommitmentUpdated", log); err != nil {
		return nil, err
	}
	event.Raw = log
	return event, nil
}

// OPSuccinctL2OutputOracleRollupConfigHashUpdatedIterator is returned from FilterRollupConfigHashUpdated and is used to iterate over the raw logs and unpacked data for RollupConfigHashUpdated events raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleRollupConfigHashUpdatedIterator struct {
	Event *OPSuccinctL2OutputOracleRollupConfigHashUpdated // Event containing the contract specifics and raw log

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
func (it *OPSuccinctL2OutputOracleRollupConfigHashUpdatedIterator) Next() bool {
	// If the iterator failed, stop iterating
	if it.fail != nil {
		return false
	}
	// If the iterator completed, deliver directly whatever's available
	if it.done {
		select {
		case log := <-it.logs:
			it.Event = new(OPSuccinctL2OutputOracleRollupConfigHashUpdated)
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
		it.Event = new(OPSuccinctL2OutputOracleRollupConfigHashUpdated)
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
func (it *OPSuccinctL2OutputOracleRollupConfigHashUpdatedIterator) Error() error {
	return it.fail
}

// Close terminates the iteration process, releasing any pending underlying
// resources.
func (it *OPSuccinctL2OutputOracleRollupConfigHashUpdatedIterator) Close() error {
	it.sub.Unsubscribe()
	return nil
}

// OPSuccinctL2OutputOracleRollupConfigHashUpdated represents a RollupConfigHashUpdated event raised by the OPSuccinctL2OutputOracle contract.
type OPSuccinctL2OutputOracleRollupConfigHashUpdated struct {
	OldRollupConfigHash [32]byte
	NewRollupConfigHash [32]byte
	Raw                 types.Log // Blockchain specific contextual infos
}

// FilterRollupConfigHashUpdated is a free log retrieval operation binding the contract event 0x5d9ebe9f09b0810b3546b30781ba9a51092b37dd6abada4b830ce54a41ac6a4b.
//
// Solidity: event RollupConfigHashUpdated(bytes32 indexed oldRollupConfigHash, bytes32 indexed newRollupConfigHash)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) FilterRollupConfigHashUpdated(opts *bind.FilterOpts, oldRollupConfigHash [][32]byte, newRollupConfigHash [][32]byte) (*OPSuccinctL2OutputOracleRollupConfigHashUpdatedIterator, error) {

	var oldRollupConfigHashRule []interface{}
	for _, oldRollupConfigHashItem := range oldRollupConfigHash {
		oldRollupConfigHashRule = append(oldRollupConfigHashRule, oldRollupConfigHashItem)
	}
	var newRollupConfigHashRule []interface{}
	for _, newRollupConfigHashItem := range newRollupConfigHash {
		newRollupConfigHashRule = append(newRollupConfigHashRule, newRollupConfigHashItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.FilterLogs(opts, "RollupConfigHashUpdated", oldRollupConfigHashRule, newRollupConfigHashRule)
	if err != nil {
		return nil, err
	}
	return &OPSuccinctL2OutputOracleRollupConfigHashUpdatedIterator{contract: _OPSuccinctL2OutputOracle.contract, event: "RollupConfigHashUpdated", logs: logs, sub: sub}, nil
}

// WatchRollupConfigHashUpdated is a free log subscription operation binding the contract event 0x5d9ebe9f09b0810b3546b30781ba9a51092b37dd6abada4b830ce54a41ac6a4b.
//
// Solidity: event RollupConfigHashUpdated(bytes32 indexed oldRollupConfigHash, bytes32 indexed newRollupConfigHash)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) WatchRollupConfigHashUpdated(opts *bind.WatchOpts, sink chan<- *OPSuccinctL2OutputOracleRollupConfigHashUpdated, oldRollupConfigHash [][32]byte, newRollupConfigHash [][32]byte) (event.Subscription, error) {

	var oldRollupConfigHashRule []interface{}
	for _, oldRollupConfigHashItem := range oldRollupConfigHash {
		oldRollupConfigHashRule = append(oldRollupConfigHashRule, oldRollupConfigHashItem)
	}
	var newRollupConfigHashRule []interface{}
	for _, newRollupConfigHashItem := range newRollupConfigHash {
		newRollupConfigHashRule = append(newRollupConfigHashRule, newRollupConfigHashItem)
	}

	logs, sub, err := _OPSuccinctL2OutputOracle.contract.WatchLogs(opts, "RollupConfigHashUpdated", oldRollupConfigHashRule, newRollupConfigHashRule)
	if err != nil {
		return nil, err
	}
	return event.NewSubscription(func(quit <-chan struct{}) error {
		defer sub.Unsubscribe()
		for {
			select {
			case log := <-logs:
				// New log arrived, parse the event and forward to the user
				event := new(OPSuccinctL2OutputOracleRollupConfigHashUpdated)
				if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "RollupConfigHashUpdated", log); err != nil {
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

// ParseRollupConfigHashUpdated is a log parse operation binding the contract event 0x5d9ebe9f09b0810b3546b30781ba9a51092b37dd6abada4b830ce54a41ac6a4b.
//
// Solidity: event RollupConfigHashUpdated(bytes32 indexed oldRollupConfigHash, bytes32 indexed newRollupConfigHash)
func (_OPSuccinctL2OutputOracle *OPSuccinctL2OutputOracleFilterer) ParseRollupConfigHashUpdated(log types.Log) (*OPSuccinctL2OutputOracleRollupConfigHashUpdated, error) {
	event := new(OPSuccinctL2OutputOracleRollupConfigHashUpdated)
	if err := _OPSuccinctL2OutputOracle.contract.UnpackLog(event, "RollupConfigHashUpdated", log); err != nil {
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
