# base-genesis

The hidden, always-built `base genesis` command generates the fixed, single-chain
Base development network. Contract execution, beacon-state construction, and
validator-key generation are in-process. Runtime needs only the Base binary and a
built contract directory: no compiler, Foundry, Go, RPC connection, or subprocess.

```sh
just contracts                 # Prepare/cache .contracts/artifacts (Docker on cache miss)
base genesis --artifacts .contracts/artifacts \
  --output-dir .devnet/l1/configs --l2-output-dir .devnet/l2/configs \
  --azul-block 20 --beryl-block 21 --cobalt-block 22 --denim-block 27
just genesis-test              # Includes artifact-dependent tests
just devnet up                 # Builds the shared image and starts the HA devnet
just devnet smoke              # Transfers, blob transaction, and portal deposit
```

Compose's setup, conductor helper, and nodes share `base-devnet:local`. The
`base-devnet` Docker stage packages only the unified binary with contracts at
`/opt/base/contracts`, selected through `BASE_GENESIS_ARTIFACTS`. The binary is
unchanged. Operator builds stop at `base` and do not depend on the contracts stage.
Ordinary devnet startup needs no host artifact export or bind mount.

System tests prepare the default local export on demand using Python 3, then call
`GenesisOutput::generate` directly; they have no setup container. Setting
`BASE_GENESIS_ARTIFACTS` selects an existing export without provisioning or
overwriting it. Standalone library callers pass `ContractArtifacts` explicitly;
the generator never downloads artifacts, invokes tools, or reads the environment.

## Contract updates

1. Change the full commit in `etc/upstream-pins/contracts.rev`.
2. Run `just contracts`. On a cache miss, the build-only stage
   clones **base/contracts**, runs its `just deps` and `just build-no-tests`, and
   exports ordinary ABI/bytecode JSON plus its preinstall/predeploy constants.
   The export is replaced only after a successful build, so removed contracts cannot
   linger and failures preserve the previous cache. The build-only `just` binary is
   pinned and its release archive is SHA-256 verified.
3. Run `just genesis-test` and `just devnet up`; verify advancing L1/L2 heads,
   transfers through the client and builder, and an L1 portal deposit on L2.

The preparation helper locks across concurrent test processes and verifies the
revision, build/export helper inputs, complete file list, and artifact checksums
before reuse. A cache hit does not invoke Docker or access the network.
`just contracts-rebuild` forces a fresh export. This artifact-cache lock is separate
from the generator's output lock.

The smoke command needs an EIP-7594-capable `cast` (tested with 1.7.1) for its L1
blob test. This is a host testing tool, not a generator or runtime dependency.

There are no checked-in contract archives, allocation fixtures, or generated SSZ.
An existing compiled checkout can also be exported with
`python3 etc/scripts/devnet/export-contracts.py CHECKOUT EMPTY_OUTPUT REVISION`.
The exporter fails on ambiguous compiler profiles or changed constant formats.
The Rust ABI encoder fails on changed constructor/initializer interfaces rather
than silently deploying against an old ABI. Semantically changed contracts still
require reviewing the small deployment sequence and testing the resulting chain.

`Deployment` follows the core `scripts/deploy/SystemDeploy.s.sol` and
`scripts/L2Genesis.s.sol` rules in Base contracts. Revm executes real CREATE/CREATE2
constructors and initializer calls; there is no Solidity-script interpreter or
cheatcode host. Only protocol-defined L2 proxy/code/storage setup is performed
directly. The deployment helper `AddressManagerDeployer` is ordinary constructor
bytecode, not a runtime script. Workspace Revm/Alloy versions are used unchanged.

The ordinary devnet excludes OPCM, Cannon/MIPS, old fault games, `ETHLockbox` and the
optional multiproof/verifier stack. It retains Base's `SuperchainConfig` proxy:
`SystemConfig` still needs it for guardian and pause checks. This is not a
production deployer, and does not configure proving/withdrawal games. The local
Nitro workflow remains a separate deployment layer.

## Upgrade signals and configuration

The **real Base `ProtocolVersions`** registry is deployed and initialized with the
ordered genesis schedule and minimum version. Bootstrap execution uses timestamp
zero, before exporting the chosen genesis timestamp. Live ownership, one-hour
notice/freeze windows, ordering, and schedule commitments are not weakened.
`upgrade-signal.env` points nodes at that registry; the devnet helper uses
`setTimestamp`, not the test mock's unrestricted `setSchedule` API. Already-active
forks cannot be moved. `MockProtocolVersions` remains only for explicit node tests.
`UPGRADE_SIGNAL_PREINSTALL=false` disables writing the runtime signal environment;
it does not replace or remove the core deployed registry.

Supported inputs are output/artifact locations, chain IDs, role/peer identities,
slot duration, activation administrator, six upgrade block settings, and signal
settings. Existing environment names are preserved. A nonzero
`BASE_DEVNET_TIMESTAMP` and nonzero 32-byte `BASE_DEVNET_SALT` make chain state
reproducible; JWTs and encrypted keystores remain randomized. Use `base genesis --help` for the CLI.

Cobalt starts the 200ms cadence. Subsequent upgrade blocks must align to whole
seconds: Cobalt 22 / Denim 27 / Zenith 102 is valid, Denim 25 is not. Genesis hashes
are computed from the final exported state and configuration.

Isthmus and Jovian are active at genesis by default. An explicit `--isthmus-block`
also moves Jovian to that block, keeping the registry and execution/rollup fork
schedules ordered. Deferred activation uses Holocene genesis fee parameters until
Jovian activates, and leaves the corresponding `GasPriceOracle` flags unset for
the canonical upgrade deposits. Nonzero registry timestamps must be ordered and
meet the notice period read from the pinned `ProtocolVersions.MIN_NOTICE`, measured
from bootstrap timestamp zero. A scheduled upgrade requires a nonzero minimum
protocol version.

The initial L2 minimum base fee is 1 gwei in `SystemConfig`, the rollup bootstrap
configuration, and Jovian genesis extra data. This is distinct from the portal's
deposit resource-metering fee floor. Before Jovian, the header uses Holocene fee
encoding instead.

Completed files are checksummed and bound to both inputs and artifact contents.
Changed, partial, or legacy output requires explicit regeneration (`just devnet
up` clears the disposable network). A lock prevents concurrent generation into
the same L1 directory; the generator never deletes caller-owned node datadirs.
Validator keys are written only under `cl/validator_data`; legacy mnemonic,
deposit-block, and duplicate `validator_keys` files are no longer generated.
All development keys are public and must never control real funds.

Beacon generation uses Lighthouse v8.2.2, matching the devnet client, and the
existing minimal/Fulu one-validator preset. Its epoch-zero `BLOB_SCHEDULE` matches
the EL's BPO2 maximum of 21 blobs. Beacon configuration uses standard YAML.
Its libraries provide EIP-2333/2334 key derivation and EIP-2335 encryption.
The EL/CL templates and peer defaults are text assets; no deployment-specific
binary fixture is needed to update contracts.
