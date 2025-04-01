# Upgrading OP Succinct

Each new release of `op-succinct` will specify if it includes:

- New verification keys
- Contract changes 
- New `op-succinct` binary version

Based on what's included:

- Contract changes → Upgrade the relevant contracts
- New verification keys → Update `aggregationVkey`, `rangeVkeyCommitment` and `rollupConfigHash` parameters
- New binary → Upgrade Docker images

### Upgrade Contract

1. Check out the latest release of `op-succinct` from [here](https://github.com/succinctlabs/op-succinct/releases).
2. Follow the instructions [here](./contracts/upgrade.md) to upgrade the relevant contracts.

### Update Contract Parameters

If you just need to update the `aggregationVkey`, `rangeVkeyCommitment` or `rollupConfigHash` parameters and not upgrade the contract itself, follow these steps:

1. Check out the latest release of `op-succinct` from [here](https://github.com/succinctlabs/op-succinct/releases).
2. Follow the instructions [here](./contracts/update-parameters.md) to update the parameters of the relevant contracts.

### Upgrade Docker Images

If you're using Docker, you can use the images associated with the latest release of OP Succinct from GitHub Container Registry (GHCR) here: https://github.com/succinctlabs/op-succinct/pkgs/container/op-succinct%2Fop-succinct.

For example, to use version `v2.0.0`, you can use `ghcr.io/succinctlabs/op-succinct:v2.0.0`.
