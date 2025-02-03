# Derivation

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Ecotone: Blob Retrieval](#ecotone-blob-retrieval)
- [Blob Encoding](#blob-encoding)
- [Network upgrade automation transactions](#network-upgrade-automation-transactions)
  - [L1Block Deployment](#l1block-deployment)
  - [GasPriceOracle Deployment](#gaspriceoracle-deployment)
  - [L1Block Proxy Update](#l1block-proxy-update)
  - [GasPriceOracle Proxy Update](#gaspriceoracle-proxy-update)
  - [GasPriceOracle Enable Ecotone](#gaspriceoracle-enable-ecotone)
  - [Beacon block roots contract deployment (EIP-4788)](#beacon-block-roots-contract-deployment-eip-4788)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Ecotone: Blob Retrieval

With the Ecotone upgrade the retrieval stage is extended to support an additional DA source:
[EIP-4844] blobs. After the Ecotone upgrade we modify the iteration over batcher transactions to
treat transactions of transaction-type == `0x03` (`BLOB_TX_TYPE`) differently. If the batcher
transaction is a blob transaction, then its calldata MUST be ignored should it be present. Instead:

- For each blob hash in `blob_versioned_hashes`, retrieve the blob that matches it. A blob may be
  retrieved from any of a number different sources. Retrieval from a local beacon-node, through
  the `/eth/v1/beacon/blob_sidecars/` endpoint, with `indices` filter to skip unrelated blobs, is
  recommended. For each retrieved blob:
  - The blob SHOULD (MUST, if the source is untrusted) be cryptographically verified against its
    versioned hash.
  - If the blob has a [valid encoding](#blob-encoding), decode it into its continuous byte-string
    and pass that on to the next phase. Otherwise the blob is ignored.

Note that batcher transactions of type blob must be processed in the same loop as other batcher
transactions to preserve the invariant that batches are always processed in the order they appear
in the block. We ignore calldata in blob transactions so that it may be used in the future for
batch metadata or other purposes.

## Blob Encoding

Each blob in a [EIP-4844] transaction really consists of `FIELD_ELEMENTS_PER_BLOB = 4096` field elements.

Each field element is a number in a prime field of
`BLS_MODULUS = 52435875175126190479447740508185965837690552500527637822603658699938581184513`.
This number does not represent a full `uint256`: `math.log2(BLS_MODULUS) = 254.8570894...`

The [L1 consensus-specs](https://github.com/ethereum/consensus-specs/blob/dev/specs/deneb/polynomial-commitments.md)
describe the encoding of this polynomial.
The field elements are encoded as big-endian integers (`KZG_ENDIANNESS = big`).

To save computational overhead, only `254` bits per field element are used for rollup data.

For efficient data encoding, `254` bits (equivalent to `31.75` bytes) are utilized.
`4` elements combine to effectively use `127` bytes.

`127` bytes of application-layer rollup data is encoded at a time, into 4 adjacent field elements of the blob:

```python
# read(N): read the next N bytes from the application-layer rollup-data. The next read starts where the last stopped.
# write(V): append V (one or more bytes) to the raw blob.
bytes tailA = read(31)
byte x = read(1)
byte A = x & 0b0011_1111
write(A)
write(tailA)

bytes tailB = read(31)
byte y = read(1)
byte B = (y & 0b0000_1111) | (x & 0b1100_0000) >> 2)
write(B)
write(tailB)

bytes tailC = read(31)
byte z = read(1)
byte C = z & 0b0011_1111
write(C)
write(tailC)

bytes tailD = read(31)
byte D = ((z & 0b1100_0000) >> 2) | ((y & 0b1111_0000) >> 4)
write(D)
write(tailD)
```

Each written field element looks like this:

- Starts with one of the prepared 6-bit left-padded byte values, to keep the field element within valid range.
- Followed by 31 bytes of application-layer data, to fill the low 31 bytes of the field element.

The written output should look like this:

```text
<----- element 0 -----><----- element 1 -----><----- element 2 -----><----- element 3 ----->
| byte A |  tailA...  || byte B |  tailB...  || byte C |  tailC...  || byte D |  tailD...  |
```

The above is repeated 1024 times, to fill all `4096` elements,
with a total of `(4 * 31 + 3) * 1024 = 130048` bytes of data.

When decoding a blob, the top-most two bits of each field-element must be 0,
to make the encoding/decoding bijective.

The first byte of rollup-data (second byte in first field element) is used as a version-byte.

In version `0`, the next 3 bytes of data are used to encode the length of the rollup-data, as big-endian `uint24`.
Any trailing data, past the length delimiter, must be 0, to keep the encoding/decoding bijective.
If the length is larger than `130048 - 4`, the blob is invalid.

If any of the encoding is invalid, the blob as a whole must be ignored.

[EIP-4844]: https://eips.ethereum.org/EIPS/eip-4844

## Network upgrade automation transactions

The Ecotone hardfork activation block contains the following transactions, in this order:

- L1 Attributes Transaction, using the pre-Ecotone `setL1BlockValues`
- User deposits from L1
- Network Upgrade Transactions
  - L1Block deployment
  - GasPriceOracle deployment
  - Update L1Block Proxy ERC-1967 Implementation Slot
  - Update GasPriceOracle Proxy ERC-1967 Implementation Slot
  - GasPriceOracle Enable Ecotone
  - Beacon block roots contract deployment (EIP-4788)

To not modify or interrupt the system behavior around gas computation, this block will not include any sequenced
transactions by setting `noTxPool: true`.

### L1Block Deployment

The `L1Block` contract is upgraded to process the new Ecotone L1-data-fee parameters and L1 blob base-fee.

A deposit transaction is derived with the following attributes:

- `from`: `0x4210000000000000000000000000000000000000`
- `to`: `null`
- `mint`: `0`
- `value`: `0`
- `gasLimit`: `375,000`
- `data`: `0x60806040523480156100105...` ([full bytecode](../../static/bytecode/ecotone-l1-block-deployment.txt))
- `sourceHash`: `0x877a6077205782ea15a6dc8699fa5ebcec5e0f4389f09cb8eda09488231346f8`,
  computed with the "Upgrade-deposited" type, with `intent = "Ecotone: L1 Block Deployment"

This results in the Ecotone L1Block contract being deployed to `0x07dbe8500fc591d1852B76feE44d5a05e13097Ff`, to verify:

```bash
cast compute-address --nonce=0 0x4210000000000000000000000000000000000000
Computed Address: 0x07dbe8500fc591d1852B76feE44d5a05e13097Ff
```

Verify `sourceHash`:

```bash
cast keccak $(cast concat-hex 0x0000000000000000000000000000000000000000000000000000000000000002 $(cast keccak "Ecotone: L1 Block Deployment"))
# 0x877a6077205782ea15a6dc8699fa5ebcec5e0f4389f09cb8eda09488231346f8
```

Verify `data`:

```bash
git checkout 5996d0bc1a4721f2169ba4366a014532f31ea932
pnpm clean && pnpm install && pnpm build
jq -r ".bytecode.object" packages/contracts-bedrock/forge-artifacts/L1Block.sol/L1Block.json
```

This transaction MUST deploy a contract with the following code hash
`0xc88a313aa75dc4fbf0b6850d9f9ae41e04243b7008cf3eadb29256d4a71c1dfd`.

### GasPriceOracle Deployment

The `GasPriceOracle` contract is upgraded to support the new Ecotone L1-data-fee parameters. Post fork this contract
will use the blob base fee to compute the gas price for L1-data-fee transactions.

A deposit transaction is derived with the following attributes:

- `from`: `0x4210000000000000000000000000000000000001`
- `to`: `null`,
- `mint`: `0`
- `value`: `0`
- `gasLimit`: `1,000,000`
- `data`: `0x60806040523480156100...` ([full bytecode](../../static/bytecode/ecotone-gas-price-oracle-deployment.txt))
- `sourceHash`: `0xa312b4510adf943510f05fcc8f15f86995a5066bd83ce11384688ae20e6ecf42`
  computed with the "Upgrade-deposited" type, with `intent = "Ecotone: Gas Price Oracle Deployment"

This results in the Ecotone GasPriceOracle contract being deployed to `0xb528D11cC114E026F138fE568744c6D45ce6Da7A`,
to verify:

```bash
cast compute-address --nonce=0 0x4210000000000000000000000000000000000001
Computed Address: 0xb528D11cC114E026F138fE568744c6D45ce6Da7A
```

Verify `sourceHash`:

```bash
❯ cast keccak $(cast concat-hex 0x0000000000000000000000000000000000000000000000000000000000000002 $(cast keccak "Ecotone: Gas Price Oracle Deployment"))
# 0xa312b4510adf943510f05fcc8f15f86995a5066bd83ce11384688ae20e6ecf42
```

Verify `data`:

```bash
git checkout 5996d0bc1a4721f2169ba4366a014532f31ea932
pnpm clean && pnpm install && pnpm build
jq -r ".bytecode.object" packages/contracts-bedrock/forge-artifacts/GasPriceOracle.sol/GasPriceOracle.json
```

This transaction MUST deploy a contract with the following code hash
`0x8b71360ea773b4cfaf1ae6d2bd15464a4e1e2e360f786e475f63aeaed8da0ae5`.

### L1Block Proxy Update

This transaction updates the L1Block Proxy ERC-1967 implementation slot to point to the new L1Block deployment.

A deposit transaction is derived with the following attributes:

- `from`: `0x0000000000000000000000000000000000000000`
- `to`: `0x4200000000000000000000000000000000000015` (L1Block Proxy)
- `mint`: `0`
- `value`: `0`
- `gasLimit`: `50,000`
- `data`: `0x3659cfe600000000000000000000000007dbe8500fc591d1852b76fee44d5a05e13097ff`
- `sourceHash`: `0x18acb38c5ff1c238a7460ebc1b421fa49ec4874bdf1e0a530d234104e5e67dbc`
  computed with the "Upgrade-deposited" type, with `intent = "Ecotone: L1 Block Proxy Update"

Verify data:

```bash
cast concat-hex $(cast sig "upgradeTo(address)") $(cast abi-encode "upgradeTo(address)" 0x07dbe8500fc591d1852B76feE44d5a05e13097Ff)
0x3659cfe600000000000000000000000007dbe8500fc591d1852b76fee44d5a05e13097ff
```

Verify `sourceHash`:

```bash
cast keccak $(cast concat-hex 0x0000000000000000000000000000000000000000000000000000000000000002 $(cast keccak "Ecotone: L1 Block Proxy Update"))
# 0x18acb38c5ff1c238a7460ebc1b421fa49ec4874bdf1e0a530d234104e5e67dbc
```

### GasPriceOracle Proxy Update

This transaction updates the GasPriceOracle Proxy ERC-1967 implementation slot to point to the new GasPriceOracle
deployment.

A deposit transaction is derived with the following attributes:

- `from`: `0x0000000000000000000000000000000000000000`
- `to`: `0x420000000000000000000000000000000000000F` (Gas Price Oracle Proxy)
- `mint`: `0`
- `value`: `0`
- `gasLimit`: `50,000`
- `data`: `0x3659cfe6000000000000000000000000b528d11cc114e026f138fe568744c6d45ce6da7a`
- `sourceHash`: `0xee4f9385eceef498af0be7ec5862229f426dec41c8d42397c7257a5117d9230a`
  computed with the "Upgrade-deposited" type, with `intent = "Ecotone: Gas Price Oracle Proxy Update"`

Verify data:

```bash
cast concat-hex $(cast sig "upgradeTo(address)") $(cast abi-encode "upgradeTo(address)" 0xb528D11cC114E026F138fE568744c6D45ce6Da7A)
0x3659cfe6000000000000000000000000b528d11cc114e026f138fe568744c6d45ce6da7a
```

Verify `sourceHash`:

```bash
cast keccak $(cast concat-hex 0x0000000000000000000000000000000000000000000000000000000000000002 $(cast keccak "Ecotone: Gas Price Oracle Proxy Update"))
# 0xee4f9385eceef498af0be7ec5862229f426dec41c8d42397c7257a5117d9230a
```

### GasPriceOracle Enable Ecotone

This transaction informs the GasPriceOracle to start using the Ecotone gas calculation formula.

A deposit transaction is derived with the following attributes:

- `from`: `0xDeaDDEaDDeAdDeAdDEAdDEaddeAddEAdDEAd0001` (Depositer Account)
- `to`: `0x420000000000000000000000000000000000000F` (Gas Price Oracle Proxy)
- `mint`: `0`
- `value`: `0`
- `gasLimit`: `80,000`
- `data`: `0x22b90ab3`
- `sourceHash`: `0x0c1cb38e99dbc9cbfab3bb80863380b0905290b37eb3d6ab18dc01c1f3e75f93`,
  computed with the "Upgrade-deposited" type, with `intent = "Ecotone: Gas Price Oracle Set Ecotone"

Verify data:

```bash
cast sig "setEcotone()"
0x22b90ab3
```

Verify `sourceHash`:

```bash
cast keccak $(cast concat-hex 0x0000000000000000000000000000000000000000000000000000000000000002 $(cast keccak "Ecotone: Gas Price Oracle Set Ecotone"))
# 0x0c1cb38e99dbc9cbfab3bb80863380b0905290b37eb3d6ab18dc01c1f3e75f93
```

### Beacon block roots contract deployment (EIP-4788)

[EIP-4788] introduces a "Beacon block roots" contract, that processes and exposes the beacon-block-root values.
at address `BEACON_ROOTS_ADDRESS = 0x000F3df6D732807Ef1319fB7B8bB8522d0Beac02`.

For deployment, [EIP-4788] defines a pre-[EIP-155] legacy transaction, sent from a key that is derived such that the
transaction signature validity is bound to message-hash, which is bound to the input-data, containing the init-code.

However, this type of transaction requires manual deployment and gas-payments.
And since the processing is an integral part of the chain processing, and has to be repeated for every OP-Stack chain,
the deployment is approached differently here.

Some chains may already have a user-submitted instance of the [EIP-4788] transaction.
This is cryptographically guaranteed to be correct, but may result in the upgrade transaction
deploying a second contract, with the next nonce. The result of this deployment can be ignored.

A Deposit transaction is derived with the following attributes:

- `from`: `0x0B799C86a49DEeb90402691F1041aa3AF2d3C875`, as specified in the EIP.
- `to`: null
- `mint`: `0`
- `value`: `0`
- `gasLimit`: `0x3d090`, as specified in the EIP.
- `isCreation`: `true`
- `data`:
  `0x60618060095f395ff33373fffffffffffffffffffffffffffffffffffffffe14604d57602036146024575f5ffd5b5f35801560495762001fff810690815414603c575f5ffd5b62001fff01545f5260205ff35b5f5ffd5b62001fff42064281555f359062001fff015500`
- `isSystemTx`: `false`, as per the Regolith upgrade, even the system-generated transactions spend gas.
- `sourceHash`: `0x69b763c48478b9dc2f65ada09b3d92133ec592ea715ec65ad6e7f3dc519dc00c`,
  computed with the "Upgrade-deposited" type, with `intent = "Ecotone: beacon block roots contract deployment"`

The contract address upon deployment is computed as `rlp([sender, nonce])`, which will equal:

- `BEACON_ROOTS_ADDRESS` if deployed
- a different address (`0xE3aE1Ae551eeEda337c0BfF6C4c7cbA98dce353B`) if `nonce = 1`:
  when a user already submitted the EIP transaction before the upgrade.

Verify `BEACON_ROOTS_ADDRESS`:

```bash
cast compute-address --nonce=0 0x0B799C86a49DEeb90402691F1041aa3AF2d3C875
# Computed Address: 0x000F3df6D732807Ef1319fB7B8bB8522d0Beac02
```

Verify `sourceHash`:

```bash
cast keccak $(cast concat-hex 0x0000000000000000000000000000000000000000000000000000000000000002 $(cast keccak "Ecotone: beacon block roots contract deployment"))
# 0x69b763c48478b9dc2f65ada09b3d92133ec592ea715ec65ad6e7f3dc519dc00c
```

[EIP-4788]: https://eips.ethereum.org/EIPS/eip-4788
[EIP-155]: https://eips.ethereum.org/EIPS/eip-155
