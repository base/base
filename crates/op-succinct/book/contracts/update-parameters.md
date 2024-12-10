# Updating `OPSuccinctL2OutputOracle` Parameters

If you just need to update the `aggregationVkey`, `rangeVkeyCommitment` or `rollupConfigHash` parameters and not upgrade the contract itself, you can use the `just update-parameters` command.

The command will only update the parameters in the contract if they don't match the verification keys or the rollup config hash locally.

## 1. Configure your environment

First, ensure that you have the correct environment variables set in your `.env` file. See the [Configuration](./configuration.md) section for more information.

## 2. Update the parameters

If you are updating the parameters with an EOA `ADMIN` key, you will execute the parameter update calls with the `ADMIN` key. If you do not have the `ADMIN` key, the call below will output the raw calldata for the parameter update calls, which can be executed by the "owner" in a separate context.

### Updating Parameters with an EOA `ADMIN` key

To update the parameters of the `OPSuccinctL2OutputOracle` contract with an EOA `ADMIN` key, run the following command in `/contracts`.

```bash
just update-parameters
```

### Updating Parameters with a non-EOA `ADMIN` key

If the owner of the `OPSuccinctL2OutputOracle` is not an EOA (e.g. multisig, contract), set `EXECUTE_UPGRADE_CALL` to `false` in your `.env` file. This will output the raw calldata for the parameter update calls, which can be executed by the owner in a separate context.

| Parameter | Description |
|-----------|-------------|
| `EXECUTE_UPGRADE_CALL` | Set to `false` to output the raw calldata for the parameter update calls. |

Then, run the following command in `/contracts`.

```bash
just update-parameters
```

```shell
% just update-parameters
warning: op-succinct-scripts@0.1.0: fault-proof built with release-client-lto profile
warning: op-succinct-scripts@0.1.0: range built with release-client-lto profile
warning: op-succinct-scripts@0.1.0: native_host_runner built with release profile
    Finished `release` profile [optimized] target(s) in 0.35s
     Running `target/release/fetch-rollup-config --env-file .env`
[⠊] Compiling...
Script ran successfully.

== Logs ==
  The calldata for upgrading the aggregationVkey is:
  0xc4cb03ec005e5786785a9c61015fa1f0543831fb0e0602684473de8758496556010b1d08
  The calldata for upgrading the rangeVkeyCommitment is:
  0xbc91ce33472e8f9b2f650ae74c7997a3272ef5b50be834145b44cf7f1d52b58235bd6018
```