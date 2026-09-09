// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

contract ParentBlockhashGuard {
    error ParentBlockHashWasCanonical(bytes32 actual);

    function succeedsOnlyWhenParentBlockHashIsCanonical(
        bytes32 canonicalParentBlockHash
    ) external view returns (bytes32) {
        bytes32 parentBlockHash = blockhash(block.number - 1);

        if (parentBlockHash == canonicalParentBlockHash) {
            revert ParentBlockHashWasCanonical(parentBlockHash);
        }

        return parentBlockHash;
    }
}
