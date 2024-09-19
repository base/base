// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Initializable} from "@openzeppelin/contracts/proxy/utils/Initializable.sol";
import {ISemver} from "@optimism/src/universal/ISemver.sol";
import {Types} from "@optimism/src/libraries/Types.sol";
import {Constants} from "@optimism/src/libraries/Constants.sol";
import {SP1VerifierGateway} from "@sp1-contracts/src/SP1VerifierGateway.sol";

/// @custom:proxied
/// @title ZKL2OutputOracle
/// @notice The ZKL2OutputOracle contains an array of L2 state outputs, where each output is a
///         commitment to the state of the L2 chain. Other contracts like the OptimismPortal use
///         these outputs to verify information about the state of L2.
///         This is a diff of Optimism's original L2OutputOracle, but where outputs are ZK verified.
///         https://github.com/ethereum-optimism/optimism/blob/develop/packages/contracts-bedrock/src/L1/L2OutputOracle.sol
contract ZKL2OutputOracle is Initializable, ISemver {
    /// @notice The number of the first L2 block recorded in this contract.
    uint256 public startingBlockNumber;

    /// @notice The timestamp of the first L2 block recorded in this contract.
    uint256 public startingTimestamp;

    /// @notice An array of L2 output proposals.
    Types.OutputProposal[] internal l2Outputs;

    /// @notice The interval in L2 blocks at which checkpoints must be submitted.
    /// @custom:network-specific
    uint256 public submissionInterval;

    /// @notice The time between L2 blocks in seconds. Once set, this value MUST NOT be modified.
    /// @custom:network-specific
    uint256 public l2BlockTime;

    /// @notice The address of the challenger. Can be updated via upgrade.
    /// @custom:network-specific
    address public challenger;

    /// @notice The address of the proposer. Can be updated via upgrade.
    /// @custom:network-specific
    address public proposer;

    /// @notice The minimum time (in seconds) that must elapse before a withdrawal can be finalized.
    /// @custom:network-specific
    uint256 public finalizationPeriodSeconds;

    /// @notice The chain ID of the L2 chain.
    uint256 public chainId;

    /// @notice The verification key of the SP1 program.
    bytes32 public vkey;

    /// @notice The deployed SP1VerifierGateway contract to request proofs from.
    SP1VerifierGateway public verifierGateway;

    /// @notice The owner of the contract, who has admin permissions.
    address public owner;

    /// @notice The hash of the chain's rollup config, used to verify ZK proofs.
    bytes32 public rollupConfigHash;

    /// @notice A trusted mapping of block numbers to block hashes.
    mapping(uint256 => bytes32) public historicBlockHashes;

    /// @notice Parameters to initialize the ZK version of the contract.
    struct ZKInitParams {
        uint256 chainId;
        bytes32 vkey;
        address verifierGateway;
        bytes32 startingOutputRoot;
        address owner;
        bytes32 rollupConfigHash;
    }

    /// @notice Struct containing the public values committed to for the SP1 proof.
    struct PublicValuesStruct {
        bytes32 l1Head;
        bytes32 l2PreRoot;
        bytes32 claimRoot;
        uint256 claimBlockNum;
        uint256 chainId;
        bytes32 rollupConfigHash;
    }

    ////////////////////////////////////////////////////////////
    //                         Events                         //
    ////////////////////////////////////////////////////////////

    /// @notice Emitted when an output is proposed.
    /// @param outputRoot    The output root.
    /// @param l2OutputIndex The index of the output in the l2Outputs array.
    /// @param l2BlockNumber The L2 block number of the output root.
    /// @param l1Timestamp   The L1 timestamp when proposed.
    event OutputProposed(
        bytes32 indexed outputRoot, uint256 indexed l2OutputIndex, uint256 indexed l2BlockNumber, uint256 l1Timestamp
    );

    /// @notice Emitted when outputs are deleted.
    /// @param prevNextOutputIndex Next L2 output index before the deletion.
    /// @param newNextOutputIndex  Next L2 output index after the deletion.
    event OutputsDeleted(uint256 indexed prevNextOutputIndex, uint256 indexed newNextOutputIndex);

    /// @notice Emitted when the vkey is updated.
    /// @param oldVkey The old vkey.
    /// @param newVkey The new vkey.
    event UpdatedVKey(bytes32 indexed oldVkey, bytes32 indexed newVkey);

    /// @notice Emitted when the verifier gateway is updated.
    /// @param oldVerifierGateway The old verifier gateway.
    /// @param newVerifierGateway The new verifier gateway.
    event UpdatedVerifierGateway(address indexed oldVerifierGateway, address indexed newVerifierGateway);

    /// @notice Emitted when ownership is transferred.
    /// @param previousOwner The previous owner.
    /// @param newOwner      The new owner.
    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);

    /// @notice Emitted when the rollup config hash is updated.
    /// @param oldRollupConfigHash The old rollup config hash.
    /// @param newRollupConfigHash The new rollup config hash.
    event UpdatedRollupConfigHash(bytes32 indexed oldRollupConfigHash, bytes32 indexed newRollupConfigHash);

    /// @notice Semantic version.
    /// @custom:semver 2.0.0
    string public constant version = "2.0.0";

    ////////////////////////////////////////////////////////////
    //                        Modifiers                       //
    ////////////////////////////////////////////////////////////

    modifier onlyOwner() {
        require(msg.sender == owner, "L2OutputOracle: caller is not the owner");
        _;
    }

    ////////////////////////////////////////////////////////////
    //                        Functions                       //
    ////////////////////////////////////////////////////////////

    /// @notice Constructs the ZKL2OutputOracle contract. Disables initializers.
    constructor() {
        _disableInitializers();
    }

    /// @notice Initializer.
    /// @param _submissionInterval  Interval in blocks at which checkpoints must be submitted.
    /// @param _l2BlockTime         The time per L2 block, in seconds.
    /// @param _startingBlockNumber The number of the first L2 block.
    /// @param _startingTimestamp   The timestamp of the first L2 block.
    /// @param _proposer            The address of the proposer.
    /// @param _challenger          The address of the challenger.
    /// @param _finalizationPeriodSeconds The minimum time (in seconds) that must elapse before a withdrawal
    ///                                   can be finalized.
    /// @param _zkInitParams        The chain ID, vkey, verifier gateway, owner, and starting output root for the ZK version of the contract.
    /// @dev Starting block number, timestamp and output root are ignored for upgrades where these values already exist.
    function initialize(
        uint256 _submissionInterval,
        uint256 _l2BlockTime,
        uint256 _startingBlockNumber,
        uint256 _startingTimestamp,
        address _proposer,
        address _challenger,
        uint256 _finalizationPeriodSeconds,
        ZKInitParams memory _zkInitParams
    ) public reinitializer(2) {
        require(_submissionInterval > 0, "L2OutputOracle: submission interval must be greater than 0");
        require(_l2BlockTime > 0, "L2OutputOracle: L2 block time must be greater than 0");
        require(
            _startingTimestamp <= block.timestamp,
            "L2OutputOracle: starting L2 timestamp must be less than current time"
        );

        submissionInterval = _submissionInterval;
        l2BlockTime = _l2BlockTime;
        proposer = _proposer;
        challenger = _challenger;
        finalizationPeriodSeconds = _finalizationPeriodSeconds;

        if (l2Outputs.length == 0) {
            l2Outputs.push(
                Types.OutputProposal({
                    outputRoot: _zkInitParams.startingOutputRoot,
                    timestamp: uint128(_startingTimestamp),
                    l2BlockNumber: uint128(_startingBlockNumber)
                })
            );

            startingBlockNumber = _startingBlockNumber;
            startingTimestamp = _startingTimestamp;
        }

        chainId = _zkInitParams.chainId;
        _transferOwnership(_zkInitParams.owner);
        _updateVKey(_zkInitParams.vkey);
        _updateVerifierGateway(_zkInitParams.verifierGateway);
        _updateRollupConfigHash(_zkInitParams.rollupConfigHash);
    }

    /// @notice Getter for the submissionInterval.
    ///         Public getter is legacy and will be removed in the future. Use `submissionInterval` instead.
    /// @return Submission interval.
    /// @custom:legacy
    function SUBMISSION_INTERVAL() external view returns (uint256) {
        return submissionInterval;
    }

    /// @notice Getter for the l2BlockTime.
    ///         Public getter is legacy and will be removed in the future. Use `l2BlockTime` instead.
    /// @return L2 block time.
    /// @custom:legacy
    function L2_BLOCK_TIME() external view returns (uint256) {
        return l2BlockTime;
    }

    /// @notice Getter for the challenger address.
    ///         Public getter is legacy and will be removed in the future. Use `challenger` instead.
    /// @return Address of the challenger.
    /// @custom:legacy
    function CHALLENGER() external view returns (address) {
        return challenger;
    }

    /// @notice Getter for the proposer address.
    ///         Public getter is legacy and will be removed in the future. Use `proposer` instead.
    /// @return Address of the proposer.
    /// @custom:legacy
    function PROPOSER() external view returns (address) {
        return proposer;
    }

    /// @notice Getter for the finalizationPeriodSeconds.
    ///         Public getter is legacy and will be removed in the future. Use `finalizationPeriodSeconds` instead.
    /// @return Finalization period in seconds.
    /// @custom:legacy
    function FINALIZATION_PERIOD_SECONDS() external view returns (uint256) {
        return finalizationPeriodSeconds;
    }

    /// @notice Deletes all output proposals after and including the proposal that corresponds to
    ///         the given output index. Only the challenger address can delete outputs.
    /// @param _l2OutputIndex Index of the first L2 output to be deleted.
    ///                       All outputs after this output will also be deleted.
    function deleteL2Outputs(uint256 _l2OutputIndex) external {
        require(msg.sender == challenger, "L2OutputOracle: only the challenger address can delete outputs");

        // Make sure we're not *increasing* the length of the array.
        require(
            _l2OutputIndex < l2Outputs.length, "L2OutputOracle: cannot delete outputs after the latest output index"
        );

        // Do not allow deleting any outputs that have already been finalized.
        require(
            block.timestamp - l2Outputs[_l2OutputIndex].timestamp < finalizationPeriodSeconds,
            "L2OutputOracle: cannot delete outputs that have already been finalized"
        );

        uint256 prevNextL2OutputIndex = nextOutputIndex();

        // Use assembly to delete the array elements because Solidity doesn't allow it.
        assembly {
            sstore(l2Outputs.slot, _l2OutputIndex)
        }

        emit OutputsDeleted(prevNextL2OutputIndex, _l2OutputIndex);
    }

    /// @notice Accepts an outputRoot and the timestamp of the corresponding L2 block.
    ///         The timestamp must be equal to the current value returned by `nextTimestamp()` in
    ///         order to be accepted. This function may only be called by the Proposer.
    /// @param _outputRoot    The L2 output of the checkpoint block.
    /// @param _l2BlockNumber The L2 block number that resulted in _outputRoot.
    /// @param _l1BlockHash   A block hash which must be included in the current chain.
    /// @param _l1BlockNumber The block number with the specified block hash.
    function proposeL2Output(
        bytes32 _outputRoot,
        uint256 _l2BlockNumber,
        bytes32 _l1BlockHash,
        uint256 _l1BlockNumber,
        bytes memory _proof
    ) external payable {
        require(
            msg.sender == proposer || proposer == address(0),
            "L2OutputOracle: only the proposer address can propose new outputs"
        );

        require(
            _l2BlockNumber >= nextBlockNumber(),
            "L2OutputOracle: block number must be greater than or equal to next expected block number"
        );

        require(
            computeL2Timestamp(_l2BlockNumber) < block.timestamp,
            "L2OutputOracle: cannot propose L2 output in the future"
        );

        require(_outputRoot != bytes32(0), "L2OutputOracle: L2 output proposal cannot be the zero hash");

        require(vkey != bytes32(0), "L2OutputOracle: vkey must be set before proposing an output");

        require(
            historicBlockHashes[_l1BlockNumber] == _l1BlockHash,
            "L2OutputOracle: proposed block hash and number are not checkpointed"
        );

        PublicValuesStruct memory publicValues = PublicValuesStruct({
            l1Head: _l1BlockHash,
            l2PreRoot: l2Outputs[latestOutputIndex()].outputRoot,
            claimRoot: _outputRoot,
            claimBlockNum: _l2BlockNumber,
            chainId: chainId,
            rollupConfigHash: rollupConfigHash
        });

        verifierGateway.verifyProof(vkey, abi.encode(publicValues), _proof);

        emit OutputProposed(_outputRoot, nextOutputIndex(), _l2BlockNumber, block.timestamp);

        l2Outputs.push(
            Types.OutputProposal({
                outputRoot: _outputRoot,
                timestamp: uint128(block.timestamp),
                l2BlockNumber: uint128(_l2BlockNumber)
            })
        );
    }

    /// @notice Checkpoints a block hash at a given block number.
    /// @param _blockNumber Block number to checkpoint the hash at.
    /// @param _blockHash   Hash of the block at the given block number.
    /// @dev Block number must be in the past 256 blocks or this will revert.
    /// @dev Passing both inputs as zero will automatically checkpoint the most recent blockhash.
    function checkpointBlockHash(uint256 _blockNumber, bytes32 _blockHash) external {
        require(blockhash(_blockNumber) == _blockHash, "L2OutputOracle: block hash and number cannot be checkpointed");
        historicBlockHashes[_blockNumber] = _blockHash;
    }

    /// @notice Returns an output by index. Needed to return a struct instead of a tuple.
    /// @param _l2OutputIndex Index of the output to return.
    /// @return The output at the given index.
    function getL2Output(uint256 _l2OutputIndex) external view returns (Types.OutputProposal memory) {
        return l2Outputs[_l2OutputIndex];
    }

    /// @notice Returns the index of the L2 output that checkpoints a given L2 block number.
    ///         Uses a binary search to find the first output greater than or equal to the given
    ///         block.
    /// @param _l2BlockNumber L2 block number to find a checkpoint for.
    /// @return Index of the first checkpoint that commits to the given L2 block number.
    function getL2OutputIndexAfter(uint256 _l2BlockNumber) public view returns (uint256) {
        // Make sure an output for this block number has actually been proposed.
        require(
            _l2BlockNumber <= latestBlockNumber(),
            "L2OutputOracle: cannot get output for a block that has not been proposed"
        );

        // Make sure there's at least one output proposed.
        require(l2Outputs.length > 0, "L2OutputOracle: cannot get output as no outputs have been proposed yet");

        // Find the output via binary search, guaranteed to exist.
        uint256 lo = 0;
        uint256 hi = l2Outputs.length;
        while (lo < hi) {
            uint256 mid = (lo + hi) / 2;
            if (l2Outputs[mid].l2BlockNumber < _l2BlockNumber) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }

        return lo;
    }

    /// @notice Returns the L2 output proposal that checkpoints a given L2 block number.
    ///         Uses a binary search to find the first output greater than or equal to the given
    ///         block.
    /// @param _l2BlockNumber L2 block number to find a checkpoint for.
    /// @return First checkpoint that commits to the given L2 block number.
    function getL2OutputAfter(uint256 _l2BlockNumber) external view returns (Types.OutputProposal memory) {
        return l2Outputs[getL2OutputIndexAfter(_l2BlockNumber)];
    }

    /// @notice Returns the number of outputs that have been proposed.
    ///         Will revert if no outputs have been proposed yet.
    /// @return The number of outputs that have been proposed.
    function latestOutputIndex() public view returns (uint256) {
        return l2Outputs.length - 1;
    }

    /// @notice Returns the index of the next output to be proposed.
    /// @return The index of the next output to be proposed.
    function nextOutputIndex() public view returns (uint256) {
        return l2Outputs.length;
    }

    /// @notice Returns the block number of the latest submitted L2 output proposal.
    ///         If no proposals been submitted yet then this function will return the starting
    ///         block number.
    /// @return Latest submitted L2 block number.
    function latestBlockNumber() public view returns (uint256) {
        return l2Outputs.length == 0 ? startingBlockNumber : l2Outputs[l2Outputs.length - 1].l2BlockNumber;
    }

    /// @notice Computes the block number of the next L2 block that needs to be checkpointed.
    /// @return Next L2 block number.
    function nextBlockNumber() public view returns (uint256) {
        return latestBlockNumber() + submissionInterval;
    }

    /// @notice Returns the L2 timestamp corresponding to a given L2 block number.
    /// @param _l2BlockNumber The L2 block number of the target block.
    /// @return L2 timestamp of the given block.
    function computeL2Timestamp(uint256 _l2BlockNumber) public view returns (uint256) {
        return startingTimestamp + ((_l2BlockNumber - startingBlockNumber) * l2BlockTime);
    }

    ////////////////////////////////////////////////////////////
    //                         Admin                          //
    ////////////////////////////////////////////////////////////

    function transferOwnership(address _newOwner) external onlyOwner {
        _transferOwnership(_newOwner);
    }

    function _transferOwnership(address _newOwner) internal {
        emit OwnershipTransferred(owner, _newOwner);
        owner = _newOwner;
    }

    function updateVKey(bytes32 _vkey) external onlyOwner {
        _updateVKey(_vkey);
    }

    function _updateVKey(bytes32 _vkey) internal {
        emit UpdatedVKey(vkey, _vkey);
        vkey = _vkey;
    }

    function updateVerifierGateway(address _verifierGateway) external onlyOwner {
        _updateVerifierGateway(_verifierGateway);
    }

    function _updateVerifierGateway(address _verifierGateway) internal {
        emit UpdatedVerifierGateway(address(verifierGateway), _verifierGateway);
        verifierGateway = SP1VerifierGateway(_verifierGateway);
    }

    function updateRollupConfigHash(bytes32 _rollupConfigHash) external onlyOwner {
        _updateRollupConfigHash(_rollupConfigHash);
    }

    function _updateRollupConfigHash(bytes32 _rollupConfigHash) internal {
        emit UpdatedRollupConfigHash(rollupConfigHash, _rollupConfigHash);
        rollupConfigHash = _rollupConfigHash;
    }
}
