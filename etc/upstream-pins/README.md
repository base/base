# Reth workspace pin

`reth.toml` is the source of truth for every git-based `reth-*` crate in
`[workspace.dependencies]`. Do not edit those Cargo.toml lines by hand.

## Commands

```bash
just pin-reth
just check-reth-pin
just pin-reth-test
just reth-prepare-release --upstream v2.5.1 --pr 26708 --pr 26766
just reth-drop-pin --release v2.6.0
```

`--pr 26708` is a PR on `--upstream-repo` (default `paradigmxyz/reth`). A
full GitHub pull URL is fetched from that repository. Allowed sources are
`--upstream-repo` and `--fork` (default `base/reth`). Use a `base/reth` URL
for Base-specific patches instead of opening them upstream.

`--line` is inferred from `releases/<line>` when you are on that branch;
otherwise pass it. `--fork` can point at a personal fork until `base/reth`
is the tagged backport repo.

Preview without pushing or opening a PR:

```bash
just reth-prepare-release --upstream v2.5.1 --pr 26708 --line v1.3.0 \
  --dry-run --skip-push --no-commit --no-pr
```

## Workflow

1. Decide the official Reth tag and the PRs to carry.
2. Run `just reth-prepare-release`. It squashes each PR onto that tag in
   the fork, publishes `vX.Y.Z-base.N`, rewrites this manifest and
   Cargo.toml, and opens a `base/base` PR.
3. Review and merge that PR. After merge, `just check-reth-pin` is the
   local check that Cargo still matches this file.
4. When an official Reth release contains **every** carried *upstream* PR
   and no `base/reth` PRs remain, run `just reth-drop-pin --release vX.Y.Z`.

The pin commit is created from `origin/releases/<line>` when that branch
exists, otherwise `origin/main`. Override with `--base-branch`. Extra
commits on the local HEAD are not included.

A squash conflict is the only hard stop. Fix the PR against that official
tag and rerun. There is no conflict-resolution loop.

## Limitations

These are intentional. They are not handled automatically:

- **`--pr` replaces the patch set.** Pass every PR you still want to
  carry. Apply refuses to drop a carried PR unless it is listed in
  `[[resolved]]`, so omitting one fails instead of appending.
- **`reth-drop-pin` is all-or-nothing for upstream PRs.** It records every
  `[[patches]]` entry as contained in `--release`. If only some landed,
  rerun `reth-prepare-release` against the new official tag with the
  leftover `--pr` list, and add `[[resolved]]` rows for the PRs that
  landed. Drop refuses while any `base/reth` PR remains in `[[patches]]`.
- **PRs already in `--upstream`.** If a PR is already in the official tag,
  squash hits an empty diff and aborts. Do not pass it.
- **Fork-only retirement.** Stopping a `base/reth` PR is manual. Do not
  record it against an official Reth tag; those patches never land in
  `paradigmxyz/reth`.
- **CI.** `just check::reth-pin` exists but is not part of
  `just check::all` or GitHub Actions.
- **Generated PR body.** The helper does not run `just pin-reth-test` or
  `cargo check`. Add Heimdall CMS fields if the target repo requires
  them.
- **Custom `--fork`.** `deny.toml` allows `paradigmxyz/reth` and
  `base/reth`. A different fork URL needs its own `allow-git` entry.
- **`base/reth` tags.** Protect `v*-base.*` tags so they cannot be moved
  after publish.
