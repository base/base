// SPDX-License-Identifier: MIT
pragma solidity 0.8.15;

/// @notice Minimal subset of `IAnchorStateRegistry` that `Verifier.nullify` consults.
interface IAnchorStateRegistryLike {
    function isGameRegistered(address game) external view returns (bool);
    function isGameRespected(address game) external view returns (bool);
    function isGameBlacklisted(address game) external view returns (bool);
    function isGameRetired(address game) external view returns (bool);
}

/// @title MockVerifier
/// @notice Stands in for `TEEVerifier` / `ZKVerifier` on the challenger E2E's
///         Anvil fork, so no real proof is ever needed.
/// @dev Installed with `anvil_setCode` over a real verifier, keeping its storage.
///      The layout must match the real `Verifier` base, whose only storage
///      variable is `bool nullified` at slot 0 (`ANCHOR_STATE_REGISTRY` is an
///      immutable and lives in code). The registry address cannot be an
///      immutable here, because no constructor runs, so the driver writes it to
///      slot 1, which the real verifier never uses. `uint256` keeps it out of
///      slot 0: a `bool` and an `address` would otherwise pack together.
///
///      Behaviour matches the real verifier everywhere except the proof check:
///      `verify` accepts any proof while not nullified, `nullify` keeps the real
///      registered/respected/not-blacklisted/not-retired guard, and `nullified`
///      reads storage. A plain "return true" runtime would make `nullified()`
///      return true too, and `AggregateVerifier._proofRefutedUpdate` would then
///      treat every game on the fork as refuted.
contract MockVerifier {
    bool public nullified;
    uint256 internal anchorStateRegistry;

    error Nullified();
    error NotProperGame();

    function verify(bytes calldata, bytes32, bytes32) external view returns (bool) {
        if (nullified) revert Nullified();
        return true;
    }

    function nullify() external {
        IAnchorStateRegistryLike registry = IAnchorStateRegistryLike(address(uint160(anchorStateRegistry)));
        if (
            !registry.isGameRegistered(msg.sender) || !registry.isGameRespected(msg.sender)
                || registry.isGameBlacklisted(msg.sender) || registry.isGameRetired(msg.sender)
        ) revert NotProperGame();
        nullified = true;
    }

    function ANCHOR_STATE_REGISTRY() external view returns (address) {
        return address(uint160(anchorStateRegistry));
    }

    function version() external pure returns (string memory) {
        return "challenger-e2e-mock";
    }
}
