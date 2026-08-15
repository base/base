# `baseup`

Installer for Base release binaries published from this repository.

## Quick Install

Use the raw GitHub bootstrap from `main`:

```bash
curl -fsSL https://raw.githubusercontent.com/base/base/main/baseup/install | bash
```

## Usage

```bash
baseup                                # Install the latest release binaries
baseup -i v0.6.0                      # Install a specific release tag
baseup --bin base                     # Install only the unified base binary
baseup --bin base-reth-node           # Install only the node binary
baseup --bin base-consensus           # Install only the consensus binary
baseup --bin basectl                  # Install only basectl
baseup --bin all                      # Install all published binaries
baseup verify-release                 # Verify latest published release archive signatures without installing
baseup verify-release -i v1.0.0       # Verify published archive signatures for a specific release tag
baseup -v                             # Print the baseup installer version
baseup --update                       # Update baseup itself
baseup --help                         # Show help
```

## Installed Binaries

By default, `baseup` installs every binary this repo publishes in GitHub releases today:

- `base`
- `base-reth-node`
- `base-consensus`
- `basectl`

## Verification

`baseup` verifies every release archive before installing it:

- downloads `<binary>-<version>-<target>.tar.gz`
- checks `<archive>.sha256`
- imports the pinned Base Releases public key into a temporary GPG keyring
- verifies `<archive>.asc` with GPG and requires fingerprint `5EFE7BCFCD85682711F9FC30904841FFEBD38BAD`
- verifies GitHub SLSA provenance when `gh` is installed and authenticated

Use `--unsafe-skip-verify` only for local testing; checksum verification is still required.

To validate release artifacts without installing binaries, run:

```bash
baseup verify-release
```

By default, `verify-release` fetches the GitHub release asset list and checks every published archive matching the binaries and targets that `baseup` supports. Missing binary/target archives are skipped. If an archive is published, its `.sha256` and `.asc` sidecars are required and must verify. Use `--bin` or `--target` to narrow the check; the command fails if the filters match no published archives.

## Supported Targets

`baseup` matches the release workflow in this repo:

- Linux: `x86_64`, `arm64`
- macOS: Apple Silicon (`arm64`)

## Installation Directory

Default: `~/.base/bin`

`baseup` installs only to user-writable directories and does not use `sudo`.

Customize with:

```bash
BASEUP_HOME=/custom/path baseup
```

or

```bash
BASE_BIN_DIR=/custom/path/bin baseup
```

## Hosting

The scripts pull directly from `main` in this repository on `raw.githubusercontent.com`:

- bootstrap uses `https://raw.githubusercontent.com/base/base/main/baseup/install`
- self-update defaults to `https://raw.githubusercontent.com/base/base/main/baseup/baseup`
