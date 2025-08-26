# Updating `OPSuccinctL2OutputOracle` Parameters

OP Succinct supports a rolling update process when [program binaries](/advanced/verify-binaries.md) must be reproduced and only the `aggregationVkey`, `rangeVkeyCommitment` or `rollupConfigHash` parameters change. For example, this could happen if

-   The SP1 version changes
-   An optimization to the range program is released
-   Some L2 parameters change

## Rolling update guide

1. Generate new elfs, vkeys, and a rollup config hash by following [this guide](/advanced/verify-binaries.md).
2. From the project's root, run `just add_config my_upgrade`.
    - This will automatically fetch the `aggregationVkey` and `rangeVkeyCommitment` from the [`/elf`](https://github.com/succinctlabs/op-succinct/tree/main/elf) directory, and the `rollupConfigHash` from the `L2_RPC` set in the `.env`. The output will look like the following:

```bash
$ just add-config my_upgrade

...

== Logs ==
  Added OpSuccinct config: my_upgrade

## Setting up 1 EVM.

==========================

Chain 11155111

Estimated gas price: 0.002818893 gwei

Estimated total gas used for script: 147070

Estimated amount required: 0.00000041457459351 ETH

==========================

##### sepolia
✅  [Success] Hash: 0xa87279416385a17518f8cc27a28fa43432b1bf7dba6a1983cdf5146220a4ec7a
Block: 8570449
Paid: 0.00000020644771056 ETH (100561 gas * 0.00205296 gwei)

✅ Sequence #1 on sepolia | Total Paid: 0.00000020644771056 ETH (100561 gas * avg 0.00205296 gwei)

==========================

ONCHAIN EXECUTION COMPLETE & SUCCESSFUL.

...

```

1. Spin up a new proposer with the [`OP_SUCCINCT_CONFIG_NAME`](../proposer.md#optional-environment-variables) environment variable set to the name of the config you added. For this example, you would set `OP_SUCCINCT_CONFIG_NAME="my_upgrade"` in your `.env` file.
2. Shut down your old proposer.
3. For security, delete your old `OpSuccinctConfig` by running `just remove-config old_config`.

### Using an EOA admin key

If you are the owner of the `OPSuccinctL2OutputOracle` contract, you can set `ADMIN_PK` in your `.env` to directly add and remove configurations. If unset, this will default to the value of `PRIVATE_KEY`.

### Updating Parameters with a non-EOA `ADMIN_PK`

If the owner of the `OPSuccinctL2OutputOracle` is not an EOA (e.g. multisig, contract), set `EXECUTE_UPGRADE_CALL` to `false` in your `.env` file. This will output the raw calldata for the parameter update calls, which can be executed by the owner in a separate context.

| Parameter | Description |
|-----------|-------------|
| `EXECUTE_UPGRADE_CALL` | Set to `false` to output the raw calldata for the parameter update calls. |

Then, run the following command from the project root.

```bash
$ just add-config new_config

...

== Logs ==
  The calldata for adding the OP Succinct configuration is:
  0x47c37e9c1614abfc873fd38dcc6705b30385...
Warning: No transactions to broadcast.
```
