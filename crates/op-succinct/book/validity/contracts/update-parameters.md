# Updating `OPSuccinctL2OutputOracle` Parameters

If you just need to update the `aggregationVkey`, `rangeVkeyCommitment` or `rollupConfigHash` parameters and not upgrade the contract itself, you can use the `just add-config` and `just remove-config` command.

## 1. Configure your environment

First, ensure that you have the correct environment variables set in your `.env` file. See the [Environment Variables](./environment.md) section for more information.

## 2. Updating configurations

Upon the first deployment, there is a genesis config named `opsuccinct_genesis`. Proposers will interact with this config by default. You can add or remove configurations using the following commands:

### Adding a Configuration

To add a new configuration, run:

```bash
just add-config <config_name>
```

### Removing a Configuration

To remove an existing configuration, run:

```bash
just remove-config <config_name>
```

### Permissions

#### Using an EOA `ADMIN_PK` key

If you are the owner of the `OPSuccinctL2OutputOracle` contract, you can set `ADMIN_PK` in your `.env` to directly add and remove configurations. If unset, this will default to the value of `PRIVATE_KEY`.

```bash
$ just add-config new_config

...

== Logs ==
  Added OpSuccinct config: new_config

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

#### Updating Parameters with a non-EOA `ADMIN_PK`

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

## Rolling upgrade guide

1. Perform some update that changes your `aggregationVkey`, `rangeVkeyCommitment` or `rollupConfigHash` locally. For more information on producing the elfs and vkeys, see [this page](../../advanced/verify-binaries.md).
2. From the project root, run the following command to add a new config. `just add_config my_upgrade`. This will automatically fetch the `aggregationVkey` and `rangeVkeyCommitment` from the `elf` directory, and the `rollupConfigHash` from the `L2_RPC` set in the `.env`.
3. Spin up a new proposer that interacts with this config, by changing [`OP_SUCCINCT_CONFIG_NAME`](../proposer.md#optional-environment-variables).
4. Shut down your old proposer.
5. For security, delete your old `OpSuccinctConfig` with `just remove-config old_config`.
