// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {IDisputeGame} from "@optimism/src/dispute/interfaces/IDisputeGame.sol";
import {LibCWIA} from "@solady/utils/legacy/LibCWIA.sol";
import {ISemver} from "@optimism/src/universal/ISemver.sol";

contract OPSuccinctDisputeGameFactory is ISemver {
    using LibCWIA for address;

    /// @notice The owner of the contract, who has admin permissions.
    address public owner;

    /// @notice The address of the OP Succinct DisputeGame implementation contract.
    address public gameImpl;

    /// @notice Semantic version.
    /// @custom:semver v1.0.0-rc2
    string public constant version = "v1.0.0-rc2";

    ////////////////////////////////////////////////////////////
    //                        Modifiers                       //
    ////////////////////////////////////////////////////////////

    modifier onlyOwner() {
        require(msg.sender == owner, "OPSuccinctDisputeGameFactory: caller is not the owner");
        _;
    }

    ////////////////////////////////////////////////////////////
    //                        Functions                       //
    ////////////////////////////////////////////////////////////

    /// @notice Constructs the OPSuccinctDisputeGameFactory contract.
    constructor(address _owner, address _gameImpl) {
        owner = _owner;
        gameImpl = _gameImpl;
    }

    /// @notice Creates a new DisputeGame proxy contract.
    function create(bytes32 _rootClaim, uint256 _l2BlockNumber, uint256 _l1BlockNumber, bytes memory _proof)
        external
        payable
    {
        IDisputeGame game = IDisputeGame(
            gameImpl.clone(
                abi.encodePacked(msg.sender, _rootClaim, bytes32(0), abi.encode(_l2BlockNumber, _l1BlockNumber, _proof))
            )
        );

        game.initialize{value: msg.value}();
    }

    /// Updates the owner address.
    /// @param _owner The new owner address.
    function transferOwnership(address _owner) external onlyOwner {
        owner = _owner;
    }

    /// @notice Sets the implementation address.
    /// @param _implementation New implementation address.
    function setImplementation(address _implementation) external onlyOwner {
        gameImpl = _implementation;
    }
}
