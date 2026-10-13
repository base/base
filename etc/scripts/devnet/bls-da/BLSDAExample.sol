// SPDX-License-Identifier: MIT
pragma solidity 0.8.37;

/// @notice Research-only ETH-transfer bundle. Not audited; do not fund on a public chain.
/// Uses the BLS proof-of-possession scheme (G1 signatures): registration verifies
/// a PoP over the canonical compressed public key before admitting the account.
/// Transfers still sign individual intents, not a shared batch message.
/// Public keys are immutable; deployment/registration costs are not amortized away
/// in the demo's separate setup measurement. No arbitrary calls or key rotation.
contract BLSDAExample {
    bytes constant P = hex"1a0111ea397fe69a4b1ba7b6434bacd764774b84f38512bf6730d2a0f6b0f6241eabfffeb153ffffb9feffffffffaaab";
    bytes constant DST = "BLS_SIG_BLS12381G1_XMD:SHA-256_SSWU_RO_POP_";
    bytes constant POP_DST = "BLS_POP_BLS12381G1_XMD:SHA-256_SSWU_RO_POP_";
    bytes32 constant DOMAIN = keccak256("base-bls-da-example:v2:eth-transfer");
    uint256 constant RECORD_BYTES = 44; // id:u32 | nonce:u64 | to:address | value:u96
    uint256 constant MAX_TRANSFERS = 64;
    uint256 constant HALF_SECP_ORDER = 0x7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a0;

    struct Account {
        bytes publicKey; // EIP-2537 G2, 256 bytes, not compressed wire format
        address ecdsaOwner;
        uint64 nonce;
        uint256 balance;
    }

    mapping(uint32 => Account) public accounts;
    uint32 public count;
    // Fixed BLS12-381 -G2 generator, EIP-2537 c0/c1 encoding.
    bytes constant NEGATIVE_GENERATOR = hex"00000000000000000000000000000000024aa2b2f08f0a91260805272dc51051c6e47ad4fa403b02b4510b647ae3d1770bac0326a805bbefd48056c8c121bdb8"
        hex"0000000000000000000000000000000013e02b6052719f607dacd3a088274f65596bd0d09920b61ab5da61bbdc7f5049334cf11213945d57e5ac7d055d042b7e"
        hex"000000000000000000000000000000000d1b3cc2c7027888be51d9ef691d77bcb679afda66c73f17f9ee3837a55024f78c71363275a75d75d86bab79f74782aa"
        hex"0000000000000000000000000000000013fa4d4a0ad8b1ce186ed5061789213d993923066dddaf1040bc3ff59f825c78df74f2d75467e25e0f55f8a00fa030ed";
    bool private entered;

    function register(bytes calldata publicKey, address ecdsaOwner, bytes calldata proof) external payable returns (uint32 id) {
        require(publicKey.length == 256 && ecdsaOwner != address(0), "key length/owner");
        require(keccak256(publicKey) != keccak256(new bytes(256)), "identity key");
        // Multiplication by one validates canonical encoding, curve, and subgroup.
        bytes memory checked = precompile(0x0e, abi.encodePacked(publicKey, uint256(1)), 256);
        require(keccak256(checked) == keccak256(publicKey), "invalid key");
        // CFRG PopVerify hashes the canonical compressed key, not the padded
        // EIP-2537 representation. Use c1 || c0, sorting y.c1 before y.c0.
        bytes memory compressed = bytes.concat(publicKey[80:128], publicKey[16:64]);
        bytes memory y = publicKey[208:256];
        if (keccak256(y) == keccak256(new bytes(48))) y = publicKey[144:192];
        compressed[0] = bytes1(uint8(compressed[0]) | 0x80 | (less(subtract(P, y), y) ? 0x20 : 0));
        bytes memory pairs = bytes.concat(
            hashToG1(compressed, POP_DST), publicKey, decompress(proof), NEGATIVE_GENERATOR
        );
        require(abi.decode(precompile(0x0f, pairs, 32), (uint256)) == 1, "BLS proof of possession");
        // Membership in this immutable registry implies a validated PoP. Its
        // bytes stay in historical DA; they need not occupy execution storage.
        id = ++count;
        accounts[id] = Account(publicKey, ecdsaOwner, 0, msg.value);
    }

    /// @notice Identical transfer packing/state machine for the ECDSA control and BLS.
    function execute(bytes calldata records, bytes calldata signatures, bool bls) external {
        require(!entered, "reentrant");
        entered = true;
        uint256 n = records.length / RECORD_BYTES;
        require(n > 0 && n <= MAX_TRANSFERS && records.length == n * RECORD_BYTES, "records");
        require(signatures.length == (bls ? 48 : 65 * n), "signature length");
        bytes memory pairs;
        uint32 previous;
        for (uint256 i; i < n; ++i) {
            bytes calldata record = records[i * RECORD_BYTES:(i + 1) * RECORD_BYTES];
            uint32 id = uint32(bytes4(record[:4]));
            require(id > previous && id <= count, "distinct ordered accounts");
            previous = id;
            Account storage account = accounts[id];
            require(uint64(bytes8(record[4:12])) == account.nonce, "nonce");
            uint96 value = uint96(bytes12(record[32:44]));
            require(account.balance >= value, "balance");
            bytes32 digest = keccak256(abi.encodePacked(DOMAIN, block.chainid, address(this), record));
            if (bls) {
                pairs = bytes.concat(pairs, hashToG1(abi.encodePacked(digest), DST), account.publicKey);
            } else {
                bytes calldata sig = signatures[i * 65:(i + 1) * 65];
                bytes32 r = bytes32(sig[:32]);
                bytes32 s = bytes32(sig[32:64]);
                uint8 v = uint8(sig[64]);
                require(uint256(s) <= HALF_SECP_ORDER && (v == 27 || v == 28), "ECDSA encoding");
                require(ecrecover(digest, v, r, s) == account.ecdsaOwner, "ECDSA signature");
            }
            account.nonce++;
            account.balance -= value;
        }
        if (bls) {
            pairs = bytes.concat(pairs, decompress(signatures), NEGATIVE_GENERATOR);
            require(abi.decode(precompile(0x0f, pairs, 32), (uint256)) == 1, "BLS signature");
        }
        // Authorize the entire bundle before external effects. Failed sends roll back all state.
        for (uint256 i; i < n; ++i) {
            bytes calldata record = records[i * RECORD_BYTES:(i + 1) * RECORD_BYTES];
            (bool ok,) = address(bytes20(record[12:32])).call{value: uint96(bytes12(record[32:44]))}("");
            require(ok, "transfer");
        }
        entered = false;
    }

    /// @notice RFC 9380 expand_message_xmd(SHA-256), hash_to_field(count=2), map + add.
    /// EIP-2537 MAP already clears the cofactor; its outputs are subgroup points.
    function hashToG1(bytes memory message, bytes memory dst) public view returns (bytes memory) {
        require(dst.length <= 255, "DST length");
        bytes memory dstPrime = abi.encodePacked(dst, uint8(dst.length));
        bytes32 b0 = sha256(abi.encodePacked(new bytes(64), message, uint16(128), uint8(0), dstPrime));
        bytes32 b1 = sha256(abi.encodePacked(b0, uint8(1), dstPrime));
        bytes32 b2 = sha256(abi.encodePacked(b0 ^ b1, uint8(2), dstPrime));
        bytes32 b3 = sha256(abi.encodePacked(b0 ^ b2, uint8(3), dstPrime));
        bytes32 b4 = sha256(abi.encodePacked(b0 ^ b3, uint8(4), dstPrime));
        bytes memory u0 = modexp(abi.encodePacked(b1, b2), hex"01");
        bytes memory u1 = modexp(abi.encodePacked(b3, b4), hex"01");
        bytes memory q0 = precompile(0x10, bytes.concat(new bytes(16), u0), 128);
        bytes memory q1 = precompile(0x10, bytes.concat(new bytes(16), u1), 128);
        return precompile(0x0b, bytes.concat(q0, q1), 128);
    }

    /// @notice Decode canonical compressed G1 (48 bytes) to EIP-2537 (128 bytes).
    /// The pairing precompile subsequently enforces subgroup membership.
    function decompress(bytes memory compressed) public view returns (bytes memory) {
        require(compressed.length == 48, "G1 length");
        uint8 flags = uint8(compressed[0]);
        require(flags & 0xc0 == 0x80, "G1 flags/infinity");
        bool high = flags & 0x20 != 0;
        compressed[0] = bytes1(flags & 0x1f);
        require(less(compressed, P), "G1 x range");
        bytes memory rhs = modexp(compressed, hex"03");
        uint256 carry = 4;
        for (uint256 i = 48; i > 0;) {
            --i;
            carry += uint8(rhs[i]);
            rhs[i] = bytes1(uint8(carry));
            carry >>= 8;
        }
        if (!less(rhs, P)) rhs = subtract(rhs, P);
        // (p + 1) / 4; p == 3 mod 4.
        bytes memory y = modexp(rhs, hex"0680447a8e5ff9a692c6e9ed90d2eb35d91dd2e13ce144afd9cc34a83dac3d8907aaffffac54ffffee7fbfffffffeaab");
        require(keccak256(modexp(y, hex"02")) == keccak256(rhs), "G1 not on curve");
        bytes memory negativeY = subtract(P, y);
        if (less(y, negativeY) == high) y = negativeY;
        require(less(y, P), "G1 y range");
        return bytes.concat(new bytes(16), compressed, new bytes(16), y);
    }

    function less(bytes memory a, bytes memory b) private pure returns (bool) {
        for (uint256 i; i < 48; ++i) {
            if (a[i] != b[i]) return uint8(a[i]) < uint8(b[i]);
        }
        return false;
    }

    function subtract(bytes memory a, bytes memory b) private pure returns (bytes memory result) {
        result = new bytes(48);
        uint256 borrow;
        for (uint256 i = 48; i > 0;) {
            --i;
            uint256 v = 256 + uint8(a[i]) - uint8(b[i]) - borrow;
            result[i] = bytes1(uint8(v));
            borrow = v < 256 ? 1 : 0;
        }
        require(borrow == 0, "subtraction");
    }

    function modexp(bytes memory base, bytes memory exponent) private view returns (bytes memory) {
        return precompile(5, abi.encodePacked(base.length, exponent.length, uint256(48), base, exponent, P), 48);
    }

    function precompile(uint160 target, bytes memory input, uint256 size) private view returns (bytes memory result) {
        bool ok;
        (ok, result) = address(target).staticcall(input);
        require(ok && result.length == size, "precompile");
    }
}
