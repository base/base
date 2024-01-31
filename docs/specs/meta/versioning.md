# Versioning

**Table of Contents**

<!-- toc -->

## Go modules

Go modules that are currently versioned:

```text
./op-service
./op-bindings
./op-batcher
./op-node
./op-proposer
./op-e2e
```

Go modules which are not yet versioned:

```text
./indexer          (changesets)
./proxyd           (changesets)
```

### versioning process

Since changesets versioning is not compatible with Go we are moving away from it.
Starting with new bedrock modules, Go-compatible tags will be used,
formatted as `modulename/vX.Y.Z` where `vX.Y.Z` is semver.

## Typescript

See Changesets.
