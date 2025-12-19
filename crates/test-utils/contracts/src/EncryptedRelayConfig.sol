// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title EncryptedRelayConfig
/// @notice On-chain configuration for the encrypted transaction relay system.
/// @dev Stores encryption keys, attestation keys, and PoW difficulty settings.
contract EncryptedRelayConfig {
    /// @notice Current X25519 encryption public key (32 bytes).
    bytes32 public encryptionPubkey;

    /// @notice Previous encryption public key (for grace period during rotation).
    bytes32 public previousEncryptionPubkey;

    /// @notice Ed25519 public key for attesting sequencer nodes (32 bytes).
    bytes32 public attestationPubkey;

    /// @notice Current proof-of-work difficulty (leading zero bits required).
    uint8 public powDifficulty;

    /// @notice How long attestations remain valid (seconds).
    uint64 public attestationValiditySeconds;

    /// @notice Admin address that can update configuration.
    address public admin;

    /// @notice Emitted when the encryption key is rotated.
    /// @param newKey The new encryption public key.
    /// @param previousKey The previous encryption public key (now in grace period).
    event EncryptionKeyRotated(bytes32 indexed newKey, bytes32 previousKey);

    /// @notice Emitted when the attestation key is updated.
    /// @param newKey The new attestation public key.
    event AttestationKeyUpdated(bytes32 indexed newKey);

    /// @notice Emitted when the PoW difficulty is updated.
    /// @param newDifficulty The new difficulty value.
    event DifficultyUpdated(uint8 newDifficulty);

    /// @notice Emitted when the attestation validity window is updated.
    /// @param newValidity The new validity window in seconds.
    event AttestationValidityUpdated(uint64 newValidity);

    /// @notice Emitted when admin is transferred.
    /// @param newAdmin The new admin address.
    event AdminTransferred(address indexed newAdmin);

    error Unauthorized();
    error InvalidKey();
    error InvalidDifficulty();

    modifier onlyAdmin() {
        if (msg.sender != admin) revert Unauthorized();
        _;
    }

    /// @notice Initializes the config contract.
    /// @param _encryptionPubkey Initial encryption public key.
    /// @param _attestationPubkey Initial attestation public key.
    /// @param _powDifficulty Initial PoW difficulty.
    /// @param _attestationValiditySeconds Initial attestation validity window.
    constructor(
        bytes32 _encryptionPubkey,
        bytes32 _attestationPubkey,
        uint8 _powDifficulty,
        uint64 _attestationValiditySeconds
    ) {
        if (_encryptionPubkey == bytes32(0)) revert InvalidKey();
        if (_attestationPubkey == bytes32(0)) revert InvalidKey();
        if (_powDifficulty > 32) revert InvalidDifficulty();

        admin = msg.sender;
        encryptionPubkey = _encryptionPubkey;
        attestationPubkey = _attestationPubkey;
        powDifficulty = _powDifficulty;
        attestationValiditySeconds = _attestationValiditySeconds;
    }

    /// @notice Rotates the encryption key.
    /// @dev The current key becomes the previous key (grace period).
    /// @param newKey The new encryption public key.
    function rotateEncryptionKey(bytes32 newKey) external onlyAdmin {
        if (newKey == bytes32(0)) revert InvalidKey();

        bytes32 oldKey = encryptionPubkey;
        previousEncryptionPubkey = oldKey;
        encryptionPubkey = newKey;

        emit EncryptionKeyRotated(newKey, oldKey);
    }

    /// @notice Updates the attestation public key.
    /// @param newKey The new attestation public key.
    function setAttestationPubkey(bytes32 newKey) external onlyAdmin {
        if (newKey == bytes32(0)) revert InvalidKey();

        attestationPubkey = newKey;
        emit AttestationKeyUpdated(newKey);
    }

    /// @notice Updates the PoW difficulty.
    /// @param newDifficulty The new difficulty (0-32).
    function setDifficulty(uint8 newDifficulty) external onlyAdmin {
        if (newDifficulty > 32) revert InvalidDifficulty();

        powDifficulty = newDifficulty;
        emit DifficultyUpdated(newDifficulty);
    }

    /// @notice Updates the attestation validity window.
    /// @param newValidity The new validity window in seconds.
    function setAttestationValidity(uint64 newValidity) external onlyAdmin {
        attestationValiditySeconds = newValidity;
        emit AttestationValidityUpdated(newValidity);
    }

    /// @notice Transfers admin to a new address.
    /// @param newAdmin The new admin address.
    function transferAdmin(address newAdmin) external onlyAdmin {
        admin = newAdmin;
        emit AdminTransferred(newAdmin);
    }

    /// @notice Checks if a given pubkey is valid (current or previous).
    /// @param pubkey The pubkey to check.
    /// @return True if the pubkey is current or previous (in grace period).
    function isValidEncryptionKey(bytes32 pubkey) external view returns (bool) {
        return pubkey == encryptionPubkey || 
               (previousEncryptionPubkey != bytes32(0) && pubkey == previousEncryptionPubkey);
    }

    /// @notice Returns all config values in a single call.
    /// @return _encryptionPubkey Current encryption key.
    /// @return _previousEncryptionPubkey Previous encryption key.
    /// @return _attestationPubkey Attestation key.
    /// @return _powDifficulty PoW difficulty.
    /// @return _attestationValiditySeconds Attestation validity window.
    function getConfig() external view returns (
        bytes32 _encryptionPubkey,
        bytes32 _previousEncryptionPubkey,
        bytes32 _attestationPubkey,
        uint8 _powDifficulty,
        uint64 _attestationValiditySeconds
    ) {
        return (
            encryptionPubkey,
            previousEncryptionPubkey,
            attestationPubkey,
            powDifficulty,
            attestationValiditySeconds
        );
    }
}
