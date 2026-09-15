# Beacon genesis reference

The reference beacon state was generated using ethpandaops/eth-beacon-genesis at
`f57c0fb4606f9a28e5eecb61546efa9b658183b2`, the previous devnet setup image's pin.

Inputs: `../assets/l1.json` with timestamp `1700000000`, chain ID `1337`, the
minimal beacon config from `BeaconGenesis` with 12-second slots, and one validator
from `test test test test test test test test test test test junk` (index zero).
No deployed L1 contracts are needed for this reference. Only its SHA-256 is retained:

`41dcec51a3774fe7bbb60b49502bbd6f75898bd17d814041d770893716c8a56c`

Use the resolved beacon config written by `BeaconGenesis` as `config.yaml`.
Create `mnemonics.yaml` with:

```yaml
- mnemonic: "test test test test test test test test test test test junk"
  count: 1
```

```sh
jq '.timestamp = "0x6553f100"' ../assets/l1.json > l1.json
eth-genesis-state-generator beaconchain \
  --eth1-config l1.json --config config.yaml --mnemonics mnemonics.yaml \
  --state-output beacon.ssz
sha256sum beacon.ssz
```

Compare the hash with the expected digest in `../src/beacon.rs`. Update it only after
regenerating with the pinned independent generator and reviewing the input changes.

This checks the complete state encoding, validator identity, execution header
linkage, and fork initialization against an independent implementation.
