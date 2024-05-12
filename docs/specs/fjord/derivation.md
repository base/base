# Fjord L2 Chain Derivation Changes

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Protocol Parameter Changes](#protocol-parameter-changes)
  - [Timestamp Activation](#timestamp-activation)
  - [Constant Maximum Sequencer Drift](#constant-maximum-sequencer-drift)
    - [Rationale](#rationale)
    - [Security Considerations](#security-considerations)
  - [Increasing `MAX_RLP_BYTES_PER_CHANNEL` and `MAX_CHANNEL_BANK_SIZE`](#increasing-max_rlp_bytes_per_channel-and-max_channel_bank_size)
    - [Rationale](#rationale-1)
    - [Security Considerations](#security-considerations-1)
- [Brotli Channel Compression](#brotli-channel-compression)
- [Network upgrade automation transactions](#network-upgrade-automation-transactions)
  - [GasPriceOracle Deployment](#gaspriceoracle-deployment)
  - [GasPriceOracle Proxy Update](#gaspriceoracle-proxy-update)
  - [GasPriceOracle Enable Fjord](#gaspriceoracle-enable-fjord)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

# Protocol Parameter Changes

The following table gives an overview of the changes in parameters.

| Parameter | Pre-Fjord (default) value | Fjord value | Notes |
| --------- | ------------------------- | ----------- | ----- |
| `max_sequencer_drift` | 600 | 1800 | Was a protocol parameter since Bedrock. Now becomes a constant. |
| `MAX_RLP_BYTES_PER_CHANNEL` | 10,000,000 | 100,000,000 | Protocol Constant is increasing. |
| `MAX_CHANNEL_BANK_SIZE` | 100,000,000 | 1,000,000,000 | Protocol Constant is increasing. |

## Timestamp Activation

Fjord, like other network upgrades, is activated at a timestamp.
Changes to the L2 Block execution rules are applied when the `L2 Timestamp >= activation time`.
Changes to derivation are applied when it is considering data from a L1 Block whose timestamp
is greater than or equal to the activation timestamp.
The change of the `max_sequencer_drift` parameter activates with the L1 origin block timestamp.

If Fjord is not activated at genesis, it must be activated at least one block after the Ecotone
activation block. This ensures that the network upgrade transactions don't conflict.

## Constant Maximum Sequencer Drift

With Fjord, the `max_sequencer_drift` parameter becomes a constant of value `1800` _seconds_,
translating to a fixed maximum sequencer drift of 30 minutes.

Before Fjord, this was a chain parameter that was set once at chain creation, with a default
value of `600` seconds, i.e., 10 minutes. Most chains use this value currently.

### Rationale

Discussions amongst chain operators came to the unilateral conclusion that a larger value than the
current default would be easier to work with. If a sequencer's L1 connection breaks, this drift
value determines how long it can still produce blocks without violating the timestamp drift
derivation rules.

It was furthermore agreed that configurability after this increase is not important. So it is being
made a constant. An alternative idea that is being considered for a future hardfork is to make this
an L1-configurable protocol parameter via the `SystemConfig` update mechanism.

### Security Considerations

The rules around the activation time are deliberately being kept simple, so no other logic needs to
be applied other than to change the parameter to a constant. The first Fjord block would in theory
accept older L1-origin timestamps than its predecessor. However, since the L1 origin timestamp must
also increase, the only noteworthy scenario that can happen is that the first few Fjord blocks will
be in the same epoch as the the last pre-Fjord blocks, even if these blocks would not be allowed to
have these L1-origin timestamps according to pre-Fjord rules. So the same L1 timestamp would be
shared within a pre- and post-Fjord mixed epoch. This is considered a feature and is not considered
a security issue.

## Increasing `MAX_RLP_BYTES_PER_CHANNEL` and `MAX_CHANNEL_BANK_SIZE`

With Fjord, `MAX_RLP_BYTES_PER_CHANNEL` will be increased from 10,000,000 bytes to 100,000,000 bytes,
and `MAX_CHANNEL_BANK_SIZE` will be increased from 100,000,000 bytes to 1,000,000,000 bytes.

The usage of `MAX_RLP_BYTES_PER_CHANNEL` is defined in [Channel Format](../protocol/derivation.md#channel-format).
The usage of `MAX_CHANNEL_BANK_SIZE` is defined in [Channel Bank Pruning](../protocol/derivation.md#pruning).

Span Batches previously had a limit `MAX_SPAN_BATCH_SIZE` which was equal to `MAX_RLP_BYTES_PER_CHANNEL`.
Fjord creates a new constant `MAX_SPAN_BATCH_ELEMENT_COUNT` for the element count limit & removes
`MAX_SPAN_BATCH_SIZE`. The size of the channel is still checked with `MAX_RLP_BYTES_PER_CHANNEL`.

The new value will be used when the timestamp of the L1 origin of the derivation pipeline >= the Fjord activation
timestamp.

### Rationale

A block with a gas limit of 30 Million gas has a maximum theoretical size of 7.5 Megabytes by being filled up
with transactions have only zeroes. Currently, a byte with the value `0` consumes 4 gas.
If the block gas limit is raised above 40 Million gas, it is possible to create a block that is large than
`MAX_RLP_BYTES_PER_CHANNEL`.
L2 blocks cannot be split across channels which means that a block that is larger than `MAX_RLP_BYTES_PER_CHANNEL`
cannot be batch submitted.
By raising this limit to 100,000,000 bytes, we can batch submit blocks with a gas limit of up to 400 Million Gas.
In addition, we are able to improve compression ratios by increasing the amount of data that can be inserted into a
single channel.
With 33% compression ratio over 6 blobs, we are currently submitting 2.2 MB of compressed data & 0.77 MB of uncompressed
data per channel.
This will allow use to use up to approximately 275 blobs per channel.

Raising `MAX_CHANNEL_BANK_SIZE` is helpful to ensure that we are able to process these larger channels. We retain the
same ratio of 10 between `MAX_RLP_BYTES_PER_CHANNEL` and `MAX_CHANNEL_BANK_SIZE`.

### Security Considerations

Raising the these limits increases the amount of resources a rollup node would require.
Specifically nodes may have to allocate large chunks of memory for a channel and will have to potentially allocate more
memory to the channel bank.
`MAX_RLP_BYTES_PER_CHANNEL` was originally added to avoid zip bomb attacks.
The system is still exposed to these attacks, but these limits are straightforward to handle in a node.

The Fault Proof environment is more constrained than a typical node and increasing these limits will require more
resources than are currently required.
The change in `MAX_CHANNEL_BANK_SIZE` is not relevant to the first implementation of Fault Proofs because this limit
only tells the node when to start pruning & once memory is allocated in the FPVM, it is not garbage collected.
This means that increasing `MAX_CHANNEL_BANK_SIZE` does not increase the maximum resource usage of the FPP.

Increasing `MAX_RLP_BYTES_PER_CHANNEL` could cause more resource usage in FPVM; however, we consider this
increase reasonable because this increase is in the amount of data handled at once rather than the total
amount of data handled in the program. Instead of using a single channel, the batcher could submit 10 channels
prior to this change which would cause the Fault Proof Program to consume a very similar amount of resources.

# Brotli Channel Compression

[legacy-channel-format]: ../protocol/derivation.md#channel-format

Fjord introduces a new versioned channel encoding format to support alternate compression
algorithms, with the [legacy channel format][legacy-channel-format] remaining supported. The
versioned format is as follows:

```text
channel_encoding = `channel_version_byte ++ compress(rlp_batches)`
```

The `channel_version_byte` must never have its 4 lower order bits set to `0b1000 = 8` or `0b1111 =
15`, which are reserved for usage by the header byte of zlib encoded data (see page 5 of
[RFC-1950][rfc1950]). This allows a channel decoder to determine if a channel encoding is legacy or
versioned format by testing for these bit values. If the channel encoding is determined to be
versioned format, the only valid `channel_version_byte` is `1`, which indicates `compress()` is the
Brotli compression algorithm (as specified in [RFC-7932][rfc7932]) with no custom dictionary.

[rfc7932]: https://datatracker.ietf.org/doc/html/rfc7932
[rfc1950]: https://www.rfc-editor.org/rfc/rfc1950.html

# Network upgrade automation transactions

The Fjord hardfork activation block contains the following transactions, in this order:

- L1 Attributes Transaction
- User deposits from L1
- Network Upgrade Transactions
  - GasPriceOracle deployment
  - Update GasPriceOracle Proxy ERC-1967 Implementation Slot
  - GasPriceOracle Enable Fjord

To not modify or interrupt the system behavior around gas computation, this block will not include any sequenced
transactions by setting `noTxPool: true`.

## GasPriceOracle Deployment

The `GasPriceOracle` contract is upgraded to support the new Fjord L1 data fee computation. Post fork this contract
will use FastLZ to compute the L1 data fee.

To perform this upgrade, a deposit transaction is derived with the following attributes:

- `from`: `0x4210000000000000000000000000000000000002`
- `to`: `null`,
- `mint`: `0`
- `value`: `0`
- `gasLimit`: `1,450,000`
- `data`: `0x60806040523...` ([full bytecode](../static/bytecode/fjord-gas-price-oracle-deployment.txt))
- `sourceHash`: `0x86122c533fdcb89b16d8713174625e44578a89751d96c098ec19ab40a51a8ea3`
  computed with the "Upgrade-deposited" type, with `intent = "Fjord: Gas Price Oracle Deployment"

This results in the Fjord GasPriceOracle contract being deployed to `0xa919894851548179A0750865e7974DA599C0Fac7`,
to verify:

```bash
cast compute-address --nonce=0 0x4210000000000000000000000000000000000002
Computed Address: 0xa919894851548179A0750865e7974DA599C0Fac7
```

Verify `sourceHash`:

```bash
cast keccak $(cast concat-hex 0x0000000000000000000000000000000000000000000000000000000000000002 $(cast keccak "Fjord: Gas Price Oracle Deployment"))
# 0x86122c533fdcb89b16d8713174625e44578a89751d96c098ec19ab40a51a8ea3
```

Verify `data`:

```bash
git checkout fbdba16ce5fe0207ceeb8487d762807888aa43f5 (update once merged)
pnpm clean && pnpm install && pnpm build
jq -r ".bytecode.object" packages/contracts-bedrock/forge-artifacts/GasPriceOracle.sol/GasPriceOracle.json
```

This transaction MUST deploy a contract with the following code hash
`0xa8682d0cb2cf24478bda32c1058ed3ca741134c5a319e80f29a30f45fe5b8245`.

## GasPriceOracle Proxy Update

This transaction updates the GasPriceOracle Proxy ERC-1967 implementation slot to point to the new GasPriceOracle
deployment.

A deposit transaction is derived with the following attributes:

- `from`: `0x0000000000000000000000000000000000000000`
- `to`: `0x420000000000000000000000000000000000000F` (Gas Price Oracle Proxy)
- `mint`: `0`
- `value`: `0`
- `gasLimit`: `50,000`
- `data`: `0x3659cfe6000000000000000000000000a919894851548179a0750865e7974da599c0fac7`
- `sourceHash`: `0x1e6bb0c28bfab3dc9b36ffb0f721f00d6937f33577606325692db0965a7d58c6`
  computed with the "Upgrade-deposited" type, with `intent = "Fjord: Gas Price Oracle Proxy Update"`

Verify data:

```bash
cast concat-hex $(cast sig "upgradeTo(address)") $(cast abi-encode "upgradeTo(address)" 0xa919894851548179A0750865e7974DA599C0Fac7)
# 0x3659cfe6000000000000000000000000a919894851548179a0750865e7974da599c0fac7
```

Verify `sourceHash`:

```bash
cast keccak $(cast concat-hex 0x0000000000000000000000000000000000000000000000000000000000000002 $(cast keccak "Fjord: Gas Price Oracle Proxy Update"))
# 0x1e6bb0c28bfab3dc9b36ffb0f721f00d6937f33577606325692db0965a7d58c6
```

## GasPriceOracle Enable Fjord

This transaction informs the GasPriceOracle to start using the Fjord gas calculation formula.

A deposit transaction is derived with the following attributes:

- `from`: `0xDeaDDEaDDeAdDeAdDEAdDEaddeAddEAdDEAd0001` (Depositer Account)
- `to`: `0x420000000000000000000000000000000000000F` (Gas Price Oracle Proxy)
- `mint`: `0`
- `value`: `0`
- `gasLimit`: `90,000`
- `data`: `0x8e98b106`
- `sourceHash`: `0xbac7bb0d5961cad209a345408b0280a0d4686b1b20665e1b0f9cdafd73b19b6b`,
  computed with the "Upgrade-deposited" type, with `intent = "Fjord: Gas Price Oracle Set Fjord"

Verify data:

```bash
cast sig "setFjord()"
0x8e98b106
```

Verify `sourceHash`:

```bash
cast keccak $(cast concat-hex 0x0000000000000000000000000000000000000000000000000000000000000002 $(cast keccak "Fjord: Gas Price Oracle Set Fjord"))
# 0xbac7bb0d5961cad209a345408b0280a0d4686b1b20665e1b0f9cdafd73b19b6b
```
