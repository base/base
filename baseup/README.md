# `baseup`

Installer for Base release binaries published from this repository.

## Quick Install

Use the raw GitHub bootstrap immediately:

```bash
curl -fsSL https://raw.githubusercontent.com/base/base/main/baseup/install | bash
```

If GitHub Pages is enabled for this repository, the same bootstrap can be served from:

```bash
curl -fsSL https://base.github.io/base/install | bash
```

## Usage

```bash
baseup                                # Install the latest release binaries
baseup -i v0.6.0                      # Install a specific release tag
baseup --bin base-reth-node           # Install only the node binary
baseup --bin basectl                  # Install only basectl
baseup --bin all                      # Install all published binaries
baseup -v                             # Print the baseup installer version
baseup --update                       # Update baseup itself
baseup --help                         # Show help
```

## Installed Binaries

By default, `baseup` installs every binary this repo publishes in GitHub releases today:

- `base-reth-node`
- `basectl`

## Supported Targets

`baseup` matches the release workflow in this repo:

- Linux: `x86_64`, `arm64`
- macOS: Apple Silicon (`arm64`)

## Installation Directory

Default: `~/.base/bin`

Customize with:

```bash
BASEUP_HOME=/custom/path baseup
```

or

```bash
BASE_BIN_DIR=/custom/path/bin baseup
```

## Hosting

The scripts are written to work with GitHub-hosted URLs:

- bootstrap and self-update default to `raw.githubusercontent.com`
- `.github/workflows/baseup-pages.yml` can publish `/install` and `/baseup` to GitHub Pages for a shorter URL
