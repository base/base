# Upgrading to new `op-succinct` version

Each new release of `op-succinct` will specify if it includes:

- New verification keys
- Contract changes 
- New `op-succinct` binary version

Based on what's included:

- Contract changes → Upgrade the `OPSuccinctL2OutputOracle` contract
- New verification keys → Update `aggregationVkey`, `rangeVkeyCommitment` and `rollupConfigHash` parameters
- New binary → Upgrade Docker images

### Upgrade Docker Containers

If you're using Docker, upgrade your containers to use the latest version of `op-succinct` by checking out the [latest release](https://github.com/succinctlabs/op-succinct/releases). 

Docker images are not built for releases, but we support a `docker compose` setup for the latest version of `op-succinct`.

### Upgrade Contract

1. Check out the latest release of `op-succinct` from [here](https://github.com/succinctlabs/op-succinct/releases).
2. Follow the instructions [here](../contracts/upgrade.md) to upgrade the `OPSuccinctL2OutputOracle` contract.


Note: As of release `beta-v0.3.0`, the `aggregationVkey`, `rangeVkeyCommitment` and `rollupConfigHash` are upgradeable without re-initializing the contract.

### Update Contract Parameters

If you just need to update the `aggregationVkey`, `rangeVkeyCommitment` or `rollupConfigHash` parameters and not upgrade the contract itself, follow these steps:

1. Check out the latest release of `op-succinct` from [here](https://github.com/succinctlabs/op-succinct/releases).
2. Follow the instructions [here](../contracts/update-parameters.md) to update the parameters of the `OPSuccinctL2OutputOracle` contract.
