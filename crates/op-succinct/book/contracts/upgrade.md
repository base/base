# Upgrading `OPSuccinctL2OutputOracle`

Similar to the `L2OutputOracle` contract, the `OPSuccinctL2OutputOracle` is managed via an upgradeable proxy. The upgrade process is the same as the `L2OutputOracle` contract.

## 1. Decide on the target `OPSuccinctL2OutputOracle` contract code

### (Recommanded) Using `OPSuccinctL2OutputOracle` from a release

Check out the latest release of `op-succinct` from [here](https://github.com/succinctlabs/op-succinct/releases). You can always find the latest version of the `OPSuccinctL2OutputOracle` on the latest release.

### Manual Changes to `OPSuccinctL2OutputOracle`

If you want to manually upgrade the `OPSuccinctL2OutputOracle` contract, follow these steps:

1. Make the relevant changes to the `OPSuccinctL2OutputOracle` contract.

2. Then, manually bump the `initializerVersion` version in the `OPSuccinctL2OutputOracle` contract. You will need to manually bump the `initializerVersion` version in the `OPSuccinctL2OutputOracle` contract to keep track of the upgrade history.
  ```solidity
  // Original initializerVersion
  uint256 public constant initializerVersion = 1;
  ```

  ```solidity
  // Increment the initializerVersion to 2.
  uint256 public constant initializerVersion = 2;
  ```

## 2. Configure your environment

Ensure that you have the correct environment variables set in your environment file. See the [Configuration](./configuration.md) section for more information.

## 3. Upgrade the `OPSuccinctL2OutputOracle` contract

Get the address of the existing `OPSuccinctL2OutputOracle` contract. If you are upgrading with an EOA `ADMIN` key, you will execute the upgrade call with the `ADMIN` key. If you do not have the `ADMIN` key, the call below will output the raw calldata for the upgrade call, which can be executed by the "owner" in a separate context.

### Upgrading with an EOA `ADMIN` key

To update the `L2OutputOracle` implementation with an EOA `ADMIN` key, run the following command in `/contracts`.

```bash
just upgrade-oracle
```

### Upgrading with a non-EOA `ADMIN` key

If the owner of the `L2OutputOracle` is not an EOA (e.g. multisig, contract), set `EXECUTE_UPGRADE_CALL` to `false` in your `.env` file. This will output the raw calldata for the upgrade call, which can be executed by the owner in a separate context.

| Parameter | Description |
|-----------|-------------|
| `EXECUTE_UPGRADE_CALL` | Set to `false` to output the raw calldata for the upgrade call. |

Then, run the following command in `/contracts`.

```bash
just upgrade-oracle
```

```shell
% just upgrade-oracle
warning: op-succinct-scripts@0.1.0: fault-proof built with release-client-lto profile
warning: op-succinct-scripts@0.1.0: range built with release-client-lto profile
warning: op-succinct-scripts@0.1.0: native_host_runner built with release profile
    Finished `release` profile [optimized] target(s) in 0.35s
     Running `target/release/fetch-rollup-config --env-file .env`
[⠊] Compiling...
Script ran successfully.

== Logs ==
  The calldata for upgrading the contract with the new initialization parameters is:
  0x4f1ef2860000000000000000000000007f5d6a5b55ee82090aedc859b40808103b30e46900000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000184c0e8e2a100000000000000000000000000000000000000000000000000000000000004b00000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000135161100000000000000000000000000000000000000000000000000000000674107ce000000000000000000000000ded0000e32f8f40414d3ab3a830f735a3553e18e000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001346d7ea10cc78c48e3da6f1890bb16cf27e962202f0f9b2c5c57f3cfb0c559ec1dca031e9fc246ec47246109ebb324a357302110de5d447af13a07f527620000000000000000000000004cb20fa9e6fdfe8fdb6ce0942c5f40d49c8986469ad6f24abc0df5e4cab37c1efae21643938ed5393389ce9e747524a59546a8785e32d5f9f902c6a46cb75cbdb083ea67b9475d7026542a009dc9d99072f4bdf100000000000000000000000000000000000000000000000000000000

## Setting up 1 EVM.
```