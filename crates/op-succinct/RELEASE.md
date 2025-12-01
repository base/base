# Release Process

This document describes the release process for op-succinct.

## Versioning

op-succinct follows [Semantic Versioning](https://semver.org/):

- **MAJOR**: Breaking changes to contracts, APIs, or configuration
- **MINOR**: New features, backward-compatible changes
- **PATCH**: Bug fixes, performance improvements

### Version Variants

- `vX.Y.Z` - Standard release
- `vX.Y.Z-rc.N` - Release candidate

## Pre-Release Checklist

Before creating a release:

- [ ] All CI checks pass on `main`
- [ ] Update version in `Cargo.toml` (workspace.package.version) to match the new release version
- [ ] Run `just build-elfs` to build the ELF files

## Creating a Release

### 1. Update Version

Update the workspace version in `Cargo.toml`:

```toml
[workspace.package]
version = "X.Y.Z"
```

### 2. Build ELF Files

Run `just build-elfs` to build the ELF files:

```bash
just build-elfs
```

### 3. Create Version Bump PR

```bash
git checkout -b release/vX.Y.Z
git add Cargo.toml Cargo.lock elf/
git commit -m "chore: bump to vX.Y.Z"
git push origin release/vX.Y.Z
```

Merge the PR after review.

### 4. Create and Push Tag

```bash
git checkout main
git pull origin main
git tag vX.Y.Z
git push origin vX.Y.Z
```

### 5. Create GitHub Release

1. Go to [Releases](https://github.com/succinctlabs/op-succinct/releases)
2. Click "Draft a new release"
3. Select the tag `vX.Y.Z`
4. Click "Generate release notes" for auto-generated changelog
5. Run `just vkeys` and copy the markdown table output into the release notes
6. Review and publish release

## What Gets Published

When a tag is pushed:

**Validity Mode (OP Succinct)**:
- `ghcr.io/succinctlabs/op-succinct/op-succinct:vX.Y.Z`
- `ghcr.io/succinctlabs/op-succinct/op-succinct-celestia:vX.Y.Z`
- `ghcr.io/succinctlabs/op-succinct/op-succinct-eigenda:vX.Y.Z`

**Fault Proof Mode (OP Succinct Lite)**:
- `ghcr.io/succinctlabs/op-succinct/lite-proposer:vX.Y.Z`
- `ghcr.io/succinctlabs/op-succinct/lite-proposer-celestia:vX.Y.Z`
- `ghcr.io/succinctlabs/op-succinct/lite-proposer-eigenda:vX.Y.Z`
- `ghcr.io/succinctlabs/op-succinct/lite-challenger:vX.Y.Z`

## Release Candidates

For testing before a stable release:

```bash
git tag vX.Y.Z-rc.1
git push origin vX.Y.Z-rc.1
```

Mark the GitHub release as "pre-release".

## Changelog

See [GitHub Releases](https://github.com/succinctlabs/op-succinct/releases) for the full changelog.
